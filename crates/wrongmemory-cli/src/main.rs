//! `wrongmemory`: CLI for the per-user WrongMemory daemon, plus the MCP stdio
//! bridge (`wrongmemory mcp`). JSON output goes to stdout, diagnostics to
//! stderr; exit codes follow `wrongmemory_contracts::exit_code`.

#[cfg(unix)]
mod autostart_unix;
mod connect;
mod http;
mod mcp;
mod ops;
mod spawn;
mod ui;
mod user_path;

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand, ValueEnum};
use serde_json::{Value, json};
use wrongmemory_contracts::{PolicyProfile, StoreKind, exit_code, method};
use wrongmemory_ipc::{CallError, Client, DataRoot};

#[derive(Debug, Parser)]
#[command(
    name = "wrongmemory",
    version,
    about = "WrongMemory: per-user SAGE-compatible memory service"
)]
struct Cli {
    /// Data root (default: WRONGMEMORY_HOME or ~/.wrongmemory).
    #[arg(long, global = true)]
    data_root: Option<PathBuf>,
    /// Print machine-readable JSON.
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Debug, Subcommand)]
enum Cmd {
    /// Start, stop, or query the daemon.
    Daemon {
        #[command(subcommand)]
        action: DaemonCmd,
    },
    /// Register, list, inspect, or detach projects.
    Project {
        #[command(subcommand)]
        action: ProjectCmd,
    },
    /// Create, list, or revoke scoped client grants.
    Grants {
        #[command(subcommand)]
        action: GrantCmd,
    },
    /// Search a project's memory.
    Search {
        project: String,
        query: String,
        #[arg(long, default_value_t = 10)]
        limit: usize,
        #[arg(long = "kind")]
        kinds: Vec<String>,
    },
    /// Show one memory record.
    Get { project: String, id: String },
    /// Memory anchored to a path.
    ForPath {
        project: String,
        path: String,
        #[arg(long, default_value_t = 10)]
        limit: usize,
    },
    /// Create an authoritative memory (host credential).
    Remember {
        project: String,
        text: String,
        #[arg(long)]
        kind: Option<String>,
        #[arg(long = "tag")]
        tags: Vec<String>,
        /// File anchors (repeatable).
        #[arg(long = "file")]
        files: Vec<String>,
        #[arg(long)]
        idempotency_key: Option<String>,
    },
    /// Review queue.
    Candidates {
        #[command(subcommand)]
        action: CandidateCmd,
    },
    /// Read-only inspection of a sage.db (no daemon, no writes).
    Inspect { store: PathBuf },
    /// Read-only health report.
    Doctor,
    /// Read-only dashboard in the browser: projects, memories, candidates,
    /// live activity, audit trail and health. Serves 127.0.0.1 only.
    Ui {
        /// Loopback port (default: a free one).
        #[arg(long, default_value_t = 0)]
        port: u16,
        /// Print the link without opening a browser.
        #[arg(long)]
        no_open: bool,
    },
    /// MCP stdio bridge for external coding tools.
    Mcp {
        #[arg(long)]
        project: String,
        /// Grant ID; its secret is read from the private data root.
        #[arg(long)]
        grant: String,
        /// Attributed client label (not proof of identity).
        #[arg(long, default_value = "mcp-client")]
        client: String,
    },
    /// Optional loopback MCP over Streamable HTTP (127.0.0.1 only).
    Http {
        #[command(subcommand)]
        cmd: HttpCmd,
    },
    /// Add the owned `wrongmemory` MCP entry to a client configuration.
    Connect(connect::ConnectArgs),
    /// Remove the owned MCP entry and guidance block.
    Disconnect(connect::DisconnectArgs),
    /// Read-only client checks.
    Client {
        #[command(subcommand)]
        action: ClientCmd,
    },
    /// Consistent online backup of a project store (never overwrites).
    Backup(ops::BackupArgs),
    /// Read-only migration planning for an existing store.
    Migrate {
        #[command(subcommand)]
        action: ops::MigrateCmd,
    },
    /// Bounded, redacted diagnostics bundle.
    Diagnostics {
        #[command(subcommand)]
        action: ops::DiagnosticsCmd,
    },
    /// Per-user start at sign-in (Windows Run key, systemd user unit, or
    /// macOS LaunchAgent).
    Autostart {
        #[command(subcommand)]
        action: ops::AutostartCmd,
    },
    /// Install the CLI and daemon into a per-user directory.
    Install(ops::InstallArgs),
    /// Stop the daemon and remove installed binaries and autostart.
    Uninstall(ops::UninstallArgs),
}

