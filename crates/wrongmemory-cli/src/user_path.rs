//! Puts the install directory on the user's PATH (WM-700).
//!
//! `wrongmemory install` adds it, `wrongmemory uninstall` takes it out again,
//! so a WrongMemory installed by the scripts, by WrongStack
//! (`wstack sage service install`) or by hand is callable as `wrongmemory`
//! from a new terminal.
//!
//! * Windows: the user `Path` value under `HKCU\Environment`, read and written
//!   raw so `%VAR%` entries stay unexpanded and the value keeps its
//!   `REG_EXPAND_SZ` type; then `WM_SETTINGCHANGE` so new terminals see it.
//! * Unix: a marked block in `~/.profile` and in the current shell's startup
//!   file (`~/.bashrc`, plus `~/.bash_profile` when it exists;
//!   `$ZDOTDIR/.zshrc`), or a `conf.d` file for fish. The block only prepends
//!   the directory when it is missing, so sourcing it twice is harmless.
//!
//! `WRONGMEMORY_NO_MODIFY_PATH=1` (or `install --no-modify-path`) leaves the
//! PATH alone. Tests redirect every location: `WRONGMEMORY_ENV_KEY_OVERRIDE`
//! names a scratch registry key, and the Unix files follow `$HOME`.

use std::path::Path;

pub const NO_MODIFY_ENV: &str = "WRONGMEMORY_NO_MODIFY_PATH";
#[cfg(windows)]
pub const ENV_KEY_OVERRIDE: &str = "WRONGMEMORY_ENV_KEY_OVERRIDE";

/// True when the environment asks to leave the PATH alone.
pub fn disabled_by_env() -> bool {
    std::env::var_os(NO_MODIFY_ENV).is_some_and(|v| !v.is_empty() && v != "0")
}

/// Ensures `dir` is on the user's PATH. Returns the places changed (empty
/// when it already was).
pub fn add(dir: &Path) -> Result<Vec<String>, String> {
    imp::add(dir)
}

/// Removes `dir` from the user's PATH where WrongMemory put it. Returns the
/// places changed.
pub fn remove(dir: &Path) -> Result<Vec<String>, String> {
    imp::remove(dir)
}

#[cfg(windows)]
mod imp {
    use std::os::windows::ffi::OsStrExt;
    use std::path::Path;

