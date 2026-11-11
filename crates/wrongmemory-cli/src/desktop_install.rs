//! Optional desktop bundle admission. No database access or GUI activation.
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::io;
use std::path::{Component, Path, PathBuf};

#[derive(Deserialize)]
struct Entry {
    path: String,
    sha256: String,
    bytes: u64,
}
#[derive(Deserialize)]
struct Manifest {
    schema: String,
    version: String,
    files: Vec<Entry>,
}
fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}
fn plain(path: &Path) -> io::Result<()> {
    let m = std::fs::symlink_metadata(path)?;
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if m.file_attributes() & 0x400 != 0 {
            return Err(invalid(
                "Reparse points are not admitted in a desktop bundle",
            ));
        }
    }
    if m.file_type().is_symlink() {
        return Err(invalid("Symlinks are not admitted in a desktop bundle"));
    }
    Ok(())
}
fn resource(path: &str) -> bool {
    matches!(path, "README.md" | "LICENSE" | "THIRD_PARTY.json")
        || ["licenses/", "sources/"].iter().any(|p| {
            path.strip_prefix(p)
                .is_some_and(|s| !s.is_empty() && !s.contains(['/', '\\', ':']))
        })
}
fn manifest(root: &Path) -> io::Result<Manifest> {
    plain(root)?;
    plain(&root.join("MANIFEST.json"))?;
    let bytes = std::fs::read(root.join("MANIFEST.json"))?;
    if bytes.len() > 2 * 1024 * 1024 {
        return Err(invalid("Oversized desktop manifest"));
    }
    let m: Manifest = serde_json::from_slice(&bytes).map_err(|e| invalid(&e.to_string()))?;
    if m.schema != "wrongmemory/desktop-package@1" || m.version != env!("CARGO_PKG_VERSION") {
        return Err(invalid(
            "Desktop bundle schema/version does not match this CLI",
        ));
    }
    let mut seen = std::collections::HashSet::new();
    for e in &m.files {
        let p = Path::new(&e.path);
        if !seen.insert(e.path.as_str())
            || e.path.contains(['\\', ':'])
            || p.components().any(|c| !matches!(c, Component::Normal(_)))
            || !(resource(&e.path)
                || matches!(
                    e.path.as_str(),
                    "bin/wrongmemory.exe" | "bin/wrongmemoryd.exe" | "bin/wrongmemory-desktop.exe"
                ))
        {
            return Err(invalid("Invalid desktop manifest path"));
        }
        for ancestor in p.ancestors().skip(1).filter(|p| !p.as_os_str().is_empty()) {
            plain(&root.join(ancestor))?;
        }
        let file = root.join(p);
        plain(&file)?;
        let bytes = std::fs::read(&file)?;
        let hash: String = Sha256::digest(&bytes)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        if bytes.len() as u64 != e.bytes || hash != e.sha256 {
            return Err(invalid("Desktop bundle integrity mismatch"));
        }
    }
    for required in [
        "README.md",
        "LICENSE",
        "THIRD_PARTY.json",
        "bin/wrongmemory-desktop.exe",
        "bin/wrongmemory.exe",
        "bin/wrongmemoryd.exe",
    ] {
        if !seen.contains(required) {
            return Err(invalid("Incomplete desktop bundle"));
        }
    }
    Ok(m)
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;
    fn bundle(root: &Path) {
        std::fs::create_dir_all(root.join("bin")).unwrap();
        std::fs::create_dir_all(root.join("licenses")).unwrap();
        let files: Vec<_> = [
            "README.md",
            "LICENSE",
            "THIRD_PARTY.json",
            "licenses/notice.txt",
            "bin/wrongmemory.exe",
            "bin/wrongmemoryd.exe",
            "bin/wrongmemory-desktop.exe",
        ]
        .iter()
        .map(|name| {
            let bytes = name.as_bytes();
            std::fs::write(root.join(name), bytes).unwrap();
            let hash: String = Sha256::digest(bytes)
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect();
            serde_json::json!({"path":name,"bytes":bytes.len(),"sha256":hash})
        })
        .collect();
        std::fs::write(root.join("MANIFEST.json"), serde_json::to_vec(&serde_json::json!({"schema":"wrongmemory/desktop-package@1","version":env!("CARGO_PKG_VERSION"),"files":files})).unwrap()).unwrap();
    }
    #[test]
    fn admits_complete_bundle_and_refuses_tampering_before_replacement() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        bundle(&source);
        let prefix = temp.path().join("installed");
        prepare(&source.join("bin"), &prefix).unwrap();
        let resources = prefix.join("desktop-resources");
        std::fs::write(resources.join("user-note.txt"), "keep").unwrap();
        std::fs::write(source.join("bin/wrongmemory-desktop.exe"), "tampered").unwrap();
        assert!(
            prepare(&source.join("bin"), &prefix)
                .unwrap_err()
                .to_string()
                .contains("integrity")
        );
        uninstall(&prefix).unwrap();
        assert_eq!(
            std::fs::read_to_string(resources.join("user-note.txt")).unwrap(),
            "keep"
        );
        assert!(!resources.join("LICENSE").exists());
        assert!(!temp.path().join("registry.db").exists());
    }
    #[test]
    fn unsafe_ownership_and_bundle_paths_leave_external_files_intact() {
        let temp = tempfile::tempdir().unwrap();
        let resources = temp.path().join("desktop-resources");
        std::fs::create_dir(&resources).unwrap();
        let outside = temp.path().join("keep.txt");
        std::fs::write(&outside, "keep").unwrap();
        std::fs::write(resources.join("owned.json"), r#"["../keep.txt"]"#).unwrap();
        assert!(uninstall(temp.path()).is_err());
        assert_eq!(std::fs::read_to_string(outside).unwrap(), "keep");
        let source = temp.path().join("source");
        bundle(&source);
        let mut m: serde_json::Value =
            serde_json::from_slice(&std::fs::read(source.join("MANIFEST.json")).unwrap()).unwrap();
        m["files"][0]["path"] = serde_json::json!("../keep.txt");
        std::fs::write(
            source.join("MANIFEST.json"),
            serde_json::to_vec(&m).unwrap(),
        )
        .unwrap();
        assert!(prepare(&source.join("bin"), &temp.path().join("new-install")).is_err());
        assert!(!temp.path().join("new-install").exists());
    }
}

