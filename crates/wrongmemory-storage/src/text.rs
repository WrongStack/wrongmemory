//! Text normalization ported from `packages/sage/src/memory-text.ts` and
//! `paths.ts`.
//!
//! JS `/\s/` is not Rust `char::is_whitespace`: JS includes U+FEFF and
//! excludes U+0085 (NEL); Rust's White_Space property is the reverse for those
//! two code points. [`is_js_space`] reproduces the ECMAScript set exactly.
//! `String.prototype.toLowerCase` and Rust `str::to_lowercase` both apply the
//! Unicode full lowercase mapping including the final-sigma rule; oracle key
//! answers (contracts/fixtures/record-shape.json) remain the authority.

use unicode_normalization::UnicodeNormalization;

/// ECMAScript `WhiteSpace` + `LineTerminator` (the `\s` class).
pub fn is_js_space(c: char) -> bool {
    matches!(
        c,
        '\u{0009}'
            | '\u{000A}'
            | '\u{000B}'
            | '\u{000C}'
            | '\u{000D}'
            | '\u{0020}'
            | '\u{00A0}'
            | '\u{1680}'
            | '\u{2000}'
            ..='\u{200A}'
                | '\u{2028}'
                | '\u{2029}'
                | '\u{202F}'
                | '\u{205F}'
                | '\u{3000}'
                | '\u{FEFF}'
    )
}

/// `text.replace(/\s+/g, ' ').trim()` — `trim` strips the same set as `\s`.
pub fn collapse_js_space(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut pending_space = false;
    for c in text.chars() {
        if is_js_space(c) {
            pending_space = true;
        } else {
            if pending_space && !out.is_empty() {
                out.push(' ');
            }
            pending_space = false;
            out.push(c);
        }
    }
    out
}

/// `normalizeText`: whitespace collapse + trim, no case/NFKC change.
pub fn normalize_text(text: &str) -> String {
    collapse_js_space(text)
}

/// `normalizeTextKey`: `NFKC -> toLowerCase -> collapse \s -> trim`.
/// Stored in `memories.canonical_text` / `candidates.canonical_text`.
pub fn normalize_text_key(text: &str) -> String {
    let folded: String = text.nfkc().collect::<String>().to_lowercase();
    collapse_js_space(&folded)
}

/// `nfkc-lower-v1` fold used for `memories.unicode_text`.
pub fn unicode_fold(text: &str) -> String {
    text.nfkc().collect::<String>().to_lowercase()
}

/// `normalizeSlashes`: backslashes to `/`, collapse repeated `/`.
pub fn normalize_slashes(value: &str) -> String {
    let replaced = value.replace('\\', "/");
    let mut out = String::with_capacity(replaced.len());
    let mut prev_slash = false;
    for c in replaced.chars() {
        if c == '/' {
            if !prev_slash {
                out.push(c);
            }
            prev_slash = true;
        } else {
            out.push(c);
            prev_slash = false;
        }
    }
    out
}

/// `ancestorPaths`: `a/b/c` -> `[a/b/c, a/b, a]`; `.` -> `[.]`.
pub fn ancestor_paths(project_path: &str) -> Vec<String> {
    let normalized = normalize_slashes(project_path);
    if normalized == "." {
        return vec![".".into()];
    }
    let parts: Vec<&str> = normalized.split('/').filter(|p| !p.is_empty()).collect();
    (1..=parts.len())
        .rev()
        .map(|i| parts[..i].join("/"))
        .collect()
}

/// `path.posix.dirname` for normalized relative paths.
pub fn posix_dirname(p: &str) -> String {
    let trimmed = p.trim_end_matches('/');
    match trimmed.rfind('/') {
        None => ".".into(),
        Some(0) => "/".into(),
        Some(i) => trimmed[..i].to_string(),
    }
}

/// Number of UTF-16 code units (JS `string.length`).
pub fn js_length(text: &str) -> usize {
    text.encode_utf16().count()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn js_space_set_differs_from_rust_whitespace_where_expected() {
        assert!(is_js_space('\u{FEFF}') && !'\u{FEFF}'.is_whitespace());
        assert!(!is_js_space('\u{0085}') && '\u{0085}'.is_whitespace());
    }

    #[test]
    fn text_key_matches_sage_rules() {
        assert_eq!(normalize_text_key("  Use\tPNPM \r\n now  "), "use pnpm now");
        // NFKC folds the full-width and ligature forms before lowercasing.
        assert_eq!(normalize_text_key("ＡＢＣ ﬁle"), "abc file");
        assert_eq!(normalize_text("\u{FEFF}a\u{00A0}\u{00A0}b "), "a b");
        // NEL is not JS whitespace, so it survives.
        assert_eq!(normalize_text("a\u{0085}b"), "a\u{0085}b");
    }

    #[test]
    fn final_sigma_lowercases_like_js() {
        assert_eq!(unicode_fold("ΟΔΟΣ"), "οδος");
    }

    #[test]
    fn paths() {
        assert_eq!(normalize_slashes("src\\\\a//b.ts"), "src/a/b.ts");
        assert_eq!(ancestor_paths("a/b/c"), vec!["a/b/c", "a/b", "a"]);
        assert_eq!(ancestor_paths("."), vec!["."]);
        assert_eq!(posix_dirname("a.ts"), ".");
        assert_eq!(posix_dirname("src/x/a.ts"), "src/x");
        assert_eq!(js_length("🚀a"), 3);
    }
}
