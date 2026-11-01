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

/// `tokenize` (memory-text.ts): NFKC + lowercase, split on
/// `[^\p{L}\p{N}_.-]+`, keep terms of at least 3 UTF-16 units, dedupe.
pub fn tokenize_default(text: &str) -> Vec<String> {
    let folded = unicode_fold(text);
    let mut out: Vec<String> = Vec::new();
    for t in folded
        .split(|c: char| !(c.is_alphabetic() || c.is_numeric() || matches!(c, '_' | '.' | '-')))
    {
        if js_length(t) >= 3 && !out.iter().any(|x| x == t) {
            out.push(t.to_string());
        }
    }
    out
}

/// Lenient ISO-8601 parse to epoch milliseconds, covering the shapes
/// `Date.parse` accepts in SAGE data: `YYYY-MM-DD`, optional `THH:MM`,
/// `:SS`, fraction, and `Z` / `±HH:MM` (no offset = UTC, as for date-only).
pub fn parse_iso_ms(s: &str) -> Option<i64> {
    let b = s.trim().as_bytes();
    let num = |r: std::ops::Range<usize>| -> Option<i64> {
        let t = std::str::from_utf8(b.get(r)?).ok()?;
        if t.bytes().all(|c| c.is_ascii_digit()) {
            t.parse().ok()
        } else {
            None
        }
    };
    if b.len() < 10 || b[4] != b'-' || b[7] != b'-' {
        return None;
    }
    let (y, mo, d) = (num(0..4)?, num(5..7)?, num(8..10)?);
    if !(1..=12).contains(&mo) || !(1..=31).contains(&d) {
        return None;
    }
    let (mut h, mut mi, mut sec, mut ms, mut off) = (0, 0, 0, 0i64, 0i64);
    let mut i = 10;
    if i < b.len() {
        if b[i] != b'T' && b[i] != b' ' {
            return None;
        }
        h = num(i + 1..i + 3)?;
        if b.get(i + 3) != Some(&b':') {
            return None;
        }
        mi = num(i + 4..i + 6)?;
        i += 6;
        if b.get(i) == Some(&b':') {
            sec = num(i + 1..i + 3)?;
            i += 3;
            if b.get(i) == Some(&b'.') {
                let start = i + 1;
                let mut end = start;
                while end < b.len() && b[end].is_ascii_digit() {
                    end += 1;
                }
                if end == start {
                    return None;
                }
                let frac = std::str::from_utf8(&b[start..end.min(start + 3)]).ok()?;
                ms = format!("{frac:0<3}").parse().ok()?;
                i = end;
            }
        }
        match b.get(i) {
            None => {}
            Some(b'Z') if i + 1 == b.len() => {}
            Some(&c) if (c == b'+' || c == b'-') && b.len() == i + 6 && b[i + 3] == b':' => {
                let oh = num(i + 1..i + 3)?;
                let om = num(i + 4..i + 6)?;
                off = (oh * 60 + om) * if c == b'+' { 1 } else { -1 };
            }
            _ => return None,
        }
    }
    if h > 23 || mi > 59 || sec > 59 {
        return None;
    }
    let (yy, m) = if mo <= 2 {
        (y - 1, mo + 9)
    } else {
        (y, mo - 3)
    };
    let era = yy.div_euclid(400);
    let yoe = yy - era * 400;
    let doy = (153 * m + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some(((days * 86_400 + h * 3600 + mi * 60 + sec) - off * 60) * 1000 + ms)
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
        assert_eq!(parse_iso_ms("1970-01-01T00:00:00.000Z"), Some(0));
        assert_eq!(
            parse_iso_ms("2026-11-01T12:00:00.123Z"),
            Some(1_793_534_400_123)
        );
        assert_eq!(parse_iso_ms("2026-11-01"), Some(1_793_491_200_000));
        assert_eq!(
            parse_iso_ms("2026-11-01T14:00:00+02:00"),
            Some(1_793_534_400_000)
        );
        assert!(parse_iso_ms("not a date").is_none());
    }
}
