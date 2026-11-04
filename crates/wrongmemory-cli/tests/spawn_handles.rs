//! An auto-started daemon must inherit no handle from the process chain that
//! launched it. Clearing the launcher's own stdio inheritance is not enough:
//! a coding tool (or test harness) can leak further inheritable handles into
//! `wrongmemory`, and `CreateProcess` with inheritance on (or `exec` of a
//! descriptor without close-on-exec) would pass them to the long-lived
//! daemon, keeping the tool's pipes open after the CLI exits.

#![cfg(any(windows, unix))]

use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

/// Make `writer` inheritable by every child this process spawns, as a leaky
/// parent would.
#[cfg(windows)]
fn leak_into_children(writer: &std::io::PipeWriter) {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Foundation::{HANDLE_FLAG_INHERIT, SetHandleInformation};
    // SAFETY: a valid, owned pipe handle; only the inherit flag changes.
    let ok = unsafe {
        SetHandleInformation(
            writer.as_raw_handle(),
            HANDLE_FLAG_INHERIT,
            HANDLE_FLAG_INHERIT,
        )
    };
    assert_ne!(ok, 0, "SetHandleInformation failed");
}

#[cfg(unix)]
fn leak_into_children(writer: &std::io::PipeWriter) {
    use std::os::fd::AsRawFd;
    let fd = writer.as_raw_fd();
    // SAFETY: a valid, owned descriptor; only its close-on-exec flag changes.
    let ok = unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFD);
        flags >= 0 && libc::fcntl(fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) == 0
    };
    assert!(ok, "clearing FD_CLOEXEC failed");
}

fn wm(home: &Path) -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_wrongmemory"));
    c.arg("--data-root")
        .arg(home)
        .env_remove("WRONGMEMORY_FAULT");
    c
}

struct StopDaemon<'a>(&'a Path);

impl Drop for StopDaemon<'_> {
    fn drop(&mut self) {
        let _ = wm(self.0).args(["daemon", "stop"]).output();
    }
}

#[test]
fn autostarted_daemon_inherits_no_handle_from_its_launcher() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let _stop = StopDaemon(&home);

    // An extra inheritable handle in the launcher chain: the write end of a
    // pipe, exactly what a parent's leaked stdout pipe looks like.
    let (mut reader, writer) = std::io::pipe().unwrap();
    leak_into_children(&writer);

    // `daemon start` inherits the write end and starts the daemon.
    let out = wm(&home)
        .args(["daemon", "start"])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "daemon start: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    drop(writer);

    // Only the daemon could still hold the write end now: EOF proves it does not.
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = tx.send(reader.read_to_end(&mut buf).map(|_| buf.len()));
    });
    let got = rx.recv_timeout(Duration::from_secs(10));
    assert!(
        matches!(got, Ok(Ok(0))),
        "the daemon still holds a handle inherited from its launcher ({got:?})"
    );
}
