//! E2B / CubeSandbox backend — run session tools inside a remote E2B-compatible
//! microVM instead of a local Podman container.
//!
//! Two protocols, verified live against E2B (`envd` 0.6.6):
//! - **Control plane** — REST at `api_url`: `POST /sandboxes` (`{templateID, timeout}`
//!   → `{sandboxID, …}`), `DELETE /sandboxes/{id}`; `X-API-KEY` auth.
//! - **Data plane** — the sandbox's `envd` at `https://49983-{id}.{domain}`, ConnectRPC:
//!   - `process.Process/Start` — server-streaming. POST `application/connect+json` with a
//!     5-byte-framed JSON `{process:{cmd,args,cwd,envs}, tag, stdin}`. The response is framed
//!     `ProcessEvent`s — `start{pid}` → `data{stdout|stderr}` (base64) → `end{exitCode?}` —
//!     terminated by a `0x02` end-of-stream frame. (`exitCode` is omitted when it is 0.)
//!   - `SendInput` / `CloseStdin` — unary (`application/json`), target the process by `tag`.
//!
//! One client hits both E2B cloud and a self-hosted CubeSandbox (same API).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::Engine as _;
use serde_json::json;
use tokio_stream::StreamExt;

use crate::error::IsolationError;
use crate::session_sandbox::{BgTask, ExecResult, Sandbox};

/// The port `envd` listens on inside every sandbox.
const ENVD_PORT: u32 = 49983;

/// Configuration for an E2B-compatible backend (E2B cloud or self-hosted CubeSandbox).
#[derive(Debug, Clone)]
pub struct E2bConfig {
    /// Control-plane base URL, e.g. `https://api.e2b.dev`, or a CubeSandbox URL.
    pub api_url: String,
    /// API key sent as `X-API-KEY` (a placeholder string for local CubeSandbox).
    pub api_key: String,
    /// Sandbox template id or alias, e.g. `base`.
    pub template: String,
    /// Domain the sandbox's `envd` traffic is served from, e.g. `e2b.app`.
    pub domain: String,
}

/// A client for an E2B-compatible control plane + the sandboxes it creates.
/// Cloning is cheap (`reqwest::Client` is internally reference-counted).
#[derive(Clone)]
pub struct E2bClient {
    http: reqwest::Client,
    cfg: E2bConfig,
}

/// A live sandbox: its id, its `envd` base URL, and (secure mode only) an access token.
#[derive(Clone)]
pub struct E2bSession {
    pub sandbox_id: String,
    envd_base: String,
    access_token: Option<String>,
}

impl E2bClient {
    pub fn new(cfg: E2bConfig) -> Self {
        Self {
            http: reqwest::Client::new(),
            cfg,
        }
    }

    /// Create a sandbox with a maximum lifetime (seconds) and a set of
    /// environment variables. The vars are set at create time and persist into
    /// **every** later `envd` process (verified live) — that's how a git token is
    /// made available to the agent's own later `git push`, not just the clone.
    pub async fn create(
        &self,
        timeout_secs: u64,
        env: &BTreeMap<String, String>,
    ) -> Result<E2bSession, IsolationError> {
        let mut body = json!({ "templateID": self.cfg.template, "timeout": timeout_secs });
        if !env.is_empty() {
            body["envVars"] = json!(env);
        }
        let resp = self
            .http
            .post(format!("{}/sandboxes", self.cfg.api_url))
            .header("X-API-KEY", &self.cfg.api_key)
            .json(&body)
            .send()
            .await
            .map_err(|e| IsolationError::E2b(format!("create request: {e}")))?;
        let status = resp.status();
        let body = resp
            .text()
            .await
            .map_err(|e| IsolationError::E2b(format!("create body: {e}")))?;
        if !status.is_success() {
            return Err(IsolationError::E2b(format!(
                "create sandbox failed ({status}): {body}"
            )));
        }
        let v: serde_json::Value = serde_json::from_str(&body)?;
        let sandbox_id = v["sandboxID"]
            .as_str()
            .ok_or_else(|| IsolationError::E2b("create response missing sandboxID".into()))?
            .to_string();
        let access_token = v["envdAccessToken"].as_str().map(str::to_string);
        let envd_base = format!("https://{ENVD_PORT}-{sandbox_id}.{}", self.cfg.domain);
        Ok(E2bSession {
            sandbox_id,
            envd_base,
            access_token,
        })
    }

