//! Local authenticated IPC for WrongMemory (WM-ADR-004).
//!
//! Windows uses a named pipe whose DACL admits only the current user's SID and
//! LocalSystem, with remote clients rejected. Unix uses a socket in the data
//! root's owner-only runtime directory, guarded by a `flock`, and the daemon
//! drops peers running as another uid. On both, a client verifies the serving
//! process runs as the same user before sending any credential.

pub mod client;
pub mod endpoint;
pub mod frame;
#[cfg(unix)]
pub mod unix;
#[cfg(windows)]
pub mod win;

pub use client::{CallError, Client};
pub use endpoint::{DataRoot, Discovery};
pub use frame::{FrameError, read_frame, write_frame};
