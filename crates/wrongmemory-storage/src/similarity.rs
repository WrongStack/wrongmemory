//! `memory-similarity.ts` port: near-duplicate and contradiction heuristics
//! shared by remember merge and hygiene, and relationship scoring used by
//! `findRelatedSage`.

use std::collections::HashSet;

use wrongmemory_contracts::{MemoryAnchor, SageRecord};

use crate::text::{normalize_slashes, tokenize_default, unicode_fold};

const NEAR_DUP_OVERLAP_THRESHOLD: f64 = 0.88;
const NEAR_DUP_MIN_TOKENS: usize = 5;

const NEGATION_CUES: &[&str] = &[
    "not",
    "never",
    "none",
    "neither",
    "nor",
    "cannot",
    "cant",
    "no_longer",
    "don",
    "doesn",
    "isn",
    "aren",
    "wasn",
    "weren",
    "hasn",
    "haven",
    "didn",
    "couldn",
    "shouldn",
    "wouldn",
];

fn token_set(text: &str) -> HashSet<String> {
    tokenize_default(text).into_iter().collect()
}

/// Szymkiewicz–Simpson overlap of the token sets.
pub fn text_token_overlap(a: &str, b: &str) -> f64 {
    let (l, r) = (token_set(a), token_set(b));
    if l.is_empty() || r.is_empty() {
        return 0.0;
    }
    l.intersection(&r).count() as f64 / l.len().min(r.len()) as f64
}

pub fn structural_anchor_key(a: &MemoryAnchor) -> Option<String> {
    if a.kind == "command"
        && let Some(c) = &a.command
    {
        return Some(format!("command:{}", unicode_fold(c.trim()).trim()));
    }
    if a.kind == "agent"
        && let Some(r) = &a.role
    {
        return Some(format!("agent:{}", r.to_lowercase()));
    }
    if a.kind == "symbol"
        && let Some(s) = &a.symbol
    {
        let sym = s.trim().to_lowercase();
        return Some(match &a.path {
            Some(p) => format!("symbol:{}#{sym}", normalize_slashes(p).to_lowercase()),
            None => format!("symbol:{sym}"),
        });
    }
    a.path
        .as_ref()
        .map(|p| format!("{}:{}", a.kind, normalize_slashes(p).to_lowercase()))
}

fn share_structural_anchor(a: &[MemoryAnchor], b: &[MemoryAnchor]) -> bool {
    if a.is_empty() || b.is_empty() {
        return false;
    }
    let keys: HashSet<String> = a.iter().filter_map(structural_anchor_key).collect();
    b.iter()
        .filter_map(structural_anchor_key)
        .any(|k| keys.contains(&k))
}

pub fn is_near_duplicate(
    left_text: &str,
    left_kind: &str,
    left_anchors: &[MemoryAnchor],
    right: &SageRecord,
) -> bool {
    if left_kind != right.kind {
        return false;
    }
    if tokenize_default(left_text).len() < NEAR_DUP_MIN_TOKENS
        || tokenize_default(&right.text).len() < NEAR_DUP_MIN_TOKENS
    {
        return false;
    }
    let overlap = text_token_overlap(left_text, &right.text);
    overlap >= NEAR_DUP_OVERLAP_THRESHOLD
        || (overlap >= 0.72 && share_structural_anchor(left_anchors, &right.anchors))
}

/// `foldNegationPhrases`: `no longer` -> `no_longer` (case-insensitive).
fn fold_negation(text: &str) -> String {
    let re = fancy_regex::Regex::new(r"(?i)\bno\s+longer\b").expect("pattern");
    re.replace_all(text, "no_longer").into_owned()
}

pub fn is_possibly_contradictory(a: &str, b: &str) -> bool {
    let ta = tokenize_default(&fold_negation(a));
    let tb = tokenize_default(&fold_negation(b));
    if ta.len() < 5 || tb.len() < 5 {
        return false;
    }
    let sa: HashSet<&String> = ta.iter().collect();
    let sb: HashSet<&String> = tb.iter().collect();
    let overlap = sa.intersection(&sb).count();
    let min = sa.len().min(sb.len());
    if min == 0 || (overlap as f64) / (min as f64) < 0.72 {
        return false;
    }
    let neg = |t: &&String| NEGATION_CUES.contains(&t.as_str());
    let diff_a: Vec<&&String> = sa.iter().filter(|t| !sb.contains(**t)).collect();
    let diff_b: Vec<&&String> = sb.iter().filter(|t| !sa.contains(**t)).collect();
    if diff_a.is_empty() {
        return diff_b.iter().any(|t| neg(t));
    }
    if diff_b.is_empty() {
        return diff_a.iter().any(|t| neg(t));
    }
    diff_a.iter().any(|t| neg(t)) != diff_b.iter().any(|t| neg(t))
}

