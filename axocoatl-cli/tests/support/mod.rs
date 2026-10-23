//! A real `axocoatl serve` child process for integration tests.
#![allow(dead_code)]

use std::fs::File;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

pub const STARTUP_TIMEOUT: Duration = Duration::from_secs(120);

pub fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

pub fn axocoatl(root: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_axocoatl"));
    // A relative socket path keeps the Unix socket under SUN_LEN even when
    // the temporary directory has a long path.
    command
        .current_dir(root)
        .env("AXOCOATL_DATA_DIR", root.join("data"))
        .env("AXOCOATL_SOCKET_PATH", "ipc/axocoatl.sock");
    command
}

pub struct Daemon {
    pub child: Child,
    root: PathBuf,
    launches: usize,
}

impl Daemon {
    pub async fn start(root: &Path, config: &Path, port: u16, launches: usize) -> Self {
        Self::start_command(axocoatl(root), root, config, port, launches).await
    }

    /// [`Daemon::start`] for a test that needs no Podman: the daemon probes
    /// Podman's default connection, whatever CONTAINER_HOST or
    /// CONTAINER_CONNECTION the developer's shell sets. It starts whether or
    /// not Podman answers there, but refuses to start when a connection
    /// named in those variables does not answer.
    pub async fn start_without_podman(
        root: &Path,
        config: &Path,
        port: u16,
        launches: usize,
    ) -> Self {
        let mut command = axocoatl(root);
        command
            .env_remove("CONTAINER_HOST")
            .env_remove("CONTAINER_CONNECTION");
        Self::start_command(command, root, config, port, launches).await
    }

    async fn start_command(
        mut command: Command,
        root: &Path,
        config: &Path,
        port: u16,
        launches: usize,
    ) -> Self {
        let ipc = root.join("ipc");
        std::fs::create_dir_all(&ipc).unwrap();
        std::fs::set_permissions(&ipc, std::fs::Permissions::from_mode(0o700)).unwrap();
        let stdout = File::create(root.join(format!("stdout-{launches}.log"))).unwrap();
        let stderr = File::create(root.join(format!("stderr-{launches}.log"))).unwrap();
        let child = command
            .args(["serve", "--config"])
            .arg(config)
            .env("RUST_LOG", "info")
            .stdin(Stdio::null())
            .stdout(stdout)
            .stderr(stderr)
            .spawn()
            .unwrap();
        let mut daemon = Self {
            child,
            root: root.to_path_buf(),
            launches,
        };
        daemon.wait_for_health(port).await;
        daemon
    }

    async fn wait_for_health(&mut self, port: u16) {
        let client = reqwest::Client::new();
        let deadline = Instant::now() + STARTUP_TIMEOUT;
        while Instant::now() < deadline {
            if let Some(status) = self.child.try_wait().unwrap() {
                panic!("axocoatl serve exited with {status}\n{}", self.logs());
            }
            if let Ok(response) = client
                .get(format!("http://127.0.0.1:{port}/health/live"))
                .send()
                .await
            {
                if response.status().is_success() {
                    return;
                }
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        panic!("axocoatl serve did not become healthy\n{}", self.logs());
    }

    pub fn logs(&self) -> String {
        (0..=self.launches)
            .flat_map(|launch| {
                ["stdout", "stderr"].map(|stream| {
                    std::fs::read_to_string(self.root.join(format!("{stream}-{launch}.log")))
                        .unwrap_or_default()
                })
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    pub fn stop(&mut self) {
        if self.child.try_wait().unwrap().is_some() {
            return;
        }
        let _ = Command::new("kill")
            .args(["-TERM", &self.child.id().to_string()])
            .status();
        let deadline = Instant::now() + Duration::from_secs(30);
        while Instant::now() < deadline {
            if self.child.try_wait().unwrap().is_some() {
                return;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        self.stop();
    }
}

pub fn client(port: u16) -> reqwest::Client {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .resolve(
            "localhost",
            std::net::SocketAddr::from(([127, 0, 0, 1], port)),
        )
        .build()
        .unwrap()
}

/// Answer native Ollama's contract probes on `server` the way Ollama 0.40.6
/// does, before any other `/api/chat` mock: the over-long prompt is refused
/// with HTTP 400, the one-token request stops after one token.
pub async fn mount_contract_probes(server: &wiremock::MockServer) {
    struct Probe;
    fn kind(body: &[u8]) -> Option<bool> {
        let value: serde_json::Value = serde_json::from_slice(body).ok()?;
        if value.pointer("/options/num_predict") != Some(&serde_json::json!(1))
            || value.get("truncate") != Some(&serde_json::json!(false))
        {
            return None;
        }
        let content = value.pointer("/messages/0/content")?.as_str()?;
        if content == axocoatl_llm_ollama::CONTRACT_STOP_PROBE_PROMPT {
            Some(false)
        } else {
            content.starts_with("x x ").then_some(true)
        }
    }
    impl wiremock::Match for Probe {
        fn matches(&self, request: &wiremock::Request) -> bool {
            request.url.path() == "/api/chat" && kind(&request.body).is_some()
        }
    }
    wiremock::Mock::given(wiremock::matchers::method("POST"))
        .and(Probe)
        .respond_with(|request: &wiremock::Request| {
            let body: serde_json::Value = serde_json::from_slice(&request.body).unwrap_or_default();
            if kind(&request.body) == Some(true) {
                return wiremock::ResponseTemplate::new(400).set_body_json(serde_json::json!({
                    "error": "request exceeds the available context size, try increasing it"
                }));
            }
            let record = serde_json::json!({"model": body["model"],
                "created_at": "2026-10-19T00:00:00Z",
                "message": {"role": "assistant", "content": "ready"}, "done": true,
                "done_reason": "length", "prompt_eval_count": 14, "eval_count": 1});
            wiremock::ResponseTemplate::new(200)
                .set_body_raw(format!("{record}\n"), "application/x-ndjson")
        })
        .with_priority(1)
        .mount(server)
        .await;
}
