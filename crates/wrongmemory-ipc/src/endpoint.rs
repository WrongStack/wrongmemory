//! Data root, private directories, endpoint naming, and discovery files.
//!
//! Layout under the data root (architecture §3):
//!
//! ```text
//! registry.db
//! runtime/endpoint.json   discovery metadata, no secret
//! runtime/token           host/admin credential (owner-private directory)
//! logs/
//! backups/
//! projects/<id>/sage.db   managed stores for non-WrongStack projects
//! ```

use std::io;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};
use wrongmemory_contracts::EndpointInfo;

pub const DATA_ROOT_ENV: &str = "WRONGMEMORY_HOME";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DataRoot {
    root: PathBuf,
}

impl DataRoot {
    /// Explicit root, else `WRONGMEMORY_HOME`, else `~/.wrongmemory`.
    pub fn resolve(explicit: Option<&Path>) -> io::Result<Self> {
        let root = if let Some(p) = explicit {
            p.to_path_buf()
        } else if let Some(p) = std::env::var_os(DATA_ROOT_ENV).filter(|v| !v.is_empty()) {
            PathBuf::from(p)
        } else {
            let home = std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" })
                .ok_or_else(|| {
                    io::Error::new(io::ErrorKind::NotFound, "cannot determine home directory")
                })?;
            PathBuf::from(home).join(".wrongmemory")
        };
        let root = std::path::absolute(root)?;
        Ok(Self { root })
    }

    pub fn path(&self) -> &Path {
        &self.root
    }
    pub fn registry_db(&self) -> PathBuf {
        self.root.join("registry.db")
    }
    pub fn runtime_dir(&self) -> PathBuf {
        self.root.join("runtime")
    }
    pub fn logs_dir(&self) -> PathBuf {
        self.root.join("logs")
    }
    pub fn backups_dir(&self) -> PathBuf {
        self.root.join("backups")
    }
    /// Managed out-of-tree SAGE-compatible stores (`projects/<opaque-id>/sage.db`).
    pub fn projects_dir(&self) -> PathBuf {
        self.root.join("projects")
    }
    /// Owner-private grant credentials (`grants/<grant-id>.secret`). Client
    /// configurations reference only the nonsecret grant ID (WM-ADR-005).
    pub fn grants_dir(&self) -> PathBuf {
        self.root.join("grants")
    }
    pub fn grant_secret_file(&self, grant_id: &str) -> io::Result<PathBuf> {
        if grant_id.is_empty()
            || !grant_id
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "grant id must be [A-Za-z0-9_-]+",
            ));
        }
        Ok(self.grants_dir().join(format!("{grant_id}.secret")))
    }
    pub fn write_grant_secret(&self, grant_id: &str, secret: &str) -> io::Result<()> {
        create_private_dir(&self.grants_dir())?;
        write_private_atomic(&self.grant_secret_file(grant_id)?, secret.as_bytes())
    }
    pub fn read_grant_secret(&self, grant_id: &str) -> io::Result<String> {
        Ok(std::fs::read_to_string(self.grant_secret_file(grant_id)?)?
            .trim()
            .to_string())
    }
    pub fn remove_grant_secret(&self, grant_id: &str) -> io::Result<()> {
        match std::fs::remove_file(self.grant_secret_file(grant_id)?) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            other => other,
        }
    }
    pub fn endpoint_file(&self) -> PathBuf {
        self.runtime_dir().join("endpoint.json")
    }
    pub fn token_file(&self) -> PathBuf {
        self.runtime_dir().join("token")
    }

    /// Create the root and its subdirectories. On Windows each directory that
    /// does not yet exist is created with a protected DACL (current user +
    /// LocalSystem only).
    pub fn ensure_private_dirs(&self) -> io::Result<()> {
        for dir in [
            self.root.clone(),
            self.runtime_dir(),
            self.logs_dir(),
            self.backups_dir(),
            self.projects_dir(),
        ] {
            create_private_dir(&dir)?;
        }
        Ok(())
    }

    /// Endpoint name: unique per OS user and per data root, so test and
    /// portable roots never collide with the normal daemon.
    pub fn endpoint_name(&self) -> io::Result<String> {
        let user = user_identity()?;
        let mut key = self.root.to_string_lossy().into_owned();
        if cfg!(windows) {
            key = key.replace('/', "\\").to_lowercase();
        }
        let digest = Sha256::digest(format!("{user}\0{key}").as_bytes());
        let short = hex::encode(&digest[..8]);
        Ok(if cfg!(windows) {
            format!(r"\\.\pipe\wrongmemory-{short}")
        } else {
            self.runtime_dir()
                .join(format!("wrongmemory-{short}.sock"))
                .to_string_lossy()
                .into_owned()
        })
    }
}

/// Stable identity of the current OS user (SID on Windows).
pub fn user_identity() -> io::Result<String> {
    #[cfg(windows)]
    {
        crate::win::current_user_sid()
    }
    #[cfg(not(windows))]
    {
        Ok(std::env::var("USER").unwrap_or_default())
    }
}

