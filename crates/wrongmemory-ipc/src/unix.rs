//! Unix private endpoint (WM-ADR-004, WM-802).
//!
//! The daemon listens on a Unix domain socket inside the data root's
//! owner-only `runtime/` directory (mode 0700; a pre-existing directory owned
//! by another account is refused). Three properties mirror the Windows pipe:
//!
//! - **One daemon per data root.** An exclusive `flock` on
//!   `runtime/daemon.lock` is the OS-backed guard. The kernel releases it when
//!   the holder exits, so a stale socket file left by a crash never blocks a
//!   restart, and two starters never both bind.
//! - **Only the owner may connect.** Besides the directory mode, the daemon
//!   checks every peer's uid (`SO_PEERCRED` / `getpeereid`) and drops
//!   connections from other accounts before reading a byte.
//! - **Clients verify the server.** A client refuses to send any credential to
//!   a socket whose server runs as another account.

use std::fs::{File, OpenOptions, TryLockError};
use std::io;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::Duration;

use tokio::net::{UnixListener, UnixStream};

/// Longest socket path the platform accepts, excluding the terminating NUL
/// (`sizeof(sockaddr_un.sun_path) - 1`).
#[cfg(any(target_os = "linux", target_os = "android"))]
pub const MAX_SOCKET_PATH: usize = 107;
#[cfg(not(any(target_os = "linux", target_os = "android")))]
pub const MAX_SOCKET_PATH: usize = 103;

/// Lock file next to the socket; holding it is being the daemon.
pub const LOCK_FILE: &str = "daemon.lock";

/// Real uid of this process.
pub fn current_uid() -> u32 {
    // SAFETY: getuid has no preconditions and cannot fail.
    unsafe { libc::getuid() }
}

/// Whether process `pid` exists. A process this user may not signal counts as
/// running (conservative), like the Windows check.
pub fn process_alive(pid: u32) -> bool {
    let Ok(pid) = libc::pid_t::try_from(pid) else {
        return false;
    };
    if pid <= 0 {
        return false;
    }
    // SAFETY: signal 0 only performs the existence and permission checks.
    if unsafe { libc::kill(pid, 0) } == 0 {
        return true;
    }
    io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

/// Refuse a socket path the kernel would truncate or reject.
pub fn check_socket_path(path: &Path) -> io::Result<()> {
    let len = path.as_os_str().len();
    if len > MAX_SOCKET_PATH {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "socket path {} is {len} bytes; this platform allows {MAX_SOCKET_PATH}. Use a shorter data root (--data-root or WRONGMEMORY_HOME)",
                path.display()
            ),
        ));
    }
    Ok(())
}

/// Why binding the daemon socket failed.
#[derive(Debug)]
pub enum BindError {
    /// Another process holds the data root's daemon lock. The runtime
    /// directory is private to this user, so the holder is this user's daemon
    /// (running, starting, or draining).
    Held,
    Io(io::Error),
}

impl std::fmt::Display for BindError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BindError::Held => f.write_str("another daemon holds this data root"),
            BindError::Io(e) => e.fmt(f),
        }
    }
}

impl std::error::Error for BindError {}

impl From<io::Error> for BindError {
    fn from(e: io::Error) -> Self {
        BindError::Io(e)
    }
}

/// The daemon's listening socket plus the lock that makes it the only one.
///
/// Dropping it removes the socket file while the lock is still held, then
/// releases the lock, so a successor never finds a socket it did not bind.
pub struct SocketListener {
    listener: UnixListener,
    path: PathBuf,
    uid: u32,
    // Declared last: fields drop in order, so the lock outlives the socket.
    _lock: File,
}

