//! A bounded operator client. No SQLite, shell, arbitrary-method API or daemon
//! shutdown. UI cancellation never reuses an abandoned IPC stream.
use std::path::PathBuf;
use std::sync::mpsc::{self, SyncSender};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::sync::oneshot;
use wrongmemory_contracts::{GrantInfo, method};
use wrongmemory_ipc::{CallError, Client, DataRoot};

use crate::model::Query;

pub type Reply = Result<Value, Failure>;

#[derive(Clone, Debug)]
pub struct Failure {
    pub code: String,
    pub message: String,
    /// A timed-out mutation may have committed. Never retry it automatically.
    pub uncertain: bool,
}

impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for Failure {}

impl From<CallError> for Failure {
    fn from(e: CallError) -> Self {
        Self {
            code: e.code().map(|c| c.as_str()).unwrap_or("UNAVAILABLE").into(),
            message: e.to_string(),
            uncertain: false,
        }
    }
}

#[derive(Clone, Debug)]
pub enum Read {
    Overview,
    Library(Query),
    Memory {
        project: String,
        id: String,
    },
    Review {
        project: String,
    },
    Graph {
        project: String,
        node: String,
    },
    Health {
        project: String,
    },
    Clients,
    Events {
        project: String,
        generation: u64,
        instance: Option<String>,
    },
}

#[derive(Clone, Debug)]
pub enum Change {
    Edit {
        project: String,
        id: String,
        revision: i64,
        text: String,
    },
    Accept {
        project: String,
        candidate: String,
    },
    Reject {
        project: String,
        candidate: String,
        reason: String,
    },
    Resolve {
        project: String,
        candidate: String,
        decision: String,
    },
    Recover {
        project: String,
        id: String,
    },
    Delete {
        project: String,
        id: String,
        reason: String,
    },
    RunMaintenance {
        project: String,
    },
    CreateGrant {
        project: String,
        label: String,
    },
    RevokeGrant {
        grant: String,
    },
}

#[derive(Clone, Debug)]
pub enum Command {
    Read(Read),
    Change(Change),
    SavePreferences(crate::preferences::Preferences),
}

struct Work {
    command: Command,
    reply: oneshot::Sender<Reply>,
}

#[derive(Clone)]
pub struct Service {
    send: SyncSender<Work>,
}

impl Service {
    pub fn new(root: DataRoot) -> std::io::Result<Self> {
        let (send, receive) = mpsc::sync_channel::<Work>(32);
        std::thread::Builder::new().name("wrongmemory-desktop-ipc".into()).spawn(move || {
            let runtime = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
                Ok(r) => r,
                Err(_) => return,
            };
            let mut session = Session { root, client: None };
            while let Ok(work) = receive.recv() {
                // Not admitted yet: a superseded UI request can be skipped.
                if work.reply.is_closed() { continue; }
                let mutation = matches!(work.command, Command::Change(_));
                let result = runtime.block_on(async {
                    tokio::time::timeout(Duration::from_secs(15), session.execute(work.command)).await
                });
                let reply = match result {
                    Ok(mut reply) => {
                        if mutation && let Err(error) = &mut reply
                            && matches!(error.code.as_str(), "UNAVAILABLE" | "PROTOCOL") {
                            error.uncertain = true;
                        }
                        reply
                    }
                    Err(_) => {
                        session.client = None;
                        Err(Failure { code: "TIMEOUT".into(), message: if mutation {
                            "The outcome is unknown. Refresh and inspect the record before submitting another change."
                        } else { "The service did not reply in 15 seconds. Reconnect to try again." }.into(), uncertain: mutation })
                    }
                };
                let _ = work.reply.send(reply);
            }
            // Dropping the last UI sender closes only this client connection.
        })?;
        Ok(Self { send })
    }

    pub fn request(&self, command: Command) -> Result<oneshot::Receiver<Reply>, Failure> {
        let (reply, receive) = oneshot::channel();
        self.send
            .try_send(Work { command, reply })
            .map_err(|_| Failure {
                code: "BUSY".into(),
                message: "The desktop request queue is full or closed. Wait, then refresh.".into(),
                uncertain: false,
            })?;
        Ok(receive)
    }
}

struct Session {
    root: DataRoot,
    client: Option<Client>,
}

