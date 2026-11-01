//! Input rules ported from `memory-input-validation.ts`, `store-helpers.ts`
//! (`looksLikeSecret`), `shared/progress-log.ts` and
//! `shared/memory-validity.ts` at the baseline commit.
//!
//! The JS patterns use lookaround, so they run on `fancy-regex`. JS `\b` is
//! ASCII-word based while the Rust engines are Unicode-aware; patterns here
//! are wrapped in `(?-u:...)`-free form and checked against the oracle's
//! known answers where available.

use std::sync::LazyLock;

use fancy_regex::Regex;
use serde_json::{Map, Value};
use wrongmemory_contracts::{ErrorBody, ErrorCode, MAX_METADATA_ITEMS, MemoryAnchor};

use crate::text::{js_length, normalize_text, tokenize_default};

fn invalid(msg: impl Into<String>) -> ErrorBody {
    ErrorBody::new(ErrorCode::InvalidRequest, msg)
}

fn re(p: &str) -> Regex {
    Regex::new(p).expect("baseline pattern compiles")
}

static SECRET: LazyLock<Regex> = LazyLock::new(|| {
    re(&format!(
        "(?i){}",
        [
            r"-----BEGIN [A-Z ]*PRIVATE KEY(?: BLOCK)?-----",
            r#"(?<![A-Za-z0-9])(?:api[_-]?key|secret|token|password)(?![A-Za-z0-9])[A-Za-z0-9_.-]*\s*[:=]\s*['"]?[A-Za-z0-9_\-./+=]{16,}"#,
            r"\b[A-Za-z0-9_]{20,}\.[A-Za-z0-9_-]{20,}\.[A-Za-z0-9_-]{20,}\b",
            r"\b(?:sk-(?:ant-)?|gh[pousr]_|github_pat_|xox[baprs]-)[A-Za-z0-9_-]{16,}\b",
            r"\bAKIA[0-9A-Z]{16}\b",
            r"\bAIza[0-9A-Za-z_-]{35}\b",
            r"\bhf_[A-Za-z0-9]{20,}\b",
            r"\b(?:sk|pk|rk)_(?:live|test)_[A-Za-z0-9]{20,}\b",
            r"\bnpm_[A-Za-z0-9]{36,}\b",
            r"\b[MNO][A-Za-z\d]{23,}\.[A-Za-z\d_-]{6,}\.[A-Za-z\d_-]{27,}\b",
        ]
        .join("|")
    ))
});

static EPHEMERAL: LazyLock<Vec<Regex>> = LazyLock::new(|| {
    [
        r"(?i)^(wip|todo|fixme|hack)\b(?![\s-]*(?:lists?|items?|tools?|boards?|panels?|sync(?:s|ing)?|entr(?:y|ies)|comments?|markers?|tracking|widgets?|views?|state|mode)\b)",
        r"(?i)\b(still working on|looking into|need to (?:fix|check|investigate)|will (?:fix|look|check) (?:this|that|it) later)\b",
        r"(?i)^(debugging|investigating|checking|reading) (the )?(file|code|issue|bug)\b",
        r"(?i)^(fixed|updated|changed) (the )?(bug|issue|test|file)\.?$",
    ]
    .iter()
    .map(|p| re(p))
    .collect()
});

static PROGRESS: LazyLock<Vec<Regex>> = LazyLock::new(|| {
    [
        r"(?i)\brounds?\s+r?\d+\s*(?:/|of)\s*\d+\b|\br\d{1,3}-20\d{6}\b|\(\d{1,3}/\d{1,3} session\)|\brounds?\s+r?\d+(?:-\d+)?\b(?=[^.]{0,40}\b(?:session|campaign|20\d\d))",
        r"(?i)\bLANDED\b|\bNO-PROVEN-BUG\b|\bzero (?:production )?edits\b|\bfixed-and-verified\b|\bfixed-verification-incomplete\b|\boutcome\s*:?\s*`?(?:no-proven-bug|fixed)",
        r"(?i)\b\d{1,5}/\d{1,5}\b(?=[^.\n]{0,30}\b(?:pass|green|tests?|suites?)\b)|\b(?:suites?|tests?)\s+\d{1,5}/\d{1,5}\b",
        r"(?i)\bHEAD\s+[0-9a-f]{7,40}\b",
        r"(?i)\bremaining\b[^.]{0,80}\b(?:rounds?|for future|surfaces?)\b|\bnext round\b",
    ]
    .iter()
    .map(|p| re(p))
    .collect()
});