    use windows_sys::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_SUCCESS};
    use windows_sys::Win32::System::Environment::ExpandEnvironmentStringsW;
    use windows_sys::Win32::System::Registry::{
        HKEY, HKEY_CURRENT_USER, KEY_QUERY_VALUE, KEY_SET_VALUE, REG_EXPAND_SZ,
        REG_OPTION_NON_VOLATILE, RegCloseKey, RegCreateKeyExW, RegQueryValueExW, RegSetValueExW,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        HWND_BROADCAST, SMTO_ABORTIFHUNG, SendMessageTimeoutW, WM_SETTINGCHANGE,
    };

    use super::ENV_KEY_OVERRIDE;

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    fn subkey() -> (String, bool) {
        match std::env::var(ENV_KEY_OVERRIDE) {
            Ok(k) if !k.is_empty() => (k, true),
            _ => ("Environment".to_string(), false),
        }
    }

    struct Key(HKEY);

    impl Drop for Key {
        fn drop(&mut self) {
            // SAFETY: the handle came from RegCreateKeyExW and is closed once.
            unsafe { RegCloseKey(self.0) };
        }
    }

    fn open(name: &str) -> Result<Key, String> {
        let name = wide(name);
        let mut key: HKEY = std::ptr::null_mut();
        // SAFETY: valid NUL-terminated name and out-pointer; no class or
        // security attributes.
        let rc = unsafe {
            RegCreateKeyExW(
                HKEY_CURRENT_USER,
                name.as_ptr(),
                0,
                std::ptr::null(),
                REG_OPTION_NON_VOLATILE,
                KEY_QUERY_VALUE | KEY_SET_VALUE,
                std::ptr::null(),
                &mut key,
                std::ptr::null_mut(),
            )
        };
        if rc != ERROR_SUCCESS {
            return Err(format!("cannot open HKCU\\{}: error {rc}", subkey().0));
        }
        Ok(Key(key))
    }

    /// The raw (unexpanded) `Path` value; empty when it does not exist.
    fn read(key: &Key) -> Result<String, String> {
        let name = wide("Path");
        let mut len: u32 = 0;
        // SAFETY: size query with a null data pointer.
        let rc = unsafe {
            RegQueryValueExW(
                key.0,
                name.as_ptr(),
                std::ptr::null(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &mut len,
            )
        };
        if rc == ERROR_FILE_NOT_FOUND {
            return Ok(String::new());
        }
        if rc != ERROR_SUCCESS {
            return Err(format!("cannot read the user Path: error {rc}"));
        }
        let mut buf = vec![0u16; (len as usize).div_ceil(2) + 1];
        let mut bytes = (buf.len() * 2) as u32;
        // SAFETY: `buf` holds `bytes` bytes.
        let rc = unsafe {
            RegQueryValueExW(
                key.0,
                name.as_ptr(),
                std::ptr::null(),
                std::ptr::null_mut(),
                buf.as_mut_ptr().cast(),
                &mut bytes,
            )
        };
        if rc != ERROR_SUCCESS {
            return Err(format!("cannot read the user Path: error {rc}"));
        }
        let n = (bytes as usize / 2).min(buf.len());
        let s = String::from_utf16_lossy(&buf[..n]);
        Ok(s.trim_end_matches('\0').to_string())
    }

    fn write(key: &Key, value: &str) -> Result<(), String> {
        let name = wide("Path");
        let data = wide(value);
        // SAFETY: `data` is NUL-terminated UTF-16; the size includes the NUL.
        let rc = unsafe {
            RegSetValueExW(
                key.0,
                name.as_ptr(),
                0,
                REG_EXPAND_SZ,
                data.as_ptr().cast(),
                (data.len() * 2) as u32,
            )
        };
        if rc != ERROR_SUCCESS {
            return Err(format!("cannot write the user Path: error {rc}"));
        }
        Ok(())
    }

    fn expand(entry: &str) -> String {
        let src = wide(entry);
        let mut buf = vec![0u16; 1024];
        loop {
            // SAFETY: `src` is NUL-terminated; `buf` holds `buf.len()` u16s.
            let n = unsafe {
                ExpandEnvironmentStringsW(src.as_ptr(), buf.as_mut_ptr(), buf.len() as u32)
            } as usize;
            if n == 0 {
                return entry.to_string();
            }
            if n <= buf.len() {
                return String::from_utf16_lossy(&buf[..n - 1]);
            }
            buf.resize(n, 0);
        }
    }

    fn same(entry: &str, dir: &str) -> bool {
        let norm = |s: &str| s.trim().trim_end_matches(['\\', '/']).to_lowercase();
        norm(&expand(entry)) == norm(dir)
    }

    fn broadcast() {
        let env: Vec<u16> = std::ffi::OsStr::new("Environment")
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();
        let mut result: usize = 0;
        // SAFETY: broadcast of WM_SETTINGCHANGE with a NUL-terminated string;
        // SMTO_ABORTIFHUNG and a 5 s cap keep a hung window from blocking us.
        unsafe {
            SendMessageTimeoutW(
                HWND_BROADCAST,
                WM_SETTINGCHANGE,
                0,
                env.as_ptr() as isize,
                SMTO_ABORTIFHUNG,
                5000,
                &mut result,
            )
        };
    }

    pub fn add(dir: &Path) -> Result<Vec<String>, String> {
        let dir = dir.display().to_string();
        let (name, overridden) = subkey();
        let key = open(&name)?;
        let raw = read(&key)?;
        let entries: Vec<&str> = raw.split(';').filter(|e| !e.trim().is_empty()).collect();
        if entries.iter().any(|e| same(e, &dir)) {
            return Ok(Vec::new());
        }
        let mut next: Vec<&str> = entries;
        next.push(&dir);
        write(&key, &next.join(";"))?;
        drop(key);
        if !overridden {
            broadcast();
        }
        Ok(vec![format!("user PATH (HKCU\\{name})")])
    }

    pub fn remove(dir: &Path) -> Result<Vec<String>, String> {
        let dir = dir.display().to_string();
        let (name, overridden) = subkey();
        let key = open(&name)?;
        let raw = read(&key)?;
        let entries: Vec<&str> = raw.split(';').filter(|e| !e.trim().is_empty()).collect();
        let kept: Vec<&str> = entries.iter().copied().filter(|e| !same(e, &dir)).collect();
        if kept.len() == entries.len() {
            return Ok(Vec::new());
        }
        write(&key, &kept.join(";"))?;
        drop(key);
        if !overridden {
            broadcast();
        }
        Ok(vec![format!("user PATH (HKCU\\{name})")])
    }
}

