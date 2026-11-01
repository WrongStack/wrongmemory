//! `wrongmemory-bench`: reproducible latency/resource benchmark and a
//! synthetic recall evaluation against the real daemon over real IPC.
//!
//! * Corpus `s` = 1,000 memories, `m` = 25,000 (validation §7), generated
//!   deterministically from a seeded PRNG; each memory has a unique "code"
//!   token so ground truth is exact. Ten percent are session-private to a
//!   different session and serve as hard negatives that must never leak.
//! * Latency: warm lexical search, path lookup, and remember, measured
//!   client-side (includes IPC) over N calls per run, reported per run and as
//!   a distribution across runs.
//! * Quality (synthetic, NOT a held-out real-task corpus): recall@5 for
//!   queries naming a target's code plus topic words; leak count of
//!   session-private hard negatives (must be 0).
//!
//! The daemon binary is taken from `--daemon` or next to this executable.

mod soak;

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, bail};
use clap::Parser;
use serde_json::{Value, json};
use wrongmemory_contracts::method;
use wrongmemory_ipc::{Client, DataRoot};

#[derive(Debug, Parser)]
struct Args {
    /// Corpus size: s (1,000) or m (25,000).
    #[arg(long, default_value = "s")]
    corpus: String,
    /// Number of measured runs (each with fresh warm-up).
    #[arg(long, default_value_t = 5)]
    runs: usize,
    /// Calls per operation per run.
    #[arg(long, default_value_t = 200)]
    calls: usize,
    /// Output JSON file.
    #[arg(long)]
    out: Option<PathBuf>,
    /// Path to wrongmemoryd.
    #[arg(long)]
    daemon: Option<PathBuf>,
    /// Soak mode (WM-604): run a mixed multi-project load for this many
    /// seconds instead of the latency benchmark.
    #[arg(long, default_value_t = 0)]
    soak_secs: u64,
    /// Soak: projects. More than the daemon's open-store cap (32) forces
    /// store eviction and reopen under load.
    #[arg(long, default_value_t = 40)]
    soak_projects: usize,
    /// Soak: concurrent client connections.
    #[arg(long, default_value_t = 8)]
    soak_clients: usize,
}

/// SplitMix64: deterministic, dependency-free.
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    fn pick<'a>(&mut self, v: &'a [&'a str]) -> &'a str {
        v[(self.next() % v.len() as u64) as usize]
    }
}

const COMPONENTS: &[&str] = &[
    "scheduler",
    "parser",
    "renderer",
    "gateway",
    "indexer",
    "allocator",
    "planner",
    "resolver",
    "uploader",
    "tokenizer",
    "router",
    "cache",
    "watcher",
    "migrator",
    "exporter",
    "verifier",
];
const VERBS: &[&str] = &[
    "validates",
    "batches",
    "retries",
    "normalizes",
    "rejects",
    "streams",
    "compacts",
    "throttles",
    "serializes",
    "signs",
    "shards",
    "reorders",
    "deduplicates",
    "persists",
    "rotates",
    "audits",
];
const OBJECTS: &[&str] = &[
    "manifests",
    "headers",
    "checkpoints",
    "payloads",
    "cursors",
    "segments",
    "snapshots",
    "tokens",
    "requests",
    "leases",
    "journals",
    "frames",
    "indexes",
    "receipts",
    "bundles",
    "credentials",
];
const REASONS: &[&str] = &[
    "to bound memory usage",
    "because the upstream API is flaky",
    "so replays stay idempotent",
    "to keep ordering deterministic",
    "because Windows paths are case-insensitive",
    "to avoid duplicate side effects",
    "so partial writes never surface",
    "to respect rate limits",
];
const KINDS: &[&str] = &[
    "fact",
    "decision",
    "convention",
    "warning",
    "workflow",
    "bug_root_cause",
];

fn code_token(i: usize) -> String {
    // Letters only so FTS tokenization keeps it whole: e.g. "zqkab".
    let mut n = i + 7919;
    let mut s = String::from("zq");
    for _ in 0..4 {
        s.push((b'a' + (n % 26) as u8) as char);
        n /= 26;
    }
    s
}

fn percentile(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let idx = ((p / 100.0) * (sorted.len() - 1) as f64).round() as usize;
    sorted[idx.min(sorted.len() - 1)]
}

