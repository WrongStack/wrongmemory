//! Scoped change feed (WM-207, architecture §9).
//!
//! In memory only: per-project generation counter plus a bounded ring of
//! committed change events, published after a mutation has committed (the
//! reply path), never before. Events carry ids and revisions, never memory
//! text. Visibility is decided at delivery: session-owned entries go only to
//! the owning session, and memory ids are re-checked against the store with
//! the caller's viewer so a hidden or foreign record is never disclosed.
//! A gap outside the ring, or a different daemon instance, forces a resync.

use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;

use serde_json::Value;
use wrongmemory_contracts::{ChangeEvent, method};
use wrongmemory_registry::now_ms;

pub const DEFAULT_RING: usize = 1024;

#[derive(Debug, Clone)]
pub struct Entry {
    pub event: ChangeEvent,
    /// Session that owns the changed record (session scope), if any.
    pub owner_session: Option<String>,
}

#[derive(Default)]
struct Feed {
    generation: u64,
    ring: VecDeque<Entry>,
}

pub struct Events {
    feeds: Mutex<HashMap<String, Feed>>,
    capacity: usize,
}

pub struct Since {
    pub generation: u64,
    pub resync: bool,
    pub entries: Vec<Entry>,
}

impl Events {
    pub fn new(capacity: usize) -> Self {
        Self {
            feeds: Mutex::new(HashMap::new()),
            capacity: capacity.max(1),
        }
    }

    /// Append drafts, assigning consecutive generations.
    pub fn publish(&self, project_id: &str, drafts: Vec<Entry>) {
        if drafts.is_empty() {
            return;
        }
        let Ok(mut feeds) = self.feeds.lock() else {
            return;
        };
        let feed = feeds.entry(project_id.to_string()).or_default();
        for mut e in drafts {
            feed.generation += 1;
            e.event.generation = feed.generation;
            feed.ring.push_back(e);
            while feed.ring.len() > self.capacity {
                feed.ring.pop_front();
            }
        }
    }

    /// Entries after `generation`, or a resync marker when the range is gone.
    pub fn since(&self, project_id: &str, generation: u64) -> Since {
        let Ok(feeds) = self.feeds.lock() else {
            return Since {
                generation: 0,
                resync: true,
                entries: vec![],
            };
        };
        let Some(feed) = feeds.get(project_id) else {
            return Since {
                generation: 0,
                resync: generation != 0,
                entries: vec![],
            };
        };
        if generation > feed.generation {
            return Since {
                generation: feed.generation,
                resync: true,
                entries: vec![],
            };
        }
        let oldest = feed
            .ring
            .front()
            .map(|e| e.event.generation)
            .unwrap_or(feed.generation + 1);
        if generation + 1 < oldest {
            return Since {
                generation: feed.generation,
                resync: true,
                entries: vec![],
            };
        }
        Since {
            generation: feed.generation,
            resync: false,
            entries: feed
                .ring
                .iter()
                .filter(|e| e.event.generation > generation)
                .cloned()
                .collect(),
        }
    }
}

fn draft(change: &str) -> ChangeEvent {
    ChangeEvent {
        generation: 0,
        change: change.to_string(),
        memory_id: None,
        candidate_id: None,
        revision: None,
        telemetry: false,
        at_ms: now_ms(),
    }
}