#[cfg(unix)]
mod imp {
    use std::path::{Path, PathBuf};

    pub const BEGIN: &str = "# >>> wrongmemory >>>";
    pub const END: &str = "# <<< wrongmemory <<<";

    /// POSIX single-quoted word.
    fn sh_quote(s: &str) -> String {
        format!("'{}'", s.replace('\'', r"'\''"))
    }

    /// fish single-quoted word.
    fn fish_quote(s: &str) -> String {
        format!("'{}'", s.replace('\\', r"\\").replace('\'', r"\'"))
    }

    /// The block `install.sh` writes too; keep the two identical.
    pub fn sh_block(dir: &str) -> String {
        format!(
            "{BEGIN}\n\
             # Added by the WrongMemory installer; `wrongmemory uninstall` removes this block.\n\
             wm_bin={}\n\
             case \":$PATH:\" in *\":$wm_bin:\"*) ;; *) PATH=\"$wm_bin:$PATH\"; export PATH ;; esac\n\
             unset wm_bin\n\
             {END}\n",
            sh_quote(dir)
        )
    }

    pub fn fish_file(dir: &str) -> String {
        let q = fish_quote(dir);
        format!(
            "# Added by the WrongMemory installer; `wrongmemory uninstall` removes this file.\n\
             if not contains -- {q} $PATH\n    set -gx PATH {q} $PATH\nend\n"
        )
    }

    fn home() -> Result<PathBuf, String> {
        std::env::var_os("HOME")
            .filter(|h| !h.is_empty())
            .map(PathBuf::from)
            .ok_or_else(|| "HOME is not set".to_string())
    }

    fn shell() -> String {
        std::env::var("SHELL")
            .ok()
            .and_then(|s| {
                Path::new(&s)
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
            })
            .unwrap_or_default()
    }

    fn fish_conf(home: &Path) -> PathBuf {
        let config = std::env::var_os("XDG_CONFIG_HOME")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
            .unwrap_or_else(|| home.join(".config"));
        config.join("fish").join("conf.d").join("wrongmemory.fish")
    }

    /// POSIX startup files for this user's shell.
    fn sh_files(home: &Path) -> Vec<PathBuf> {
        let mut files = vec![home.join(".profile")];
        match shell().as_str() {
            "bash" => {
                files.push(home.join(".bashrc"));
                // Login bash reads only the first of these that exists.
                if home.join(".bash_profile").exists() {
                    files.push(home.join(".bash_profile"));
                }
            }
            "zsh" => {
                let zdot = std::env::var_os("ZDOTDIR")
                    .filter(|v| !v.is_empty())
                    .map(PathBuf::from)
                    .unwrap_or_else(|| home.to_path_buf());
                files.push(zdot.join(".zshrc"));
            }
            _ => {}
        }
        files
    }

    /// `text` with every WrongMemory block naming `dir` removed (all blocks
    /// when `dir` is `None`).
    pub fn strip(text: &str, dir: Option<&str>) -> String {
        let mut out = String::with_capacity(text.len());
        let mut block: Option<String> = None;
        for line in text.split_inclusive('\n') {
            match &mut block {
                None if line.trim_end() == BEGIN => block = Some(line.to_string()),
                None => out.push_str(line),
                Some(b) => {
                    b.push_str(line);
                    if line.trim_end() == END {
                        let ours =
                            dir.is_none_or(|d| b.contains(&format!("wm_bin={}\n", sh_quote(d))));
                        if !ours {
                            out.push_str(b);
                        }
                        block = None;
                    }
                }
            }
        }
        // An unterminated block is not ours to drop.
        if let Some(b) = block {
            out.push_str(&b);
        }
        out
    }