    /// Kill (delete) a sandbox. Best-effort — matches Podman `stop`'s semantics.
    pub async fn kill(&self, sandbox_id: &str) -> Result<(), IsolationError> {
        let _ = self
            .http
            .delete(format!("{}/sandboxes/{sandbox_id}", self.cfg.api_url))
            .header("X-API-KEY", &self.cfg.api_key)
            .send()
            .await;
        Ok(())
    }

    /// (Re)set a sandbox's remaining lifetime to `timeout_secs` from now. Called
    /// periodically by the keep-alive loop so a live session's VM doesn't
    /// self-terminate at its original TTL.
    pub async fn set_timeout(
        &self,
        sandbox_id: &str,
        timeout_secs: u64,
    ) -> Result<(), IsolationError> {
        let resp = self
            .http
            .post(format!(
                "{}/sandboxes/{sandbox_id}/timeout",
                self.cfg.api_url
            ))
            .header("X-API-KEY", &self.cfg.api_key)
            .json(&json!({ "timeout": timeout_secs }))
            .send()
            .await
            .map_err(|e| IsolationError::E2b(format!("set_timeout request: {e}")))?;
        if !resp.status().is_success() {
            return Err(IsolationError::E2b(format!(
                "set_timeout failed: HTTP {}",
                resp.status()
            )));
        }
        Ok(())
    }

    /// Run a command in the sandbox and collect `{stdout, stderr, exit}`.
    pub async fn exec(
        &self,
        session: &E2bSession,
        argv: &[&str],
        cwd: &str,
        timeout: Duration,
    ) -> Result<ExecResult, IsolationError> {
        self.run(session, argv, cwd, None, timeout).await
    }

    /// Run a command with `stdin` piped in (the write path — `SendInput` + `CloseStdin`).
    pub async fn exec_stdin(
        &self,
        session: &E2bSession,
        argv: &[&str],
        stdin: &str,
        cwd: &str,
        timeout: Duration,
    ) -> Result<ExecResult, IsolationError> {
        self.run(session, argv, cwd, Some(stdin.as_bytes()), timeout)
            .await
    }

    /// Start a command via `envd`, stream the response, and — when `stdin` is
    /// given — send it (by `tag`) once the process has started, then close stdin.
    async fn run(
        &self,
        session: &E2bSession,
        argv: &[&str],
        cwd: &str,
        stdin: Option<&[u8]>,
        timeout: Duration,
    ) -> Result<ExecResult, IsolationError> {
        match tokio::time::timeout(timeout, self.run_inner(session, argv, cwd, stdin)).await {
            Ok(result) => result,
            Err(_) => Err(IsolationError::Timeout(timeout)),
        }
    }

    async fn run_inner(
        &self,
        session: &E2bSession,
        argv: &[&str],
        cwd: &str,
        stdin: Option<&[u8]>,
    ) -> Result<ExecResult, IsolationError> {
        let (cmd, args) = argv
            .split_first()
            .ok_or_else(|| IsolationError::E2b("empty argv".into()))?;
        let tag = format!("axo-{}", uuid::Uuid::new_v4());
        let payload = serde_json::to_vec(&json!({
            "process": { "cmd": cmd, "args": args, "envs": {}, "cwd": cwd },
            "tag": tag,
            "stdin": stdin.is_some(),
        }))?;

        let resp = self
            .envd(session, "Start")
            .header("Content-Type", "application/connect+json")
            .body(connect_frame(&payload))
            .send()
            .await
            .map_err(|e| IsolationError::E2b(format!("Start request: {e}")))?;
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(IsolationError::E2b(format!(
                "Start failed ({status}): {body}"
            )));
        }

