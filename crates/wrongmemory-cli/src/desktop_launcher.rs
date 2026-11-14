//! Application-menu launcher for the optional native desktop on Linux.
//!
//! `install --desktop` writes a Desktop Entry and its icon under the user's
//! XDG data directory so the desktop appears in the application menu, and
//! `uninstall` removes them. Both files carry an ownership marker; a file
//! without it, a symlink or anything that is not a regular file is left alone.
//! macOS and Windows get no launcher here (the desktop is started as
//! `wrongmemory-desktop`); their entries need platform bundling that is not
//! implemented.
#![cfg_attr(not(any(target_os = "linux", target_os = "freebsd")), allow(dead_code))]

use std::io;
use std::path::{Path, PathBuf};

/// Matches the window `app_id` the desktop sets, so the compositor links the
/// window to this entry and its icon.
pub const APP_ID: &str = "com.wrongstack.WrongMemory";
const OWNED: &str = "X-WrongMemory-Owned=true";
const ICON: &[u8] = include_bytes!("../assets/wrongmemory-desktop.svg");

pub struct Paths {
    pub entry: PathBuf,
    pub icon: PathBuf,
}

/// Launcher locations below an XDG data directory (`$XDG_DATA_HOME`, else
/// `~/.local/share`).
pub fn paths(data_home: &Path) -> Paths {
    Paths {
        entry: data_home
            .join("applications")
            .join(format!("{APP_ID}.desktop")),
        icon: data_home
            .join("icons/hicolor/scalable/apps")
            .join(format!("{APP_ID}.svg")),
    }
}

/// The XDG data directory for this user, or `None` when it cannot be found.
pub fn data_home() -> Option<PathBuf> {
    std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| {
            std::env::var_os("HOME")
                .filter(|h| !h.is_empty())
                .map(|h| PathBuf::from(h).join(".local/share"))
        })
}

/// One argument of a Desktop Entry `Exec` key: quoted when it holds a reserved
/// character, with `"`, `` ` ``, `$` and `\` escaped inside the quotes, `%`
/// doubled, and the string-level backslash escape applied last.
fn exec_arg(arg: &str) -> String {
    const RESERVED: &str = " \t\n\"'\\><~|&;$*?#()`";
    let quoted = if arg.chars().any(|c| RESERVED.contains(c)) {
        let mut out = String::from("\"");
        for c in arg.chars() {
            if matches!(c, '"' | '`' | '$' | '\\') {
                out.push('\\');
            }
            out.push(c);
        }
        out.push('"');
        out
    } else {
        arg.to_string()
    };
    quoted.replace('%', "%%").replace('\\', "\\\\")
}

/// A string value: only the backslash needs the string-level escape.
fn string_value(value: &str) -> String {
    value.replace('\\', "\\\\")
}

pub fn entry_text(exe: &Path, icon: &Path) -> io::Result<String> {
    let exe = exe
        .to_str()
        .ok_or_else(|| invalid("The desktop executable path is not UTF-8"))?;
    let icon = icon
        .to_str()
        .ok_or_else(|| invalid("The icon path is not UTF-8"))?;
    if exe.chars().chain(icon.chars()).any(char::is_control) {
        return Err(invalid("A launcher path contains control characters"));
    }
    Ok(format!(
        "[Desktop Entry]\n\
         Type=Application\n\
         Version=1.5\n\
         Name=WrongMemory\n\
         GenericName=Knowledge workbench\n\
         Comment=Inspect and review project knowledge from the WrongMemory service\n\
         TryExec={}\n\
         Exec={}\n\
         Icon={}\n\
         Terminal=false\n\
         Categories=Development;\n\
         StartupWMClass={APP_ID}\n\
         {OWNED}\n",
        string_value(exe),
        exec_arg(exe),
        string_value(icon),
    ))
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

/// A path that may be replaced or removed: absent, or a regular file that is
/// not a symlink.
fn replaceable(path: &Path) -> io::Result<bool> {
    match std::fs::symlink_metadata(path) {
        Ok(m) if m.file_type().is_file() => Ok(true),
        Ok(_) => Err(invalid("Launcher path is not a regular file; left alone")),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e),
    }
}

fn owned_entry(path: &Path) -> io::Result<bool> {
    if !replaceable(path)? {
        return Ok(false);
    }
    Ok(std::fs::read_to_string(path)?
        .lines()
        .any(|line| line.trim() == OWNED))
}