pub fn prepare(src: &Path, prefix: &Path) -> io::Result<PathBuf> {
    if !cfg!(windows) {
        return Err(invalid(
            "Desktop installation currently supports Windows only",
        ));
    }
    let root = src
        .parent()
        .ok_or_else(|| invalid("Desktop bundle root missing"))?;
    let m = manifest(root)?; // Validate every byte before stopping a service or replacing binaries.
    std::fs::create_dir_all(prefix)?;
    plain(prefix)?;
    let resources = prefix.join("desktop-resources");
    if resources.exists() {
        plain(&resources)?;
    }
    std::fs::create_dir_all(&resources)?;
    for e in m.files.iter().filter(|e| resource(&e.path)) {
        let dst = resources.join(&e.path);
        if let Some(parent) = dst.parent() {
            if parent.exists() {
                plain(parent)?;
            }
            std::fs::create_dir_all(parent)?;
        }
        if dst.exists() {
            plain(&dst)?;
        }
        // Resource text is replaced through the same private atomic writer.
        wrongmemory_ipc::endpoint::write_private_atomic(&dst, &std::fs::read(root.join(&e.path))?)?;
    }
    // A narrow list of owned files, never authoritative executable paths.
    let owned: Vec<_> = m
        .files
        .iter()
        .filter(|e| resource(&e.path))
        .map(|e| &e.path)
        .collect();
    wrongmemory_ipc::endpoint::write_private_atomic(
        &resources.join("owned.json"),
        &serde_json::to_vec(&owned)?,
    )?;
    Ok(resources)
}

pub fn uninstall(prefix: &Path) -> io::Result<()> {
    let resources = prefix.join("desktop-resources");
    if !resources.exists() {
        return Ok(());
    }
    plain(&resources)?;
    plain(&resources.join("owned.json"))?;
    let owned: Vec<String> = serde_json::from_slice(&std::fs::read(resources.join("owned.json"))?)?;
    if owned.iter().any(|p| {
        !resource(p)
            || Path::new(p)
                .components()
                .any(|c| !matches!(c, Component::Normal(_)))
    }) {
        return Err(invalid(
            "Invalid desktop resource ownership list; files preserved",
        ));
    }
    // Preflight the entire removal before deleting a file. Unmanaged files stay.
    for p in &owned {
        let file = resources.join(p);
        if let Some(parent) = file.parent() {
            plain(parent)?;
        }
        if file.exists() {
            plain(&file)?;
        }
    }
    for p in owned {
        let file = resources.join(p);
        if file.exists() {
            std::fs::remove_file(file)?;
        }
    }
    std::fs::remove_file(resources.join("owned.json"))?;
    for dir in ["licenses", "sources"] {
        let _ = std::fs::remove_dir(resources.join(dir));
    }
    let _ = std::fs::remove_dir(resources);
    Ok(())
}
