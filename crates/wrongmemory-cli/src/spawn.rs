//! Daemon discovery and on-demand start (mcp-and-client-integration §2.1).
//!
//! The daemon is started detached from the caller so that a coding tool's
//! process-tree cleanup (Windows Job Objects) does not kill the shared
//! service when the bridge exits.

use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use wrongmemory_ipc::{CallError, Client, DataRoot};

/// `wrongmemoryd` next to the running `wrongmemory` executable.
pub fn daemon_exe() -> anyhow::Result<PathBuf> {
    let me = std::env::current_exe()?;
    let dir = me
        .parent()
        .ok_or_else(|| anyhow::anyhow!("executable has no parent directory"))?;
    let exe = dir.join(format!("wrongmemoryd{}", std::env::consts::EXE_SUFFIX));
    if !exe.exists() {
        anyhow::bail!("{} not found next to {}", exe.display(), me.display());
    }
    Ok(exe)
}

pub fn spawn_daemon(root: &DataRoot) -> anyhow::Result<u32> {
    let exe = daemon_exe()?;
    let mut cmd = Command::new(&exe);
    cmd.arg("--data-root")
        .arg(root.path())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // CreateProcess inherits every inheritable handle. Our own stdio
        // handles are the coding tool's pipes; a long-lived daemon holding a
        // copy would keep the tool's stdout pipe open forever after the
        // bridge exits. Make them non-inheritable before spawning.
        disable_std_handle_inheritance();
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        const CREATE_BREAKAWAY_FROM_JOB: u32 = 0x0100_0000;
        let base = DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW;
        cmd.creation_flags(base | CREATE_BREAKAWAY_FROM_JOB);
        match cmd.spawn() {
            Ok(child) => return Ok(child.id()),
            // A job that forbids breakaway rejects the flag (access denied):
            // fall back to a plain detached start.
            Err(_) => {
                cmd.creation_flags(base);
            }
        }
    }
    Ok(cmd.spawn()?.id())
}

#[cfg(windows)]
fn disable_std_handle_inheritance() {
    use windows_sys::Win32::Foundation::{
        HANDLE_FLAG_INHERIT, INVALID_HANDLE_VALUE, SetHandleInformation,
    };
    use windows_sys::Win32::System::Console::{
        GetStdHandle, STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
    };
    for which in [STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE] {
        // SAFETY: GetStdHandle has no preconditions; SetHandleInformation is
        // only called on a non-null, valid handle and only clears a flag.
        unsafe {
            let h = GetStdHandle(which);
            if !h.is_null() && h != INVALID_HANDLE_VALUE {
                SetHandleInformation(h, HANDLE_FLAG_INHERIT, 0);
            }
        }
    }
}

/// Connect with the host credential, starting the daemon when allowed.
pub async fn connect_host(
    root: &DataRoot,
    label: &str,
    autostart: bool,
) -> Result<Client, CallError> {
    match Client::connect_host(root, label).await {
        Err(CallError::Unavailable(_)) if autostart => {
            start_and_wait(root).await?;
            Client::connect_host(root, label).await
        }
        other => other,
    }
}

/// Connect with a grant credential, starting the daemon when needed.
pub async fn connect_grant(
    root: &DataRoot,
    grant_id: &str,
    label: &str,
    autostart: bool,
) -> Result<Client, CallError> {
    match Client::connect_grant(root, grant_id, label).await {
        Err(CallError::Unavailable(m)) if autostart && !m.contains("credential") => {
            start_and_wait(root).await?;
            Client::connect_grant(root, grant_id, label).await
        }
        other => other,
    }
}

pub async fn start_and_wait(root: &DataRoot) -> Result<(), CallError> {
    spawn_daemon(root).map_err(|e| CallError::Unavailable(format!("cannot start daemon: {e}")))?;
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match Client::connect_host(root, "wrongmemory-start-probe").await {
            Ok(_) => return Ok(()),
            Err(CallError::Unavailable(m)) => {
                if Instant::now() > deadline {
                    return Err(CallError::Unavailable(format!(
                        "daemon did not become ready within 10s: {m}"
                    )));
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            Err(e) => return Err(e),
        }
    }
}
