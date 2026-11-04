//! Soak mode (WM-604): a mixed read/write load over many projects and
//! concurrent connections for a fixed duration. Reports throughput, error
//! count, per-operation latency, and the daemon's RSS, handle and thread
//! count after seeding and at the end (leak indicators). Long runs also
//! sample RSS, handles, threads, CPU time, open stores, total WAL size,
//! total data size and the running operation and error counts every
//! `SAMPLE_EVERY`, so a slow trend shows up, not only the two end points.
//! With `--out`, each sample is also appended and synced to
//! `<stem>.samples.jsonl` as it is taken, so a run cut short keeps its
//! trend. The report ends with the daemon's own maintenance jobs.
//!
//! `rate` paces the load to a total number of operations per second (0 runs
//! flat out). `idle_secs` adds an idle phase after the load: the daemon keeps
//! its stores open and the report gives its average CPU use over that phase.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::Path;
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::bail;
use serde_json::{Map, Value, json};
use wrongmemory_contracts::method;
use wrongmemory_ipc::{Client, DataRoot};

use crate::{COMPONENTS, OBJECTS, Rng, VERBS, code_token, dist, rss_bytes, start};

/// Interval of the in-run samples.
const SAMPLE_EVERY: Duration = Duration::from_secs(60);

/// Total size of the `*-wal` files under `dir` (every project store).
fn wal_bytes(dir: &Path) -> u64 {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    entries
        .filter_map(Result::ok)
        .map(|e| {
            let path = e.path();
            match e.file_type() {
                Ok(t) if t.is_dir() => wal_bytes(&path),
                Ok(_) if path.to_string_lossy().ends_with("-wal") => {
                    e.metadata().map_or(0, |m| m.len())
                }
                _ => 0,
            }
        })
        .sum()
}

/// Total size of every file under `dir` (stores, WAL, registry, logs).
fn data_bytes(dir: &Path) -> u64 {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    entries
        .filter_map(Result::ok)
        .map(|e| match e.file_type() {
            Ok(t) if t.is_dir() => data_bytes(&e.path()),
            Ok(_) => e.metadata().map_or(0, |m| m.len()),
            _ => 0,
        })
        .sum()
}

/// Handle count, thread count and total CPU seconds of a process.
#[derive(Clone, Copy)]
struct ProcStats {
    handles: u64,
    threads: u64,
    cpu_secs: f64,
}

/// Reads `ProcStats` (Windows) with one PowerShell call.
fn proc_stats(pid: u32) -> Option<ProcStats> {
    let script = format!(
        "$p = Get-Process -Id {pid}; $c = [cultureinfo]::InvariantCulture; \
         '{{0}} {{1}} {{2}}' -f $p.HandleCount, $p.Threads.Count, \
         $p.TotalProcessorTime.TotalSeconds.ToString($c)"
    );
    let out = Command::new("powershell")
        .args(["-NoProfile", "-Command", &script])
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let mut it = text.split_whitespace();
    Some(ProcStats {
        handles: it.next()?.parse().ok()?,
        threads: it.next()?.parse().ok()?,
        cpu_secs: it.next()?.parse().ok()?,
    })
}

fn unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

type Lat = BTreeMap<&'static str, Vec<f64>>;

/// Running totals across workers, read by every sample.
#[derive(Default)]
struct Progress {
    ops: AtomicU64,
    errors: AtomicU64,
}

/// `<out stem>.samples.jsonl` next to the report: a header line, then one
/// line per sample, each synced to disk, so a run cut short by a crash or a
/// power loss still leaves its trend behind.
struct Journal(std::fs::File);

impl Journal {
    fn create(out: &Path, header: &Value) -> anyhow::Result<Self> {
        if let Some(d) = out.parent() {
            std::fs::create_dir_all(d)?;
        }
        let stem = out.file_stem().unwrap_or_default().to_string_lossy();
        let path = out.with_file_name(format!("{stem}.samples.jsonl"));
        let mut j = Journal(std::fs::File::create(path)?);
        j.append(header)?;
        Ok(j)
    }

    fn append(&mut self, line: &Value) -> anyhow::Result<()> {
        writeln!(self.0, "{line}")?;
        self.0.sync_data()?;
        Ok(())
    }
}

/// The daemon's own maintenance during the run: jobs per state across the
/// projects, and how many projects had at least one successful pass.
async fn maintenance_jobs(c: &mut Client, projects: &[String]) -> Value {
    let mut by_state: BTreeMap<String, u64> = BTreeMap::new();
    let mut with_run = 0u64;
    for p in projects {
        let Ok(v) = c
            .call_raw(method::JOBS_LIST, Some(p), json!({"limit": 50}), None)
            .await
        else {
            *by_state.entry("unreadable".into()).or_default() += 1;
            continue;
        };
        let jobs = v["jobs"].as_array().cloned().unwrap_or_default();
        if jobs.iter().any(|j| j["state"] == "succeeded") {
            with_run += 1;
        }
        for j in &jobs {
            let state = j["state"].as_str().unwrap_or("unknown").to_string();
            *by_state.entry(state).or_default() += 1;
        }
    }
    json!({"jobsByState": by_state, "projectsWithASucceededPass": with_run})
}

async fn worker(
    root: DataRoot,
    projects: Vec<String>,
    w: usize,
    deadline: Instant,
    every: Option<Duration>,
    progress: Arc<Progress>,
) -> anyhow::Result<(Lat, Vec<String>, usize)> {
    let mut c = Client::connect_host(&root, "soak").await?;
    let mut rng = Rng(0xC11E + w as u64);
    let mut lat: Lat = BTreeMap::new();
    let mut errors = Vec::new();
    let mut n = 0usize;
    let paced_from = tokio::time::Instant::now();
    while Instant::now() < deadline {
        if let Some(every) = every {
            // Fixed schedule: a slow call is not made up for by a burst.
            let due = paced_from + every.mul_f64(n as f64);
            if due > tokio::time::Instant::now() {
                tokio::time::sleep_until(due).await;
            }
        }
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
                progress.errors.fetch_add(1, Ordering::Relaxed);
            }
        }
        progress.ops.fetch_add(1, Ordering::Relaxed);
        n += 1;
    }
    Ok((lat, errors, n))
}

