//! SAGE record DTOs (`packages/sage/src/memory-model.ts`, `types.ts` at the
//! baseline commit).
//!
//! Vocabulary fields (kind, scope, status, ...) are strings, not Rust enums: a
//! record written by a newer SAGE with a new kind must still decode and
//! round-trip. Validation against the known vocabulary happens on input paths.
//!
//! Every struct carries `extra` (flattened): fields this build does not know
//! survive a decode/encode cycle. Mutations that rewrite an existing record
//! must patch its original JSON object rather than re-encode this view, so the
//! JS `null` vs absent distinction is preserved as well.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// `SAGE_SCHEMA_VERSION` (record/manifest layer; distinct from the SQLite
/// `schema_meta.version` = 5).
pub const SAGE_RECORD_SCHEMA_VERSION: i64 = 1;
/// `SQLITE_SCHEMA_VERSION` at the baseline.
pub const SAGE_SQLITE_SCHEMA_VERSION: i64 = 5;

pub const SAGE_KINDS: &[&str] = &[
    "fact",
    "decision",
    "convention",
    "preference",
    "warning",
    "anti_pattern",
    "workflow",
    "bug_root_cause",
    "file_note",
    "symbol_note",
    "command_note",
    "summary",
    "memory_review",
    "tool_outcome",
    "error_pattern",
    "session_digest",
    "role_operational",
    "task_outcome",
    "security_signal",
    "fleet_convention",
];

pub const SAGE_SCOPES: &[&str] = &["project", "user", "session", "file", "symbol"];

pub const SAGE_STATUSES: &[&str] = &[
    "active",
    "stale",
    "superseded",
    "contradicted",
    "archived",
    "deleted",
];

pub const PERSISTENCE_CLASSES: &[&str] = &["permanent", "long_lived", "short_lived"];

pub const ANCHOR_TYPES: &[&str] = &[
    "file",
    "directory",
    "symbol",
    "package",
    "command",
    "test",
    "git",
    "agent",
];

pub const SOURCE_TYPES: &[&str] = &[
    "user",
    "session",
    "tool_result",
    "project_instruction",
    "file",
    "test",
    "command",
    "legacy_memory",
];

pub const CANDIDATE_STATUSES: &[&str] = &["pending", "accepted", "rejected", "merged"];

pub const SUGGESTED_ACTIONS: &[&str] = &[
    "delete",
    "archive",
    "keep",
    "investigate",
    "update",
    "consolidate",
];

pub const GRAPH_RELATIONS: &[&str] = &[
    "about_file",
    "about_directory",
    "about_symbol",
    "about_package",
    "about_command",
    "about_agent",
    "derived_from",
    "validated_by",
    "invalidated_by",
    "supersedes",
    "contradicts",
    "related_to",
    "same_topic",
];

/// `memory-input-validation.ts`: maximum text length in UTF-16 code units.
pub const MAX_TEXT_CHARS: usize = 20_000;
/// Maximum items in each metadata array (tags, anchors, sources, ...).
pub const MAX_METADATA_ITEMS: usize = 128;
/// `MemoryValidity.checks` upper bound.
pub const MAX_VALIDITY_CHECKS: usize = 4;

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MemoryAnchor {
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub symbol: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MemoryAudience {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub roles: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_types: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub modes: Option<Vec<String>>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MemorySource {
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ValidityCheck {
    #[serde(rename = "type")]
    pub kind: String,
    pub path: String,
    pub text: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MemoryValidity {
    pub statement: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checks: Option<Vec<ValidityCheck>>,
}

/// A SAGE memory record (`Sage` in memory-model.ts).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SageRecord {
    pub id: String,
    #[serde(default)]
    pub revision: i64,
    pub scope: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub legacy_scope: Option<String>,
    pub kind: String,
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_policy: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub persistence: Option<String>,
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    pub importance: f64,
    pub confidence: f64,
    pub freshness: f64,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub anchors: Vec<MemoryAnchor>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audience: Option<MemoryAudience>,
    #[serde(default)]
    pub sources: Vec<MemorySource>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub validity: Option<MemoryValidity>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub feedback: Option<Vec<Value>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supersedes: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub superseded_by: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub contradicts: Option<Vec<String>>,
    pub created_at: String,
    pub updated_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_accessed_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_verified_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stale_reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_used_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub injection_count: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub use_count: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner_session_id: Option<String>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// A review proposal (`MemoryCandidate` in types.ts).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MemoryCandidate {
    #[serde(default = "one")]
    pub schema_version: i64,
    pub id: String,
    pub status: String,
    pub text: String,
    pub kind: String,
    pub scope: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner_session_id: Option<String>,
    pub confidence: f64,
    pub importance: f64,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub anchors: Vec<MemoryAnchor>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audience: Option<MemoryAudience>,
    #[serde(default)]
    pub sources: Vec<MemorySource>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub validity: Option<MemoryValidity>,
    pub created_at: String,
    pub updated_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_memory_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_revision: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review_reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub suggested_action: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supersedes_memory_ids: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supersedes_revisions: Option<Vec<i64>>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

fn one() -> i64 {
    1
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn unknown_fields_round_trip() {
        let raw = json!({
            "id": "01J", "revision": 3, "scope": "project", "kind": "fact",
            "status": "active", "text": "çalışma 🚀", "importance": 0.6,
            "confidence": 0.8, "freshness": 1, "tags": ["a"],
            "anchors": [{"type": "file", "path": "src/a.ts", "lineStart": 4, "futureAnchorField": true}],
            "sources": [{"type": "user", "excerptHash": "h"}],
            "createdAt": "2026-11-01T00:00:00.000Z", "updatedAt": "2026-11-01T00:00:00.000Z",
            "futureTopLevel": {"nested": [1, 2]}, "kindFromTheFuture": "x"
        });
        let rec: SageRecord = serde_json::from_value(raw.clone()).unwrap();
        assert_eq!(rec.anchors[0].extra["lineStart"], json!(4));
        let back = serde_json::to_value(&rec).unwrap();
        assert_eq!(back["futureTopLevel"], raw["futureTopLevel"]);
        assert_eq!(back["anchors"][0]["futureAnchorField"], json!(true));
        assert_eq!(back["sources"][0]["excerptHash"], json!("h"));
        assert_eq!(back["text"], raw["text"]);
    }

    #[test]
    fn unknown_vocabulary_still_decodes() {
        let rec: SageRecord = serde_json::from_value(json!({
            "id": "x", "revision": 1, "scope": "galaxy", "kind": "new_kind", "status": "frozen",
            "text": "t", "importance": 0.5, "confidence": 0.5, "freshness": 0.5,
            "createdAt": "a", "updatedAt": "b"
        }))
        .unwrap();
        assert_eq!(rec.kind, "new_kind");
    }

    #[test]
    fn vocabulary_sizes_match_baseline() {
        assert_eq!(SAGE_KINDS.len(), 20);
        assert_eq!(SAGE_SCOPES.len(), 5);
        assert_eq!(SAGE_STATUSES.len(), 6);
    }
}