impl Session {
    async fn call(
        &mut self,
        method: &str,
        project: Option<&str>,
        args: Value,
        key: Option<&str>,
    ) -> Reply {
        let mut client = match self.client.take() {
            Some(client) => client,
            None => Client::connect_host(&self.root, "wrongmemory-desktop").await?,
        };
        let result = client.call_raw(method, project, args, key).await;
        // Transport failures and timeout drop the connection, not just a read
        // future. A later call must not consume a stale response.
        if result.is_ok() || matches!(result, Err(CallError::Remote(_))) {
            self.client = Some(client);
        }
        result.map_err(Failure::from)
    }

    async fn execute(&mut self, command: Command) -> Reply {
        match command {
            Command::SavePreferences(preferences) => {
                preferences.save(&self.root).map_err(|e| Failure {
                    code: "PREFERENCES".into(),
                    message: e.to_string(),
                    uncertain: false,
                })?;
                Ok(json!({"saved":true}))
            }
            Command::Read(read) => match read {
                Read::Overview => {
                    let health = self
                        .call(method::DAEMON_HEALTH, None, json!({}), None)
                        .await?;
                    let projects = self
                        .call(method::PROJECT_LIST, None, json!({}), None)
                        .await?;
                    Ok(json!({"health":health,"projects":projects["projects"]}))
                }
                Read::Library(q) => {
                    let mut args = json!({"limit": 100});
                    if let Some(status) = q.status {
                        args["statuses"] = json!([status]);
                    }
                    if let Some(kind) = q.kind {
                        args["kind"] = json!(kind);
                    }
                    if !q.text.is_empty() {
                        args["query"] = json!(q.text);
                    }
                    if let Some(cursor) = q.cursor {
                        args["cursor"] = json!(cursor);
                    }
                    self.call(method::MEMORY_LIST_PAGE, Some(&q.project), args, None)
                        .await
                }
                Read::Memory { project, id } => {
                    self.call(method::MEMORY_GET, Some(&project), json!({"id":id}), None)
                        .await
                }
                Read::Review { project } => {
                    self.call(
                        method::CANDIDATE_LIST,
                        Some(&project),
                        json!({"status":"pending","limit":100}),
                        None,
                    )
                    .await
                }
                Read::Graph { project, node } => {
                    self.call(
                        method::MEMORY_GRAPH,
                        Some(&project),
                        json!({"query":node,"maxDepth":2,"limit":100}),
                        None,
                    )
                    .await
                }
                Read::Health { project } => {
                    let stats = self
                        .call(method::MEMORY_STATS, Some(&project), json!({}), None)
                        .await?;
                    let doctor = self
                        .call(method::MEMORY_DOCTOR, Some(&project), json!({}), None)
                        .await?;
                    let maintenance = self
                        .call(method::MAINTENANCE_STATUS, Some(&project), json!({}), None)
                        .await?;
                    Ok(json!({"stats":stats,"doctor":doctor,"maintenance":maintenance}))
                }
                Read::Clients => self.call(method::GRANT_LIST, None, json!({}), None).await,
                Read::Events {
                    project,
                    generation,
                    instance,
                } => {
                    self.call(
                        method::EVENTS_SINCE,
                        Some(&project),
                        json!({"generation":generation,"daemonInstanceId":instance}),
                        None,
                    )
                    .await
                }
            },
            Command::Change(change) => {
                let nonce = wrongmemory_ipc::endpoint::generate_token().map_err(|_| Failure {
                    code: "ENTROPY_UNAVAILABLE".into(),
                    message:
                        "A safe request identity could not be created. No change was submitted."
                            .into(),
                    uncertain: false,
                })?;
                let key = format!("desktop-{nonce}");
                match change {
                    Change::Edit {
                        project,
                        id,
                        revision,
                        text,
                    } => {
                        if revision < 1 || text.trim().is_empty() {
                            return Err(Failure {
                                code: "INVALID_REQUEST".into(),
                                message: "A current revision and nonempty text are required."
                                    .into(),
                                uncertain: false,
                            });
                        }
                        self.call(
                            method::MEMORY_UPDATE,
                            Some(&project),
                            json!({"id":id,"expectedRevision":revision,"text":text}),
                            Some(&key),
                        )
                        .await
                    }
                    Change::Accept { project, candidate } => {
                        self.call(
                            method::CANDIDATE_ACCEPT,
                            Some(&project),
                            json!({"id":candidate}),
                            Some(&key),
                        )
                        .await
                    }
                    Change::Reject {
                        project,
                        candidate,
                        reason,
                    } => {
                        self.call(
                            method::CANDIDATE_REJECT,
                            Some(&project),
                            json!({"id":candidate,"reason":reason}),
                            Some(&key),
                        )
                        .await
                    }
                    Change::Resolve {
                        project,
                        candidate,
                        decision,
                    } => {
                        self.call(
                            method::CANDIDATE_RESOLVE,
                            Some(&project),
                            json!({"id":candidate,"decision":decision}),
                            Some(&key),
                        )
                        .await
                    }
                    Change::Recover { project, id } => {
                        self.call(
                            method::MEMORY_RECOVER,
                            Some(&project),
                            json!({"id":id}),
                            Some(&key),
                        )
                        .await
                    }
                    Change::Delete {
                        project,
                        id,
                        reason,
                    } => {
                        self.call(
                            method::MEMORY_DELETE,
                            Some(&project),
                            json!({"id":id,"reason":reason,"force":true}),
                            Some(&key),
                        )
                        .await
                    }
                    Change::RunMaintenance { project } => {
                        self.call(
                            method::MAINTENANCE_RUN,
                            Some(&project),
                            json!({}),
                            Some(&key),
                        )
                        .await
                    }
                    Change::RevokeGrant { grant } => {
                        self.call(method::GRANT_REVOKE, None, json!({"grantId":grant}), None)
                            .await
                    }
                    Change::CreateGrant { project, label } => {
                        let result = self.call(method::GRANT_CREATE, None, json!({"label":label,"profile":"read-propose","projects":[project]}), None).await?;
                        // The daemon stores the secret privately. Project only
                        // public metadata so credentials cannot enter view state.
                        let grant: GrantInfo = serde_json::from_value(result["grant"].clone())
                            .map_err(|_| Failure {
                                code: "PROTOCOL".into(),
                                message: "Invalid grant reply.".into(),
                                uncertain: true,
                            })?;
                        Ok(json!({"grant":grant}))
                    }
                }
            }
        }
    }
}

