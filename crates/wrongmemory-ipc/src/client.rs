//! Rust IPC client shared by the CLI, the MCP stdio bridge, and tests.

use std::time::Duration;

use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;
use tokio::io::BufReader;
use wrongmemory_contracts::{
    AuthRequest, AuthResponse, Credential, ErrorBody, ErrorCode, Hello, MAX_FRAME_BYTES,
    PROTOCOL_MAJOR, PROTOCOL_MINOR, Request, Response,
};

use crate::endpoint::{DataRoot, Discovery};
use crate::frame::{FrameError, read_frame, write_frame};

#[derive(Debug, thiserror::Error)]
pub enum CallError {
    /// No daemon is reachable for this data root (missing/stale discovery).
    #[error("WrongMemory service is not running: {0}")]
    Unavailable(String),
    /// The grant's secret is not readable in this data root: the grant was
    /// revoked, or belongs to another data root. Starting a daemon cannot
    /// help, and the daemon is not at fault.
    #[error("grant credential unavailable: {0}")]
    Credential(String),
    #[error("transport: {0}")]
    Transport(#[from] FrameError),
    #[error("{0}")]
    Remote(ErrorBody),
    #[error("unexpected response: {0}")]
    Protocol(String),
}

impl CallError {
    pub fn code(&self) -> Option<ErrorCode> {
        match self {
            CallError::Remote(e) => Some(e.code),
            _ => None,
        }
    }
}

#[cfg(windows)]
type Stream = tokio::net::windows::named_pipe::NamedPipeClient;
#[cfg(unix)]
type Stream = tokio::net::UnixStream;

pub struct Client {
    stream: BufReader<Stream>,
    hello: Hello,
    next_id: u64,
}

impl Client {
    /// Connect with the owner-private host credential (CLI / trusted host).
    pub async fn connect_host(root: &DataRoot, client_name: &str) -> Result<Self, CallError> {
        let discovery = load_discovery(root)?;
        let cred = Credential::Host {
            token: discovery.token.clone(),
        };
        Self::connect_inner(
            &discovery.info.endpoint,
            cred,
            client_name,
            Some(&discovery.token),
        )
        .await
    }

    /// Connect with a scoped grant (external MCP bridges). The secret is read
    /// from the private data root; it never appears in arguments or config.
    pub async fn connect_grant(
        root: &DataRoot,
        grant_id: &str,
        client_name: &str,
    ) -> Result<Self, CallError> {
        let secret = root.read_grant_secret(grant_id).map_err(|e| {
            CallError::Credential(if e.kind() == std::io::ErrorKind::NotFound {
                format!(
                    "grant {grant_id} has no secret in this data root (revoked, or created under another data root)"
                )
            } else {
                format!("cannot read the secret of grant {grant_id}: {e}")
            })
        })?;
        let discovery = load_discovery(root)?;
        let cred = Credential::Grant {
            grant_id: grant_id.to_string(),
            secret,
        };
        Self::connect_inner(
            &discovery.info.endpoint,
            cred,
            client_name,
            Some(&discovery.token),
        )
        .await
    }

    /// Connect with an explicit endpoint and credential.
    pub async fn connect_with(
        endpoint: &str,
        credential: Credential,
        client_name: &str,
    ) -> Result<Self, CallError> {
        Self::connect_inner(endpoint, credential, client_name, None).await
    }