fn dist(mut v: Vec<f64>) -> Value {
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    json!({
        "n": v.len(),
        "p50": percentile(&v, 50.0),
        "p95": percentile(&v, 95.0),
        "p99": percentile(&v, 99.0),
        "max": v.last().copied().unwrap_or(0.0),
    })
}

fn rss_bytes(pid: u32) -> Option<u64> {
    let out = Command::new("tasklist")
        .args(["/FI", &format!("PID eq {pid}"), "/FO", "CSV", "/NH"])
        .output()
        .ok()?;
    let line = String::from_utf8_lossy(&out.stdout).to_string();
    let field = line
        .split("\",\"")
        .last()?
        .trim()
        .trim_end_matches('"')
        .to_string();
    let kb: String = field.chars().filter(|c| c.is_ascii_digit()).collect();
    kb.parse::<u64>().ok().map(|k| k * 1024)
}

struct Daemon(Child);
impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

async fn start(exe: &Path, home: &Path) -> anyhow::Result<(Daemon, Client)> {
    let child = Command::new(exe)
        .arg("--data-root")
        .arg(home)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .with_context(|| format!("starting {}", exe.display()))?;
    let root = DataRoot::resolve(Some(home))?;
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if let Ok(c) = Client::connect_host(&root, "bench").await {
            return Ok((Daemon(child), c));
        }
        if Instant::now() > deadline {
            bail!("daemon did not become ready");
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let n = match args.corpus.as_str() {
        "s" => 1_000,
        "m" => 25_000,
        other => bail!("unknown corpus {other}"),
    };
    let exe = match args.daemon {
        Some(p) => p,
        None => std::env::current_exe()?
            .parent()
            .unwrap()
            .join(format!("wrongmemoryd{}", std::env::consts::EXE_SUFFIX)),
    };
    if args.soak_secs > 0 {
        return soak::run(
            &exe,
            args.soak_secs,
            args.soak_projects,
            args.soak_clients,
            args.out.as_deref(),
        )
        .await;
    }
    let tmp = tempfile::tempdir()?;
    let home = tmp.path().join("home");
    let proj = tmp.path().join("proj");
    std::fs::create_dir_all(&proj)?;
    let (daemon, mut c) = start(&exe, &home).await?;
    let reg = c
        .call_raw(
            method::PROJECT_REGISTER,
            None,
            json!({"root": proj.display().to_string(), "storeKind": "managed"}),
            None,
        )
        .await?;
    let p = reg["project"]["projectId"].as_str().unwrap().to_string();

    // ── Load corpus ─────────────────────────────────────────────────────
    let mut rng = Rng(0x5EED_2026_1101);
    let mut paths = Vec::new();
    let load_started = Instant::now();
    let mut private_ids = Vec::new();
    let mut public = Vec::new(); // (id, code, component, object, path)
    c.call_raw(
        method::SESSION_ACQUIRE,
        None,
        json!({"hostSessionId": "bench-owner"}),
        None,
    )
    .await?;
    for i in 0..n {
        let (comp, verb, obj, why) = (
            rng.pick(COMPONENTS),
            rng.pick(VERBS),
            rng.pick(OBJECTS),
            rng.pick(REASONS),
        );
        let code = code_token(i);
        let path = format!("src/{comp}/{obj}_{}.rs", i % 97);
        let private = i % 10 == 9;
        let text = format!("The {comp} {verb} {obj} {why}; tracked as {code} in {path}.");
        let mut params = json!({
            "text": text, "kind": rng.pick(KINDS), "tags": [comp, obj],
            "anchors": [{"type": "file", "path": path}],
        });
        if private {
            params["scope"] = json!("session");
        }
        let r = c
            .call_raw(method::MEMORY_REMEMBER, Some(&p), params, None)
            .await?;
        let id = r["id"].as_str().unwrap().to_string();
        if private {
            private_ids.push(id);
        } else {
            public.push((id, code, comp, obj, path.clone()));
        }
        paths.push(path);
    }
    let load_secs = load_started.elapsed().as_secs_f64();
    c.call_raw(method::SESSION_RELEASE, None, json!({}), None)
        .await?;
    let rss_loaded = rss_bytes(daemon.0.id());

    // ── Latency runs ────────────────────────────────────────────────────
    let mut runs = Vec::new();
    let mut all_search = Vec::new();
    let mut all_path = Vec::new();
    let mut all_write = Vec::new();
    for run in 0..args.runs {
        // Warm-up.
        for k in 0..20 {
            let (_, code, ..) = &public[(k * 31 + run) % public.len()];
            c.call_raw(
                method::MEMORY_SEARCH,
                Some(&p),
                json!({"query": code, "limit": 10}),
                None,
            )
            .await?;
        }
        let (mut s, mut f, mut w) = (Vec::new(), Vec::new(), Vec::new());
        for k in 0..args.calls {
            let (_, code, comp, obj, path) = &public[(k * 7919 + run * 13) % public.len()];
            let q = if k % 2 == 0 {
                code.clone()
            } else {
                format!("{comp} {obj}")
            };
            let t = Instant::now();
            c.call_raw(
                method::MEMORY_SEARCH,
                Some(&p),
                json!({"query": q, "limit": 10}),
                None,
            )
            .await?;
            s.push(t.elapsed().as_secs_f64() * 1000.0);
            let t = Instant::now();
            c.call_raw(
                method::MEMORY_FOR_PATH,
                Some(&p),
                json!({"path": path, "limit": 10}),
                None,
            )
            .await?;
            f.push(t.elapsed().as_secs_f64() * 1000.0);
        }
        for k in 0..(args.calls / 4).max(1) {
            let t = Instant::now();
            c.call_raw(
                method::CANDIDATE_PROPOSE,
                Some(&p),
                json!({"text": format!("Run {run} proposal {k}: the exporter signs bundles before upload {}", code_token(n + run * 10_000 + k))}),
                None,
            )
            .await?;
            w.push(t.elapsed().as_secs_f64() * 1000.0);
        }
        runs.push(json!({"run": run, "search": dist(s.clone()), "forPath": dist(f.clone()), "propose": dist(w.clone())}));
        all_search.extend(s);
        all_path.extend(f);
        all_write.extend(w);
    }

    // ── Synthetic recall evaluation ────────────────────────────────────
    let sample = public.len().min(300);
    let (mut hit_at_5, mut leaks) = (0usize, 0usize);
    for k in 0..sample {
        let (id, code, comp, obj, _) = &public[(k * 104_729) % public.len()];
        let r = c
            .call_raw(
                method::MEMORY_SEARCH,
                Some(&p),
                json!({"query": format!("{comp} {obj} {code}"), "limit": 5}),
                None,
            )
            .await?;
        let ids: Vec<&str> = r["hits"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|h| h["memory"]["id"].as_str())
            .collect();
        if ids.contains(&id.as_str()) {
            hit_at_5 += 1;
        }
        let raw = r.to_string();
        leaks += private_ids
            .iter()
            .filter(|pid| raw.contains(pid.as_str()))
            .count();
    }
    let rss_end = rss_bytes(daemon.0.id());

    let report = json!({
        "schema": "wrongmemory/bench@1",
        "corpus": args.corpus, "memories": n, "privateHardNegatives": private_ids.len(),
        "machine": {
            "os": std::env::consts::OS, "arch": std::env::consts::ARCH,
            "logicalCpus": std::thread::available_parallelism().map(|v| v.get()).unwrap_or(0),
        },
        "daemonExe": exe.display().to_string(),
        "loadSeconds": load_secs,
        "rssBytesAfterLoad": rss_loaded, "rssBytesAtEnd": rss_end,
        "latencyMsAllRuns": {"search": dist(all_search), "forPath": dist(all_path), "propose": dist(all_write)},
        "runs": runs,
        "syntheticRecall": {
            "queries": sample, "recallAt5": hit_at_5 as f64 / sample as f64,
            "sessionLeaks": leaks,
            "note": "synthetic code-token corpus; not a held-out real-task evaluation",
        },
    });
    let text = serde_json::to_string_pretty(&report)?;
    if let Some(out) = args.out {
        if let Some(d) = out.parent() {
            std::fs::create_dir_all(d)?;
        }
        std::fs::write(&out, &text)?;
    }
    println!("{text}");
    let _ = c
        .call_raw(method::DAEMON_SHUTDOWN, None, json!({}), None)
        .await;
    drop(daemon);
    if leaks > 0 {
        bail!("session-private hard negatives leaked: {leaks}");
    }
    Ok(())
}