impl SocketListener {
    /// Take the data root's daemon lock and bind `path`. Must run inside a
    /// Tokio runtime.
    pub fn bind(path: &Path) -> Result<Self, BindError> {
        check_socket_path(path)?;
        let dir = path.parent().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "socket path has no parent")
        })?;
        crate::endpoint::create_private_dir(dir)?;
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(dir.join(LOCK_FILE))?;
        match lock.try_lock() {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => return Err(BindError::Held),
            Err(TryLockError::Error(e)) => return Err(e.into()),
        }
        // The lock is ours: whatever sits at the path is a predecessor's
        // leftover (a crash skips the cleanup in `Drop`).
        match std::fs::remove_file(path) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e.into()),
            _ => {}
        }
        let listener = UnixListener::bind(path)?;
        // The directory is already private; the socket's own mode is a
        // second barrier.
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
        Ok(Self {
            listener,
            path: path.to_path_buf(),
            uid: current_uid(),
            _lock: lock,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Accept one connection. A peer running as another account is closed
    /// and reported as `PermissionDenied`; the caller logs it and goes on.
    pub async fn accept(&self) -> io::Result<UnixStream> {
        let (stream, _) = self.listener.accept().await?;
        let cred = stream.peer_cred()?;
        if cred.uid() != self.uid {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!(
                    "refused a connection from uid {} (pid {:?})",
                    cred.uid(),
                    cred.pid()
                ),
            ));
        }
        Ok(stream)
    }
}

impl Drop for SocketListener {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Refuse a socket whose server does not run as the current user.
pub fn verify_same_user_server(stream: &UnixStream) -> io::Result<()> {
    let cred = stream.peer_cred()?;
    let me = current_uid();
    if cred.uid() != me {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "socket server (pid {:?}) runs as uid {}, not the current user {me}; refusing to send credentials",
                cred.pid(),
                cred.uid()
            ),
        ));
    }
    Ok(())
}

/// Connect to the daemon socket and verify that its server runs as the current
/// user before returning the stream.
pub async fn connect(path: &str, deadline: Duration) -> io::Result<UnixStream> {
    let stream = tokio::time::timeout(deadline, UnixStream::connect(path))
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "socket connect timed out"))??;
    verify_same_user_server(&stream)?;
    Ok(stream)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn own_pid_is_alive_and_a_reaped_child_is_not() {
        assert!(process_alive(std::process::id()));
        let mut child = std::process::Command::new("true").spawn().unwrap();
        let pid = child.id();
        child.wait().unwrap();
        assert!(!process_alive(pid));
        assert!(!process_alive(0));
        assert!(!process_alive(u32::MAX));
    }

    #[test]
    fn overlong_socket_path_is_refused_with_a_hint() {
        let long = PathBuf::from(format!("/tmp/{}/daemon.sock", "x".repeat(MAX_SOCKET_PATH)));
        let e = check_socket_path(&long).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::InvalidInput);
        assert!(e.to_string().contains("data root"));
        assert!(check_socket_path(Path::new("/tmp/r/runtime/daemon.sock")).is_ok());
    }

    #[tokio::test]
    async fn second_bind_is_held_and_a_stale_socket_is_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("runtime").join("daemon.sock");
        let first = SocketListener::bind(&sock).unwrap();
        assert!(matches!(SocketListener::bind(&sock), Err(BindError::Held)));
        let mode = std::fs::metadata(dir.path().join("runtime"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o700);
        let mode = std::fs::metadata(&sock).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        drop(first);
        assert!(!sock.exists(), "drop removes the socket file");

        // A crashed daemon leaves its socket file behind but not its lock.
        std::os::unix::net::UnixListener::bind(&sock).unwrap();
        assert!(sock.exists());
        let again = bind_when_free(&sock).await;
        drop(again);
    }

    /// A `fork` elsewhere in this test binary shares the lock's open file
    /// description until its `exec`, so a just-released lock can read as held
    /// for that instant.
    async fn bind_when_free(sock: &Path) -> SocketListener {
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        loop {
            match SocketListener::bind(sock) {
                Ok(l) => return l,
                Err(BindError::Held) if std::time::Instant::now() < deadline => {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
                Err(e) => panic!("bind: {e}"),
            }
        }
    }

    #[tokio::test]
    async fn verified_connect_reaches_own_server() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("runtime").join("daemon.sock");
        let listener = SocketListener::bind(&sock).unwrap();
        let path = sock.to_string_lossy().into_owned();
        let (client, server) =
            tokio::join!(connect(&path, Duration::from_secs(5)), listener.accept());
        client.unwrap();
        server.unwrap();
    }

    #[tokio::test]
    async fn connect_to_a_missing_socket_fails() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nothing.sock");
        assert!(
            connect(&path.to_string_lossy(), Duration::from_secs(1))
                .await
                .is_err()
        );
    }
}
