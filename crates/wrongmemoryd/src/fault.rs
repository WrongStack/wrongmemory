//! Fault injection for crash/retry tests (WM-602).
//!
//! Inert in release builds: the environment variable is only read when
//! `cfg!(debug_assertions)`. Format:
//!
//! ```text
//! WRONGMEMORY_FAULT=<point>[:<method>]
//! ```
//!
//! Points:
//! * `after_commit_before_reply` — the daemon aborts the process right after a
//!   successful mutation committed and before the response frame is written.
//!   Proves the durable receipt returns the original result on retry
//!   (architecture §8, "crash after commit before reply").
//! * `before_commit` — handled in `wrongmemory-storage` (`store.rs`,
//!   `with_receipt`): the outermost receipt transaction fails with an
//!   injected I/O error before `COMMIT`, so the mutation must roll back.
//!
//! * `slow_lifecycle` — the daemon waits 1.5 s after binding its endpoint
//!   before publishing discovery, and 1.5 s after it stops accepting before
//!   retracting it. Widens the restart windows in which a client reads the
//!   previous daemon's discovery record (WM-606).
//!
//! The optional `<method>` restricts the fault to one IPC method.

use std::sync::OnceLock;

fn spec() -> Option<&'static (String, Option<String>)> {
    static SPEC: OnceLock<Option<(String, Option<String>)>> = OnceLock::new();
    SPEC.get_or_init(|| {
        if !cfg!(debug_assertions) {
            return None;
        }
        let raw = std::env::var("WRONGMEMORY_FAULT").ok()?;
        let mut parts = raw.splitn(2, ':');
        let point = parts.next()?.trim().to_string();
        let method = parts.next().map(|m| m.trim().to_string());
        Some((point, method))
    })
    .as_ref()
}

const MUTATIONS: &[&str] = &[
    "memory.remember",
    "memory.update",
    "memory.feedback",
    "memory.delete",
    "memory.recordUsage",
    "memory.recover",
    "memory.backfill",
    "memory.hygiene",
    "candidate.propose",
    "candidate.accept",
    "candidate.reject",
    "candidate.resolve",
];

/// Called after dispatch returned success and before the reply is written.
pub fn after_commit_before_reply(method: &str) {
    if let Some((point, filter)) = spec()
        && point == "after_commit_before_reply"
        && MUTATIONS.contains(&method)
        && filter.as_deref().is_none_or(|f| f == method)
    {
        eprintln!("wrongmemoryd: injected fault after_commit_before_reply ({method})");
        std::process::abort();
    }
}

/// Called between binding the endpoint and publishing discovery, and
/// between leaving the accept loop and retracting discovery.
pub async fn lifecycle_pause() {
    if let Some((point, _)) = spec()
        && point == "slow_lifecycle"
    {
        tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
    }
}
