//! Real private IPC and disposable stores. This suite is mandatory in desktop
//! CI; a missing core CLI is a setup failure, never a silently skipped pass.
use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::{Value, json};
use wrongmemory_desktop::model::Query;
use wrongmemory_desktop::preferences::{Appearance, Preferences};
use wrongmemory_desktop::service::{self, Change, Read, Service};
use wrongmemory_ipc::{Client, DataRoot};

#[test]
fn presentation_settings_work_without_starting_or_opening_a_store() {
    let temp = tempfile::tempdir().unwrap();
    let root = DataRoot::resolve(Some(temp.path())).unwrap();
    let service = Service::new(root.clone()).unwrap();
    let preferences = Preferences {
        appearance: Appearance::Dark,
        project: Some("p_fixture".into()),
        sidebar_width: 260.,
        ..Default::default()
    };
    let reply = service
        .request(service::Command::SavePreferences(preferences))
        .unwrap()
        .blocking_recv()
        .unwrap()
        .unwrap();
    assert_eq!(reply["saved"], true);
    assert_eq!(
        Preferences::load(&root).unwrap().appearance,
        Appearance::Dark
    );
    assert!(!root.registry_db().exists());
    assert!(!root.token_file().exists());
}

struct Fixture {
    _temp: tempfile::TempDir,
    cli: PathBuf,
    root: DataRoot,
    project: String,
}

impl Fixture {
    fn new() -> Self {
        let cli = std::env::var_os("WRONGMEMORY_TEST_CLI")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("../../target/debug")
                    .join(format!("wrongmemory{}", std::env::consts::EXE_SUFFIX))
            });
        assert!(
            cli.is_file(),
            "Build the core CLI/daemon first, or set WRONGMEMORY_TEST_CLI"
        );
        let temp = tempfile::tempdir().unwrap();
        let root = DataRoot::resolve(Some(&temp.path().join("service"))).unwrap();
        let project_root = temp.path().join("project");
        std::fs::create_dir_all(project_root.join("src")).unwrap();
        std::fs::write(project_root.join("src/main.rs"), "fn main() {}\n").unwrap();
        let registered = run(
            &cli,
            &root,
            &[
                "project",
                "add",
                project_root.to_str().unwrap(),
                "--managed",
            ],
        );
        Self {
            _temp: temp,
            cli,
            root,
            project: registered["project"]["projectId"].as_str().unwrap().into(),
        }
    }

    fn service(&self) -> Service {
        Service::new(self.root.clone()).unwrap()
    }

    fn remember(&self) -> String {
        run(
            &self.cli,
            &self.root,
            &[
                "remember",
                &self.project,
                "Release builds must run cargo fmt before packaging",
                "--kind",
                "convention",
                "--file",
                "src/main.rs",
            ],
        )["id"]
            .as_str()
            .unwrap()
            .into()
    }

    fn host(&self, method: &str, params: Value) -> Value {
        runtime().block_on(async {
            let mut client = Client::connect_host(&self.root, "desktop-test-host")
                .await
                .unwrap();
            client
                .call_raw(method, Some(&self.project), params, None)
                .await
                .unwrap()
        })
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = Command::new(&self.cli)
            .arg("--data-root")
            .arg(self.root.path())
            .args(["daemon", "stop"])
            .output();
    }
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}

fn run(cli: &Path, root: &DataRoot, args: &[&str]) -> Value {
    let out = Command::new(cli)
        .arg("--data-root")
        .arg(root.path())
        .arg("--json")
        .args(args)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "fixture command failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).unwrap()
}

fn read(service: &Service, read: Read) -> Value {
    service
        .request(service::Command::Read(read))
        .unwrap()
        .blocking_recv()
        .unwrap()
        .unwrap()
}
fn change(service: &Service, change: Change) -> Value {
    service
        .request(service::Command::Change(change))
        .unwrap()
        .blocking_recv()
        .unwrap()
        .unwrap()
}

#[test]
fn library_evidence_graph_and_hidden_ids_use_real_policy() {
    let fixture = Fixture::new();
    let id = fixture.remember();
    let service = fixture.service();
    let overview = read(&service, Read::Overview);
    assert_eq!(overview["projects"].as_array().unwrap().len(), 1);
    let page = read(
        &service,
        Read::Library(Query {
            project: fixture.project.clone(),
            ..Default::default()
        }),
    );
    assert_eq!(page["memories"].as_array().unwrap().len(), 1);
    let memory = read(
        &service,
        Read::Memory {
            project: fixture.project.clone(),
            id: id.clone(),
        },
    );
    assert_eq!(memory["id"], id);
    let graph = read(
        &service,
        Read::Graph {
            project: fixture.project.clone(),
            node: format!("mem:{id}"),
        },
    );
    assert!(!graph["memories"].as_array().unwrap().is_empty());
    assert!(!graph["edges"].as_array().unwrap().is_empty());
    let private=runtime().block_on(async {
        let mut host=Client::connect_host(&fixture.root,"private-session-fixture").await.unwrap();
        host.call_raw("session.acquire",None,json!({"hostSessionId":"another-agent"}),None).await.unwrap();
        host.call_raw("memory.remember",Some(&fixture.project),json!({"text":"Private session deployment evidence must not appear in the desktop library","scope":"session","kind":"fact","sources":[{"type":"user"}]}),Some("private-fixture")).await.unwrap()["id"].as_str().unwrap().to_string()
    });
    let page = read(
        &service,
        Read::Library(Query {
            project: fixture.project.clone(),
            ..Default::default()
        }),
    );
    assert_eq!(page["memories"].as_array().unwrap().len(), 1);
    let error = |id: String| {
        service
            .request(service::Command::Read(Read::Memory {
                project: fixture.project.clone(),
                id,
            }))
            .unwrap()
            .blocking_recv()
            .unwrap()
            .unwrap_err()
    };
    let hidden = error(private);
    let absent = error("absent-memory".into());
    assert_eq!(hidden.code, absent.code);
    assert_eq!(hidden.message, absent.message);
}