/// Write (or refresh) the launcher for `exe` below `data_home`. An existing
/// entry that WrongMemory did not write is never replaced.
pub fn install(data_home: &Path, exe: &Path) -> io::Result<Paths> {
    let p = paths(data_home);
    if replaceable(&p.entry)? && !owned_entry(&p.entry)? {
        return Err(invalid(
            "An application entry with this name exists and is not owned by WrongMemory",
        ));
    }
    if replaceable(&p.icon)? && std::fs::read(&p.icon)? != ICON && !owned_entry(&p.entry)? {
        return Err(invalid(
            "An icon with this name exists and is not owned by WrongMemory",
        ));
    }
    let text = entry_text(exe, &p.icon)?;
    for (path, bytes) in [(&p.icon, ICON), (&p.entry, text.as_bytes())] {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let tmp = path.with_extension("wrongmemory-tmp");
        std::fs::write(&tmp, bytes)?;
        std::fs::rename(&tmp, path)?;
    }
    Ok(p)
}

/// Remove the launcher when WrongMemory owns it. Returns the removed paths.
pub fn uninstall(data_home: &Path) -> io::Result<Vec<PathBuf>> {
    let p = paths(data_home);
    // Preflight both files before removing either.
    let entry = owned_entry(&p.entry)?;
    let icon = entry && replaceable(&p.icon)?;
    let mut removed = Vec::new();
    if icon {
        std::fs::remove_file(&p.icon)?;
        removed.push(p.icon);
    }
    if entry {
        std::fs::remove_file(&p.entry)?;
        removed.push(p.entry);
    }
    Ok(removed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exec_quoting_follows_the_desktop_entry_spec() {
        assert_eq!(
            exec_arg("/opt/wm/wrongmemory-desktop"),
            "/opt/wm/wrongmemory-desktop"
        );
        assert_eq!(exec_arg("/home/a b/wm"), "\"/home/a b/wm\"");
        assert_eq!(exec_arg("/x/100%/wm"), "/x/100%%/wm");
        // `$` is escaped inside quotes, then the backslash itself is escaped.
        assert_eq!(exec_arg("/x/$y"), "\"/x/\\\\$y\"");
        assert!(entry_text(Path::new("/x\n/y"), Path::new("/i.svg")).is_err());
    }

    #[test]
    fn install_and_uninstall_touch_only_owned_files() {
        let temp = tempfile::tempdir().unwrap();
        let data = temp.path();
        let exe = data.join("bin with space/wrongmemory-desktop");
        let p = install(data, &exe).unwrap();
        let text = std::fs::read_to_string(&p.entry).unwrap();
        assert!(text.contains("Exec=\"") && text.contains(OWNED));
        assert!(text.contains(&format!("StartupWMClass={APP_ID}")));
        assert_eq!(std::fs::read(&p.icon).unwrap(), ICON);
        // Refreshing an owned launcher is allowed.
        install(data, &exe).unwrap();
        assert_eq!(uninstall(data).unwrap().len(), 2);
        assert!(!p.entry.exists() && !p.icon.exists());

        // A foreign entry with the same name is neither replaced nor removed.
        std::fs::create_dir_all(p.entry.parent().unwrap()).unwrap();
        std::fs::write(&p.entry, "[Desktop Entry]\nName=Someone else\n").unwrap();
        assert!(install(data, &exe).is_err());
        assert!(uninstall(data).unwrap().is_empty());
        assert!(p.entry.exists());
    }

    /// CI sets WRONGMEMORY_REQUIRE_DESKTOP_FILE_VALIDATE=1 on Linux so the
    /// generated entry is checked by freedesktop's validator.
    #[test]
    fn generated_entry_passes_desktop_file_validate() {
        let available = std::process::Command::new("desktop-file-validate")
            .arg("--help")
            .output()
            .is_ok();
        if !available {
            assert!(
                std::env::var_os("WRONGMEMORY_REQUIRE_DESKTOP_FILE_VALIDATE").is_none(),
                "desktop-file-validate is required but not installed"
            );
            return;
        }
        let temp = tempfile::tempdir().unwrap();
        let p = install(
            temp.path(),
            Path::new("/opt/Wrong Memory/wrongmemory-desktop"),
        )
        .unwrap();
        let out = std::process::Command::new("desktop-file-validate")
            .arg(&p.entry)
            .output()
            .unwrap();
        assert!(
            out.status.success() && out.stdout.is_empty(),
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
    }
}