        let mut stream = resp.bytes_stream();
        let mut decoder = FrameDecoder::new();
        let mut out = ExecOutput::default();
        let mut input_sent = stdin.is_none();

        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|e| IsolationError::E2b(format!("Start stream: {e}")))?;
            decoder.push(&chunk);
            while let Some(frame) = decoder.next_frame()? {
                match frame {
                    Frame::End(v) => {
                        if let Some(err) = v.get("error") {
                            return Err(IsolationError::E2b(format!("envd stream error: {err}")));
                        }
                    }
                    Frame::Event(v) => {
                        out.apply(&v);
                        // The process is now live — safe to send stdin by tag.
                        if !input_sent && out.saw_start {
                            if let Some(data) = stdin {
                                self.send_input(session, &tag, data).await?;
                                self.close_stdin(session, &tag).await?;
                            }
                            input_sent = true;
                        }
                    }
                }
            }
        }
        Ok(out.into_result())
    }

    /// Write to a running process's stdin (unary Connect), selecting it by `tag`.
    async fn send_input(
        &self,
        session: &E2bSession,
        tag: &str,
        data: &[u8],
    ) -> Result<(), IsolationError> {
        let encoded = base64::engine::general_purpose::STANDARD.encode(data);
        self.envd_unary(
            session,
            "SendInput",
            &json!({ "process": { "tag": tag }, "input": { "stdin": encoded } }),
        )
        .await
    }

    /// Close a running process's stdin (unary Connect), selecting it by `tag`.
    async fn close_stdin(&self, session: &E2bSession, tag: &str) -> Result<(), IsolationError> {
        self.envd_unary(session, "CloseStdin", &json!({ "process": { "tag": tag } }))
            .await
    }

    /// A unary `envd` Connect call (`application/json`).
    async fn envd_unary(
        &self,
        session: &E2bSession,
        method: &str,
        body: &serde_json::Value,
    ) -> Result<(), IsolationError> {
        let resp = self
            .envd(session, method)
            .header("Content-Type", "application/json")
            .json(body)
            .send()
            .await
            .map_err(|e| IsolationError::E2b(format!("{method}: {e}")))?;
        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Err(IsolationError::E2b(format!(
                "{method} failed ({status}): {text}"
            )));
        }
        Ok(())
    }

    /// A request builder pointed at a `process.Process/{method}` envd endpoint,
    /// with the Connect version header and (secure mode) the access token.
    fn envd(&self, session: &E2bSession, method: &str) -> reqwest::RequestBuilder {
        let mut req = self
            .http
            .post(format!("{}/process.Process/{method}", session.envd_base))
            .header("Connect-Protocol-Version", "1");
        if let Some(token) = &session.access_token {
            req = req.header("Authorization", format!("Bearer {token}"));
        }
        req
    }

    /// Stream a background command's combined output into `log` (tail-trimmed to
    /// 64 KiB) and set `status` when it exits. Best-effort — used by
    /// `spawn_background`, which returns before this completes.
    async fn run_to_log(
        &self,
        session: &E2bSession,
        command: &str,
        cwd: &str,
        log: Arc<Mutex<String>>,
        status: Arc<Mutex<String>>,
    ) {
        let payload = match serde_json::to_vec(&json!({
            "process": { "cmd": "sh", "args": ["-c", command], "envs": {}, "cwd": cwd },
            "stdin": false,
        })) {
            Ok(p) => p,
            Err(e) => return set(&status, format!("failed: {e}")),
        };
        let resp = match self
            .envd(session, "Start")
            .header("Content-Type", "application/connect+json")
            .body(connect_frame(&payload))
            .send()
            .await
        {
            Ok(r) => r,
            Err(e) => return set(&status, format!("failed: {e}")),
        };
        if !resp.status().is_success() {
            return set(&status, format!("failed: HTTP {}", resp.status()));
        }

        let b64 = base64::engine::general_purpose::STANDARD;
        let mut stream = resp.bytes_stream();
        let mut decoder = FrameDecoder::new();
        while let Some(chunk) = stream.next().await {
            let Ok(chunk) = chunk else { break };
            decoder.push(&chunk);
            while let Ok(Some(frame)) = decoder.next_frame() {
                let Frame::Event(v) = frame else { continue };
                let event = &v["event"];
                if let Some(data) = event.get("data") {
                    for key in ["stdout", "stderr"] {
                        if let Some(s) = data.get(key).and_then(|x| x.as_str()) {
                            if let Ok(bytes) = b64.decode(s) {
                                append_tail(&log, &String::from_utf8_lossy(&bytes));
                            }
                        }
                    }
                } else if let Some(end) = event.get("end") {
                    let code = end.get("exitCode").and_then(|x| x.as_i64()).unwrap_or(0);
                    set(&status, format!("exited ({code})"));
                }
            }
        }
    }
}

