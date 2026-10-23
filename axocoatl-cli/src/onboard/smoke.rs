//! Step 7: a first fix on a sample repository, end to end.
//!
//! The sample is a small Node repository with one failing `node --test`
//! (paging drops the last item), checked in as
//! `tests/fixtures/newuser/node-plain` and embedded here. It is copied to a
//! temporary directory; `axocoatl run` fixes it against the running daemon
//! (or one started for the run and stopped after), writing a receipt beside
//! the copy; `axocoatl receipt verify` checks the receipt against this
//! computer's key and the repository's files.

use std::path::Path;
use std::time::Duration;

use super::host::Cmd;
use super::{Choice, Onboarding, WriterChoice};

/// The task of the first run.
pub(crate) const TASK: &str = "Fix the bug: paging skips the last item";

/// The sample repository's files.
pub(crate) const FILES: &[(&str, &str)] = &[
    (
        "package.json",
        include_str!("../../assets/first-run/node-plain/package.json"),
    ),
    (
        "src/paging.js",
        include_str!("../../assets/first-run/node-plain/src/paging.js"),
    ),
    (
        "test/paging.test.js",
        include_str!("../../assets/first-run/node-plain/test/paging.test.js"),
    ),
    (
        "README.md",
        include_str!("../../assets/first-run/node-plain/README.md"),
    ),
    (
        ".gitignore",
        include_str!("../../assets/first-run/node-plain/.gitignore"),
    ),
];

/// Write the sample repository into `directory`.
pub(crate) fn write_sample(directory: &Path) -> std::io::Result<()> {
    for (name, text) in FILES {
        let path = directory.join(name);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, text)?;
    }
    commit_sample(directory)
}

/// Make the sample a Git repository with one commit, as a person's own
/// repository is: a run captures its change against `HEAD`, and without a
/// commit the checks around the change establish nothing. The commit names
/// a sample identity, so it needs no Git identity of the person's.
fn commit_sample(directory: &Path) -> std::io::Result<()> {
    let git = |args: &[&str]| -> std::io::Result<()> {
        let output = std::process::Command::new("git")
            .args([
                "-c",
                "init.defaultBranch=main",
                "-c",
                "user.name=Axocoatl sample",
                "-c",
                "user.email=sample@axocoatl.invalid",
                "-c",
                "commit.gpgSign=false",
            ])
            .args(args)
            .current_dir(directory)
            .output()?;
        if output.status.success() {
            Ok(())
        } else {
            Err(std::io::Error::other(format!(
                "git {} failed: {}",
                args.join(" "),
                String::from_utf8_lossy(&output.stderr).trim()
            )))
        }
    };
    git(&["init", "-q"])?;
    git(&["add", "-A"])?;
    git(&["commit", "-q", "-m", "Sample repository for the first run"])
}

/// The `axocoatl run` arguments of the first run: the models named
/// explicitly, so the run does not depend on how the daemon reads defaults.
pub(crate) fn run_args(
    writer: WriterChoice,
    choice: &Choice,
    repo: &Path,
    receipt: &Path,
    record: &Path,
    config: &Path,
) -> Vec<String> {
    let mut args = vec![
        "run".to_string(),
        writer.loadout().to_string(),
        "--task".to_string(),
        TASK.to_string(),
        "--repo".to_string(),
        repo.display().to_string(),
        "--receipt".to_string(),
        receipt.display().to_string(),
        "--record".to_string(),
        record.display().to_string(),
        "--config".to_string(),
        config.display().to_string(),
    ];
    let writer_model = match writer {
        WriterChoice::Ollama | WriterChoice::Openrouter => {
            choice.writer_model.as_ref().map(|model| model.spec())
        }
        WriterChoice::ClaudeCode | WriterChoice::Codex => choice.external_writer_model.clone(),
    };
    if let Some(model) = writer_model {
        args.extend(["--model".to_string(), format!("writer={model}")]);
    }
    if let Some(reviewer) = &choice.reviewer_model {
        args.extend([
            "--model".to_string(),
            format!("reviewer={}", reviewer.spec()),
        ]);
    }
    if let Some(credential) = choice.codex_credential {
        args.extend(["--param".to_string(), format!("credential={credential}")]);
    }
    args
}