/// `wrongmemory http`.
#[derive(Debug, Subcommand)]
enum HttpCmd {
    /// Serve MCP at http://127.0.0.1:<port>/mcp/<project-id> until Ctrl+C.
    Serve {
        /// Loopback port (default: a free one, printed on stdout).
        #[arg(long, default_value_t = 0)]
        port: u16,
        /// Attributed client label (not proof of identity).
        #[arg(long, default_value = "mcp-http")]
        client: String,
    },
    /// Print the `Authorization` header for a grant, for a client's header
    /// helper. The value is a credential: do not store it in a config file.
    Header {
        #[arg(long)]
        grant: String,
    },
}

#[derive(Debug, Subcommand)]
enum DaemonCmd {
    Start,
    Status,
    Stop,
}

#[derive(Debug, Subcommand)]
enum ProjectCmd {
    /// Register a project root.
    Add {
        root: PathBuf,
        /// Create a service-managed store instead of adopting `.wrongstack/memories/sage.db`.
        #[arg(long)]
        managed: bool,
        /// Request write ownership of an adopted store.
        #[arg(long)]
        write: bool,
        /// Acknowledge that a legacy SAGE owner (server.json) is stopped.
        #[arg(long)]
        force_owner: bool,
        #[arg(long)]
        name: Option<String>,
    },
    List,
    Show {
        project: String,
    },
    Detach {
        project: String,
    },
    /// Hand a managed project back to in-process SAGE: copy its store to
    /// `<root>/.wrongstack/memories/sage.db` and detach it.
    Export {
        project: String,
    },
    /// Show or choose the project's semantic provider (WM-508): `none`
    /// (lexical only, the default) or `ngram-hash` (built-in sub-word
    /// similarity; offline, no model). The vector cache is derived data under
    /// `<data root>/cache/semantic/` and is deleted when disabled.
    Semantic {
        project: String,
        #[arg(long)]
        provider: Option<String>,
    },
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum ProfileArg {
    ReadOnly,
    ReadPropose,
    Writer,
    Curator,
}

impl From<ProfileArg> for PolicyProfile {
    fn from(p: ProfileArg) -> Self {
        match p {
            ProfileArg::ReadOnly => PolicyProfile::ReadOnly,
            ProfileArg::ReadPropose => PolicyProfile::ReadPropose,
            ProfileArg::Writer => PolicyProfile::Writer,
            ProfileArg::Curator => PolicyProfile::Curator,
        }
    }
}

#[derive(Debug, Subcommand)]
enum GrantCmd {
    Create {
        #[arg(long)]
        label: String,
        #[arg(long, value_enum, default_value = "read-propose")]
        profile: ProfileArg,
        #[arg(long = "project", required = true)]
        projects: Vec<String>,
    },
    List,
    Revoke {
        grant: String,
    },
}

#[derive(Debug, Subcommand)]
enum CandidateCmd {
    List {
        project: String,
        #[arg(long, default_value = "pending")]
        status: String,
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
    Propose {
        project: String,
        text: String,
        #[arg(long)]
        kind: Option<String>,
        #[arg(long = "file")]
        files: Vec<String>,
    },
}

#[derive(Debug, Subcommand)]
enum ClientCmd {
    /// Check a client's configuration, executable, grant and daemon.
    Doctor(connect::DoctorArgs),
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let root = match DataRoot::resolve(cli.data_root.as_deref()) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("wrongmemory: cannot resolve data root: {e}");
            return ExitCode::from(exit_code::INVALID_INPUT as u8);
        }
    };
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
    {
        Ok(r) => r,
        Err(e) => {
            eprintln!("wrongmemory: cannot start runtime: {e}");
            return ExitCode::from(exit_code::INTERNAL as u8);
        }
    };
    let code = runtime.block_on(run(cli, root));
    ExitCode::from(code as u8)
}