/// Wrap a message in a Connect envelope: `[flag=0x00][len: u32 big-endian][payload]`.
fn connect_frame(payload: &[u8]) -> Vec<u8> {
    let mut frame = Vec::with_capacity(5 + payload.len());
    frame.push(0x00);
    frame.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    frame.extend_from_slice(payload);
    frame
}

/// Set a shared status/log cell, ignoring a poisoned lock (best-effort).
fn set(cell: &Arc<Mutex<String>>, value: String) {
    if let Ok(mut guard) = cell.lock() {
        *guard = value;
    }
}

/// Append to a shared log, keeping only the last 64 KiB (background tasks are chatty).
fn append_tail(cell: &Arc<Mutex<String>>, text: &str) {
    if let Ok(mut guard) = cell.lock() {
        guard.push_str(text);
        if guard.len() > 64 * 1024 {
            let cut = guard.len() - 64 * 1024;
            guard.drain(..cut);
        }
    }
}

/// A [`Sandbox`] backed by a remote E2B-compatible microVM. Session tools run
/// against it exactly as they do against the local Podman `SessionSandbox`.
///
/// Command execution (the file tools + `bash`) and background tasks work today;
/// interactive PTY terminals are not wired yet (they need a remote-PTY stream —
/// a later phase), so `spawn_pty` returns an error and the session has no
/// terminals until then.
pub struct E2bSandbox {
    client: E2bClient,
    session: E2bSession,
    /// The working directory inside the sandbox (the confinement root for file tools).
    root: PathBuf,
    tasks: Mutex<Vec<E2bBgHandle>>,
    /// Keep-alive loop that periodically extends the VM's TTL so a live session
    /// doesn't self-terminate. Only the owning handle (from `start`) has one;
    /// `with_root` views share the VM and carry `None`. Aborted on stop/drop.
    keepalive: Option<tokio::task::AbortHandle>,
}

struct E2bBgHandle {
    id: String,
    command: String,
    status: Arc<Mutex<String>>,
    log: Arc<Mutex<String>>,
}

impl E2bSandbox {
    /// Create a fresh remote sandbox for a session, rooted at `root` (a path
    /// inside the sandbox, e.g. a cloned repo or `/home/user`). `env` is set on
    /// the VM at create time and visible to every later command (e.g. a git
    /// token read by an in-VM credential helper).
    pub async fn start(
        cfg: E2bConfig,
        timeout_secs: u64,
        root: impl Into<PathBuf>,
        env: &BTreeMap<String, String>,
    ) -> Result<Self, IsolationError> {
        let client = E2bClient::new(cfg);
        let session = client.create(timeout_secs, env).await?;

        // Keep the VM alive while this handle lives: re-extend the TTL to
        // `timeout_secs` every half-life (min 30s) so it never lapses under an
        // active session. Best-effort; a failed extension just retries next tick.
        let ka_client = client.clone();
        let ka_id = session.sandbox_id.clone();
        let period = Duration::from_secs((timeout_secs / 2).max(30));
        let keepalive = tokio::spawn(async move {
            loop {
                tokio::time::sleep(period).await;
                let _ = ka_client.set_timeout(&ka_id, timeout_secs).await;
            }
        })
        .abort_handle();

        Ok(Self {
            client,
            session,
            root: root.into(),
            tasks: Mutex::new(Vec::new()),
            keepalive: Some(keepalive),
        })
    }

    /// The underlying sandbox id.
    pub fn sandbox_id(&self) -> &str {
        &self.session.sandbox_id
    }
}

impl Drop for E2bSandbox {
    fn drop(&mut self) {
        // Never let the keep-alive loop outlive its handle (it would keep a VM
        // alive with no owner). The VM itself still lapses at its TTL if `stop`
        // was never called.
        if let Some(ka) = &self.keepalive {
            ka.abort();
        }
    }
}

#[async_trait::async_trait]
impl Sandbox for E2bSandbox {
    fn root(&self) -> &Path {
        &self.root
    }

    async fn exec(&self, argv: &[&str], timeout: Duration) -> Result<ExecResult, IsolationError> {
        self.client
            .exec(&self.session, argv, &self.root.to_string_lossy(), timeout)
            .await
    }