/// Arguments as a person would type them (quoted where the shell needs it).
fn shown(args: &[String]) -> String {
    args.iter()
        .map(|arg| super::host::shell_quote(arg))
        .collect::<Vec<_>>()
        .join(" ")
}

/// `GET /health` of the daemon on `host:port` (public, no token).
pub(crate) fn health_url(host: &str, port: u16) -> String {
    format!(
        "{}/health",
        crate::plain_server_url(host, port).trim_end_matches('/')
    )
}

async fn healthy(url: &str) -> bool {
    let Ok(client) = reqwest::Client::builder()
        .timeout(Duration::from_secs(3))
        .build()
    else {
        return false;
    };
    client
        .get(url)
        .send()
        .await
        .is_ok_and(|response| response.status().is_success())
}

/// A daemon started for the first run, stopped when dropped.
struct TemporaryDaemon(std::process::Child);

impl Drop for TemporaryDaemon {
    fn drop(&mut self) {
        #[cfg(unix)]
        {
            // SAFETY: kill with this child's pid and SIGTERM, which `serve`
            // answers with its graceful shutdown.
            unsafe {
                libc::kill(self.0.id() as libc::pid_t, libc::SIGTERM);
            }
            let deadline = std::time::Instant::now() + Duration::from_secs(60);
            while std::time::Instant::now() < deadline {
                if matches!(self.0.try_wait(), Ok(Some(_))) {
                    return;
                }
                std::thread::sleep(Duration::from_millis(250));
            }
        }
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// A new directory for the first run in the temporary directory: an
/// unpredictable name, created (never reused) owner-only, so nothing another
/// user placed in a shared `/tmp` is written through.
pub(crate) fn first_run_directory(parent: &Path) -> std::io::Result<std::path::PathBuf> {
    let base = parent.join(format!(
        "axocoatl-first-run-{}",
        uuid::Uuid::new_v4().simple()
    ));
    let mut builder = std::fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(&base)?;
    Ok(base)
}

/// How long a daemon that is starting (the service just started, or one
/// already running) may take to answer: its first start downloads an
/// embedding model.
const DAEMON_START_WAIT: Duration = Duration::from_secs(300);
/// How long a daemon that was already running may take to answer on the
/// configured port before onboarding says it runs another configuration.
const RUNNING_DAEMON_WAIT: Duration = Duration::from_secs(if cfg!(test) { 1 } else { 15 });

/// Wait until `url` answers, at most `wait`.
async fn wait_healthy(url: &str, wait: Duration) -> bool {
    let deadline = tokio::time::Instant::now() + wait;
    loop {
        if healthy(url).await {
            return true;
        }
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

/// When the Axocoatl daemon answering at `url` started (Unix milliseconds):
/// `None` when no Axocoatl `/health` answers there (another program's does
/// not name a version), `Some(None)` for a daemon before 1.5.0, which does
/// not say.
async fn started_at(url: &str) -> Option<Option<u64>> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(3))
        .build()
        .ok()?;
    let response = client.get(url).send().await.ok()?;
    if !response.status().is_success() {
        return None;
    }
    let health: serde_json::Value = response.json().await.ok()?;
    health.get("version")?.as_str()?;
    Some(
        health
            .get("started_at_ms")
            .and_then(serde_json::Value::as_u64),
    )
}

/// Whether `config` last changed after `started` (Unix milliseconds).
fn changed_since(config: &Path, started: u64) -> bool {
    std::fs::metadata(config)
        .and_then(|metadata| metadata.modified())
        .ok()
        .and_then(|modified| modified.duration_since(std::time::UNIX_EPOCH).ok())
        .is_some_and(|modified| modified.as_millis() > u128::from(started))
}

/// Whether the Axocoatl daemon answering at `url` may run an older
/// configuration than `config`: it started before the file last changed, or
/// it does not say when it started (before 1.5.0, older than this CLI).
async fn started_before(url: &str, config: &Path) -> bool {
    match started_at(url).await {
        None => false,
        Some(None) => true,
        Some(Some(started)) => changed_since(config, started),
    }
}

/// Whether a daemon that answers at `url` after a restart runs `config` as
/// it is now: it started after the file last changed, or it does not say (an
/// Axocoatl before 1.5.0, which read the file when it restarted).
async fn restarted_current(url: &str, config: &Path) -> bool {
    match started_at(url).await {
        None => false,
        Some(None) => true,
        Some(Some(started)) => !changed_since(config, started),
    }
}

/// Whether `axocoatl service status` says the Always-On Service runs.
async fn service_running(o: &Onboarding) -> bool {
    if crate::doctor::installed_service_program(&o.host.home, o.host.macos).is_none() {
        return false;
    }
    let exe = o.host.exe.display().to_string();
    o.host
        .probe(&exe, &["service", "status"])
        .await
        .is_some_and(|status| {
            status.success
                && status.stdout.lines().any(|line| {
                    line.trim_start()
                        .strip_prefix("running:")
                        .is_some_and(|value| value.trim() == "yes")
                })
        })
}

/// The process of the daemon on the IPC socket `socket`, from the socket's
/// peer credentials (none when the daemon is in a PID namespace this
/// process cannot see).
async fn daemon_pid(socket: &Path) -> Option<i32> {
    tokio::net::UnixStream::connect(socket)
        .await
        .ok()?
        .peer_cred()
        .ok()?
        .pid()
        .filter(|pid| *pid > 0)
}

/// Whether process `pid` still runs (a zombie on Linux does not).
fn process_runs(pid: i32) -> bool {
    // SAFETY: signal 0 only checks that the process exists.
    let exists = unsafe { libc::kill(pid, 0) } == 0
        || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM);
    exists
        && !std::fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|stat| {
            stat.rsplit_once(')')
                .is_some_and(|(_, rest)| rest.trim_start().starts_with('Z'))
        })
}