pub struct Options {
    pub secs: u64,
    pub projects: usize,
    pub clients: usize,
    /// Total operations per second across all clients; 0 runs flat out.
    pub rate: f64,
    /// Idle phase after the load, for the daemon's idle CPU use.
    pub idle_secs: u64,
}

pub async fn run(exe: &Path, o: &Options, out: Option<&Path>) -> anyhow::Result<()> {
    let (secs, project_count, clients) = (o.secs, o.projects, o.clients);
    // Each client makes one call every `clients / rate` seconds.
    let every = (o.rate > 0.0).then(|| Duration::from_secs_f64(clients as f64 / o.rate));
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
    let stats0 = proc_stats(pid);
    let daemon_version = c
        .call_raw(method::DAEMON_HEALTH, None, json!({}), None)
        .await?["daemonVersion"]
        .clone();
    let machine = json!({
        "os": std::env::consts::OS,
        "logicalCpus": std::thread::available_parallelism().map(|v| v.get()).unwrap_or(0),
    });
    let started = Instant::now();
    let started_unix = unix_secs();
    let deadline = started + Duration::from_secs(secs);
    let mut journal = match out {
        Some(out) => Some(Journal::create(
            out,
            &json!({
                "schema": "wrongmemory/soak-samples@1",
                "startedUnixSecs": started_unix,
                "plannedSecs": secs,
                "projects": project_count,
                "clients": clients,
                "targetRate": o.rate,
                "daemonVersion": daemon_version,
                "rssBytesAfterSeed": rss0,
                "handlesAfterSeed": stats0.map(|s| s.handles),
                "threadsAfterSeed": stats0.map(|s| s.threads),
                "machine": machine,
            }),
        )?),
        None => None,
    };
    let root = DataRoot::resolve(Some(&home))?;
    let progress = Arc::new(Progress::default());
    let tasks: Vec<_> = (0..clients)
        .map(|w| {
            tokio::spawn(worker(
                root.clone(),
                projects.clone(),
                w,
                deadline,
                every,
                progress.clone(),
            ))
        })
        .collect();
    let mut samples = Vec::new();
    while Instant::now() + SAMPLE_EVERY <= deadline {
        tokio::time::sleep(SAMPLE_EVERY).await;
        let health = c
            .call_raw(method::DAEMON_HEALTH, None, json!({}), None)
            .await
            .unwrap_or(Value::Null);
        let stats = proc_stats(pid);
        let sample = json!({
            "elapsedSecs": started.elapsed().as_secs(),
            "unixSecs": unix_secs(),
            "ops": progress.ops.load(Ordering::Relaxed),
            "errors": progress.errors.load(Ordering::Relaxed),
            "rssBytes": rss_bytes(pid),
            "handles": stats.map(|s| s.handles),
            "threads": stats.map(|s| s.threads),
            "cpuSecs": stats.map(|s| s.cpu_secs),
            "openStores": health["openStores"],
            "walBytes": wal_bytes(&home),
            "dataBytes": data_bytes(&home),
        });
        if let Some(j) = journal.as_mut() {
            j.append(&sample)?;
        }
        samples.push(sample);
    }
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
    let stats_end = proc_stats(pid);
    let rss_end = rss_bytes(pid);
    let idle = if o.idle_secs > 0 {
        let before = proc_stats(pid);
        let t = Instant::now();
        tokio::time::sleep(Duration::from_secs(o.idle_secs)).await;
        let after = proc_stats(pid);
        let wall = t.elapsed().as_secs_f64();
        let cpu = before.zip(after).map(|(b, a)| a.cpu_secs - b.cpu_secs);
        let open = c
            .call_raw(method::DAEMON_HEALTH, None, json!({}), None)
            .await
            .unwrap_or(Value::Null);
        json!({
            "seconds": wall,
            "cpuSecs": cpu,
            "cpuPercentOfOneCore": cpu.map(|c| 100.0 * c / wall),
            "rssBytesAtEnd": rss_bytes(pid),
            "openStoresAtEnd": open["openStores"],
        })
    } else {
        Value::Null
    };
    let maintenance = maintenance_jobs(&mut c, &projects).await;
    let report = json!({
        "schema": "wrongmemory/soak@4",
        "daemonVersion": daemon_version,
        "startedUnixSecs": started_unix,
        "seconds": elapsed,
        "projects": project_count,
        "clients": clients,
        "targetRate": o.rate,
        "ops": ops,
        "opsPerSecond": ops as f64 / elapsed,
        "errors": error_count,
        "firstErrors": errors,
        "latencyMs": all.into_iter().map(|(k, v)| (k.to_string(), dist(v))).collect::<Map<_, _>>(),
        "rssBytes": {"afterSeed": rss0, "end": rss_end},
        "handles": {"afterSeed": stats0.map(|s| s.handles), "end": stats_end.map(|s| s.handles)},
        "threads": {"afterSeed": stats0.map(|s| s.threads), "end": stats_end.map(|s| s.threads)},
        "openStoresAtEnd": health["openStores"],
        "walBytesAtEnd": wal_bytes(&home),
        "dataBytesAtEnd": data_bytes(&home),
        "idle": idle,
        "maintenance": maintenance,
        "samples": samples,
        "machine": machine,
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
