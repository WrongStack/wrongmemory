//! Daemon discovery and on-demand start (mcp-and-client-integration §2.1).
//!
//! The daemon is started detached from the caller so that a coding tool's
//! process-tree cleanup (Windows Job Objects) does not kill the shared
//! service when the bridge exits.

use std::path::PathBuf;
#[cfg(not(windows))]
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
    #[cfg(windows)]
    {
        win::spawn_detached(
            &exe,
            &[std::ffi::OsStr::new("--data-root"), root.path().as_os_str()],
        )
    }
    #[cfg(not(windows))]
    {
        let child = Command::new(&exe)
            .arg("--data-root")
            .arg(root.path())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?;
        Ok(child.id())
    }
}

/// Windows start without handle inheritance.
///
/// `std::process::Command` always calls `CreateProcessW` with
/// `bInheritHandles = TRUE`, so a long-lived daemon would inherit every
/// inheritable handle this CLI holds, including handles a coding tool or
/// harness leaked into it (not just our own stdio, which is all
/// `SetHandleInformation` on the std handles could cover). The daemon needs
/// none: its stdio is unused and it logs to its own file. So it is created
/// here with inheritance off, detached, in a new process group, without a
/// window, and outside the caller's job when the job allows breakaway.
#[cfg(windows)]
mod win {
    use std::ffi::OsStr;
    use std::os::windows::ffi::OsStrExt;
    use std::path::Path;

    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{
        CREATE_BREAKAWAY_FROM_JOB, CREATE_NEW_PROCESS_GROUP, CREATE_NO_WINDOW,
        CREATE_UNICODE_ENVIRONMENT, CreateProcessW, DETACHED_PROCESS, PROCESS_INFORMATION,
        STARTUPINFOW,
    };

    /// Quote one argument for `CommandLineToArgvW` / the MSVC runtime.
    pub(super) fn quote(arg: &OsStr, out: &mut Vec<u16>) {
        const SPACE: u16 = b' ' as u16;
        const TAB: u16 = b'\t' as u16;
        const QUOTE: u16 = b'"' as u16;
        const BACKSLASH: u16 = b'\\' as u16;
        let wide: Vec<u16> = arg.encode_wide().collect();
        let needs = wide.is_empty() || wide.iter().any(|&c| c == SPACE || c == TAB || c == QUOTE);
        if !needs {
            out.extend_from_slice(&wide);
            return;
        }
        out.push(QUOTE);
        let mut backslashes = 0usize;
        for &c in &wide {
            if c == BACKSLASH {
                backslashes += 1;
                continue;
            }
            // Backslashes are literal unless they precede a quote.
            let repeat = if c == QUOTE {
                2 * backslashes + 1
            } else {
                backslashes
            };
            out.extend(std::iter::repeat_n(BACKSLASH, repeat));
            backslashes = 0;
            out.push(c);
        }
        // Trailing backslashes precede the closing quote: double them.
        out.extend(std::iter::repeat_n(BACKSLASH, 2 * backslashes));
        out.push(QUOTE);
    }

    pub(super) fn spawn_detached(exe: &Path, args: &[&OsStr]) -> anyhow::Result<u32> {
        let mut cmdline = Vec::new();
        quote(exe.as_os_str(), &mut cmdline);
        for a in args {
            cmdline.push(b' ' as u16);
            quote(a, &mut cmdline);
        }
        cmdline.push(0);
        let app: Vec<u16> = exe
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();
        let base = DETACHED_PROCESS
            | CREATE_NEW_PROCESS_GROUP
            | CREATE_NO_WINDOW
            | CREATE_UNICODE_ENVIRONMENT;
        let mut last = std::io::Error::other("not started");
        for flags in [base | CREATE_BREAKAWAY_FROM_JOB, base] {
            // SAFETY: zeroed STARTUPINFOW/PROCESS_INFORMATION are valid
            // initial states; `cb` is set; both strings are NUL-terminated
            // and `cmdline` is a writable buffer as CreateProcessW requires.
            unsafe {
                let mut si: STARTUPINFOW = std::mem::zeroed();
                si.cb = std::mem::size_of::<STARTUPINFOW>() as u32;
                let mut pi: PROCESS_INFORMATION = std::mem::zeroed();
                let mut line = cmdline.clone();
                let ok = CreateProcessW(
                    app.as_ptr(),
                    line.as_mut_ptr(),
                    std::ptr::null(),
                    std::ptr::null(),
                    0, // bInheritHandles = FALSE
                    flags,
                    std::ptr::null(),
                    std::ptr::null(),
                    &si,
                    &mut pi,
                );
                if ok != 0 {
                    CloseHandle(pi.hThread);
                    CloseHandle(pi.hProcess);
                    return Ok(pi.dwProcessId);
                }
                // A job that forbids breakaway refuses the flag: retry without it.
                last = std::io::Error::last_os_error();
            }
        }
        Err(last.into())
    }
}