fn create_private_dir(dir: &Path) -> io::Result<()> {
    if dir.is_dir() {
        // Never trust a pre-existing directory's ACL: refuse one owned by
        // another account, re-apply the owner-only DACL to our own.
        #[cfg(windows)]
        return crate::win::harden_private_dir(dir);
        #[cfg(not(windows))]
        {
            use std::os::unix::fs::{MetadataExt, PermissionsExt};
            let meta = std::fs::metadata(dir)?;
            // SAFETY: getuid has no preconditions.
            let uid = unsafe { libc_getuid() };
            if meta.uid() != uid {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    format!("{} is not owned by the current user", dir.display()),
                ));
            }
            return std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
        }
    }
    if let Some(parent) = dir.parent()
        && !parent.as_os_str().is_empty()
        && !parent.exists()
    {
        std::fs::create_dir_all(parent)?;
    }
    #[cfg(windows)]
    {
        match crate::win::create_private_dir(dir) {
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => Ok(()),
            other => other,
        }
    }
    #[cfg(not(windows))]
    {
        use std::os::unix::fs::DirBuilderExt;
        match std::fs::DirBuilder::new().mode(0o700).create(dir) {
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => Ok(()),
            other => other,
        }
    }
}

#[cfg(not(windows))]
unsafe extern "C" {
    #[link_name = "getuid"]
    fn libc_getuid() -> u32;
}

fn temp_sibling(path: &Path) -> io::Result<std::path::PathBuf> {
    let dir = path
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "path has no parent"))?;
    Ok(dir.join(format!(
        ".{}.{}.tmp",
        path.file_name()
            .map(|n| n.to_string_lossy())
            .unwrap_or_default(),
        std::process::id()
    )))
}

/// Atomically write a secret: the temp file is created exclusively with an
/// owner-only DACL (Windows) or mode 0600 (Unix) before any byte is written,
/// then renamed over `path` (rename keeps the file's own security descriptor).
pub fn write_private_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let tmp = temp_sibling(path)?;
    let _ = std::fs::remove_file(&tmp);
    #[cfg(windows)]
    crate::win::write_private_new(&tmp, bytes)?;
    #[cfg(not(windows))]
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
    }
    match std::fs::rename(&tmp, path) {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            Err(e)
        }
    }
}

/// Write `bytes` to `path` atomically (temp file in the same directory + rename).
pub fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let tmp = temp_sibling(path)?;
    std::fs::write(&tmp, bytes)?;
    match std::fs::rename(&tmp, path) {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            Err(e)
        }
    }
}

/// Generate a 256-bit connection credential.
pub fn generate_token() -> io::Result<String> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).map_err(|e| io::Error::other(e.to_string()))?;
    Ok(hex::encode(bytes))
}

/// Constant-time comparison for credentials.
pub fn token_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// Discovery information a client needs to connect.
#[derive(Debug, Clone)]
pub struct Discovery {
    pub info: EndpointInfo,
    pub token: String,
}

impl Discovery {
    pub fn publish(root: &DataRoot, info: &EndpointInfo, token: &str) -> io::Result<()> {
        // Token first: a reader that sees the new endpoint must find its credential.
        write_private_atomic(&root.token_file(), token.as_bytes())?;
        write_atomic(
            &root.endpoint_file(),
            serde_json::to_vec_pretty(info)
                .map_err(io::Error::other)?
                .as_slice(),
        )
    }

    pub fn load(root: &DataRoot) -> io::Result<Self> {
        let info: EndpointInfo = serde_json::from_slice(&std::fs::read(root.endpoint_file())?)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        let token = std::fs::read_to_string(root.token_file())?
            .trim()
            .to_string();
        Ok(Self { info, token })
    }

    /// Remove discovery files only if they still describe `instance_id`; a
    /// newer daemon's metadata is never deleted by an exiting older one.
    pub fn retract(root: &DataRoot, instance_id: &str) -> io::Result<bool> {
        let current = match Self::load(root) {
            Ok(d) => d,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(false),
            Err(e) => return Err(e),
        };
        if current.info.daemon_instance_id != instance_id {
            return Ok(false);
        }
        let _ = std::fs::remove_file(root.endpoint_file());
        let _ = std::fs::remove_file(root.token_file());
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(id: &str) -> EndpointInfo {
        EndpointInfo {
            endpoint: "e".into(),
            daemon_instance_id: id.into(),
            daemon_version: "0".into(),
            pid: 1,
            protocol_major: 1,
            protocol_minor: 0,
            started_at_ms: 0,
        }
    }

    #[test]
    fn endpoint_names_differ_per_root() {
        let a = DataRoot::resolve(Some(Path::new("C:/x/a"))).unwrap();
        let b = DataRoot::resolve(Some(Path::new("C:/x/b"))).unwrap();
        assert_ne!(a.endpoint_name().unwrap(), b.endpoint_name().unwrap());
        assert_eq!(a.endpoint_name().unwrap(), a.endpoint_name().unwrap());
    }

    #[test]
    fn discovery_round_trip_and_owner_only_retract() {
        let tmp = tempfile::tempdir().unwrap();
        let root = DataRoot::resolve(Some(&tmp.path().join("home"))).unwrap();
        root.ensure_private_dirs().unwrap();
        let token = generate_token().unwrap();
        assert_eq!(token.len(), 64);
        Discovery::publish(&root, &info("one"), &token).unwrap();
        #[cfg(windows)]
        {
            assert!(crate::win::is_owner_only(&root.token_file()).unwrap());
            assert!(crate::win::is_owner_only(&root.runtime_dir()).unwrap());
        }
        // Republishing replaces the token without weakening its ACL.
        Discovery::publish(&root, &info("one"), &token).unwrap();
        #[cfg(windows)]
        assert!(crate::win::is_owner_only(&root.token_file()).unwrap());
        let d = Discovery::load(&root).unwrap();
        assert_eq!(d.token, token);
        assert!(!Discovery::retract(&root, "other").unwrap());
        assert!(root.endpoint_file().exists());
        assert!(Discovery::retract(&root, "one").unwrap());
        assert!(!root.endpoint_file().exists());
        assert!(!root.token_file().exists());
    }

    #[test]
    fn pre_existing_root_is_hardened() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        std::fs::create_dir_all(home.join("runtime")).unwrap();
        let root = DataRoot::resolve(Some(&home)).unwrap();
        root.ensure_private_dirs().unwrap();
        #[cfg(windows)]
        {
            assert!(crate::win::is_owner_only(&home).unwrap());
            assert!(crate::win::is_owner_only(&home.join("runtime")).unwrap());
        }
    }