static AGENT_ROLE: LazyLock<Regex> = LazyLock::new(|| re(r"(?i)^[a-z0-9][a-z0-9._-]{0,95}$"));

pub const STRUCTURAL_KINDS: &[&str] = &["file_note", "symbol_note", "command_note"];

pub fn looks_like_secret(text: &str) -> bool {
    SECRET.is_match(text).unwrap_or(false)
}

pub fn is_ephemeral(text: &str) -> bool {
    EPHEMERAL.iter().any(|r| r.is_match(text).unwrap_or(false))
}

pub fn is_progress_log(text: &str) -> bool {
    PROGRESS
        .iter()
        .filter(|r| r.is_match(text).unwrap_or(false))
        .count()
        >= 2
}

/// `rejectIfUnsafeInput`: every string anywhere in the input is checked.
pub fn reject_if_unsafe(input: &Value) -> Result<(), ErrorBody> {
    fn walk(v: &Value) -> bool {
        match v {
            Value::String(s) => looks_like_secret(s),
            Value::Array(a) => a.iter().any(walk),
            Value::Object(m) => m.values().any(walk),
            _ => false,
        }
    }
    if walk(input) {
        return Err(invalid(
            "SAGE refused to store text that looks like a secret or credential.",
        ));
    }
    Ok(())
}

/// `assessRememberQuality` caps (applied only to defaulted scores).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct QualityCaps {
    pub importance: f64,
    pub confidence: f64,
}

pub fn quality_caps(text: &str, kind: &str, scope: &str, anchors: usize) -> QualityCaps {
    let text = normalize_text(text);
    let mut c = QualityCaps {
        importance: 1.0,
        confidence: 1.0,
    };
    let mut cap = |imp: f64, conf: f64| {
        c.importance = c.importance.min(imp);
        c.confidence = c.confidence.min(conf);
    };
    if js_length(&text) < 12 {
        cap(0.55, 0.55);
    }
    if tokenize_default(&text).len() < 3 {
        cap(0.6, 0.6);
    }
    if anchors == 0 {
        let imp = if scope == "session" || scope == "user" {
            1.0
        } else {
            0.7
        };
        cap(imp, 0.75);
        if kind == "bug_root_cause" {
            cap(0.75, 0.65);
        }
    }
    if is_ephemeral(&text) {
        cap(0.35, 0.35);
    }
    if is_progress_log(&text) {
        cap(0.55, 1.0);
    }
    c
}

/// Text rules shared by remember and update.
pub fn validate_text(text: &str) -> Result<String, ErrorBody> {
    if js_length(text) > wrongmemory_contracts::MAX_TEXT_CHARS {
        return Err(invalid(format!(
            "SAGE text exceeds {} characters.",
            wrongmemory_contracts::MAX_TEXT_CHARS
        )));
    }
    let t = normalize_text(text);
    if t.is_empty() {
        return Err(invalid("SAGE text must not be empty."));
    }
    if js_length(&t) < 4 {
        return Err(invalid(
            "SAGE text is too short to be useful long-term memory.",
        ));
    }
    Ok(t)
}

pub fn ephemeral_rejection() -> ErrorBody {
    invalid(
        "SAGE rejected ephemeral progress text. Store durable facts, decisions, conventions, or root causes — not WIP/todo chatter. Use todos for task state.",
    )
}

pub fn validate_tags(tags: &[String]) -> Result<(), ErrorBody> {
    if tags.len() > MAX_METADATA_ITEMS {
        return Err(invalid(format!(
            "SAGE tags exceeds {MAX_METADATA_ITEMS} items."
        )));
    }
    if tags.iter().any(|t| js_length(t) > 256) {
        return Err(invalid(
            "SAGE tags must be strings no longer than 256 characters.",
        ));
    }
    Ok(())
}