/// Reuse the installed CLI's existing detached-start implementation. The fixed
/// operation is executed directly, never through a shell or user command text.
pub fn start_service(cli: &std::path::Path, root: &DataRoot) -> Result<(), String> {
    let mut command = std::process::Command::new(cli);
    command
        .args(["--data-root"])
        .arg(root.path())
        .args(["daemon", "start"]);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x0800_0000);
    }
    let mut child = command
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|e| e.to_string())?;
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    loop {
        if let Some(status) = child.try_wait().map_err(|e| e.to_string())? {
            return if status.success() {
                Ok(())
            } else {
                Err("Service start failed; run wrongmemory doctor for details.".into())
            };
        }
        if std::time::Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(
                "Service start timed out. Refresh to check whether the shared daemon started."
                    .into(),
            );
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

pub fn find_cli(explicit: Option<PathBuf>) -> Option<PathBuf> {
    let exe = format!("wrongmemory{}", std::env::consts::EXE_SUFFIX);
    let mut candidates = Vec::new();
    candidates.extend(explicit);
    if let Ok(me) = std::env::current_exe()
        && let Some(dir) = me.parent()
    {
        candidates.push(dir.join(&exe));
    }
    #[cfg(windows)]
    if let Some(local) = std::env::var_os("LOCALAPPDATA") {
        candidates.push(PathBuf::from(local).join("WrongMemory/bin").join(&exe));
    }
    #[cfg(unix)]
    if let Some(home) = std::env::var_os("HOME") {
        candidates.push(
            PathBuf::from(home)
                .join(".local/share/wrongmemory/bin")
                .join(&exe),
        );
    }
    candidates.into_iter().find(|path| path.is_file())
}

pub fn mcp_entry(cli: &std::path::Path, root: &DataRoot, project: &str, grant: &str) -> Value {
    json!({"mcpServers":{"wrongmemory":{"type":"stdio","command":cli,"args":["--data-root",root.path(),"mcp","--project",project,"--grant",grant,"--client","external-agent"]}}})
}