    #[test]
    fn token_comparison() {
        assert!(token_eq("abc", "abc"));
        assert!(!token_eq("abc", "abd"));
        assert!(!token_eq("abc", "abcd"));
    }
}

/// Domain-separation prefix for the daemon's proof of credential possession.
pub const SERVER_PROOF_CONTEXT: &str = "wrongmemory-server-proof-v1";

/// HMAC-SHA256 (RFC 2104) over `context:nonce`, keyed by the host token.
///
/// Clients that cannot verify the pipe server's OS identity (the Node
/// adapter) send `{"type":"challenge","nonce":..}` first and only send a
/// credential after checking this proof against the owner-private token, so
/// a squatted pipe name never receives a grant secret or host token.
pub fn server_proof(token: &str, nonce: &str) -> String {
    hmac_sha256_hex(token, &format!("{SERVER_PROOF_CONTEXT}:{nonce}"))
}

/// Domain separator for [`http_token`].
const HTTP_TOKEN_CONTEXT: &str = "wrongmemory-http-bearer-v1";

/// The loopback HTTP bearer token of a grant (WM-308): an HMAC of the grant
/// secret under a fixed context. A different audience from the IPC
/// credential, so the grant secret itself never travels over HTTP, and a new
/// secret (revoke + re-issue) invalidates every derived token.
pub fn http_token(grant_secret: &str) -> String {
    hmac_sha256_hex(grant_secret, HTTP_TOKEN_CONTEXT)
}

/// HMAC-SHA256 (RFC 2104) as lowercase hex.
pub fn hmac_sha256_hex(key: &str, msg: &str) -> String {
    const B: usize = 64;
    let mut key = key.as_bytes().to_vec();
    if key.len() > B {
        key = Sha256::digest(&key).to_vec();
    }
    key.resize(B, 0);
    let ipad: Vec<u8> = key.iter().map(|b| b ^ 0x36).collect();
    let opad: Vec<u8> = key.iter().map(|b| b ^ 0x5c).collect();
    let inner = Sha256::new()
        .chain_update(&ipad)
        .chain_update(msg.as_bytes())
        .finalize();
    hex::encode(
        Sha256::new()
            .chain_update(&opad)
            .chain_update(inner)
            .finalize(),
    )
}

/// Constant-time byte comparison (length is not secret).
pub fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

#[cfg(test)]
mod proof_tests {
    use super::*;

    #[test]
    fn hmac_matches_rfc4231_case_2() {
        // RFC 4231 test case 2 uses key "Jefe" and data
        // "what do ya want for nothing?"; recompute through the same core.
        const B: usize = 64;
        let mut key = b"Jefe".to_vec();
        key.resize(B, 0);
        let ipad: Vec<u8> = key.iter().map(|b| b ^ 0x36).collect();
        let opad: Vec<u8> = key.iter().map(|b| b ^ 0x5c).collect();
        let inner = Sha256::new()
            .chain_update(&ipad)
            .chain_update(b"what do ya want for nothing?")
            .finalize();
        let mac = hex::encode(
            Sha256::new()
                .chain_update(&opad)
                .chain_update(inner)
                .finalize(),
        );
        assert_eq!(
            mac,
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
        assert_eq!(
            hmac_sha256_hex("Jefe", "what do ya want for nothing?"),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
        assert_ne!(
            http_token("s"),
            hmac_sha256_hex("s", "wrongmemory-server-proof")
        );
        assert!(ct_eq(b"abc", b"abc") && !ct_eq(b"abc", b"abd") && !ct_eq(b"ab", b"abc"));
        assert_ne!(server_proof("a", "n"), server_proof("b", "n"));
        assert_eq!(server_proof("a", "n").len(), 64);
    }
}