/// `validateMemoryAnchors`.
pub fn validate_anchors(anchors: &[MemoryAnchor]) -> Result<(), ErrorBody> {
    if anchors.len() > MAX_METADATA_ITEMS {
        return Err(invalid(format!(
            "SAGE anchors exceeds {MAX_METADATA_ITEMS} items."
        )));
    }
    for a in anchors {
        if !wrongmemory_contracts::ANCHOR_TYPES.contains(&a.kind.as_str()) {
            return Err(invalid("Invalid SAGE anchor type."));
        }
        let blank = |s: &Option<String>| s.as_deref().is_none_or(|v| v.trim().is_empty());
        match a.kind.as_str() {
            "command" if blank(&a.command) => {
                return Err(invalid("Command memory anchors require a command."));
            }
            "agent" => {
                if blank(&a.role) {
                    return Err(invalid("Agent memory anchors require a role."));
                }
                if !AGENT_ROLE
                    .is_match(a.role.as_deref().unwrap_or("").trim())
                    .unwrap_or(false)
                {
                    return Err(invalid("Agent memory anchor role is invalid."));
                }
            }
            "command" => {}
            k if blank(&a.path) => {
                return Err(invalid(format!("{k} memory anchors require a path.")));
            }
            _ => {}
        }
        if a.kind == "symbol" && blank(&a.symbol) {
            return Err(invalid("Symbol memory anchors require a symbol."));
        }
        let len = |s: &Option<String>| s.as_deref().map(js_length).unwrap_or(0);
        if len(&a.path) > 4096 || len(&a.symbol) > 1024 || len(&a.command) > 8192 {
            return Err(invalid(
                "SAGE anchor strings are too long (path ≤ 4096, symbol ≤ 1024, command ≤ 8192, role ≤ 96 characters).",
            ));
        }
    }
    Ok(())
}

/// `normalizeValidity`: strict shape; relative literal paths only.
pub fn normalize_validity(v: &Value) -> Result<Option<Value>, ErrorBody> {
    let bad = || invalid("SAGE validity requires a statement and optional source checks.");
    let Value::Object(m) = v else {
        return Err(bad());
    };
    if m.keys().any(|k| k != "statement" && k != "checks") {
        return Err(invalid("Unknown SAGE validity field."));
    }
    let statement = m
        .get("statement")
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty() && js_length(s) <= 1000)
        .ok_or_else(|| invalid("SAGE validity statement must contain 1–1000 characters."))?;
    let mut out = Map::new();
    out.insert("statement".into(), Value::String(statement.trim().into()));
    if let Some(checks) = m.get("checks") {
        let arr = checks
            .as_array()
            .filter(|a| a.len() <= 4)
            .ok_or_else(|| invalid("SAGE validity supports at most four source checks."))?;
        let mut norm = Vec::new();
        for c in arr {
            let msg = "SAGE validity checks require a relative project path and a literal of 1–400 characters.";
            let o = c
                .as_object()
                .ok_or_else(|| invalid("Invalid SAGE validity check."))?;
            let path = o.get("path").and_then(Value::as_str).unwrap_or("");
            let text = o.get("text").and_then(Value::as_str);
            let lower = path.to_ascii_lowercase();
            let drive = lower.len() >= 2
                && lower.as_bytes()[0].is_ascii_alphabetic()
                && lower.as_bytes()[1] == b':';
            let ok = o
                .keys()
                .all(|k| matches!(k.as_str(), "type" | "path" | "text"))
                && o.get("type").and_then(Value::as_str) == Some("source_contains")
                && !path.trim().is_empty()
                && js_length(path) <= 500
                && !path.starts_with(['/', '\\'])
                && !drive
                && !path.split(['/', '\\']).any(|p| p == "..")
                && !path.chars().any(|ch| (ch as u32) < 0x20)
                && text.is_some_and(|t| !t.trim().is_empty() && js_length(t) <= 400);
            if !ok {
                return Err(invalid(msg));
            }
            norm.push(serde_json::json!({
                "type": "source_contains",
                "path": path.replace('\\', "/"),
                "text": text.unwrap_or(""),
            }));
        }
        if !norm.is_empty() {
            out.insert("checks".into(), Value::Array(norm));
        }
    }
    Ok(Some(Value::Object(out)))
}

