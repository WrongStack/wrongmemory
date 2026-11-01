//! `normalizeProjectPath` (baseline `packages/sage/src/paths.ts`): a
//! caller-supplied path as a project-relative, forward-slash path, with
//! symlink escapes rejected.
//!
//! * The project root is resolved through the filesystem.
//! * The input is resolved lexically against it (absolute inputs as given).
//! * If the leaf exists, its real parent is used; otherwise the deepest
//!   existing ancestor of the parent is canonicalized and the rest appended.
//!   The caller's own basename is kept, so anchor identity matches what the
//!   caller typed.
//! * A result outside the root is an error. On Windows the containment
//!   comparison is case-insensitive, like Node's `path.relative`.

use std::path::{Component, Path, PathBuf};

use crate::text::normalize_slashes;
use crate::wrongstack_paths::node_resolve;

fn strip_verbatim(p: PathBuf) -> PathBuf {
    let s = p.to_string_lossy().into_owned();
    if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
        PathBuf::from(format!(r"\\{rest}"))
    } else if let Some(rest) = s.strip_prefix(r"\\?\") {
        PathBuf::from(rest)
    } else {
        p
    }
}

fn realpath(p: &Path) -> Option<PathBuf> {
    std::fs::canonicalize(p).ok().map(strip_verbatim)
}

/// `canonicalizeExisting`: canonicalize the deepest existing ancestor and
/// re-append the components that do not exist yet.
fn canonicalize_existing(p: &Path) -> PathBuf {
    let mut tail: Vec<std::ffi::OsString> = Vec::new();
    let mut cur = p.to_path_buf();
    loop {
        if let Some(real) = realpath(&cur) {
            let mut out = real;
            for c in tail.iter().rev() {
                out.push(c);
            }
            return out;
        }
        match (cur.file_name().map(|n| n.to_os_string()), cur.parent()) {
            (Some(name), Some(parent)) => {
                tail.push(name);
                cur = parent.to_path_buf();
            }
            _ => return p.to_path_buf(),
        }
    }
}

fn same_component(a: &Component<'_>, b: &Component<'_>) -> bool {
    if cfg!(windows) {
        a.as_os_str().to_string_lossy().to_lowercase()
            == b.as_os_str().to_string_lossy().to_lowercase()
    } else {
        a == b
    }
}

/// Project-relative path of `abs` under `root`, or `None` when it escapes.
fn relative_under(root: &Path, abs: &Path) -> Option<String> {
    let rc: Vec<Component<'_>> = root.components().collect();
    let ac: Vec<Component<'_>> = abs.components().collect();
    if ac.len() < rc.len() || !rc.iter().zip(&ac).all(|(a, b)| same_component(a, b)) {
        return None;
    }
    let rest: Vec<String> = ac[rc.len()..]
        .iter()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect();
    Some(if rest.is_empty() {
        ".".to_string()
    } else {
        rest.join("/")
    })
}

/// `normalizeProjectPath(projectRoot, inputPath)`.
pub fn normalize_project_path(project_root: &Path, input: &str) -> Result<String, String> {
    let resolved_root = node_resolve(project_root);
    let root = realpath(&resolved_root).unwrap_or(resolved_root);
    let input_path = Path::new(input);
    let raw_abs = if input_path.is_absolute() {
        node_resolve(input_path)
    } else {
        node_resolve(&root.join(input_path))
    };
    let abs = match (raw_abs.parent(), raw_abs.file_name()) {
        (Some(parent), Some(base)) => {
            let base = base.to_os_string();
            match realpath(&raw_abs) {
                Some(full) => full.parent().map(|p| p.join(&base)).unwrap_or(full.clone()),
                None => canonicalize_existing(parent).join(&base),
            }
        }
        _ => raw_abs.clone(),
    };
    relative_under(&root, &abs)
        .map(|r| normalize_slashes(&r))
        .ok_or_else(|| format!("Memory path must stay inside the project root: {input}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relative_absolute_missing_and_escaping_paths() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("proj");
        std::fs::create_dir_all(root.join("src/server")).unwrap();
        std::fs::write(root.join("src/server/http.ts"), "x").unwrap();
        let n = |p: &str| normalize_project_path(&root, p);
        assert_eq!(n("src/server/http.ts").unwrap(), "src/server/http.ts");
        assert_eq!(
            n("./src//server/../server/http.ts").unwrap(),
            "src/server/http.ts"
        );
        assert_eq!(n("src/new/file.ts").unwrap(), "src/new/file.ts");
        assert_eq!(n(".").unwrap(), ".");
        assert_eq!(
            n(&root.join("src/server/http.ts").to_string_lossy()).unwrap(),
            "src/server/http.ts"
        );
        assert!(n("../outside.ts").is_err());
        assert!(n(&tmp.path().join("other.ts").to_string_lossy()).is_err());
        if cfg!(windows) {
            let upper = root.join("SRC/server/http.ts");
            assert!(n(&upper.to_string_lossy()).is_ok());
        }
    }
}
