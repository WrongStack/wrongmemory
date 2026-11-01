//! Soak mode (WM-604): a mixed read/write load over many projects and
//! concurrent connections for a fixed duration. Reports throughput, error
//! count, per-operation latency, and the daemon's RSS and handle count
//! after seeding and at the end (leak indicators).

use std::collections::BTreeMap;
use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

use anyhow::bail;
use serde_json::{Map, Value, json};
use wrongmemory_contracts::method;
use wrongmemory_ipc::{Client, DataRoot};

use crate::{COMPONENTS, OBJECTS, Rng, VERBS, code_token, dist, rss_bytes, start};

/// Open handle count of a process (Windows), via PowerShell.
fn handle_count(pid: u32) -> Option<u64> {
    let out = Command::new("powershell")
        .args([
            "-NoProfile",
            "-Command",
            &format!("(Get-Process -Id {pid}).HandleCount"),
        ])
        .output()
        .ok()?;
    String::from_utf8_lossy(&out.stdout).trim().parse().ok()
}

type Lat = BTreeMap<&'static str, Vec<f64>>;

async fn worker(
    root: DataRoot,
    projects: Vec<String>,
    w: usize,
    deadline: Instant,
) -> anyhow::Result<(Lat, Vec<String>, usize)> {
    let mut c = Client::connect_host(&root, "soak").await?;
    let mut rng = Rng(0xC11E + w as u64);
    let mut lat: Lat = BTreeMap::new();
    let mut errors = Vec::new();
    let mut n = 0usize;
    while Instant::now() < deadline {
        let p = &projects[(rng.next() as usize) % projects.len()];
        let roll = rng.next() % 100;
        let unique = code_token(1_000_000 + w * 100_000 + n);
        let (op, m, params, key): (&'static str, &str, Value, Option<String>) = match roll {
            0..=54 => (
                "search",
                method::MEMORY_SEARCH,
                json!({"query": format!("{} {}", rng.pick(COMPONENTS), rng.pick(OBJECTS)), "limit": 10}),
                None,
            ),
            55..=74 => (
                "forPath",
                method::MEMORY_FOR_PATH,
                json!({"path": format!("src/{}.rs", rng.pick(COMPONENTS)), "limit": 10}),
                None,
            ),
            75..=84 => (
                "propose",
                method::CANDIDATE_PROPOSE,
                json!({"text": format!("Soak proposal {w}-{n}: the {} {} {unique}", rng.pick(COMPONENTS), rng.pick(VERBS))}),
                None,
            ),
            85..=94 => (
                "remember",
                method::MEMORY_REMEMBER,
                json!({"text": format!("Soak fact {w}-{n}: {} {unique}", rng.pick(OBJECTS))}),
                Some(format!("soak-{w}-{n}")),
            ),
            _ => (
                "listPage",
                method::MEMORY_LIST_PAGE,
                json!({"limit": 50}),
                None,
            ),
        };
        let t = Instant::now();
        match c.call_raw(m, Some(p), params, key.as_deref()).await {
            Ok(_) => lat
                .entry(op)
                .or_default()
                .push(t.elapsed().as_secs_f64() * 1000.0),
            Err(e) => {
                if errors.len() < 5 {
                    errors.push(format!("{op}: {e}"));
                }
                lat.entry("error").or_default().push(0.0);
            }
        }
        n += 1;
    }
    Ok((lat, errors, n))
}

pub async fn run(
    exe: &Path,
    secs: u64,
    project_count: usize,
    clients: usize,
    out: Option<&Path>,
) -> anyhow::Result<()> {
    let tmp = tempfile::tempdir()?;
    let home = tmp.path().join("home");
    let (daemon, mut c) = start(exe, &home).await?;
    let pid = daemon.0.id();
    let mut projects = Vec::new();
    for i in 0..project_count {
        let dir = tmp.path().join(format!("proj{i}"));
        std::fs::create_dir_all(dir.join("src"))?;
        let reg = c
            .call_raw(
                method::PROJECT_REGISTER,
                None,
                json!({"root": dir.display().to_string(), "storeKind": "managed"}),
                None,
            )
            .await?;
        let p = reg["project"]["projectId"].as_str().unwrap().to_string();
        let mut rng = Rng(0x50AC + i as u64);
        for k in 0..100 {
            let (comp, verb, obj) = (rng.pick(COMPONENTS), rng.pick(VERBS), rng.pick(OBJECTS));
            c.call_raw(
                method::MEMORY_REMEMBER,
                Some(&p),
                json!({"text": format!("The {comp} {verb} {obj}; tracked as {}.", code_token(i * 1000 + k)),
                       "anchors": [{"type": "file", "path": format!("src/{comp}.rs")}]}),
                None,
            )
            .await?;
        }
        projects.push(p);
    }
    let rss0 = rss_bytes(pid);
    let handles0 = handle_count(pid);
    let started = Instant::now();
    let deadline = started + Duration::from_secs(secs);
    let root = DataRoot::resolve(Some(&home))?;
    let tasks: Vec<_> = (0..clients)
        .map(|w| tokio::spawn(worker(root.clone(), projects.clone(), w, deadline)))
        .collect();
    let mut all: Lat = BTreeMap::new();
    let mut errors = Vec::new();
    let mut ops = 0usize;
    for t in tasks {
        let (lat, errs, n) = t.await??;
        for (k, v) in lat {
            all.entry(k).or_default().extend(v);
        }
        errors.extend(errs);
        ops += n;
    }
    let elapsed = started.elapsed().as_secs_f64();
    let error_count = all.remove("error").map_or(0, |v| v.len());
    let health = c
        .call_raw(method::DAEMON_HEALTH, None, json!({}), None)
        .await?;
    let report = json!({
        "schema": "wrongmemory/soak@1",
        "seconds": elapsed,
        "projects": project_count,
        "clients": clients,
        "ops": ops,
        "opsPerSecond": ops as f64 / elapsed,
        "errors": error_count,
        "firstErrors": errors,
        "latencyMs": all.into_iter().map(|(k, v)| (k.to_string(), dist(v))).collect::<Map<_, _>>(),
        "rssBytes": {"afterSeed": rss0, "end": rss_bytes(pid)},
        "handles": {"afterSeed": handles0, "end": handle_count(pid)},
        "openStoresAtEnd": health["openStores"],
        "machine": {
            "os": std::env::consts::OS,
            "logicalCpus": std::thread::available_parallelism().map(|v| v.get()).unwrap_or(0),
        },
    });
    let text = serde_json::to_string_pretty(&report)?;
    if let Some(out) = out {
        if let Some(d) = out.parent() {
            std::fs::create_dir_all(d)?;
        }
        std::fs::write(out, &text)?;
    }
    println!("{text}");
    let _ = c
        .call_raw(method::DAEMON_SHUTDOWN, None, json!({}), None)
        .await;
    drop(daemon);
    if error_count > 0 {
        bail!("soak saw {error_count} failed requests");
    }
    Ok(())
}