/// `normalizeAudience`: NFKC/trim/lowercase, dedupe; empty -> None.
pub fn normalize_audience(v: &Value) -> Result<Option<Value>, ErrorBody> {
    let Value::Object(m) = v else {
        return Err(invalid("SAGE audience must be an object."));
    };
    let mut out = Map::new();
    for key in ["roles", "taskTypes", "modes"] {
        let Some(values) = m.get(key) else { continue };
        let arr = values
            .as_array()
            .filter(|a| a.iter().all(Value::is_string))
            .ok_or_else(|| invalid(format!("SAGE audience.{key} must be an array of strings.")))?;
        if arr.len() > MAX_METADATA_ITEMS {
            return Err(invalid(format!(
                "SAGE audience.{key} exceeds {MAX_METADATA_ITEMS} items."
            )));
        }
        let mut items: Vec<String> = Vec::new();
        for s in arr.iter().filter_map(Value::as_str) {
            let n = crate::text::unicode_fold(s).trim().to_string();
            if !n.is_empty() && !items.contains(&n) {
                items.push(n);
            }
        }
        if items.iter().any(|i| js_length(i) > 256) {
            return Err(invalid(format!(
                "SAGE audience.{key} values must be no longer than 256 characters."
            )));
        }
        if !items.is_empty() {
            out.insert(key.into(), serde_json::json!(items));
        }
    }
    Ok(if out.is_empty() {
        None
    } else {
        Some(Value::Object(out))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn secrets_are_detected_without_false_positives() {
        assert!(looks_like_secret(
            "AWS_SECRET_ACCESS_KEY=abcdefghijklmnop1234"
        ));
        assert!(looks_like_secret("token: 'abcdefghijklmnopqrst'"));
        assert!(looks_like_secret(&["use gh", "p_", "abcdefghijklmnopqrstuvwx"].concat()));
        assert!(looks_like_secret("-----BEGIN RSA PRIVATE KEY-----"));
        assert!(!looks_like_secret("mytoken=abcdefghijklmnopqrstuv"));
        assert!(!looks_like_secret("secretly=abcdefghijklmnopqrstuv"));
        assert!(!looks_like_secret("Use bun run for repository scripts"));
    }

    #[test]
    fn ephemeral_and_progress_patterns() {
        assert!(is_ephemeral("WIP refactor parser"));
        assert!(!is_ephemeral("TODO list widget renders lazily"));
        assert!(is_ephemeral("fixed the bug."));
        assert!(is_progress_log("Round 3 of 7 LANDED at HEAD abc1234"));
        assert!(!is_progress_log("Round 3 of 7"));
    }

    #[test]
    fn validity_rules() {
        assert!(normalize_validity(&json!({"statement": "x", "checks": [{"type": "source_contains", "path": "../etc", "text": "a"}]})).is_err());
        assert!(normalize_validity(&json!({"statement": "x", "checks": [{"type": "source_contains", "path": "C:/x", "text": "a"}]})).is_err());
        assert_eq!(
            normalize_validity(&json!({"statement": " ok ", "checks": [{"type": "source_contains", "path": "src\\a.ts", "text": "fn"}]})).unwrap().unwrap(),
            json!({"statement": "ok", "checks": [{"type": "source_contains", "path": "src/a.ts", "text": "fn"}]})
        );
        assert!(normalize_validity(&json!({"statement": "x", "extra": 1})).is_err());
    }

    #[test]
    fn audience_normalizes() {
        assert_eq!(
            normalize_audience(&json!({"roles": [" Reviewer ", "reviewer"], "modes": []})).unwrap(),
            Some(json!({"roles": ["reviewer"]}))
        );
        assert_eq!(normalize_audience(&json!({})).unwrap(), None);
    }
}