fn print(json_mode: bool, v: &Value, human: impl FnOnce(&Value) -> String) {
    if json_mode {
        println!("{}", serde_json::to_string_pretty(v).unwrap_or_default());
    } else {
        println!("{}", human(v));
    }
}

fn fail(e: &CallError) -> i32 {
    eprintln!("wrongmemory: {e}");
    match e {
        CallError::Remote(b) => b.code.exit_code(),
        CallError::Unavailable(_) => exit_code::SERVICE_UNAVAILABLE,
        CallError::Credential(_) => exit_code::DENIED,
        _ => exit_code::FAILED,
    }
}

async fn host(root: &DataRoot, autostart: bool) -> Result<Client, CallError> {
    spawn::connect_host(root, "wrongmemory-cli", autostart).await
}

fn hits_human(v: &Value) -> String {
    let mut out = String::new();
    let hits = v["hits"].as_array().cloned().unwrap_or_default();
    for h in &hits {
        let m = &h["memory"];
        out.push_str(&format!(
            "{} r{} [{}/{}] {}\n",
            m["id"].as_str().unwrap_or(""),
            m["revision"],
            m["kind"].as_str().unwrap_or(""),
            m["status"].as_str().unwrap_or(""),
            m["text"].as_str().unwrap_or("")
        ));
    }
    out.push_str(&format!(
        "{} hit(s) via {}{}",
        hits.len(),
        v["channel"].as_str().unwrap_or("?"),
        if v["truncated"].as_bool() == Some(true) {
            " (truncated)"
        } else {
            ""
        }
    ));
    out
}