/// Wait until process `pid` (when known) has exited, at most `wait`.
async fn wait_exit(pid: Option<i32>, wait: Duration) -> bool {
    let deadline = tokio::time::Instant::now() + wait;
    while pid.is_some_and(process_runs) {
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    true
}

/// How long a stopped daemon may take to end its Sessions and exit (each
/// Session's containers are removed through Podman).
const DAEMON_STOP_WAIT: Duration = Duration::from_secs(if cfg!(test) { 2 } else { 120 });

/// Replace the daemon on `port` that runs an older configuration, once the
/// person agrees: restart the Always-On Service when it runs, or stop the
/// other daemon (`axocoatl dev` in another terminal) and wait for its
/// process to exit, so that the service Step 6 just started takes the port
/// (`service_started`) or the first run starts its own daemon. `start_dev`:
/// onboarding starts `axocoatl dev` itself when it finishes. `false` with the
/// step left when that did not happen.
async fn replace_stale_daemon(
    o: &mut Onboarding,
    url: &str,
    port: u16,
    config: &Path,
    service_started: bool,
    start_dev: bool,
) -> bool {
    if service_running(o).await {
        if !o.ui.consent(
            &format!(
                "The Always-On Service started before {} changed, so the first run would use the \
                 old configuration. Restart it now (this ends any Session turn or run in \
                 progress)?",
                config.display()
            ),
            true,
            false,
        ) {
            o.step_left(
                "The Always-On Service runs the configuration from before this onboarding: \
                 restart it (`axocoatl service stop && axocoatl service start`), then run \
                 `axocoatl onboard --update --smoke-test`",
            );
            return false;
        }
        let pid = daemon_pid(&o.socket_path).await;
        let exe = o.host.exe.display().to_string();
        let stopped = o.host.run(&Cmd::new(&exe, &["service", "stop"])).await;
        // It ends its Sessions before it exits; start it again only then.
        wait_exit(pid, DAEMON_STOP_WAIT).await;
        if let Err(error) = stopped {
            if service_running(o).await {
                o.step_left(format!("{error}; see `axocoatl service status`"));
                return false;
            }
        }
        if let Err(error) = o.host.run(&Cmd::new(&exe, &["service", "start"])).await {
            o.step_left(format!("{error}; see `axocoatl service status`"));
            return false;
        }
        if wait_healthy(url, DAEMON_START_WAIT).await && restarted_current(url, config).await {
            return true;
        }
        o.step_left(format!(
            "The Always-On Service did not answer at {url} with the new configuration: see \
             `axocoatl service status`, then run `axocoatl onboard --update --smoke-test` again"
        ));
        return false;
    }
    let after = if service_started {
        "the Always-On Service you just started takes the port"
    } else if start_dev {
        "onboarding starts `axocoatl dev` when it finishes"
    } else {
        "start `axocoatl dev` again afterwards"
    };
    if !o.ui.consent(
        &format!(
            "The daemon on port {port} started before {} changed, so the first run would use the \
             old configuration. Stop it now (this ends any Session turn or run in progress; \
             {after})?",
            config.display()
        ),
        true,
        false,
    ) {
        o.step_left(format!(
            "The daemon on port {port} runs the configuration from before this onboarding: \
             restart it (stop `axocoatl dev` and start it again), then run `axocoatl onboard \
             --update --smoke-test`"
        ));
        return false;
    }
    let pid = daemon_pid(&o.socket_path).await;
    let asked = match axocoatl_daemon::ipc::IpcClient::connect(&o.socket_path).await {
        Ok(mut client) => client
            .request(&axocoatl_daemon::ipc::IpcRequest::Shutdown)
            .await
            .is_ok(),
        Err(_) => false,
    };
    if asked {
        o.ui.say(format!(
            "  Stopping the daemon on port {port} (it ends its Sessions first)…"
        ));
        let deadline = tokio::time::Instant::now() + DAEMON_STOP_WAIT;
        loop {
            // The process holds the data directory until it exits.
            if !pid.is_some_and(process_runs) {
                let ipc = axocoatl_daemon::ipc::IpcClient::connect(&o.socket_path)
                    .await
                    .is_ok();
                let stopped = match started_at(url).await {
                    None => !ipc && super::host::port_free(port),
                    // Another daemon took the port (the service, restarted
                    // by its manager); a known process has exited, so one
                    // that does not say when it started is new too.
                    Some(Some(started)) => !changed_since(config, started),
                    Some(None) => pid.is_some(),
                };
                if stopped {
                    break;
                }
            }
            if tokio::time::Instant::now() >= deadline {
                o.step_left(format!(
                    "Could not stop the daemon on port {port}, which runs the configuration from \
                     before this onboarding: stop `axocoatl dev` (or `axocoatl service stop`), \
                     then run `axocoatl onboard --update --smoke-test`"
                ));
                return false;
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        if service_started
            && !(wait_healthy(url, DAEMON_START_WAIT).await && restarted_current(url, config).await)
        {
            o.step_left(format!(
                "The Always-On Service did not answer at {url} after the older daemon stopped: \
                 see `axocoatl service status`, then run `axocoatl onboard --update \
                 --smoke-test` again"
            ));
            return false;
        }
        o.ui.ok(format!("Stopped the daemon on port {port}; {after}"));
        return true;
    }
    o.step_left(format!(
        "Could not stop the daemon on port {port}, which runs the configuration from before this \
         onboarding: stop `axocoatl dev` (or `axocoatl service stop`), then run `axocoatl onboard \
         --update --smoke-test`"
    ));
    false
}

/// Step 7; `true` when the run passed and its receipt verified.
/// `service_started`: Step 6 just started the Always-On Service, which
/// this run then uses (never a second daemon beside it). `start_dev`:
/// onboarding starts `axocoatl dev` when it finishes.
pub(crate) async fn run(
    o: &mut Onboarding,
    writer: WriterChoice,
    choice: &Choice,
    service_started: bool,
    start_dev: bool,
) -> bool {
    if o.ui.plan() {
        let args = run_args(
            writer,
            choice,
            Path::new("<temporary directory>/repo"),
            Path::new("<temporary directory>/receipt.json"),
            Path::new("<temporary directory>/record.jsonl"),
            &o.paths.config_path,
        );
        o.ui.say(format!(
            "    would copy the sample repository to a temporary directory and run: axocoatl {}",
            shown(&args)
        ));
        o.ui.say("    would then run: axocoatl receipt verify <receipt> --key \"$(axocoatl receipt key)\" --repo <repository>");
        return true;
    }
    // Tests keep it in their own temporary home.
    let parent = if cfg!(test) {
        o.host.home.clone()
    } else {
        std::env::temp_dir()
    };
    let base = match first_run_directory(&parent) {
        Ok(base) => base,
        Err(error) => {
            o.step_left(format!(
                "Could not create a directory for the first run: {error}"
            ));
            return false;
        }
    };
    let repo = base.join("repo");
    let receipt = base.join("receipt.json");
    let record = base.join("record.jsonl");
    let config = o.paths.config_path.clone();
    let args = run_args(writer, choice, &repo, &receipt, &record, &config);
    if let Err(error) = write_sample(&repo) {
        o.step_left(format!("Could not write the sample repository: {error}"));
        return false;
    }
    o.ui.say(format!("  Sample repository: {}", repo.display()));

    let (host, port) = std::fs::read_to_string(&config)
        .ok()
        .and_then(|text| axocoatl_config::parse_config(&text, &config).ok())
        .map(|config| (config.server.host.clone(), config.server.port))
        .unwrap_or_else(|| ("127.0.0.1".to_string(), 8080));
    let url = health_url(&host, port);
    let mut answers = healthy(&url).await;
    if !answers
        && (service_started
            || axocoatl_daemon::ipc::IpcClient::connect(&o.socket_path)
                .await
                .is_ok())
    {
        // A daemon is starting or runs already: a second one would contend
        // for its port and its IPC socket. Wait for it instead.
        let wait = if service_started {
            o.ui.say(format!(
                "  Waiting for the Always-On Service to answer on port {port} (its first start \
                 downloads an 87 MB embedding model once)…"
            ));
            DAEMON_START_WAIT
        } else {
            RUNNING_DAEMON_WAIT
        };
        if !wait_healthy(&url, wait).await {
            o.step_left(if service_started {
                format!(
                    "The Always-On Service did not answer at {url}: see `axocoatl service \
                     status`, then run `axocoatl onboard --update --smoke-test` again"
                )
            } else {
                format!(
                    "An Axocoatl daemon is running but does not answer at {url}, the port this \
                     configuration names: restart it so it reads {} (`axocoatl service stop && \
                     axocoatl service start`, or stop `axocoatl dev` and start it again)",
                    config.display()
                )
            });
            return false;
        }
        answers = true;
    }
    if answers && started_before(&url, &config).await {
        if !replace_stale_daemon(o, &url, port, &config, service_started, start_dev).await {
            return false;
        }
        answers = healthy(&url).await;
    }
    let _daemon = if answers {
        o.ui.ok(format!("Using the daemon running on port {port}"));
        None
    } else if !super::host::port_free(port) {
        o.step_left(format!(
            "Port {port} is in use by another program, so no daemon can start for the first run: \
             stop it, or set server.port in {}",
            config.display()
        ));
        return false;
    } else {
        o.ui.say(format!(
            "  Starting a daemon for this run (axocoatl serve on port {port}; its first start \
             downloads an 87 MB embedding model once)…"
        ));
        let log = match std::fs::File::create(base.join("daemon.log")) {
            Ok(log) => log,
            Err(error) => {
                o.step_left(format!("Could not start a daemon: {error}"));
                return false;
            }
        };
        let child = std::process::Command::new(&o.host.exe)
            .args(["serve", "--config"])
            .arg(&config)
            .stdin(std::process::Stdio::null())
            .stdout(
                log.try_clone()
                    .map(std::process::Stdio::from)
                    .unwrap_or_else(|_| std::process::Stdio::null()),
            )
            .stderr(std::process::Stdio::from(log))
            .spawn();
        let daemon = match child {
            Ok(child) => TemporaryDaemon(child),
            Err(error) => {
                o.step_left(format!("Could not start `axocoatl serve`: {error}"));
                return false;
            }
        };
        let deadline = tokio::time::Instant::now() + DAEMON_START_WAIT;
        let mut daemon = daemon;
        loop {
            if healthy(&url).await {
                break;
            }
            if let Ok(Some(status)) = daemon.0.try_wait() {
                o.step_left(format!(
                    "The daemon stopped ({status}) before the first run; see {}",
                    base.join("daemon.log").display()
                ));
                return false;
            }
            if tokio::time::Instant::now() >= deadline {
                o.step_left(format!(
                    "The daemon did not answer at {url}; see {}",
                    base.join("daemon.log").display()
                ));
                return false;
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
        Some(daemon)
    };

    let exe = o.host.exe.display().to_string();
    let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
    o.ui.say(format!("  $ axocoatl {}", shown(&args)));
    // A SIGTERM or SIGHUP to onboarding stops the daemon it started, too
    // (Ctrl+C reaches both through the terminal).
    let run_cmd = Cmd::new(&exe, &arg_refs);
    // The run is killed with onboarding's future if a signal ends it.
    let run = o.host.status(&run_cmd);
    let code = tokio::select! {
        code = run => match code {
            Ok(code) => code,
            Err(error) => {
                o.step_left(error);
                return false;
            }
        },
        signal = stop_signal() => {
            o.step_left(format!("The first run was stopped ({signal})"));
            return false;
        }
    };
    if code != Some(0) {
        o.step_left(format!(
            "The first run ended with exit {}: see its output above, the record {} and `axocoatl \
             doctor`",
            code.map(|code| code.to_string())
                .unwrap_or_else(|| "by signal".to_string()),
            record.display()
        ));
        return false;
    }
    let config_text = config.display().to_string();
    let key = o
        .host
        .probe(&exe, &["receipt", "key", "--config", &config_text])
        .await
        .filter(|probe| probe.success)
        .map(|probe| probe.stdout.trim().to_string());
    let Some(key) = key.filter(|key| !key.is_empty()) else {
        o.step_left("Could not read this computer's receipt key: run `axocoatl receipt key`");
        return false;
    };
    let receipt_text = receipt.display().to_string();
    let repo_text = repo.display().to_string();
    let verify = Cmd::new(
        &exe,
        &[
            "receipt",
            "verify",
            &receipt_text,
            "--key",
            &key,
            "--repo",
            &repo_text,
        ],
    );
    o.ui.say(format!(
        "  $ axocoatl receipt verify {receipt_text} --key \"$(axocoatl receipt key)\" --repo {repo_text}"
    ));
    match o.host.status(&verify).await {
        Ok(Some(0)) => {
            o.ui.ok(format!(
                "First run passed (exit 0), its review passed, and its receipt verifies: {}",
                receipt.display()
            ));
            true
        }
        Ok(code) => {
            o.step_left(format!(
                "The first run's receipt did not verify (exit {}): {}",
                code.map(|code| code.to_string())
                    .unwrap_or_else(|| "by signal".to_string()),
                receipt.display()
            ));
            false
        }
        Err(error) => {
            o.step_left(error);
            false
        }
    }
}

/// SIGTERM or SIGHUP, by name, when one arrives.
async fn stop_signal() -> &'static str {
    use tokio::signal::unix::{signal, SignalKind};
    match (
        signal(SignalKind::terminate()),
        signal(SignalKind::hangup()),
    ) {
        (Ok(mut terminate), Ok(mut hangup)) => tokio::select! {
            _ = terminate.recv() => "SIGTERM",
            _ = hangup.recv() => "SIGHUP",
        },
        _ => std::future::pending().await,
    }
}

/// The checked-in copy the release gate also runs, when this is a checkout.
#[cfg(test)]
pub(crate) fn checked_in_copy() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../tests/fixtures/newuser/node-plain")
}

#[cfg(test)]
mod tests {
    use super::super::config::{ModelRef, Provider};
    use super::*;

    /// The embedded sample is the checked-in fixture, file for file.
    #[test]
    fn the_embedded_sample_is_the_checked_in_fixture() {
        let checked_in = checked_in_copy();
        if !checked_in.is_dir() {
            // A packaged crate has only its own copy.
            return;
        }
        let mut on_disk = Vec::new();
        for entry in walk(&checked_in) {
            on_disk.push(
                entry
                    .strip_prefix(&checked_in)
                    .unwrap()
                    .display()
                    .to_string(),
            );
        }
        on_disk.sort();
        let mut embedded: Vec<String> = FILES.iter().map(|(name, _)| name.to_string()).collect();
        embedded.sort();
        assert_eq!(on_disk, embedded);
        for (name, text) in FILES {
            assert_eq!(
                std::fs::read_to_string(checked_in.join(name)).unwrap(),
                *text,
                "{name}"
            );
        }
    }

    fn walk(directory: &Path) -> Vec<std::path::PathBuf> {
        let mut files = Vec::new();
        for entry in std::fs::read_dir(directory).unwrap().flatten() {
            let path = entry.path();
            if path.is_dir() {
                files.extend(walk(&path));
            } else {
                files.push(path);
            }
        }
        files
    }

    #[test]
    fn the_sample_has_one_failing_node_test() {
        let directory = tempfile::tempdir().unwrap();
        write_sample(directory.path()).unwrap();
        let package: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(directory.path().join("package.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(package["scripts"]["test"], "node --test");
        let paging = std::fs::read_to_string(directory.path().join("src/paging.js")).unwrap();
        assert!(paging.contains("Math.floor(items.length / size)"));
        // A committed Git repository with nothing left over, as a run needs.
        let status = std::process::Command::new("git")
            .args(["status", "--porcelain"])
            .current_dir(directory.path())
            .output()
            .unwrap();
        assert!(status.status.success());
        assert_eq!(String::from_utf8_lossy(&status.stdout), "");
        let head = std::process::Command::new("git")
            .args(["rev-parse", "--verify", "HEAD"])
            .current_dir(directory.path())
            .output()
            .unwrap();
        assert!(head.status.success());
        if let Ok(output) = std::process::Command::new("node")
            .args(["--test", "--test-reporter=tap"])
            .current_dir(directory.path())
            .output()
        {
            let printed = String::from_utf8_lossy(&output.stdout);
            assert!(!output.status.success(), "{printed}");
            assert!(
                printed.contains("# pass 2") && printed.contains("# fail 1"),
                "{printed}"
            );
        }
    }

    /// The first run's directory has an unpredictable name and is created
    /// new and owner-only: a path someone else made (or a link) is never
    /// written through.
    #[cfg(unix)]
    #[test]
    fn the_first_run_directory_is_new_unpredictable_and_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let parent = tempfile::tempdir().unwrap();
        let one = first_run_directory(parent.path()).unwrap();
        let two = first_run_directory(parent.path()).unwrap();
        assert_ne!(one, two);
        assert!(one
            .file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with("axocoatl-first-run-"));
        assert_eq!(
            std::fs::metadata(&one).unwrap().permissions().mode() & 0o777,
            0o700
        );
        // Creating over an existing path fails instead of reusing it.
        let mut builder = std::fs::DirBuilder::new();
        assert!(builder.recursive(false).create(&one).is_err());
    }

    #[test]
    fn the_health_url_has_one_slash() {
        assert_eq!(
            health_url("127.0.0.1", 8247),
            "http://127.0.0.1:8247/health"
        );
        assert_eq!(health_url("::1", 8247), "http://[::1]:8247/health");
    }

    #[test]
    fn the_first_run_names_its_models_and_codex_credential() {
        let repo = Path::new("/tmp/f/repo");
        let receipt = Path::new("/tmp/f/receipt.json");
        let record = Path::new("/tmp/f/record.jsonl");
        let config = Path::new("/c/config.yaml");
        let local = Choice {
            writer_model: Some(ModelRef::new(Provider::Ollama, "qwen3-coder:30b")),
            reviewer_model: Some(ModelRef::new(Provider::Ollama, "gpt-oss:20b")),
            ..Choice::default()
        };
        let args = run_args(WriterChoice::Ollama, &local, repo, receipt, record, config);
        assert!(shown(&args)
            .starts_with("run fix --task 'Fix the bug: paging skips the last item' --repo"));
        assert_eq!(
            args.join(" "),
            "run fix --task Fix the bug: paging skips the last item --repo /tmp/f/repo --receipt \
             /tmp/f/receipt.json --record /tmp/f/record.jsonl --config /c/config.yaml --model \
             writer=ollama:qwen3-coder:30b --model reviewer=ollama:gpt-oss:20b"
        );
        let codex = Choice {
            reviewer_model: Some(ModelRef::new(Provider::OpenRouter, "openai/gpt-oss-120b")),
            codex_credential: Some("codex-chatgpt"),
            ..Choice::default()
        };
        let args = run_args(WriterChoice::Codex, &codex, repo, receipt, record, config);
        assert_eq!(args[1], "fix-codex");
        assert!(args.ends_with(&[
            "--model".to_string(),
            "reviewer=openrouter:openai/gpt-oss-120b".to_string(),
            "--param".to_string(),
            "credential=codex-chatgpt".to_string(),
        ]));
        assert!(!args.iter().any(|arg| arg.starts_with("writer=")));
    }
}