    fn write_if_changed(path: &Path, before: &str, after: &str) -> Result<bool, String> {
        if before == after {
            return Ok(false);
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
        }
        std::fs::write(path, after).map_err(|e| format!("{}: {e}", path.display()))?;
        Ok(true)
    }

    pub fn add(dir: &Path) -> Result<Vec<String>, String> {
        let home = home()?;
        let dir = dir.display().to_string();
        let mut changed = Vec::new();
        for file in sh_files(&home) {
            let before = std::fs::read_to_string(&file).unwrap_or_default();
            let without_ours = strip(&before, Some(&dir));
            let has_ours = without_ours != before;
            let has_other = strip(&before, None) != without_ours;
            if has_ours && !has_other {
                continue; // already set up; leave the file exactly as it is
            }
            // Drop blocks for another prefix (an earlier install elsewhere),
            // then append ours after a blank line.
            let mut after = strip(&before, None);
            if !after.is_empty() && !after.ends_with('\n') {
                after.push('\n');
            }
            if !after.is_empty() && !after.ends_with("\n\n") {
                after.push('\n');
            }
            after.push_str(&sh_block(&dir));
            if write_if_changed(&file, &before, &after)? {
                changed.push(file.display().to_string());
            }
        }
        if shell() == "fish" {
            let file = fish_conf(&home);
            let before = std::fs::read_to_string(&file).unwrap_or_default();
            if write_if_changed(&file, &before, &fish_file(&dir))? {
                changed.push(file.display().to_string());
            }
        }
        Ok(changed)
    }

    pub fn remove(dir: &Path) -> Result<Vec<String>, String> {
        let home = home()?;
        let dir = dir.display().to_string();
        let mut changed = Vec::new();
        let mut files = vec![
            home.join(".profile"),
            home.join(".bashrc"),
            home.join(".bash_profile"),
        ];
        let zdot = std::env::var_os("ZDOTDIR")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| home.clone());
        files.push(zdot.join(".zshrc"));
        for file in files {
            let Ok(before) = std::fs::read_to_string(&file) else {
                continue;
            };
            let mut after = strip(&before, Some(&dir));
            if after != before {
                // Drop the blank line add() put before the block.
                while after.ends_with("\n\n") {
                    after.pop();
                }
            }
            if write_if_changed(&file, &before, &after)? {
                changed.push(file.display().to_string());
            }
        }
        let fish = fish_conf(&home);
        if std::fs::read_to_string(&fish).is_ok_and(|t| t == fish_file(&dir)) {
            std::fs::remove_file(&fish).map_err(|e| format!("{}: {e}", fish.display()))?;
            changed.push(fish.display().to_string());
        }
        Ok(changed)
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::imp::*;

    #[test]
    fn blocks_are_replaced_and_stripped_by_prefix() {
        let user = "export EDITOR=vi\n";
        let a = format!("{user}\n{}", sh_block("/a/bin"));
        // Stripping another prefix keeps the block; stripping ours drops it.
        assert_eq!(strip(&a, Some("/b/bin")), a);
        assert_eq!(strip(&a, Some("/a/bin")), format!("{user}\n"));
        assert_eq!(strip(&a, None), format!("{user}\n"));
        // An unterminated block stays.
        let broken = format!("{user}{BEGIN}\nwm_bin='/a/bin'\n");
        assert_eq!(strip(&broken, None), broken);
    }

    #[test]
    fn quoting_survives_awkward_paths() {
        let b = sh_block("/home/o'brien/my $bin");
        assert!(b.contains(r#"wm_bin='/home/o'\''brien/my $bin'"#), "{b}");
        let f = fish_file(r"/x/it's");
        assert!(f.contains(r"'/x/it\'s'"), "{f}");
    }
}
