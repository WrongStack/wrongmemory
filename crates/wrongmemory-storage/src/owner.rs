//! OS-backed owner guard per canonical store (architecture §7, WM-INV-01).
//!
//! An exclusive lock on `<store-dir>/wrongmemory-owner.lock` held for the
//! lifetime of the service's writable handle. Only cooperating
//! implementations honor it: an unmodified SAGE project server ignores it,
//! which is why adoption of a store with live legacy metadata is refused
//! rather than "protected" by this lock.

use std::fs::{File, OpenOptions, TryLockError};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

pub const OWNER_LOCK_FILE: &str = "wrongmemory-owner.lock";

#[derive(Debug)]
pub struct OwnerGuard {
    file: File,
    path: PathBuf,
}

impl OwnerGuard {
    /// Try to take ownership. `WouldBlock` means another cooperating owner
    /// (another daemon/data root) holds the store.
    pub fn acquire(store: &Path, epoch: i64, instance_id: &str) -> io::Result<Self> {
        let dir = store
            .parent()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "store has no parent"))?;
        let path = dir.join(OWNER_LOCK_FILE);
        let mut file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&path)?;
        match file.try_lock() {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => {
                return Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    format!(
                        "{} is owned by another WrongMemory instance",
                        store.display()
                    ),
                ));
            }
            Err(TryLockError::Error(e)) => return Err(e),
        }
        // Diagnostic content only; the lock, not the content, is authoritative.
        file.set_len(0)?;
        writeln!(
            file,
            "{{\"pid\":{},\"instanceId\":\"{instance_id}\",\"epoch\":{epoch}}}",
            std::process::id()
        )?;
        file.flush()?;
        Ok(Self { file, path })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for OwnerGuard {
    fn drop(&mut self) {
        let _ = self.file.unlock();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn second_owner_is_refused_until_release() {
        let tmp = tempfile::tempdir().unwrap();
        let store = tmp.path().join("sage.db");
        let a = OwnerGuard::acquire(&store, 1, "a").unwrap();
        let err = OwnerGuard::acquire(&store, 2, "b").unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::WouldBlock);
        drop(a);
        let _b = OwnerGuard::acquire(&store, 2, "b").unwrap();
    }
}