/// Number of tokens without dedupe (`tokenizeCount`).
pub fn tokenize_count(text: &str) -> usize {
    unicode_fold(text)
        .split(|c: char| !(c.is_alphabetic() || c.is_numeric() || matches!(c, '_' | '.' | '-')))
        .filter(|t| crate::text::js_length(t) >= 3)
        .count()
}

fn normalize_anchor_path(p: Option<&str>) -> String {
    let Some(p) = p.map(str::trim).filter(|p| !p.is_empty()) else {
        return String::new();
    };
    let mut n = normalize_slashes(p).to_lowercase();
    while let Some(rest) = n.strip_prefix("./") {
        n = rest.trim_start_matches('/').to_string();
    }
    if n.is_empty() { ".".into() } else { n }
}

pub(crate) fn normalize_command(c: &str) -> String {
    crate::text::collapse_js_space(&unicode_fold(c.trim()))
}

pub(crate) fn command_family(c: &str) -> String {
    c.split(crate::text::is_js_space)
        .take(2)
        .collect::<Vec<_>>()
        .join(" ")
}

/// `scoreMemoryRelationship`.
pub fn score_relationship(
    candidate: &SageRecord,
    seeds: &[&SageRecord],
    graph_related: &HashSet<String>,
) -> f64 {
    if seeds.iter().any(|s| s.id == candidate.id) {
        return 0.0;
    }
    let mut relation: f64 = if graph_related.contains(&candidate.id) {
        8.0
    } else {
        0.0
    };
    for seed in seeds {
        let shared = candidate
            .tags
            .iter()
            .filter(|t| seed.tags.contains(t))
            .count();
        relation += (shared as f64 * 1.5).min(4.0);
        for l in &candidate.anchors {
            for r in &seed.anchors {
                let lp = normalize_anchor_path(l.path.as_deref());
                let rp = normalize_anchor_path(r.path.as_deref());
                if let (Some(ls), Some(rs)) = (&l.symbol, &r.symbol)
                    && !ls.is_empty()
                    && ls.to_lowercase() == rs.to_lowercase()
                {
                    relation += if !lp.is_empty() && lp == rp {
                        12.0
                    } else {
                        8.0
                    };
                }
                if let (Some(lc), Some(rc)) = (&l.command, &r.command)
                    && !lc.is_empty()
                    && !rc.is_empty()
                {
                    let (a, b) = (normalize_command(lc), normalize_command(rc));
                    if a == b {
                        relation += 10.0;
                    } else if command_family(&a) == command_family(&b) {
                        relation += 5.0;
                    }
                }
                if let (Some(lr), Some(rr)) = (&l.role, &r.role)
                    && !lr.is_empty()
                    && lr.to_lowercase() == rr.to_lowercase()
                {
                    relation += 12.0;
                }
                if !lp.is_empty() && !rp.is_empty() {
                    let pkg = l.kind == "package" || r.kind == "package";
                    if lp == rp {
                        relation += if pkg { 10.0 } else { 8.0 };
                    } else if lp == "."
                        || rp == "."
                        || lp.starts_with(&format!("{rp}/"))
                        || rp.starts_with(&format!("{lp}/"))
                    {
                        relation += if pkg { 6.0 } else { 3.0 };
                    }
                }
            }
        }
    }
    if relation <= 0.0 {
        return 0.0;
    }
    let persistence_bonus = match candidate.persistence.as_deref().unwrap_or("long_lived") {
        "permanent" => 2.0,
        "long_lived" => 1.0,
        _ => -1.0,
    };
    let durable = [
        "fact",
        "decision",
        "convention",
        "warning",
        "anti_pattern",
        "workflow",
        "bug_root_cause",
        "file_note",
        "symbol_note",
        "command_note",
    ]
    .contains(&candidate.kind.as_str());
    relation
        + candidate.importance * 2.0
        + candidate.confidence
        + candidate.freshness
        + persistence_bonus
        + if durable { 1.0 } else { 0.0 }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn contradiction_needs_one_sided_negation() {
        assert!(is_possibly_contradictory(
            "The build pipeline uses cached docker layers for speed",
            "The build pipeline never uses cached docker layers for speed",
        ));
        assert!(!is_possibly_contradictory(
            "The build pipeline uses cached docker layers for speed",
            "The build pipeline uses cached docker layers for speed today",
        ));
    }

    #[test]
    fn overlap_and_counts() {
        assert_eq!(
            text_token_overlap("alpha beta gamma", "alpha beta gamma delta"),
            1.0
        );
        assert_eq!(tokenize_count("foo foo bar"), 3);
    }
}