fn owner_of(record: &Value) -> Option<String> {
    (record.get("scope").and_then(Value::as_str) == Some("session"))
        .then(|| {
            record
                .get("ownerSessionId")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .flatten()
}

/// Change events implied by a successful mutation reply.
pub fn drafts_for(m: &str, result: &Value) -> Vec<Entry> {
    let s = |v: &Value, k: &str| v.get(k).and_then(Value::as_str).map(str::to_string);
    match m {
        method::MEMORY_REMEMBER
        | method::MEMORY_UPDATE
        | method::MEMORY_FEEDBACK
        | method::MEMORY_DELETE
        | method::CANDIDATE_PROPOSE => {
            if result.get("replayed").and_then(Value::as_bool) == Some(true) {
                return vec![];
            }
            let record = result.get("record").cloned().unwrap_or(Value::Null);
            let mut e = draft(m);
            if result.get("outcome").and_then(Value::as_str) == Some("candidate-pending") {
                e.candidate_id = s(result, "id");
            } else {
                e.memory_id = s(result, "id");
                e.revision = result.get("revision").and_then(Value::as_i64);
            }
            vec![Entry {
                event: e,
                owner_session: owner_of(&record),
            }]
        }
        method::MEMORY_RECOVER => {
            let mut e = draft(m);
            e.memory_id = s(result, "id");
            e.revision = result.get("revision").and_then(Value::as_i64);
            vec![Entry {
                event: e,
                owner_session: owner_of(result),
            }]
        }
        method::CANDIDATE_ACCEPT | method::CANDIDATE_REJECT | method::CANDIDATE_RESOLVE => {
            if result.get("alreadyResolved").and_then(Value::as_bool) == Some(true) {
                return vec![];
            }
            let mut out = Vec::new();
            let mut c = draft(m);
            c.candidate_id = s(result, "candidateId");
            out.push(Entry {
                event: c,
                owner_session: None,
            });
            let touched = s(result, "memoryId").or_else(|| {
                (result.get("applied").and_then(Value::as_bool) == Some(true))
                    .then(|| s(result, "targetMemoryId"))
                    .flatten()
            });
            if let Some(id) = touched {
                let mut e = draft(m);
                e.memory_id = Some(id);
                out.push(Entry {
                    event: e,
                    owner_session: None,
                });
            }
            for id in result
                .get("superseded")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
            {
                let mut e = draft(m);
                e.memory_id = Some(id.to_string());
                out.push(Entry {
                    event: e,
                    owner_session: None,
                });
            }
            out
        }
        method::MEMORY_RECORD_USAGE => {
            if result.get("recorded").and_then(Value::as_u64).unwrap_or(0) == 0 {
                return vec![];
            }
            let mut e = draft(m);
            e.telemetry = true;
            vec![Entry {
                event: e,
                owner_session: None,
            }]
        }
        method::MEMORY_BACKFILL | method::MEMORY_HYGIENE => {
            if result.get("dryRun").and_then(Value::as_bool) != Some(false) {
                return vec![];
            }
            vec![Entry {
                event: draft(m),
                owner_session: None,
            }]
        }
        _ => vec![],
    }
}

/// Event for a scheduled maintenance pass.
pub fn maintenance_entry() -> Entry {
    Entry {
        event: draft("maintenance.hygiene"),
        owner_session: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn e(id: &str) -> Entry {
        let mut d = draft("memory.remember");
        d.memory_id = Some(id.into());
        Entry {
            event: d,
            owner_session: None,
        }
    }

    #[test]
    fn ring_gap_forces_resync() {
        let ev = Events::new(3);
        assert!(!ev.since("p", 0).resync);
        ev.publish("p", vec![e("a"), e("b")]);
        let s = ev.since("p", 0);
        assert_eq!((s.generation, s.entries.len(), s.resync), (2, 2, false));
        ev.publish("p", vec![e("c"), e("d"), e("e")]);
        assert!(ev.since("p", 0).resync, "generation 1 fell out of the ring");
        let s = ev.since("p", 2);
        assert_eq!(s.entries.len(), 3);
        assert!(ev.since("p", 99).resync, "future generation");
        assert!(
            ev.since("q", 4).resync,
            "unknown feed with nonzero generation"
        );
    }

    #[test]
    fn drafts_skip_replays_and_mark_session_owner() {
        let r = json!({"outcome": "memory-committed", "id": "m1", "revision": 1, "replayed": false,
                       "record": {"scope": "session", "ownerSessionId": "s1", "text": "secret"}});
        let d = drafts_for(method::MEMORY_REMEMBER, &r);
        assert_eq!(d[0].owner_session.as_deref(), Some("s1"));
        assert!(
            !serde_json::to_string(&d[0].event)
                .unwrap()
                .contains("secret")
        );
        let mut replay = r.clone();
        replay["replayed"] = json!(true);
        assert!(drafts_for(method::MEMORY_REMEMBER, &replay).is_empty());
    }
}