#[test]
fn revision_conflicts_review_and_recovery_preserve_authoritative_state() {
    let fixture = Fixture::new();
    let id = fixture.remember();
    let service = fixture.service();
    let original = read(
        &service,
        Read::Memory {
            project: fixture.project.clone(),
            id: id.clone(),
        },
    );
    let revision = original["revision"].as_i64().unwrap();
    change(
        &service,
        Change::Edit {
            project: fixture.project.clone(),
            id: id.clone(),
            revision,
            text: "Release packaging requires cargo fmt and locked dependency validation".into(),
        },
    );
    let stale = service
        .request(service::Command::Change(Change::Edit {
            project: fixture.project.clone(),
            id: id.clone(),
            revision,
            text: "A stale editor must never replace the committed record".into(),
        }))
        .unwrap()
        .blocking_recv()
        .unwrap()
        .unwrap_err();
    assert_eq!(stale.code, "REVISION_CONFLICT");
    let proposal=fixture.host("candidate.propose",json!({"text":"Project deployment targets the staging cluster before production rollout","kind":"decision","evidence":"fixture deployment contract"}));
    let candidate = proposal["id"].as_str().unwrap().to_string();
    let pending = read(
        &service,
        Read::Review {
            project: fixture.project.clone(),
        },
    );
    assert!(
        pending["candidates"]
            .as_array()
            .unwrap()
            .iter()
            .any(|c| c["id"] == candidate)
    );
    let accepted = change(
        &service,
        Change::Accept {
            project: fixture.project.clone(),
            candidate,
        },
    );
    assert!(accepted["memoryId"].is_string());
    change(
        &service,
        Change::Delete {
            project: fixture.project.clone(),
            id: id.clone(),
            reason: "Disposable desktop lifecycle test".into(),
        },
    );
    assert_eq!(
        read(
            &service,
            Read::Memory {
                project: fixture.project.clone(),
                id: id.clone()
            }
        )["status"],
        "deleted"
    );
    change(
        &service,
        Change::Recover {
            project: fixture.project.clone(),
            id: id.clone(),
        },
    );
    assert_eq!(
        read(
            &service,
            Read::Memory {
                project: fixture.project.clone(),
                id
            }
        )["status"],
        "active"
    );
}

#[test]
fn client_configuration_never_contains_credentials_and_revocation_is_live() {
    let fixture = Fixture::new();
    let service = fixture.service();
    fixture.remember();
    let created = change(
        &service,
        Change::CreateGrant {
            project: fixture.project.clone(),
            label: "fixture-coding-client".into(),
        },
    );
    assert!(created.get("secret").is_none());
    assert!(created.get("secretFile").is_none());
    let grant = created["grant"]["grantId"].as_str().unwrap().to_string();
    assert_eq!(created["grant"]["profile"], "read-propose");
    let secret = fixture.root.read_grant_secret(&grant).unwrap();
    let config =
        service::mcp_entry(&fixture.cli, &fixture.root, &fixture.project, &grant).to_string();
    assert!(!config.contains(&secret));
    assert!(config.contains(&grant));
    assert!(config.contains("stdio"));
    let rt = runtime();
    let mut client = rt
        .block_on(Client::connect_grant(
            &fixture.root,
            &grant,
            "fixture-client",
        ))
        .unwrap();
    rt.block_on(client.call_raw(
        "memory.search",
        Some(&fixture.project),
        json!({"query":"packaging"}),
        None,
    ))
    .unwrap();
    let denied = rt
        .block_on(client.call_raw(
            "memory.remember",
            Some(&fixture.project),
            json!({"text":"A read propose client must not create authoritative records directly"}),
            None,
        ))
        .unwrap_err();
    assert_eq!(denied.code().unwrap().as_str(), "FORBIDDEN");
    change(
        &service,
        Change::RevokeGrant {
            grant: grant.clone(),
        },
    );
    assert!(!fixture.root.grant_secret_file(&grant).unwrap().exists());
    assert!(
        rt.block_on(client.call_raw(
            "memory.search",
            Some(&fixture.project),
            json!({"query":"packaging"}),
            None
        ))
        .is_err()
    );
}

#[test]
fn closing_the_desktop_client_leaves_the_daemon_and_other_clients_running() {
    let fixture = Fixture::new();
    let id = fixture.remember();
    {
        let service = fixture.service();
        read(&service, Read::Overview);
    }
    let rt = runtime();
    rt.block_on(async {
        let mut other = Client::connect_host(&fixture.root, "independent-client")
            .await
            .unwrap();
        let memory = other
            .call_raw("memory.get", Some(&fixture.project), json!({"id":id}), None)
            .await
            .unwrap();
        assert_eq!(memory["status"], "active");
        other
            .call_raw("daemon.health", None, json!({}), None)
            .await
            .unwrap();
    });
}