/// Connect with the host credential, starting the daemon when allowed.
pub async fn connect_host(
    root: &DataRoot,
    label: &str,
    autostart: bool,
) -> Result<Client, CallError> {
    match Client::connect_host(root, label).await {
        Err(e) if autostart && startable(&e) => {
            start_and_wait(root).await?;
            Client::connect_host(root, label).await
        }
        other => other,
    }
}

/// Prefix of the error `Client::connect_grant` returns when the grant's
/// secret is not in the data root.
const GRANT_SECRET_MISSING: &str = "cannot read credential for grant";

/// Whether a failed connect means "no ready daemon here (yet)" and the
/// start-and-wait path should run. Unavailable covers a missing endpoint and
/// a stale discovery record whose token a freshly bound daemon cannot prove
/// (another client is starting it right now). Transport/Protocol cover a
/// draining daemon that closed the connection mid-handshake. A missing
/// grant secret and remote (authentication) errors are final.
fn startable(e: &CallError) -> bool {
    match e {
        CallError::Unavailable(m) => !m.starts_with(GRANT_SECRET_MISSING),
        CallError::Transport(_) | CallError::Protocol(_) => true,
        CallError::Remote(_) => false,
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
        Err(e) if autostart && startable(&e) => {
            start_and_wait(root).await?;
            Client::connect_grant(root, grant_id, label).await
        }
        other => other,
    }
}

/// Start the daemon and wait until it answers with the host credential.
///
/// A start can lose to a daemon that is still draining (its pipe instances
/// hold the endpoint, so the new process exits as already running) or race
/// another client's start. Until a daemon answers, the start is retried
/// every second; extra starters exit without side effects.
pub async fn start_and_wait(root: &DataRoot) -> Result<(), CallError> {
    let spawn = || {
        spawn_daemon(root).map_err(|e| CallError::Unavailable(format!("cannot start daemon: {e}")))
    };
    spawn()?;
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut last_spawn = Instant::now();
    loop {
        match Client::connect_host(root, "wrongmemory-start-probe").await {
            Ok(_) => return Ok(()),
            // Unavailable: nothing answering yet, or a stale discovery record
            // the new daemon cannot prove. Transport/Protocol: a daemon that
            // closed the connection mid-handshake (a draining one).
            Err(e) if startable(&e) => {
                let m = e.to_string();
                if Instant::now() > deadline {
                    return Err(CallError::Unavailable(format!(
                        "daemon did not become ready within 10s: {m}"
                    )));
                }
                if last_spawn.elapsed() >= Duration::from_secs(1) {
                    spawn()?;
                    last_spawn = Instant::now();
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            Err(e) => return Err(e),
        }
    }
}

#[cfg(all(test, windows))]
mod quote_tests {
    use std::ffi::OsStr;

    fn q(s: &str) -> String {
        let mut out = Vec::new();
        super::win::quote(OsStr::new(s), &mut out);
        String::from_utf16(&out).unwrap()
    }

    #[test]
    fn arguments_survive_the_msvc_command_line_rules() {
        assert_eq!(q(r"C:\data"), r"C:\data");
        assert_eq!(q(r"C:\My Data\wm"), r#""C:\My Data\wm""#);
        // A trailing backslash before the closing quote is doubled.
        assert_eq!(q(r"C:\My Data\"), r#""C:\My Data\\""#);
        assert_eq!(q(r#"a"b"#), r#""a\"b""#);
        assert_eq!(q(r#"a\"b c"#), r#""a\\\"b c""#);
        assert_eq!(q(""), r#""""#);
    }
}