async fn run(cli: Cli, root: DataRoot) -> i32 {
    let j = cli.json;
    macro_rules! call {
        ($client:expr, $method:expr, $project:expr, $params:expr) => {
            match $client.call_raw($method, $project, $params, None).await {
                Ok(v) => v,
                Err(e) => return fail(&e),
            }
        };
    }
    macro_rules! connect {
        ($autostart:expr) => {
            match host(&root, $autostart).await {
                Ok(c) => c,
                Err(e) => return fail(&e),
            }
        };
    }
    match cli.cmd {
        Cmd::Daemon { action } => match action {
            DaemonCmd::Start => {
                let mut c = connect!(true);
                let v = call!(c, method::DAEMON_HEALTH, None, json!({}));
                print(j, &v, |v| {
                    format!(
                        "running: {} (pid {})",
                        v["daemonInstanceId"].as_str().unwrap_or(""),
                        v["pid"]
                    )
                });
            }
            DaemonCmd::Status => {
                let mut c = connect!(false);
                let v = call!(c, method::DAEMON_HEALTH, None, json!({}));
                print(j, &v, |v| {
                    serde_json::to_string_pretty(v).unwrap_or_default()
                });
            }
            DaemonCmd::Stop => {
                let mut c = connect!(false);
                let v = call!(c, method::DAEMON_SHUTDOWN, None, json!({}));
                print(j, &v, |_| "stop requested".into());
            }
        },
        Cmd::Project { action } => match action {
            ProjectCmd::Add {
                root: project_root,
                managed,
                write,
                force_owner,
                name,
            } => {
                let mut c = connect!(true);
                let params = json!({
                    "root": project_root.display().to_string(),
                    "displayName": name,
                    "storeKind": if managed { StoreKind::Managed } else { StoreKind::Adopted },
                    "write": write || managed,
                    "forceOwner": force_owner,
                });
                let v = call!(c, method::PROJECT_REGISTER, None, params);
                print(j, &v, |v| {
                    let p = &v["project"];
                    format!(
                        "{} {} {}\n  store: {} ({})\n  admission: {} — {}",
                        if v["created"].as_bool() == Some(true) {
                            "registered"
                        } else {
                            "already registered"
                        },
                        p["projectId"].as_str().unwrap_or(""),
                        p["displayName"].as_str().unwrap_or(""),
                        p["storePath"].as_str().unwrap_or(""),
                        p["storeKind"].as_str().unwrap_or(""),
                        p["admission"].as_str().unwrap_or(""),
                        p["admissionReason"].as_str().unwrap_or("")
                    )
                });
            }
            ProjectCmd::List => {
                let mut c = connect!(true);
                let v = call!(c, method::PROJECT_LIST, None, json!({}));
                print(j, &v, |v| {
                    v["projects"]
                        .as_array()
                        .cloned()
                        .unwrap_or_default()
                        .iter()
                        .map(|p| {
                            format!(
                                "{}  {:<10} {:<9} {}  {}",
                                p["projectId"].as_str().unwrap_or(""),
                                p["admission"].as_str().unwrap_or(""),
                                p["storeKind"].as_str().unwrap_or(""),
                                p["displayName"].as_str().unwrap_or(""),
                                p["storePath"].as_str().unwrap_or("")
                            )
                        })
                        .collect::<Vec<_>>()
                        .join("\n")
                });
            }
            ProjectCmd::Show { project } => {
                let mut c = connect!(true);
                let v = call!(c, method::PROJECT_GET, Some(&project), json!({}));
                print(j, &v, |v| {
                    serde_json::to_string_pretty(v).unwrap_or_default()
                });
            }
            ProjectCmd::Detach { project } => {
                let mut c = connect!(false);
                let v = call!(
                    c,
                    method::PROJECT_DETACH,
                    None,
                    json!({"projectId": project})
                );
                print(j, &v, |v| {
                    format!("detached: {} (store files are kept)", v["detached"])
                });
            }
            ProjectCmd::Semantic { project, provider } => {
                let mut c = connect!(false);
                let v = match provider {
                    Some(p) => call!(
                        c,
                        method::SEMANTIC_CONFIGURE,
                        Some(&project),
                        json!({"provider": p})
                    ),
                    None => call!(c, method::SEMANTIC_STATUS, Some(&project), json!({})),
                };
                print(j, &v, |v| {
                    let mut out = format!(
                        "semantic provider: {}",
                        v["provider"].as_str().unwrap_or_default()
                    );
                    if let Some(n) = v["indexed"].as_u64() {
                        out.push_str(&format!(" ({n} memories indexed)"));
                    }
                    out
                });
            }
            ProjectCmd::Export { project } => {
                let mut c = connect!(false);
                let v = call!(
                    c,
                    method::PROJECT_EXPORT,
                    None,
                    json!({"projectId": project})
                );
                print(j, &v, |v| {
                    format!(
                        "exported to {} ({} memories, integrity {}); project detached: {}\n  The managed store {} is kept; delete it yourself once SAGE runs on the exported copy.",
                        v["legacyStorePath"].as_str().unwrap_or_default(),
                        v["backup"]["memoryRows"],
                        v["backup"]["integrity"].as_str().unwrap_or_default(),
                        v["detached"],
                        v["managedStorePath"].as_str().unwrap_or_default()
                    )
                });
            }
        },
        Cmd::Grants { action } => match action {
            GrantCmd::Create {
                label,
                profile,
                projects,
            } => {
                let mut c = connect!(true);
                let v = call!(
                    c,
                    method::GRANT_CREATE,
                    None,
                    json!({"label": label, "profile": PolicyProfile::from(profile), "projects": projects})
                );
                print(j, &v, |v| {
                    format!(
                        "grant {} ({}) for {}\n  secret stored owner-private at {}",
                        v["grant"]["grantId"].as_str().unwrap_or(""),
                        v["grant"]["profile"].as_str().unwrap_or(""),
                        v["grant"]["projects"],
                        v["secretFile"].as_str().unwrap_or("")
                    )
                });
            }
            GrantCmd::List => {
                let mut c = connect!(true);
                let v = call!(c, method::GRANT_LIST, None, json!({}));
                print(j, &v, |v| {
                    serde_json::to_string_pretty(&v["grants"]).unwrap_or_default()
                });
            }
            GrantCmd::Revoke { grant } => {
                let mut c = connect!(false);
                let v = call!(c, method::GRANT_REVOKE, None, json!({"grantId": grant}));
                print(j, &v, |v| format!("revoked: {}", v["revoked"]));
            }
        },
        Cmd::Search {
            project,
            query,
            limit,
            kinds,
        } => {
            let mut c = connect!(true);
            let mut params = json!({"query": query, "limit": limit});
            if !kinds.is_empty() {
                params["kinds"] = json!(kinds);
            }
            let v = call!(c, method::MEMORY_SEARCH, Some(&project), params);
            print(j, &v, hits_human);
        }
        Cmd::Get { project, id } => {
            let mut c = connect!(true);
            let v = call!(c, method::MEMORY_GET, Some(&project), json!({"id": id}));
            print(j, &v, |v| {
                serde_json::to_string_pretty(v).unwrap_or_default()
            });
        }
        Cmd::ForPath {
            project,
            path,
            limit,
        } => {
            let mut c = connect!(true);
            let v = call!(
                c,
                method::MEMORY_FOR_PATH,
                Some(&project),
                json!({"path": path, "limit": limit})
            );
            print(j, &v, hits_human);
        }
        Cmd::Remember {
            project,
            text,
            kind,
            tags,
            files,
            idempotency_key,
        } => {
            let mut c = connect!(true);
            let anchors: Vec<Value> = files
                .iter()
                .map(|f| json!({"type": "file", "path": f}))
                .collect();
            let mut params = json!({"text": text, "tags": tags, "anchors": anchors});
            if let Some(k) = kind {
                params["kind"] = json!(k);
            }
            let v = match c
                .call_raw(
                    method::MEMORY_REMEMBER,
                    Some(&project),
                    params,
                    idempotency_key.as_deref(),
                )
                .await
            {
                Ok(v) => v,
                Err(e) => return fail(&e),
            };
            print(j, &v, |v| {
                format!(
                    "{} {} r{}{}",
                    v["outcome"].as_str().unwrap_or(""),
                    v["id"].as_str().unwrap_or(""),
                    v["revision"],
                    if v["replayed"].as_bool() == Some(true) {
                        " (replayed)"
                    } else {
                        ""
                    }
                )
            });
        }
        Cmd::Candidates { action } => match action {
            CandidateCmd::List {
                project,
                status,
                limit,
            } => {
                let mut c = connect!(true);
                let v = call!(
                    c,
                    method::CANDIDATE_LIST,
                    Some(&project),
                    json!({"status": status, "limit": limit})
                );
                print(j, &v, |v| {
                    v["candidates"]
                        .as_array()
                        .cloned()
                        .unwrap_or_default()
                        .iter()
                        .map(|c| {
                            format!(
                                "{} [{}] {}",
                                c["id"].as_str().unwrap_or(""),
                                c["kind"].as_str().unwrap_or(""),
                                c["text"].as_str().unwrap_or("")
                            )
                        })
                        .collect::<Vec<_>>()
                        .join("\n")
                });
            }
            CandidateCmd::Propose {
                project,
                text,
                kind,
                files,
            } => {
                let mut c = connect!(true);
                let anchors: Vec<Value> = files
                    .iter()
                    .map(|f| json!({"type": "file", "path": f}))
                    .collect();
                let mut params = json!({"text": text, "anchors": anchors});
                if let Some(k) = kind {
                    params["kind"] = json!(k);
                }
                let v = call!(c, method::CANDIDATE_PROPOSE, Some(&project), params);
                print(j, &v, |v| {
                    format!(
                        "{} {}",
                        v["outcome"].as_str().unwrap_or(""),
                        v["id"].as_str().unwrap_or("")
                    )
                });
            }
        },
        Cmd::Inspect { store } => {
            let insp = wrongmemory_storage::inspect(&store);
            let v = serde_json::to_value(&insp).unwrap_or(Value::Null);
            print(j, &v, |v| {
                serde_json::to_string_pretty(v).unwrap_or_default()
            });
            if !insp.verdict.readable() {
                return exit_code::UNSUPPORTED;
            }
        }
        Cmd::Doctor => {
            let mut report = json!({
                "dataRoot": root.path().display().to_string(),
                "sqlite": wrongmemory_storage::sqlite_version(),
                "daemonExe": spawn::daemon_exe().map(|p| p.display().to_string()).map_err(|e| e.to_string()),
            });
            match host(&root, false).await {
                Ok(mut c) => {
                    report["daemon"] = c
                        .call_raw(method::DAEMON_HEALTH, None, json!({}), None)
                        .await
                        .unwrap_or(Value::Null);
                    let projects = c
                        .call_raw(method::PROJECT_LIST, None, json!({}), None)
                        .await
                        .unwrap_or(Value::Null);
                    let mut checks = Vec::new();
                    for p in projects["projects"].as_array().cloned().unwrap_or_default() {
                        let store = PathBuf::from(p["storePath"].as_str().unwrap_or(""));
                        let insp = wrongmemory_storage::inspect(&store);
                        checks.push(json!({
                            "projectId": p["projectId"],
                            "admission": p["admission"],
                            "verdict": insp.verdict,
                            "detail": insp.detail,
                            "memories": insp.memory_rows,
                            "legacyOwnerMetadata": insp.legacy_owner_metadata,
                        }));
                    }
                    report["projects"] = json!(checks);
                }
                Err(e) => report["daemon"] = json!({"unavailable": e.to_string()}),
            }
            print(j, &report, |v| {
                serde_json::to_string_pretty(v).unwrap_or_default()
            });
        }
        Cmd::Ui { port, no_open } => {
            if let Err(e) = ui::run(root, port, !no_open).await {
                eprintln!("wrongmemory ui: {e:#}");
                return exit_code::SERVICE_UNAVAILABLE;
            }
        }
        Cmd::Mcp {
            project,
            grant,
            client,
        } => {
            if let Err(e) = mcp::serve(root, project, grant, client).await {
                eprintln!("wrongmemory mcp: {e:#}");
                // A revoked or foreign grant is a credential problem, not an
                // unavailable service.
                return match e.downcast_ref::<CallError>() {
                    Some(CallError::Credential(_)) => exit_code::DENIED,
                    _ => exit_code::SERVICE_UNAVAILABLE,
                };
            }
        }
        Cmd::Http { cmd } => match cmd {
            HttpCmd::Serve { port, client } => {
                if let Err(e) = http::serve(root, port, client).await {
                    eprintln!("wrongmemory http: {e:#}");
                    return exit_code::SERVICE_UNAVAILABLE;
                }
            }
            HttpCmd::Header { grant } => match http::authorization(&root, &grant) {
                Ok(v) => {
                    if j {
                        println!("{}", serde_json::json!({ "Authorization": v }));
                    } else {
                        println!("Authorization: {v}");
                    }
                }
                Err(e) => {
                    eprintln!("wrongmemory http: {e:#}");
                    return exit_code::SERVICE_UNAVAILABLE;
                }
            },
        },
        Cmd::Connect(args) => {
            // A data root chosen through WRONGMEMORY_HOME exists only in this
            // shell's environment, and the client starts the bridge from its
            // own. Write it into the entry like an explicit --data-root.
            let from_env = std::env::var_os(wrongmemory_ipc::endpoint::DATA_ROOT_ENV)
                .filter(|v| !v.is_empty())
                .and_then(|_| std::path::absolute(root.path()).ok());
            let data_root_arg = cli.data_root.clone().or(from_env);
            return connect::run_connect(&root, data_root_arg.as_deref(), j, args).await;
        }
        Cmd::Disconnect(args) => return connect::run_disconnect(&root, j, args).await,
        Cmd::Client {
            action: ClientCmd::Doctor(args),
        } => return connect::run_doctor(&root, j, args).await,
        Cmd::Backup(args) => return ops::run_backup(&root, j, args).await,
        Cmd::Migrate { action } => return ops::run_migrate(j, action),
        Cmd::Diagnostics { action } => return ops::run_diagnostics(&root, j, action).await,
        Cmd::Autostart { action } => return ops::run_autostart(&root, j, action),
        Cmd::Install(args) => return ops::run_install(&root, j, args).await,
        Cmd::Uninstall(args) => return ops::run_uninstall(&root, j, args).await,
    }
    exit_code::SUCCESS
}