    async fn exec_stdin(
        &self,
        argv: &[&str],
        stdin: &str,
        timeout: Duration,
    ) -> Result<ExecResult, IsolationError> {
        self.client
            .exec_stdin(
                &self.session,
                argv,
                stdin,
                &self.root.to_string_lossy(),
                timeout,
            )
            .await
    }

    fn spawn_background(&self, command: &str) -> String {
        let id = format!("task-{}", uuid::Uuid::new_v4());
        let status = Arc::new(Mutex::new("running".to_string()));
        let log = Arc::new(Mutex::new(String::new()));
        if let Ok(mut tasks) = self.tasks.lock() {
            tasks.push(E2bBgHandle {
                id: id.clone(),
                command: command.to_string(),
                status: status.clone(),
                log: log.clone(),
            });
        }
        let client = self.client.clone();
        let session = self.session.clone();
        let cwd = self.root.to_string_lossy().to_string();
        let command = command.to_string();
        tokio::spawn(async move {
            client
                .run_to_log(&session, &command, &cwd, log, status)
                .await;
        });
        id
    }

    fn spawn_pty(
        &self,
        _command: &str,
        _rows: u16,
        _cols: u16,
    ) -> Result<std::sync::Arc<crate::pty::PtyTerminal>, String> {
        Err("interactive terminals are not yet supported on the E2B backend".to_string())
    }

    fn get_terminal(&self, _id: &str) -> Option<std::sync::Arc<crate::pty::PtyTerminal>> {
        None
    }

    fn kill_terminal(&self, _id: &str) -> bool {
        false
    }

    fn list_terminals(&self) -> Vec<(String, String, bool)> {
        Vec::new()
    }