    /// When `proof_token` is set, the daemon must first prove possession of
    /// the owner-private host token (HMAC over a fresh nonce) before any
    /// credential is sent. Complements the server-SID check in `win.rs`.
    async fn connect_inner(
        endpoint: &str,
        credential: Credential,
        client_name: &str,
        proof_token: Option<&str>,
    ) -> Result<Self, CallError> {
        let stream = open(endpoint).await?;
        let mut stream = BufReader::new(stream);
        if let Some(token) = proof_token {
            let nonce = crate::endpoint::generate_token()
                .map_err(|e| CallError::Protocol(e.to_string()))?;
            write_frame(
                stream.get_mut(),
                &serde_json::json!({"type": "challenge", "nonce": nonce}),
                MAX_FRAME_BYTES,
            )
            .await?;
            let reply: Value = read_frame(&mut stream, MAX_FRAME_BYTES)
                .await?
                .ok_or_else(|| CallError::Protocol("connection closed during challenge".into()))?;
            let expected = crate::endpoint::server_proof(token, &nonce);
            let got = reply.get("proof").and_then(Value::as_str).unwrap_or("");
            if !crate::endpoint::token_eq(got, &expected) {
                return Err(CallError::Unavailable(format!(
                    "{endpoint} did not prove it is this user's WrongMemory daemon; no credential sent"
                )));
            }
        }
        let auth = AuthRequest {
            protocol_major: PROTOCOL_MAJOR,
            protocol_minor: PROTOCOL_MINOR,
            credential,
            client_name: client_name.to_string(),
            client_version: env!("CARGO_PKG_VERSION").to_string(),
        };
        write_frame(stream.get_mut(), &auth, MAX_FRAME_BYTES).await?;
        let reply: AuthResponse = read_frame(&mut stream, MAX_FRAME_BYTES)
            .await?
            .ok_or_else(|| CallError::Protocol("connection closed during authentication".into()))?;
        match reply {
            AuthResponse::Ok { hello } => Ok(Self {
                stream,
                hello: *hello,
                next_id: 1,
            }),
            AuthResponse::Error { error } => Err(CallError::Remote(error)),
        }
    }

    pub fn hello(&self) -> &Hello {
        &self.hello
    }

    /// Send one request and wait for its response.
    pub async fn call_raw(
        &mut self,
        method: &str,
        project_id: Option<&str>,
        params: Value,
        idempotency_key: Option<&str>,
    ) -> Result<Value, CallError> {
        let request_id = format!("r{}", self.next_id);
        self.next_id += 1;
        let req = Request {
            protocol: PROTOCOL_MAJOR,
            request_id: request_id.clone(),
            method: method.to_string(),
            project_id: project_id.map(str::to_string),
            params,
            idempotency_key: idempotency_key.map(str::to_string),
        };
        write_frame(self.stream.get_mut(), &req, MAX_FRAME_BYTES).await?;
        let resp: Response = read_frame(&mut self.stream, MAX_FRAME_BYTES)
            .await?
            .ok_or_else(|| CallError::Protocol("connection closed before response".into()))?;
        if resp.request_id != request_id {
            return Err(CallError::Protocol(format!(
                "response id {} does not match request {request_id}",
                resp.request_id
            )));
        }
        match (resp.result, resp.error) {
            (_, Some(err)) => Err(CallError::Remote(err)),
            (Some(v), None) => Ok(v),
            (None, None) => Ok(Value::Null),
        }
    }

    pub async fn call<P: Serialize, R: DeserializeOwned>(
        &mut self,
        method: &str,
        project_id: Option<&str>,
        params: &P,
    ) -> Result<R, CallError> {
        let params =
            serde_json::to_value(params).map_err(|e| CallError::Protocol(e.to_string()))?;
        let v = self.call_raw(method, project_id, params, None).await?;
        serde_json::from_value(v).map_err(|e| CallError::Protocol(e.to_string()))
    }
}

fn load_discovery(root: &DataRoot) -> Result<Discovery, CallError> {
    Discovery::load(root).map_err(|e| {
        CallError::Unavailable(format!(
            "no discovery metadata in {}: {e}",
            root.path().display()
        ))
    })
}

#[cfg(windows)]
async fn open(endpoint: &str) -> Result<Stream, CallError> {
    crate::win::connect(endpoint, Duration::from_secs(5))
        .await
        .map_err(|e| CallError::Unavailable(format!("cannot open {endpoint}: {e}")))
}

#[cfg(unix)]
async fn open(endpoint: &str) -> Result<Stream, CallError> {
    crate::unix::connect(endpoint, Duration::from_secs(5))
        .await
        .map_err(|e| CallError::Unavailable(format!("cannot open {endpoint}: {e}")))
}
