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

/// Durable record, next to an adopted store, that WrongMemory serves it.
///
/// The owner lock lasts only as long as the daemon holds the store: a daemon
/// that stops (or is killed) releases it, and a host that reads only the
/// lock would then open the store locally as a second, uncoordinated writer.
/// The marker survives the daemon and tells a cooperating host to reach the
/// store through WrongMemory (starting the daemon on demand) instead. It is
/// written whenever the store is opened writable and removed only when the
/// project is detached. Its content names the project and the data root.
pub const ADOPTION_MARKER_FILE: &str = "wrongmemory-adopted.json";

fn marker_path(store: &Path) -> io::Result<PathBuf> {
    store
        .parent()
        .map(|d| d.join(ADOPTION_MARKER_FILE))
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "store has no parent"))
}

/// Write (or refresh) the adoption marker atomically.
///
/// The content goes to a fresh, uniquely named file created exclusively
/// (`create_new`), so a file or symlink planted in the project directory is
/// never followed or overwritten; the rename then replaces the marker entry
/// itself.
pub fn write_adoption_marker(store: &Path, project_id: &str, data_root: &Path) -> io::Result<()> {
    let path = marker_path(store)?;
    let body = serde_json::json!({
        "schemaVersion": 1,
        "projectId": project_id,
        "dataRoot": data_root.display().to_string(),
    });
    let tmp = path.with_file_name(format!(
        ".{ADOPTION_MARKER_FILE}.{}.tmp",
        ulid::Ulid::generate()
    ));
    let written = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&tmp)
        .and_then(|mut file| {
            file.write_all(format!("{body}\n").as_bytes())?;
            file.sync_all()
        })
        .and_then(|()| std::fs::rename(&tmp, &path));
    if written.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    written
}

/// The pid recorded in SAGE's project-server metadata (`server.json`) next
/// to `store`, if the file exists and names one. Whether that process still
/// runs is the caller's question.
pub fn legacy_server_pid(store: &Path) -> Option<u32> {
    let raw = std::fs::read_to_string(store.parent()?.join("server.json")).ok()?;
    let value: serde_json::Value = serde_json::from_str(&raw).ok()?;
    value.get("pid")?.as_u64()?.try_into().ok()
}

/// Remove the adoption marker; a missing marker is not an error.
pub fn remove_adoption_marker(store: &Path) -> io::Result<()> {
    match std::fs::remove_file(marker_path(store)?) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        other => other,
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

    #[test]
    fn adoption_marker_is_written_refreshed_and_removed() {
        let tmp = tempfile::tempdir().unwrap();
        let store = tmp.path().join("sage.db");
        let marker = tmp.path().join(ADOPTION_MARKER_FILE);
        write_adoption_marker(&store, "p_1", Path::new("D:/root-a")).unwrap();
        write_adoption_marker(&store, "p_1", Path::new("D:/root-b")).unwrap();
        let value: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&marker).unwrap()).unwrap();
        assert_eq!(value["projectId"], "p_1");
        assert_eq!(value["dataRoot"], "D:/root-b");
        let names: Vec<_> = std::fs::read_dir(tmp.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, vec![std::ffi::OsString::from(ADOPTION_MARKER_FILE)]);
        remove_adoption_marker(&store).unwrap();
        remove_adoption_marker(&store).unwrap();
        assert!(!marker.exists());
    }

    #[cfg(unix)]
    #[test]
    fn adoption_marker_never_writes_through_a_planted_symlink() {
        let tmp = tempfile::tempdir().unwrap();
        let outside = tmp.path().join("outside.txt");
        std::fs::write(&outside, "keep").unwrap();
        let dir = tmp.path().join("memories");
        std::fs::create_dir(&dir).unwrap();
        std::os::unix::fs::symlink(&outside, dir.join(ADOPTION_MARKER_FILE)).unwrap();
        write_adoption_marker(&dir.join("sage.db"), "p_1", Path::new("/root")).unwrap();
        assert_eq!(std::fs::read_to_string(&outside).unwrap(), "keep");
        let meta = std::fs::symlink_metadata(dir.join(ADOPTION_MARKER_FILE)).unwrap();
        assert!(meta.file_type().is_file());
    }
}