    fn list_tasks(&self) -> Vec<BgTask> {
        self.tasks
            .lock()
            .map(|tasks| {
                tasks
                    .iter()
                    .map(|h| BgTask {
                        id: h.id.clone(),
                        command: h.command.clone(),
                        status: h.status.lock().map(|s| s.clone()).unwrap_or_default(),
                        log: h.log.lock().map(|l| l.clone()).unwrap_or_default(),
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    fn with_root(&self, root: &Path) -> Arc<dyn Sandbox> {
        // Same remote microVM (shared client + session), re-rooted at a worktree.
        // Fresh task list; does not own the VM lifecycle (no keep-alive, and only
        // the primary handle calls `stop`), so dropping a variant handle leaves
        // the VM running and its keep-alive untouched.
        Arc::new(E2bSandbox {
            client: self.client.clone(),
            session: self.session.clone(),
            root: root.to_path_buf(),
            tasks: Mutex::new(Vec::new()),
            keepalive: None,
        })
    }

    async fn stop(&self) {
        if let Some(ka) = &self.keepalive {
            ka.abort();
        }
        self.client.kill(&self.session.sandbox_id).await.ok();
    }
}

/// A Connect stream frame: a normal message, or the end-of-stream frame.
enum Frame {
    Event(serde_json::Value),
    End(serde_json::Value),
}

/// Incremental decoder for Connect's `[flag][len][payload]` framed stream.
struct FrameDecoder {
    buf: Vec<u8>,
}

impl FrameDecoder {
    fn new() -> Self {
        Self { buf: Vec::new() }
    }

    fn push(&mut self, bytes: &[u8]) {
        self.buf.extend_from_slice(bytes);
    }

    /// Pop the next complete frame, or `None` if the buffer holds a partial one.
    fn next_frame(&mut self) -> Result<Option<Frame>, IsolationError> {
        if self.buf.len() < 5 {
            return Ok(None);
        }
        let len = u32::from_be_bytes([self.buf[1], self.buf[2], self.buf[3], self.buf[4]]) as usize;
        if self.buf.len() < 5 + len {
            return Ok(None);
        }
        let flag = self.buf[0];
        let msg = self.buf[5..5 + len].to_vec();
        self.buf.drain(..5 + len);
        let v: serde_json::Value = serde_json::from_slice(&msg)
            .map_err(|e| IsolationError::E2b(format!("bad envd frame: {e}")))?;
        Ok(Some(if flag & 0x02 != 0 {
            Frame::End(v)
        } else {
            Frame::Event(v)
        }))
    }
}

/// Accumulates `ProcessEvent`s into an [`ExecResult`].
#[derive(Default)]
struct ExecOutput {
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    /// `None` until an `end` event is seen; the exit code (0 when omitted).
    exit_code: Option<i32>,
    saw_start: bool,
}

impl ExecOutput {
    fn apply(&mut self, v: &serde_json::Value) {
        let b64 = base64::engine::general_purpose::STANDARD;
        let event = &v["event"];
        if event.get("start").is_some() {
            self.saw_start = true;
        } else if let Some(data) = event.get("data") {
            if let Some(s) = data.get("stdout").and_then(|x| x.as_str()) {
                if let Ok(bytes) = b64.decode(s) {
                    self.stdout.extend_from_slice(&bytes);
                }
            }
            if let Some(s) = data.get("stderr").and_then(|x| x.as_str()) {
                if let Ok(bytes) = b64.decode(s) {
                    self.stderr.extend_from_slice(&bytes);
                }
            }
        } else if let Some(end) = event.get("end") {
            // envd omits `exitCode` when it is 0 (proto3 default-value omission).
            self.exit_code = Some(end.get("exitCode").and_then(|x| x.as_i64()).unwrap_or(0) as i32);
        }
    }

    fn into_result(self) -> ExecResult {
        ExecResult {
            stdout: String::from_utf8_lossy(&self.stdout).to_string(),
            stderr: String::from_utf8_lossy(&self.stderr).to_string(),
            // -1 signals no `end` event was seen (a truncated/incomplete stream).
            exit_code: self.exit_code.unwrap_or(-1),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(v: serde_json::Value) -> Vec<u8> {
        connect_frame(&serde_json::to_vec(&v).unwrap())
    }

    fn end_frame() -> Vec<u8> {
        let mut f = vec![0x02];
        f.extend_from_slice(&2u32.to_be_bytes());
        f.extend_from_slice(b"{}");
        f
    }

    fn decode_all(bytes: &[u8]) -> ExecOutput {
        let mut dec = FrameDecoder::new();
        dec.push(bytes);
        let mut out = ExecOutput::default();
        while let Some(f) = dec.next_frame().unwrap() {
            if let Frame::Event(v) = f {
                out.apply(&v);
            }
        }
        out
    }

    #[test]
    fn connect_frame_prefixes_length() {
        assert_eq!(connect_frame(b"hi"), vec![0x00, 0, 0, 0, 2, b'h', b'i']);
    }

    #[test]
    fn parses_the_verified_nonzero_exit_stream() {
        let b64 = base64::engine::general_purpose::STANDARD;
        let mut body = Vec::new();
        body.extend(frame(json!({"event":{"start":{"pid":1319}}})));
        body.extend(frame(
            json!({"event":{"data":{"stderr": b64.encode("oops")}}}),
        ));
        body.extend(frame(
            json!({"event":{"data":{"stdout": b64.encode("hello")}}}),
        ));
        body.extend(frame(
            json!({"event":{"end":{"exitCode":7,"exited":true,"status":"exit status 7"}}}),
        ));
        body.extend(end_frame());
        let r = decode_all(&body).into_result();
        assert_eq!(r.stdout, "hello");
        assert_eq!(r.stderr, "oops");
        assert_eq!(r.exit_code, 7);
    }

    #[test]
    fn exit_zero_has_no_exit_code_field() {
        // The exact shape envd emits for a successful command — `exitCode` absent.
        let mut body = Vec::new();
        body.extend(frame(json!({"event":{"start":{"pid":1}}})));
        body.extend(frame(
            json!({"event":{"end":{"exited":true,"status":"exit status 0"}}}),
        ));
        body.extend(end_frame());
        assert_eq!(decode_all(&body).into_result().exit_code, 0);
    }

    #[test]
    fn frame_decoder_handles_split_chunks() {
        // A frame delivered across two stream chunks must still decode.
        let full = frame(json!({"event":{"start":{"pid":9}}}));
        let (a, b) = full.split_at(3);
        let mut dec = FrameDecoder::new();
        dec.push(a);
        assert!(dec.next_frame().unwrap().is_none());
        dec.push(b);
        assert!(matches!(dec.next_frame().unwrap(), Some(Frame::Event(_))));
    }

    #[test]
    fn end_stream_error_surfaces() {
        let mut dec = FrameDecoder::new();
        dec.push(&frame(
            json!({"error":{"code":"internal","message":"boom"}}),
        ));
        // reclassify the last frame as an End frame by flipping the flag byte
        let mut raw = frame(json!({"error":{"code":"internal"}}));
        raw[0] = 0x02;
        dec.push(&raw);
        // drain to the end frame and assert error handling in run_inner's logic
        let mut saw_err = false;
        while let Some(f) = dec.next_frame().unwrap() {
            if let Frame::End(v) = f {
                if v.get("error").is_some() {
                    saw_err = true;
                }
            }
        }
        assert!(saw_err);
    }

    /// Live end-to-end test against real E2B. Ignored by default; run with:
    /// `E2B_API_KEY=... cargo test -p axocoatl-isolation -- --ignored live_`
    #[tokio::test]
    #[ignore = "hits live E2B — requires E2B_API_KEY"]
    async fn live_exec_and_stdin_against_e2b() {
        let api_key = std::env::var("E2B_API_KEY").expect("E2B_API_KEY must be set");
        let client = E2bClient::new(E2bConfig {
            api_url: "https://api.e2b.dev".into(),
            api_key,
            template: "base".into(),
            domain: "e2b.app".into(),
        });
        let session = client
            .create(120, &BTreeMap::new())
            .await
            .expect("create sandbox");

        let run = async {
            // exec: non-zero exit, stdout + stderr split.
            let r = client
                .exec(
                    &session,
                    &["sh", "-c", "printf hi; printf oops 1>&2; exit 3"],
                    "/",
                    Duration::from_secs(30),
                )
                .await?;
            assert_eq!(
                (r.stdout.as_str(), r.stderr.as_str(), r.exit_code),
                ("hi", "oops", 3)
            );

            // exec: success (exit 0, the omitted-exitCode case).
            let ok = client
                .exec(
                    &session,
                    &["sh", "-c", "echo done"],
                    "/",
                    Duration::from_secs(30),
                )
                .await?;
            assert_eq!((ok.stdout.trim(), ok.exit_code), ("done", 0));

            // exec_stdin: write a file via `cat > file`, then read it back.
            client
                .exec_stdin(
                    &session,
                    &["sh", "-c", "cat > \"$1\"", "sh", "/tmp/axo.txt"],
                    "written-via-stdin",
                    "/",
                    Duration::from_secs(30),
                )
                .await?;
            let back = client
                .exec(
                    &session,
                    &["cat", "/tmp/axo.txt"],
                    "/",
                    Duration::from_secs(30),
                )
                .await?;
            assert_eq!(back.stdout, "written-via-stdin");
            Ok::<_, IsolationError>(())
        }
        .await;

        client.kill(&session.sandbox_id).await.ok();
        run.expect("live E2B exec/stdin flow");
    }

    /// Drive `E2bSandbox` purely through the `Sandbox` trait object — the same
    /// path the session tools take — against real E2B.
    #[tokio::test]
    #[ignore = "hits live E2B — requires E2B_API_KEY"]
    async fn live_sandbox_trait_against_e2b() {
        let api_key = std::env::var("E2B_API_KEY").expect("E2B_API_KEY must be set");
        let cfg = E2bConfig {
            api_url: "https://api.e2b.dev".into(),
            api_key,
            template: "base".into(),
            domain: "e2b.app".into(),
        };
        let sandbox = E2bSandbox::start(cfg, 120, "/tmp", &BTreeMap::new())
            .await
            .expect("start sandbox");
        let sb: &dyn Sandbox = &sandbox;

        let run = async {
            // cwd is honored (the file tools + bash rely on `root` as the cwd).
            let pwd = sb.exec(&["pwd"], Duration::from_secs(30)).await?;
            assert_eq!(pwd.stdout.trim(), "/tmp");

            // write via exec_stdin (relative path, resolved against root), read back.
            sb.exec_stdin(
                &["sh", "-c", "cat > \"$1\"", "sh", "note.txt"],
                "trait-write",
                Duration::from_secs(30),
            )
            .await?;
            let back = sb
                .exec(&["cat", "note.txt"], Duration::from_secs(30))
                .await?;
            assert_eq!(back.stdout, "trait-write");

            // background task captures its output into the tracked log.
            let id = sb.spawn_background("echo bg-line");
            assert!(id.starts_with("task-"));
            tokio::time::sleep(Duration::from_millis(1500)).await;
            let tasks = sb.list_tasks();
            assert_eq!(tasks.len(), 1);
            assert!(
                tasks[0].log.contains("bg-line"),
                "bg log should capture output, got: {:?}",
                tasks[0].log
            );

            // terminals are deferred on this backend.
            assert!(sb.spawn_pty("bash", 24, 80).is_err());
            Ok::<_, IsolationError>(())
        }
        .await;

        sb.stop().await;
        run.expect("live E2bSandbox trait flow");
    }

    /// End-to-end git-native flow against real E2B, running the same commands the
    /// daemon's `start_e2b_sandbox` uses: inject a token as a sandbox env var,
    /// configure the in-VM credential helper, prove it yields Basic creds from the
    /// env (the private clone/push auth path — without needing a real token),
    /// clone a public repo, re-root at it, and `git worktree add` inside (the
    /// variant-lane mechanism).
    #[tokio::test]
    #[ignore = "hits live E2B — requires E2B_API_KEY"]
    async fn live_git_native_flow_against_e2b() {
        let api_key = std::env::var("E2B_API_KEY").expect("E2B_API_KEY must be set");
        let cfg = E2bConfig {
            api_url: "https://api.e2b.dev".into(),
            api_key,
            template: "base".into(),
            domain: "e2b.app".into(),
        };
        // Exactly the daemon's create-time env: no prompts + a (fake) git token.
        let mut env = BTreeMap::new();
        env.insert("GIT_TERMINAL_PROMPT".to_string(), "0".to_string());
        env.insert("AXO_GIT_TOKEN".to_string(), "fake-token-xyz789".to_string());
        let base = E2bSandbox::start(cfg, 180, "/home/user", &env)
            .await
            .expect("start sandbox");

        let run = async {
            // (1) Configure git — the daemon's exact setup command.
            let setup = "git config --global 'credential.https://github.com.helper' \
                 '!f() { echo username=x-access-token; echo password=$AXO_GIT_TOKEN; }; f' && \
                 git config --global 'credential.https://github.com.useHttpPath' false && \
                 git config --global user.email 'agent@axocoatl.local' && \
                 git config --global user.name 'Axocoatl Agent'";
            let r = base
                .exec(&["sh", "-c", setup], Duration::from_secs(30))
                .await?;
            assert_eq!(r.exit_code, 0, "git config failed: {}", r.stderr);

            // (2) The helper reads the injected token and produces Basic creds.
            let fill = base
                .exec(
                    &[
                        "sh",
                        "-c",
                        "printf 'protocol=https\\nhost=github.com\\n\\n' | git credential fill",
                    ],
                    Duration::from_secs(30),
                )
                .await?;
            assert!(
                fill.stdout.contains("username=x-access-token"),
                "credential fill username: {}",
                fill.stdout
            );
            assert!(
                fill.stdout.contains("password=fake-token-xyz789"),
                "credential fill password (token from env): {}",
                fill.stdout
            );

            // (3) Public clone end-to-end, then re-root and run git inside it.
            let clone = "git clone --branch master --single-branch \
                 https://github.com/octocat/Hello-World.git /home/user/hello";
            let r = base
                .exec(&["sh", "-c", clone], Duration::from_secs(120))
                .await?;
            assert_eq!(r.exit_code, 0, "clone failed: {}", r.stderr);
            let rooted = base.with_root(Path::new("/home/user/hello"));
            let head = rooted
                .exec(&["git", "rev-parse", "HEAD"], Duration::from_secs(30))
                .await?;
            assert_eq!(
                head.stdout.trim().len(),
                40,
                "unexpected HEAD: {}",
                head.stdout
            );

            // (4) `git worktree add` inside the clone — the variant-lane mechanism.
            let wt = rooted
                .exec(
                    &[
                        "git",
                        "-c",
                        "safe.directory=*",
                        "-C",
                        "/home/user/hello",
                        "worktree",
                        "add",
                        "-q",
                        "-b",
                        "axo/variant-0",
                        "/home/user/hello/.axo-variants/0",
                        "HEAD",
                    ],
                    Duration::from_secs(30),
                )
                .await?;
            assert_eq!(wt.exit_code, 0, "worktree add failed: {}", wt.stderr);
            Ok::<_, IsolationError>(())
        }
        .await;

        base.stop().await;
        run.expect("live git-native flow");
    }
}
