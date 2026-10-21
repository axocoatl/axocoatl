//! Onboarding end to end with shim programs (podman, brew, ollama, sudo,
//! systemctl, axocoatl) that log their arguments and change nothing, a fake
//! Ollama server whose cloud mode and context follow the files onboarding
//! writes, and temporary homes. Nothing on the computer running the tests
//! is read or changed: no real Podman, Homebrew, Ollama, service or port
//! 8080.

use std::path::{Path, PathBuf};

use axocoatl_isolation::podman::{ConnectionOverride, PodmanReadiness};
use axocoatl_isolation::podman_report::PodmanReport;
use serde_json::{json, Value};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::config::RunModels;
use super::detect::{Facts, OllamaServer, OllamaService, Os, OsRelease};
use super::host::testing::Shims;
use super::ui::{Answer, Mode, Ui};
use super::*;

const GIB: u64 = 1 << 30;

/// `podman info --format json` of a ready rootless runtime.
const INFO: &str = r#"{"host":{"security":{"rootless":true,"selinuxEnabled":false},
"kernel":"6.8.0","idMappings":{"uidmap":[{"container_id":0,"host_id":1000,"size":1},
{"container_id":1,"host_id":100000,"size":65536}],"gidmap":[{"container_id":0,"host_id":1000,"size":1},
{"container_id":1,"host_id":100000,"size":65536}]}},"version":{"Version":"5.8.2"}}"#;

/// A shim Podman whose state lives in files beside it: `machine` (exists),
/// `running`, `rootful`, `installed`. Read-only commands answer from the
/// state; `machine init`, `start`, `stop` and `set` change it.
fn podman_shim(shims: &Shims) {
    podman_shim_named(shims, "podman-machine-default");
}

/// [`podman_shim`] whose one machine is called `name`: the commands that
/// change it work only when they name it (Podman acts on
/// `podman-machine-default` without a name).
fn podman_shim_named(shims: &Shims, name: &str) {
    let dir = shims.dir.path().display().to_string();
    let named = if name == "podman-machine-default" {
        String::new()
    } else {
        format!(" {name}")
    };
    shims.add(
        "podman",
        &format!(
            r#"D="{dir}"
info='{info}'
case "$*" in
  "--version") echo "podman version 5.8.2" ;;
  "info --format json")
    if [ -f "$D/machine" ] && [ -f "$D/running" ]; then
      if [ -f "$D/rootful" ]; then echo "$info" | sed 's/"rootless":true/"rootless":false/'; else echo "$info"; fi
    else echo "Cannot connect to Podman" >&2; exit 125; fi ;;
  "machine list --format json")
    if [ -f "$D/machine" ]; then
      r=false; [ -f "$D/running" ] && r=true
      echo "[{{\"Name\":\"{name}\",\"Default\":true,\"Running\":$r,\"VMType\":\"applehv\",\"CPUs\":6,\"Memory\":\"8589934592\"}}]"
    else echo "[]"; fi ;;
  "machine inspect {name}")
    rf=false; [ -f "$D/rootful" ] && rf=true
    echo "[{{\"Name\":\"{name}\",\"Rootful\":$rf,\"Resources\":{{\"CPUs\":6,\"Memory\":8192}}}}]" ;;
  "machine info --format json") echo '{{"Host":{{"VMType":"applehv"}}}}' ;;
  "system connection list --format json")
    if [ -f "$D/machine" ] || [ -f "$D/connection" ]; then echo '[{{"Name":"{name}"}}]'; else echo '[]'; fi ;;
  machine\ init*) touch "$D/machine" ;;
  "machine start{named}") touch "$D/running" ;;
  "machine stop{named}") rm -f "$D/running" ;;
  "machine set --rootful=false{named}") rm -f "$D/rootful" ;;
  pull*) echo "pulled" ;;
  run*) echo 1000 ;;
  "system migrate") ;;
  *) echo "podman shim: not simulated: $*" >&2; exit 125 ;;
esac"#,
            info = INFO.replace('\n', "")
        ),
    );
}

fn podman_state(shims: &Shims, files: &[&str]) {
    for file in files {
        std::fs::write(shims.dir.path().join(file), "").unwrap();
    }
}

/// What the shim Podman reports, with no `CONTAINER_HOST` or
/// `CONTAINER_CONNECTION`, whatever the environment running the tests sets.
async fn podman_report(shims: &Shims) -> PodmanReport {
    axocoatl_isolation::podman_report::inspect_through(
        &shims.dir.path().join("podman").display().to_string(),
        ConnectionOverride::None,
    )
    .await
}

fn no_podman() -> PodmanReport {
    PodmanReport::not_installed()
}

/// A fake Ollama 0.40.6 (or `version`): its cloud mode is off once the
/// home's `~/.ollama/server.json` turns it off or `no-cloud` exists beside
/// the shims (a systemd drop-in took effect); it loads every model with
/// 4096 tokens until `OLLAMA_CONTEXT_LENGTH` is in Homebrew's service
/// environment or `big-context` exists; `pulled` lists the models the
/// `ollama` shim pulled.
struct FakeOllama {
    server: MockServer,
}

const DIGEST: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

fn installed(state: &Path, base: &[&str]) -> Vec<String> {
    let mut models: Vec<String> = base.iter().map(|m| m.to_string()).collect();
    if let Ok(pulled) = std::fs::read_to_string(state.join("pulled")) {
        models.extend(pulled.lines().map(str::to_string));
    }
    models
}

impl FakeOllama {
    async fn start(version: &str, home: &Path, state: &Path, base: &[&str]) -> Self {
        let server = MockServer::start().await;
        let version = version.to_string();
        Mock::given(method("GET"))
            .and(path("/api/version"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "version": version })))
            .mount(&server)
            .await;
        let (home_a, state_a) = (home.to_path_buf(), state.to_path_buf());
        Mock::given(method("GET"))
            .and(path("/api/status"))
            .respond_with(move |_: &wiremock::Request| {
                let config = std::fs::read_to_string(home_a.join(".ollama/server.json"))
                    .ok()
                    .and_then(|text| serde_json::from_str::<Value>(&text).ok())
                    .and_then(|value| value["disable_ollama_cloud"].as_bool())
                    .unwrap_or(false);
                let disabled = config || state_a.join("no-cloud").exists();
                ResponseTemplate::new(200).set_body_json(json!({"cloud": {
                    "disabled": disabled,
                    "source": if disabled { "config" } else { "none" }
                }}))
            })
            .mount(&server)
            .await;
        let base: Vec<String> = base.iter().map(|m| m.to_string()).collect();
        let (state_b, base_b) = (state.to_path_buf(), base.clone());
        Mock::given(method("GET"))
            .and(path("/api/tags"))
            .respond_with(move |_: &wiremock::Request| {
                let refs: Vec<&str> = base_b.iter().map(String::as_str).collect();
                let models: Vec<Value> = installed(&state_b, &refs)
                    .iter()
                    .map(|name| {
                        json!({"name": name, "model": name, "size": 13_780_173_839u64,
                            "digest": DIGEST,
                            "details": {"format": "gguf", "family": "x", "parameter_size": "20.9B"}})
                    })
                    .collect();
                ResponseTemplate::new(200).set_body_json(json!({ "models": models }))
            })
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/show"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "details": {"format": "gguf"},
                "capabilities": ["completion", "tools", "thinking"]
            })))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/generate"))
            .respond_with(|request: &wiremock::Request| {
                let body: Value = serde_json::from_slice(&request.body).unwrap();
                ResponseTemplate::new(200).set_body_json(json!({
                    "model": body["model"], "created_at": "2026-10-19T00:00:00Z",
                    "response": "", "done": true, "done_reason": "load"
                }))
            })
            .mount(&server)
            .await;
        let (home_c, state_c, base_c) = (home.to_path_buf(), state.to_path_buf(), base);
        Mock::given(method("GET"))
            .and(path("/api/ps"))
            .respond_with(move |_: &wiremock::Request| {
                let big = state_c.join("big-context").exists()
                    || std::fs::read_to_string(home_c.join(".homebrew/services/ollama.env"))
                        .is_ok_and(|text| text.contains("OLLAMA_CONTEXT_LENGTH=32768"));
                let context = if big { 32768 } else { 4096 };
                let refs: Vec<&str> = base_c.iter().map(String::as_str).collect();
                let models: Vec<Value> = installed(&state_c, &refs)
                    .iter()
                    .map(|name| {
                        json!({"name": name, "model": name, "digest": DIGEST,
                            "details": {"format": "gguf"}, "context_length": context,
                            "runner": "llamacpp"})
                    })
                    .collect();
                ResponseTemplate::new(200).set_body_json(json!({ "models": models }))
            })
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/chat"))
            .respond_with(|request: &wiremock::Request| {
                let body: Value = serde_json::from_slice(&request.body).unwrap();
                let content = body["messages"][0]["content"].as_str().unwrap_or_default();
                if content.starts_with("x x ") {
                    ResponseTemplate::new(400).set_body_json(json!({"error":
                        "the prompt is longer than the context length"}))
                } else {
                    ResponseTemplate::new(200).set_body_raw(
                        format!(
                            "{}\n",
                            json!({"model": body["model"], "message": {"role": "assistant",
                                "content": "ready"}, "done": true, "done_reason": "length",
                                "prompt_eval_count": 14, "eval_count": 1})
                        ),
                        "application/x-ndjson",
                    )
                }
            })
            .mount(&server)
            .await;
        Self { server }
    }

    fn url(&self) -> String {
        self.server.uri()
    }
}

/// An `ollama` shim: `pull X` records X as installed; `--version` says
/// 0.40.6.
fn ollama_shim(shims: &Shims) {
    let pulled = shims.dir.path().join("pulled");
    shims.add(
        "ollama",
        &format!(
            r#"case "$1" in
  --version) echo "ollama version is 0.40.6" ;;
  pull) shift; [ "$1" = "--runner" ] && shift 2; echo "$1" >> "{}" ;;
esac"#,
            pulled.display()
        ),
    );
}

/// The brew service of Ollama, set up the way `brew services start ollama`
/// leaves it, serving the fake server's port (Homebrew's service
/// environment names it). Only the macOS tests use it.
#[cfg(target_os = "macos")]
fn brew_service_on(shims: &Shims, url: &str) {
    let agents = shims.home.path().join("Library/LaunchAgents");
    std::fs::create_dir_all(&agents).unwrap();
    std::fs::write(agents.join("sh.brew.ollama.plist"), "<plist/>").unwrap();
    let services = shims.home.path().join(".homebrew/services");
    std::fs::create_dir_all(&services).unwrap();
    std::fs::write(
        services.join("ollama.env"),
        format!("OLLAMA_HOST=127.0.0.1:{}\n", detect::url_port(url)),
    )
    .unwrap();
}

fn mac(version: &str) -> Os {
    Os::MacOs {
        version: Some(version.to_string()),
        apple_silicon: true,
        chip: Some("Apple M5 Pro".to_string()),
    }
}

fn ubuntu() -> Os {
    Os::Linux {
        release: OsRelease::parse("ID=ubuntu\nID_LIKE=debian\nVERSION_ID=\"24.04\"\n"),
        wsl: None,
        systemd: true,
    }
}

async fn facts(
    os: Os,
    memory_gb: u64,
    podman: PodmanReport,
    shims: &Shims,
    ollama_url: &str,
    service: OllamaService,
) -> Facts {
    Facts {
        os,
        memory_bytes: Some(memory_gb * GIB),
        cpus: 18,
        disk_free_bytes: Some(400_000_000_000),
        containers_free_bytes: None,
        root_user: false,
        user: "alex".to_string(),
        port_8080_free: true,
        git: Some("2.39.5".to_string()),
        brew: shims
            .dir
            .path()
            .join("brew")
            .exists()
            .then(|| "7.0.10".to_string()),
        podman_path: Some(shims.dir.path().join("podman")).filter(|path| path.exists()),
        podman,
        ollama_cli: shims
            .dir
            .path()
            .join("ollama")
            .exists()
            .then(|| detect::OllamaCli {
                path: shims.dir.path().join("ollama"),
                client: Some("0.40.6".into()),
                server: Some("0.40.6".into()),
            }),
        ollama_url: ollama_url.to_string(),
        ollama_server: OllamaServer::probe(ollama_url).await,
        ollama_service: service,
        claude: None,
        codex: None,
        gh: None,
        windows_ollama: false,
        daemon_running: false,
        daemon_version: None,
        service_program: None,
        xdg_config_home: None,
        landlock_abi: Some(6),
    }
}

/// A free port that is not 8080.
fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn onboarding(shims: &Shims, ui: Ui, facts: Facts, mut args: OnboardArgs) -> Onboarding {
    if args.port.is_none() {
        args.port = Some(free_port());
    }
    let home = shims.home.path();
    let macos = facts.os.is_macos();
    Onboarding {
        ui,
        host: shims.host(macos),
        facts,
        args,
        paths: UserPaths {
            config_dir: home.join("config"),
            config_path: home.join("config").join("config.yaml"),
            data_dir: home.join("data"),
        },
        steps: Vec::new(),
        openrouter_api: "http://127.0.0.1:9/never".to_string(),
        socket_path: home.join("no-daemon.sock"),
        data_dir_override: Some(home.join("data")),
        kept: Kept::default(),
        step_keys: Default::default(),
    }
}

fn config_text(o: &Onboarding) -> String {
    std::fs::read_to_string(&o.paths.config_path).unwrap()
}

fn yes(writer: WriterChoice) -> OnboardArgs {
    OnboardArgs {
        yes: true,
        writer: Some(writer),
        start: Some(StartChoice::None),
        ..OnboardArgs::default()
    }
}

/// The plan's Run 1 path on a Mac with nothing set up, answered with
/// `--yes`: Podman installed with Homebrew, the machine created at the
/// tested size and started, Ollama's cloud turned off in the user's
/// server.json and the brew service restarted, the pair pulled, the small
/// context fixed in Homebrew's service environment, the run defaults
/// written, and the same check as doctor passing.
#[cfg(target_os = "macos")]
#[tokio::test]
async fn a_new_mac_is_set_up_with_yes_and_ends_ready() {
    let shims = Shims::new();
    shims.add(
        "brew",
        &format!(
            r#"case "$*" in
  "--version") echo "Homebrew 7.0.10" ;;
  "install podman") cp "{dir}/podman.real" "{dir}/podman"; chmod +x "{dir}/podman" ;;
  "services --help") echo 'Environment variables can be added in `$HOMEBREW_USER_CONFIG_HOME/services/<formula>.env`' ;;
  *) ;;
esac"#,
            dir = shims.dir.path().display()
        ),
    );
    podman_shim(&shims);
    std::fs::rename(
        shims.dir.path().join("podman"),
        shims.dir.path().join("podman.real"),
    )
    .unwrap();
    ollama_shim(&shims);
    let ollama = FakeOllama::start("0.40.6", shims.home.path(), shims.dir.path(), &[]).await;
    brew_service_on(&shims, &ollama.url());
    let facts = facts(
        mac("26.6"),
        48,
        no_podman(),
        &shims,
        &ollama.url(),
        OllamaService::Brew,
    )
    .await;
    let mut o = onboarding(
        &shims,
        Ui::new(Mode::Yes, false, None),
        facts,
        OnboardArgs {
            ollama_url: Some(ollama.url()),
            ..yes(WriterChoice::Ollama)
        },
    );
    let code = run(&mut o).await;
    let output = o.ui.output();
    assert_eq!(code, exit::READY, "{output}\nsteps: {:?}", o.steps);
    let calls = shims.calls();
    for expected in [
        "brew install podman",
        "podman machine init --cpus 6 --memory 8192 --disk-size 60",
        "podman machine start",
        "podman pull docker.io/library/alpine:3.20",
        "brew services restart ollama",
        "ollama pull qwen3-coder:30b",
        "ollama pull gpt-oss:20b",
    ] {
        assert!(
            calls.iter().any(|call| call == expected),
            "{expected}: {calls:?}"
        );
    }
    assert!(
        !calls.iter().any(|call| call.contains("--rootful")),
        "never rootful: {calls:?}"
    );
    // The consent questions show sizes and commands.
    assert!(
        output.contains("`brew install podman` (about 100 MB)"),
        "{output}"
    );
    assert!(output.contains("This downloads about 0.9 GB and uses up to 8 GiB of memory"));
    // The download plus a tenth for Ollama's temporary files.
    assert!(output
        .contains("Pull qwen3-coder:30b (18.6 GB) and gpt-oss:20b (13.8 GB)? Needs 35.6 GB free"));
    assert!(
        output.contains("a fix run needs at least 16384"),
        "{output}"
    );
    // Cloud off in the user's server.json, context in the brew service env.
    let server_json: Value = serde_json::from_str(
        &std::fs::read_to_string(shims.home.path().join(".ollama/server.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(server_json["disable_ollama_cloud"], true);
    assert_eq!(
        std::fs::read_to_string(shims.home.path().join(".homebrew/services/ollama.env")).unwrap(),
        format!(
            "OLLAMA_HOST=127.0.0.1:{}\nOLLAMA_CONTEXT_LENGTH=32768\n",
            detect::url_port(&ollama.url())
        )
    );
    // The configuration: the pair, the run defaults, the server.
    let text = config_text(&o);
    assert_eq!(
        config::run_models(&text),
        RunModels {
            writer: Some("ollama:qwen3-coder:30b".into()),
            reviewer: Some("ollama:gpt-oss:20b".into()),
        }
    );
    let config = axocoatl_config::parse_config(&text, &o.paths.config_path).unwrap();
    assert_eq!(config.providers.ollama.unwrap().base_url, ollama.url());
    assert_eq!(Some(config.server.port), o.args.port);
    assert!(output.contains("Ready for: axocoatl run fix"), "{output}");
    assert!(output.contains("Ready: yes"), "{output}");
    assert!(!output.contains("Not ready"), "{output}");
}

/// Without --allow-sudo, --yes runs no sudo: on Ubuntu without Podman,
/// the install is a step left with its exact command, and onboarding ends
/// not ready (exit 1) without a ✓ banner.
#[cfg(unix)]
#[tokio::test]
async fn yes_without_allow_sudo_lists_the_sudo_steps_and_runs_none() {
    let shims = Shims::new();
    shims.add("sudo", "exit 0");
    ollama_shim(&shims);
    let ollama = FakeOllama::start(
        "0.40.6",
        shims.home.path(),
        shims.dir.path(),
        &["qwen3-coder:30b", "gpt-oss:20b"],
    )
    .await;
    let facts = facts(
        ubuntu(),
        64,
        no_podman(),
        &shims,
        &ollama.url(),
        OllamaService::Systemd,
    )
    .await;
    let mut o = onboarding(
        &shims,
        Ui::new(Mode::Yes, false, None),
        facts,
        OnboardArgs {
            ollama_url: Some(ollama.url()),
            ..yes(WriterChoice::Ollama)
        },
    );
    let code = run(&mut o).await;
    let output = o.ui.output();
    assert_eq!(code, exit::NOT_READY, "{output}");
    assert!(
        !shims.calls().iter().any(|call| call.starts_with("sudo")),
        "{:?}",
        shims.calls()
    );
    assert!(o
        .steps
        .iter()
        .any(|step| step == "Install Podman: sudo apt-get install -y podman"));
    assert!(
        o.steps
            .iter()
            .any(|step| step.contains("OLLAMA_NO_CLOUD=1")),
        "{:?}",
        o.steps
    );
    assert!(output.contains("Not ready yet"), "{output}");
    assert!(!output.contains("✓ Axocoatl configured"), "{output}");
    assert!(!output.contains("Ready: yes"), "{output}");
}

/// Linux with the systemd service and --allow-sudo: Podman from apt, a
/// range of subordinate ids, and cloud off through a drop-in for the
/// service (not the user's ~/.ollama), then a re-probe until the server
/// says so.
#[cfg(unix)]
#[tokio::test]
async fn linux_uses_apt_usermod_and_a_systemd_drop_in_with_allow_sudo() {
    let shims = Shims::new();
    // No range in /etc/subuid and /etc/subgid until usermod adds one.
    shims.subordinate_ids(None);
    let dir = shims.dir.path().display().to_string();
    let etc = shims.root.path().join("etc").display().to_string();
    shims.add(
        "sudo",
        &format!(
            r#"case "$*" in
  "apt-get install -y podman") cp "{dir}/podman.real" "{dir}/podman"; chmod +x "{dir}/podman"; touch "{dir}/machine" "{dir}/running" "{dir}/no-ids" ;;
  usermod*) rm -f "{dir}/no-ids"; echo "alex:100000:65536" >> "{etc}/subuid"; echo "alex:100000:65536" >> "{etc}/subgid" ;;
  tee*) cat > "{dir}/drop-in" ;;
  "systemctl restart ollama") [ -f "{dir}/drop-in" ] && grep -q OLLAMA_NO_CLOUD "{dir}/drop-in" && touch "{dir}/no-cloud" ;;
esac"#
        ),
    );
    podman_shim(&shims);
    // Without subordinate ids until usermod runs.
    let script = std::fs::read_to_string(shims.dir.path().join("podman")).unwrap();
    shims.write_program(
        "podman",
        &script.replace(
            r#"if [ -f "$D/rootful" ]; then"#,
            r#"if [ -f "$D/no-ids" ]; then echo "$info" | sed 's/{"container_id":1,"host_id":100000,"size":65536}//g; s/},\]/}]/g'; elif [ -f "$D/rootful" ]; then"#,
        ),
    );
    std::fs::rename(
        shims.dir.path().join("podman"),
        shims.dir.path().join("podman.real"),
    )
    .unwrap();
    ollama_shim(&shims);
    std::fs::write(shims.dir.path().join("big-context"), "").unwrap();
    let ollama = FakeOllama::start(
        "0.40.6",
        shims.home.path(),
        shims.dir.path(),
        &["qwen3-coder:30b", "gpt-oss:20b"],
    )
    .await;
    let facts = facts(
        ubuntu(),
        64,
        no_podman(),
        &shims,
        &ollama.url(),
        OllamaService::Systemd,
    )
    .await;
    let mut o = onboarding(
        &shims,
        Ui::new(Mode::Yes, true, None),
        facts,
        OnboardArgs {
            ollama_url: Some(ollama.url()),
            allow_sudo: true,
            ..yes(WriterChoice::Ollama)
        },
    );
    let code = run(&mut o).await;
    let output = o.ui.output();
    let calls = shims.calls();
    assert_eq!(code, exit::READY, "{output}\n{calls:?}\n{:?}", o.steps);
    for expected in [
        "sudo apt-get install -y podman",
        "sudo usermod --add-subuids 100000-165535 --add-subgids 100000-165535 alex",
        "podman system migrate",
        "sudo install -d -m 0755 /etc/systemd/system/ollama.service.d",
        "sudo tee /etc/systemd/system/ollama.service.d/axocoatl-no-cloud.conf",
        "sudo systemctl daemon-reload",
        "sudo systemctl restart ollama",
    ] {
        assert!(
            calls.iter().any(|call| call == expected),
            "{expected}: {calls:?}"
        );
    }
    assert_eq!(
        std::fs::read_to_string(shims.dir.path().join("drop-in")).unwrap(),
        "[Service]\nEnvironment=\"OLLAMA_NO_CLOUD=1\"\n"
    );
    assert!(!shims.home.path().join(".ollama/server.json").exists());
    assert!(!calls.iter().any(|call| call.contains("launchctl")));
}

/// Ubuntu 24.04 with a ready rootless Podman (the shim's `podman info`
/// maps a range), the computer's `/etc/subuid` and `/etc/subgid` as the
/// shims' root has them, and `sudo` running `sudo_body`.
async fn linux_sandbox(shims: &Shims, sudo_body: &str, args: OnboardArgs) -> Onboarding {
    linux_sandbox_on(shims, ubuntu(), sudo_body, args).await
}

/// [`linux_sandbox`] on the Linux `os`.
async fn linux_sandbox_on(shims: &Shims, os: Os, sudo_body: &str, args: OnboardArgs) -> Onboarding {
    shims.add("sudo", sudo_body);
    if !shims.dir.path().join("podman").exists() {
        podman_shim(shims);
    }
    podman_state(shims, &["machine", "running"]);
    let mut found = facts(
        os,
        64,
        podman_report(shims).await,
        shims,
        "http://127.0.0.1:9",
        OllamaService::Manual,
    )
    .await;
    crate::doctor::read_id_ranges(
        &mut found.podman,
        &shims.host(false),
        shims.root.path(),
        &found.os,
        &found.user,
    )
    .await;
    let ui = Ui::new(Mode::Yes, args.allow_sudo, None);
    onboarding(shims, ui, found, args)
}

/// The release gate's case: the user's lines are gone from /etc/subuid and
/// /etc/subgid, while Podman's pause process still holds the old range, so
/// `podman info` reports one. Onboarding reads the files, names the exact
/// usermod fix (a range no other user has) and, under --yes without
/// --allow-sudo, runs no sudo and no sandbox container (which would fail
/// in crun on ping_group_range).
#[cfg(unix)]
#[tokio::test]
async fn a_user_without_a_subuid_range_gets_the_usermod_fix_and_no_sudo() {
    let shims = Shims::new();
    shims.subordinate_ids(Some("newuser:100000:65536\n"));
    let mut o = linux_sandbox(&shims, "exit 0", yes(WriterChoice::Ollama)).await;
    assert_eq!(
        axocoatl_isolation::podman_report::id_range_problem(&o.facts.podman),
        Some(axocoatl_isolation::podman_report::IdRangeProblem::Missing)
    );
    assert!(!sandbox::run(&mut o).await);
    let output = o.ui.output();
    let calls = shims.calls();
    assert!(
        !calls.iter().any(|call| call.starts_with("sudo")
            || call.starts_with("podman run")
            || call == "podman system migrate"),
        "{calls:?}"
    );
    assert_eq!(
        o.steps,
        ["alex has no range of subordinate user and group IDs in /etc/subuid and /etc/subgid, so \
          rootless Podman cannot unpack images or start Sessions: sudo usermod --add-subuids \
          165536-231071 --add-subgids 165536-231071 alex && podman system migrate"]
    );
    assert!(
        output.contains("not done: it needs sudo, and --yes runs sudo only with --allow-sudo"),
        "{output}"
    );
}

/// With --allow-sudo the same case adds the range, restarts rootless
/// Podman, and the sandbox check container runs.
#[cfg(unix)]
#[tokio::test]
async fn allow_sudo_adds_a_free_subuid_range_and_then_checks_the_sandbox() {
    let shims = Shims::new();
    shims.subordinate_ids(Some("newuser:100000:65536\n"));
    let etc = shims.root.path().join("etc").display().to_string();
    let sudo = format!(
        r#"case "$*" in
  "usermod --add-subuids 165536-231071 --add-subgids 165536-231071 alex") echo "alex:165536:65536" >> "{etc}/subuid"; echo "alex:165536:65536" >> "{etc}/subgid" ;;
  *) exit 1 ;;
esac"#
    );
    let mut o = linux_sandbox(
        &shims,
        &sudo,
        OnboardArgs {
            allow_sudo: true,
            ..yes(WriterChoice::Ollama)
        },
    )
    .await;
    assert!(
        sandbox::run(&mut o).await,
        "{:?}\n{}",
        o.steps,
        o.ui.output()
    );
    let calls = shims.calls();
    let usermod = calls
        .iter()
        .position(|call| {
            call == "sudo usermod --add-subuids 165536-231071 --add-subgids 165536-231071 alex"
        })
        .unwrap_or_else(|| panic!("{calls:?}"));
    let migrate = calls
        .iter()
        .position(|call| call == "podman system migrate")
        .unwrap_or_else(|| panic!("{calls:?}"));
    let proof = calls
        .iter()
        .position(|call| call.starts_with("podman run --rm --pull=never --userns=keep-id"))
        .unwrap_or_else(|| panic!("{calls:?}"));
    assert!(usermod < migrate && migrate < proof, "{calls:?}");
    assert!(o.steps.is_empty(), "{:?}", o.steps);
}

/// A range in the files that Podman's pause process does not map yet (it
/// started before the range was added) needs `podman system migrate`, no
/// sudo; --yes runs it, and the sandbox check follows.
#[cfg(unix)]
#[tokio::test]
async fn a_range_podman_has_not_applied_is_picked_up_with_migrate() {
    let shims = Shims::new();
    podman_shim(&shims);
    let script = std::fs::read_to_string(shims.dir.path().join("podman")).unwrap();
    let script = script
        .replace(
            r#"if [ -f "$D/rootful" ]; then"#,
            r#"if [ -f "$D/stale" ]; then echo "$info" | sed 's/{"container_id":1,"host_id":100000,"size":65536}//g; s/},\]/}]/g'; elif [ -f "$D/rootful" ]; then"#,
        )
        .replace(
            r#""system migrate") ;;"#,
            r#""system migrate") rm -f "$D/stale" ;;"#,
        );
    shims.write_program("podman", &script);
    podman_state(&shims, &["stale"]);
    let mut o = linux_sandbox(&shims, "exit 1", yes(WriterChoice::Ollama)).await;
    assert_eq!(
        axocoatl_isolation::podman_report::id_range_problem(&o.facts.podman),
        Some(axocoatl_isolation::podman_report::IdRangeProblem::NotApplied)
    );
    assert!(
        sandbox::run(&mut o).await,
        "{:?}\n{}",
        o.steps,
        o.ui.output()
    );
    let calls = shims.calls();
    assert!(
        calls.iter().any(|call| call == "podman system migrate"),
        "{calls:?}"
    );
    assert!(
        !calls.iter().any(|call| call.starts_with("sudo")),
        "{calls:?}"
    );
    assert!(
        calls.iter().any(|call| call.starts_with("podman run")),
        "{calls:?}"
    );
}

/// A sandbox check container that fails in crun on ping_group_range (no
/// group 0: keep-id had no subordinate range) is a step left with the
/// usermod fix, not "see the log".
#[cfg(unix)]
#[tokio::test]
async fn the_crun_ping_group_range_error_names_the_subuid_fix() {
    let shims = Shims::new();
    podman_shim(&shims);
    let script = std::fs::read_to_string(shims.dir.path().join("podman")).unwrap();
    shims.write_program(
        "podman",
        &script.replace(
            "  run*) echo 1000 ;;",
            "  run*) echo 'Error: OCI runtime error: crun: write to `/proc/sys/net/ipv4/ping_group_range`: Invalid argument' >&2; exit 126 ;;",
        ),
    );
    // The files cannot tell (ranges from SSSD), so the container decides.
    std::fs::write(shims.root.path().join("etc/nsswitch.conf"), "subid: sss\n").unwrap();
    let mut o = linux_sandbox(&shims, "exit 1", yes(WriterChoice::Ollama)).await;
    assert!(o.facts.podman.subordinate_ids.is_none());
    assert!(!sandbox::run(&mut o).await);
    assert_eq!(o.steps.len(), 1, "{:?}", o.steps);
    assert!(
        o.steps[0].ends_with(
            "Invalid argument): sudo usermod --add-subuids 100000-165535 --add-subgids \
             100000-165535 alex && podman system migrate"
        ),
        "{:?}",
        o.steps
    );
}

/// Doctor on Linux reads the files too: without the user's range it FAILs
/// "User ID ranges" with the usermod fix (so `doctor` exits 1), although
/// `podman info` still shows the old range; with the range it does not.
#[cfg(unix)]
#[tokio::test]
async fn doctor_fails_a_missing_subuid_range_on_linux() {
    let shims = Shims::new();
    podman_shim(&shims);
    podman_state(&shims, &["machine", "running"]);
    shims.subordinate_ids(Some("newuser:100000:65536\n"));
    let env = crate::doctor::Environment {
        os: ubuntu(),
        host: shims.host(false),
        ..doctor_env(&shims)
    };
    let config_path = shims.home.path().join("config.yaml");
    let report = crate::doctor::collect(&config_path, &env).await;
    assert!(!report.ready);
    let ranges = report
        .sections
        .sandbox
        .iter()
        .find(|item| item.name == "User ID ranges")
        .unwrap_or_else(|| panic!("{:?}", report.sections.sandbox));
    assert_eq!(ranges.status, crate::doctor::Status::Fail);
    assert_eq!(
        ranges.fix.as_deref(),
        Some(
            "sudo usermod --add-subuids 165536-231071 --add-subgids 165536-231071 alex && podman \
             system migrate"
        )
    );
    assert!(report
        .steps_left
        .iter()
        .any(|step| step.contains("usermod --add-subuids")));

    shims.subordinate_ids(Some("newuser:100000:65536\nalex:165536:65536\n"));
    let report = crate::doctor::collect(&config_path, &env).await;
    assert!(
        !report
            .sections
            .sandbox
            .iter()
            .any(|item| item.name == "User ID ranges"),
        "{:?}",
        report.sections.sandbox
    );
}

/// [`podman_shim`] as Podman 4.9.3 behaves for a user whose files give a
/// range: while `stale` exists beside it (a pause process started before
/// the range was added), `podman info` maps only the user's own id; once
/// `podman system migrate` removes it, a new pause process needs a setuid
/// `newuidmap` on PATH, and without one `podman info` fails with Podman's
/// own message: as `Error: …` (exit 125) before any migrate, and logged
/// through logrus with escaped quotes (exit 1) after one, as seen live on
/// Ubuntu 24.04.
fn stale_podman(shims: &Shims) {
    podman_shim(shims);
    let script = std::fs::read_to_string(shims.dir.path().join("podman")).unwrap();
    let script = script
        .replace(
            r#"if [ -f "$D/rootful" ]; then"#,
            r#"if [ -f "$D/stale" ]; then echo "$info" | sed 's/{"container_id":1,"host_id":100000,"size":65536}//g; s/},\]/}]/g'; elif [ ! -e "$D/newuidmap" ] && [ -f "$D/migrated" ]; then echo 'time="2026-10-20T20:01:02-07:00" level=error msg="invalid internal status, try resetting the pause process with \"podman system migrate\": command required for rootless mode with multiple IDs: exec: \"newuidmap\": executable file not found in $PATH"' >&2; exit 1; elif [ ! -e "$D/newuidmap" ]; then echo 'Error: command required for rootless mode with multiple IDs: exec: "newuidmap": executable file not found in $PATH' >&2; exit 125; elif [ ! -u "$D/newuidmap" ] && [ -f "$D/migrated" ]; then printf 'time="2026-10-20T20:01:02-07:00" level=error msg="invalid internal status, try resetting the pause process with \\"podman system migrate\\": cannot set up namespace using \\"%s\\": should have setuid or have filecaps setuid: exit status 1"\n' "$D/newuidmap" >&2; exit 1; elif [ ! -u "$D/newuidmap" ]; then echo "Error: cannot set up namespace using \"$D/newuidmap\": should have setuid or have filecaps setuid: exit status 1" >&2; exit 125; elif [ -f "$D/rootful" ]; then"#,
        )
        .replace(
            r#""system migrate") ;;"#,
            r#""system migrate") rm -f "$D/stale"; touch "$D/migrated" ;;"#,
        );
    shims.write_program("podman", &script);
}

/// A Podman reached through CONTAINER_HOST (another user's rootless socket
/// on this computer, or another computer) is not judged by this computer's
/// /etc/subuid: with no range for alex in the files but one in that
/// Podman's `podman info`, the sandbox step passes. When that Podman maps
/// one id, the step left says where to fix it, and onboarding runs no sudo
/// and no `podman system migrate` (Podman refuses it through an override),
/// even with --allow-sudo.
#[cfg(unix)]
#[tokio::test]
async fn a_podman_reached_through_container_host_is_not_changed_here() {
    for stale in [false, true] {
        let shims = Shims::new();
        shims.subordinate_ids(Some("newuser:100000:65536\n"));
        shims.add("sudo", "exit 0");
        stale_podman(&shims);
        podman_state(&shims, &["machine", "running"]);
        if stale {
            podman_state(&shims, &["stale"]);
        }
        let mut found = facts(
            ubuntu(),
            64,
            podman_report(&shims).await,
            &shims,
            "http://127.0.0.1:9",
            OllamaService::Manual,
        )
        .await;
        found.podman.connection =
            ConnectionOverride::Host("unix:///run/user/2000/podman/podman.sock".to_string());
        crate::doctor::read_id_ranges(
            &mut found.podman,
            &shims.host(false),
            shims.root.path(),
            &found.os,
            &found.user,
        )
        .await;
        assert!(found.podman.subordinate_ids.is_none());
        assert!(found.podman.id_map_tools.is_none());
        let args = OnboardArgs {
            allow_sudo: true,
            ..yes(WriterChoice::Ollama)
        };
        let mut o = onboarding(&shims, Ui::new(Mode::Yes, true, None), found, args);
        let ready = sandbox::run(&mut o).await;
        let calls = shims.calls();
        assert!(
            !calls
                .iter()
                .any(|call| call.starts_with("sudo") || call.contains("system migrate")),
            "{calls:?}"
        );
        if stale {
            assert!(!ready);
            assert_eq!(o.steps.len(), 1, "{:?}", o.steps);
            assert!(
                o.steps[0].starts_with(
                    "Podman reached through \
                     CONTAINER_HOST=unix:///run/user/2000/podman/podman.sock runs rootless with \
                     only one user ID mapped"
                ),
                "{:?}",
                o.steps
            );
            // `podman system migrate` only as that Podman's user, where it
            // runs: never run or offered as a command for this computer.
            assert!(
                o.steps[0].contains("then run `podman system migrate` as that user there")
                    && !o.steps[0].contains("&& podman system migrate")
                    && !o.steps[0].contains("sudo"),
                "{:?}",
                o.steps
            );
            assert!(!calls.iter().any(|call| call.starts_with("podman run")));
        } else {
            assert!(ready, "{:?}\n{}", o.steps, o.ui.output());
            assert!(calls.iter().any(|call| call.starts_with("podman run")));
        }
    }
}

/// Rootless Podman maps one id while /etc/subuid gives alex a range,
/// because newuidmap and newgidmap are missing (Debian and Ubuntu ship them
/// in uidmap, which Podman only recommends). Onboarding says so instead of
/// offering `podman system migrate` again and again: under --yes without
/// --allow-sudo it runs nothing; with it, it installs uidmap, restarts
/// rootless Podman, and the sandbox check runs.
#[cfg(unix)]
#[tokio::test]
async fn missing_newuidmap_is_installed_instead_of_only_migrating() {
    for allow_sudo in [false, true] {
        let shims = Shims::new();
        shims.id_map_tools(None);
        // Ubuntu without uidmap, as dpkg says it.
        shims.add(
            "dpkg-query",
            "echo 'dpkg-query: no packages found matching uidmap' >&2; exit 1",
        );
        stale_podman(&shims);
        podman_state(&shims, &["stale"]);
        let sudo = format!(
            r#"case "$*" in
  "apt-get install -y uidmap") for n in newuidmap newgidmap; do printf '#!/bin/sh\nexit 1\n' > "{dir}/$n"; chmod 4755 "{dir}/$n"; done ;;
  *) exit 1 ;;
esac"#,
            dir = shims.dir.path().display()
        );
        let mut o = linux_sandbox(
            &shims,
            &sudo,
            OnboardArgs {
                allow_sudo,
                ..yes(WriterChoice::Ollama)
            },
        )
        .await;
        assert_eq!(
            axocoatl_isolation::podman_report::id_range_problem(&o.facts.podman),
            Some(axocoatl_isolation::podman_report::IdRangeProblem::NoIdMapTools)
        );
        let ready = sandbox::run(&mut o).await;
        let calls = shims.calls();
        if allow_sudo {
            assert!(ready, "{:?}\n{}", o.steps, o.ui.output());
            let position = |wanted: &str| {
                calls
                    .iter()
                    .position(|call| call.starts_with(wanted))
                    .unwrap_or_else(|| panic!("{wanted}: {calls:?}"))
            };
            assert!(
                position("sudo apt-get install -y uidmap") < position("podman system migrate")
                    && position("podman system migrate") < position("podman run"),
                "{calls:?}"
            );
            assert!(o.steps.is_empty(), "{:?}", o.steps);
        } else {
            assert!(!ready);
            assert_eq!(
                o.steps,
                ["newuidmap and newgidmap are not installed, so rootless Podman maps only alex's \
                  own user ID, whatever /etc/subuid and /etc/subgid give, and cannot unpack \
                  images or start Sessions: sudo apt-get install -y uidmap && podman system \
                  migrate"]
            );
            assert!(
                !calls
                    .iter()
                    .any(|call| call.starts_with("sudo") || call.contains("system migrate")),
                "{calls:?}"
            );
        }
    }
}

/// The usual shape on Debian and Ubuntu without `uidmap` (Podman only
/// recommends it): /etc/subuid gives alex a range, so a new rootless pause
/// process needs newuidmap and `podman info` fails, with a message that
/// says to run `podman system migrate` (which alone changes nothing).
/// Onboarding offers to install uidmap instead of a step to read `podman
/// info`: under --yes only with --allow-sudo, and --plan shows it once.
/// The same holds for Podman's logged form of the failure, which a user
/// who already ran `podman system migrate` gets.
#[cfg(unix)]
#[tokio::test]
async fn a_podman_info_failing_without_newuidmap_installs_uidmap() {
    for (mode, allow_sudo, migrated) in [
        (Mode::Yes, false, false),
        (Mode::Yes, true, false),
        (Mode::Plan, false, false),
        (Mode::Yes, false, true),
        (Mode::Yes, true, true),
        (Mode::Plan, false, true),
    ] {
        let shims = Shims::new();
        shims.id_map_tools(None);
        // Ubuntu without uidmap, as dpkg says it.
        shims.add(
            "dpkg-query",
            "echo 'dpkg-query: no packages found matching uidmap' >&2; exit 1",
        );
        stale_podman(&shims);
        if migrated {
            std::fs::write(shims.dir.path().join("migrated"), "").unwrap();
        }
        let sudo = format!(
            r#"case "$*" in
  "apt-get install -y uidmap") for n in newuidmap newgidmap; do printf '#!/bin/sh\nexit 1\n' > "{dir}/$n"; chmod 4755 "{dir}/$n"; done ;;
  *) exit 1 ;;
esac"#,
            dir = shims.dir.path().display()
        );
        let mut o = linux_sandbox(
            &shims,
            &sudo,
            OnboardArgs {
                allow_sudo,
                ..yes(WriterChoice::Ollama)
            },
        )
        .await;
        o.ui = Ui::new(mode, allow_sudo, None);
        let PodmanReadiness::Unavailable(error) = &o.facts.podman.readiness else {
            panic!("{:?}", o.facts.podman.readiness);
        };
        let expected = if migrated {
            "exec: \\\"newuidmap\\\": executable file not found in $PATH\""
        } else {
            "exec: \"newuidmap\": executable file not found in $PATH"
        };
        assert!(error.contains(expected), "{error}");
        assert_eq!(
            axocoatl_isolation::podman_report::id_range_problem(&o.facts.podman),
            Some(axocoatl_isolation::podman_report::IdRangeProblem::NoIdMapTools)
        );
        let ready = sandbox::run(&mut o).await;
        let calls = shims.calls();
        let output = o.ui.output();
        match (mode, allow_sudo) {
            (Mode::Yes, true) => {
                assert!(ready, "{:?}\n{output}", o.steps);
                let position = |wanted: &str| {
                    calls
                        .iter()
                        .position(|call| call.starts_with(wanted))
                        .unwrap_or_else(|| panic!("{wanted}: {calls:?}"))
                };
                assert!(
                    position("sudo apt-get install -y uidmap") < position("podman system migrate")
                        && position("podman system migrate") < position("podman run"),
                    "{calls:?}"
                );
                assert!(o.steps.is_empty(), "{:?}", o.steps);
            }
            (Mode::Yes, false) => {
                assert!(!ready);
                assert_eq!(
                    o.steps,
                    [
                        "Rootless Podman cannot map the subordinate user and group IDs alex has: \
                      newuidmap and newgidmap are not installed, so `podman info` fails (command \
                      required for rootless mode with multiple IDs: exec: \"newuidmap\": \
                      executable file not found in $PATH) and no Session can start: sudo \
                      apt-get install -y uidmap && podman system migrate"
                    ]
                );
                assert!(
                    !calls
                        .iter()
                        .any(|call| call.starts_with("sudo") || call.contains("system migrate")),
                    "{calls:?}"
                );
                assert!(
                    !output.contains("check `podman system connection list`"),
                    "{output}"
                );
            }
            _ => {
                assert!(ready, "{:?}\n{output}", o.steps);
                assert!(
                    calls.iter().all(|call| !call.starts_with("sudo")),
                    "{calls:?}"
                );
                assert_eq!(
                    output
                        .matches("would run: sudo apt-get install -y uidmap")
                        .count(),
                    1,
                    "{output}"
                );
            }
        }
    }
}

/// `podman system migrate` without the helpers, as Podman's own message
/// suggests: the stale pause process goes, and the next `podman info`
/// fails. Doctor's Podman line then names the install, once, and no
/// second "User ID ranges" line repeats it.
#[cfg(unix)]
#[tokio::test]
async fn doctor_names_the_uidmap_install_when_podman_info_fails() {
    let shims = Shims::new();
    shims.id_map_tools(None);
    // Ubuntu without uidmap, as dpkg says it.
    shims.add(
        "dpkg-query",
        "echo 'dpkg-query: no packages found matching uidmap' >&2; exit 1",
    );
    stale_podman(&shims);
    podman_state(&shims, &["machine", "running"]);
    let env = crate::doctor::Environment {
        os: ubuntu(),
        host: shims.host(false),
        ..doctor_env(&shims)
    };
    let config_path = shims.home.path().join("config.yaml");
    let report = crate::doctor::collect(&config_path, &env).await;
    assert!(!report.ready);
    let podman = &report.sections.sandbox[0];
    assert_eq!(podman.name, "Podman");
    assert_eq!(podman.status, crate::doctor::Status::Fail);
    assert!(
        podman.detail.starts_with(
            "Podman 5.8.2 does not answer: rootless Podman cannot map the subordinate user and \
             group IDs alex has: newuidmap and newgidmap are not installed"
        ),
        "{podman:?}"
    );
    assert_eq!(
        podman.fix.as_deref(),
        Some("sudo apt-get install -y uidmap && podman system migrate")
    );
    assert!(
        !report
            .sections
            .sandbox
            .iter()
            .any(|item| item.name == "User ID ranges"),
        "{:?}",
        report.sections.sandbox
    );
    assert_eq!(
        report
            .steps_left
            .iter()
            .filter(|step| step.contains("apt-get install -y uidmap"))
            .count(),
        1,
        "{:?}",
        report.steps_left
    );
    // A newuidmap that is there but not setuid, uidmap's state not known
    // (no dpkg-query): reinstalled, in words.
    shims.id_map_tools(Some(0o755));
    let report = crate::doctor::collect(&config_path, &env).await;
    let podman = &report.sections.sandbox[0];
    assert!(
        podman.detail.contains("cannot set up namespace using"),
        "{podman:?}"
    );
    assert!(
        podman
            .fix
            .as_deref()
            .unwrap()
            .starts_with("Reinstall the package that provides"),
        "{podman:?}"
    );
    // dpkg says uidmap is installed: `apt-get install` would do nothing,
    // so the fix is its reinstall, which restores the setuid bit.
    shims.add("dpkg-query", "printf installed");
    let report = crate::doctor::collect(&config_path, &env).await;
    let podman = &report.sections.sandbox[0];
    assert_eq!(
        podman.fix.as_deref(),
        Some("sudo apt-get install --reinstall -y uidmap && podman system migrate"),
        "{podman:?}"
    );
    // The same for helpers deleted from an installed uidmap, which doctor
    // says are missing, not "not installed".
    shims.id_map_tools(None);
    let report = crate::doctor::collect(&config_path, &env).await;
    let podman = &report.sections.sandbox[0];
    assert!(
        podman.detail.contains(
            "alex has: newuidmap and newgidmap are missing, though their package uidmap is \
             installed, so `podman info` fails"
        ),
        "{podman:?}"
    );
    assert_eq!(
        podman.fix.as_deref(),
        Some("sudo apt-get install --reinstall -y uidmap && podman system migrate")
    );
}

/// `dnf install -y shadow-utils` does nothing on Fedora and RHEL, where
/// shadow-utils is always installed, and `apt-get install -y uidmap` does
/// nothing once uidmap is installed, even when a helper's file is gone:
/// onboarding offers the reinstall then, which restores it. When an install
/// or reinstall that ran changes nothing, the step left says so in words
/// and does not offer the same command again (nor run `podman system
/// migrate`, which would fail the same way).
#[cfg(unix)]
#[tokio::test]
async fn an_install_that_changes_nothing_is_not_offered_again() {
    let fedora = || Os::Linux {
        release: OsRelease::parse("ID=fedora\nVERSION_ID=42\n"),
        wsl: None,
        systemd: true,
    };
    for (os, query, answer, reinstall, restores) in [
        (
            fedora(),
            "rpm",
            "echo shadow-utils-4.17.4-1.fc42.aarch64",
            "dnf reinstall -y shadow-utils",
            true,
        ),
        (
            fedora(),
            "rpm",
            "echo shadow-utils-4.17.4-1.fc42.aarch64",
            "dnf reinstall -y shadow-utils",
            false,
        ),
        (
            ubuntu(),
            "dpkg-query",
            "printf installed",
            "apt-get install --reinstall -y uidmap",
            true,
        ),
        (
            ubuntu(),
            "dpkg-query",
            "printf installed",
            "apt-get install --reinstall -y uidmap",
            false,
        ),
    ] {
        let shims = Shims::new();
        shims.id_map_tools(None);
        stale_podman(&shims);
        shims.add(query, answer);
        // The reinstall puts the helpers back, setuid, or (a reinstall
        // that cannot help) does nothing and succeeds, as `dnf install` of
        // an installed package does.
        let restore = if restores {
            format!(
                r#"for n in newuidmap newgidmap; do printf '#!/bin/sh\nexit 1\n' > "{}/$n"; chmod 4755 "{}/$n"; done"#,
                shims.dir.path().display(),
                shims.dir.path().display()
            )
        } else {
            ":".to_string()
        };
        let sudo = format!(
            r#"case "$*" in
  "{reinstall}") {restore} ;;
  *) exit 1 ;;
esac"#
        );
        let mut o = linux_sandbox_on(
            &shims,
            os,
            &sudo,
            OnboardArgs {
                allow_sudo: true,
                ..yes(WriterChoice::Ollama)
            },
        )
        .await;
        let ready = sandbox::run(&mut o).await;
        let calls = shims.calls();
        let sudo_calls: Vec<&String> = calls
            .iter()
            .filter(|call| call.starts_with("sudo"))
            .collect();
        assert_eq!(sudo_calls, [&format!("sudo {reinstall}")], "{calls:?}");
        // Asked as a reinstall, with the command it runs.
        let output = o.ui.output();
        assert!(
            output.contains(&format!(
                "Reinstall their package with `sudo {reinstall} && podman system migrate`? \
                 `podman system migrate` stops your running containers. yes (--yes)"
            )),
            "{output}"
        );
        if restores {
            assert!(ready, "{:?}\n{}", o.steps, o.ui.output());
            let position = |wanted: &str| {
                calls
                    .iter()
                    .position(|call| call.starts_with(wanted))
                    .unwrap_or_else(|| panic!("{wanted}: {calls:?}"))
            };
            assert!(
                position("sudo") < position("podman system migrate")
                    && position("podman system migrate") < position("podman run"),
                "{calls:?}"
            );
            assert!(o.steps.is_empty(), "{:?}", o.steps);
        } else {
            assert!(!ready);
            let package = if query == "rpm" {
                "shadow-utils"
            } else {
                "uidmap"
            };
            assert_eq!(
                o.steps,
                [format!(
                    "`sudo {reinstall}` ran, but changed nothing: newuidmap and newgidmap are \
                     missing, though their package {package} is installed. Running it again \
                     would not help: find where this distribution puts newuidmap and \
                     newgidmap, make sure they are on PATH and are setuid root or have file \
                     capabilities, then run `podman system migrate`"
                )]
            );
            // Nothing after the reinstall: migrate would fail the same way.
            assert!(
                !calls.iter().any(|call| call.contains("system migrate")),
                "{calls:?}"
            );
        }
    }
}

/// An install that ran, after which the helpers are on PATH but Podman
/// still fails for want of them (it runs with another PATH): the step left
/// offers no install of their package again, in either form.
#[cfg(unix)]
#[tokio::test]
async fn an_install_is_not_offered_again_when_podman_still_lacks_the_helpers() {
    let shims = Shims::new();
    shims.id_map_tools(None);
    stale_podman(&shims);
    let script = std::fs::read_to_string(shims.dir.path().join("podman"))
        .unwrap()
        .replace(
            r#"elif [ ! -e "$D/newuidmap" ] && [ -f "$D/migrated" ]; then"#,
            r#"elif { [ ! -e "$D/newuidmap" ] || [ -f "$D/other-path" ]; } && [ -f "$D/migrated" ]; then"#,
        );
    shims.write_program("podman", &script);
    std::fs::write(shims.dir.path().join("other-path"), "").unwrap();
    shims.add("dpkg-query", "printf installed");
    let sudo = format!(
        r#"case "$*" in
  "apt-get install --reinstall -y uidmap") for n in newuidmap newgidmap; do printf '#!/bin/sh\nexit 1\n' > "{dir}/$n"; chmod 4755 "{dir}/$n"; done ;;
  *) exit 1 ;;
esac"#,
        dir = shims.dir.path().display()
    );
    let mut o = linux_sandbox(
        &shims,
        &sudo,
        OnboardArgs {
            allow_sudo: true,
            ..yes(WriterChoice::Ollama)
        },
    )
    .await;
    assert!(!sandbox::run(&mut o).await);
    let calls = shims.calls();
    assert_eq!(
        calls.iter().filter(|call| call.starts_with("sudo")).count(),
        1,
        "{calls:?}"
    );
    assert_eq!(o.steps.len(), 1, "{:?}", o.steps);
    assert!(
        o.steps[0].starts_with(
            "`sudo apt-get install --reinstall -y uidmap` ran, but changed nothing: Rootless \
             Podman cannot map"
        ),
        "{:?}",
        o.steps
    );
    assert!(
        !o.steps[0].contains("install -y uidmap &&"),
        "{:?}",
        o.steps
    );
}

/// Doctor looks for newuidmap and newgidmap on PATH only, as Podman does
/// (`exec.LookPath`): setuid helpers in the root's usr/bin that are not on
/// PATH are missing for Podman, and so for doctor, which says where they
/// are and offers no install, which would not change PATH.
#[cfg(unix)]
#[tokio::test]
async fn doctor_looks_for_the_id_map_tools_on_path_only() {
    use std::os::unix::fs::PermissionsExt;
    let shims = Shims::new();
    shims.id_map_tools(None);
    let bin = shims.root.path().join("usr/bin");
    std::fs::create_dir_all(&bin).unwrap();
    for name in ["newuidmap", "newgidmap"] {
        std::fs::write(bin.join(name), "#!/bin/sh\nexit 1\n").unwrap();
        std::fs::set_permissions(bin.join(name), std::fs::Permissions::from_mode(0o4755)).unwrap();
    }
    let host = shims.host(false);
    let root = shims.root.path();
    let tools = crate::doctor::id_map_tools(&host, root, &ubuntu()).await;
    assert_eq!(tools.missing, ["newuidmap", "newgidmap"]);
    assert_eq!(tools.off_path, ["/usr/bin/newuidmap", "/usr/bin/newgidmap"]);
    assert_eq!(
        tools.describe(),
        "newuidmap and newgidmap are in /usr/bin, which is not on PATH"
    );
    assert_eq!(tools.command(), None);
    // No dpkg-query here: whether uidmap is installed is not known.
    assert_eq!(tools.package_installed, None);
    assert_eq!(
        tools.install.as_deref(),
        Some("sudo apt-get install -y uidmap")
    );
    shims.id_map_tools(Some(0o4755));
    assert!(crate::doctor::id_map_tools(&host, root, &ubuntu())
        .await
        .usable());
    shims.id_map_tools(Some(0o755));
    let tools = crate::doctor::id_map_tools(&host, root, &ubuntu()).await;
    assert_eq!(tools.unprivileged.len(), 2, "{tools:?}");

    // Nowhere: what the package query says decides between installing
    // and reinstalling. Fedora and RHEL ship them in shadow-utils (`rpm
    // -qf`), Arch in shadow; openSUSE moved them between packages, so
    // none is named.
    shims.id_map_tools(None);
    std::fs::remove_dir_all(&bin).unwrap();
    let fedora = "ID=fedora\nVERSION_ID=42\n";
    let rhel = "ID=\"rhel\"\nVERSION_ID=\"9.8\"\n";
    for (os_release, query, answer, installed, install, described) in [
        (
            "ID=ubuntu\nVERSION_ID=\"24.04\"\n",
            "dpkg-query",
            "printf installed",
            Some(true),
            Some("sudo apt-get install --reinstall -y uidmap"),
            "newuidmap and newgidmap are missing, though their package uidmap is installed",
        ),
        (
            "ID=ubuntu\nVERSION_ID=\"24.04\"\n",
            "dpkg-query",
            "echo 'dpkg-query: no packages found matching uidmap' >&2; exit 1",
            Some(false),
            Some("sudo apt-get install -y uidmap"),
            "newuidmap and newgidmap are not installed",
        ),
        (
            "ID=debian\nVERSION_ID=\"12\"\n",
            "dpkg-query",
            "printf config-files",
            Some(false),
            Some("sudo apt-get install -y uidmap"),
            "newuidmap and newgidmap are not installed",
        ),
        (
            fedora,
            "rpm",
            "echo shadow-utils-4.17.4-1.fc42.aarch64",
            Some(true),
            Some("sudo dnf reinstall -y shadow-utils"),
            "newuidmap and newgidmap are missing, though their package shadow-utils is installed",
        ),
        (
            fedora,
            "rpm",
            "echo 'package shadow-utils is not installed'; exit 1",
            Some(false),
            Some("sudo dnf install -y shadow-utils"),
            "newuidmap and newgidmap are not installed",
        ),
        // rpm not answering: shadow-utils is always installed there, and
        // only a reinstall restores a helper's file.
        (
            rhel,
            "none",
            "",
            None,
            Some("sudo dnf reinstall -y shadow-utils"),
            "newuidmap and newgidmap are not found on PATH",
        ),
        (
            "ID=arch\n",
            "pacman",
            "echo 'shadow 4.17.4-1'",
            Some(true),
            Some("sudo pacman -S --noconfirm shadow"),
            "newuidmap and newgidmap are missing, though their package shadow is installed",
        ),
        (
            "ID=\"opensuse-tumbleweed\"\nID_LIKE=\"opensuse suse\"\n",
            "none",
            "",
            None,
            None,
            "newuidmap and newgidmap are not found on PATH",
        ),
    ] {
        for program in ["dpkg-query", "rpm", "pacman"] {
            let _ = std::fs::remove_file(shims.dir.path().join(program));
        }
        if query != "none" {
            shims.add(query, answer);
        }
        let os = Os::Linux {
            release: OsRelease::parse(os_release),
            wsl: None,
            systemd: true,
        };
        let tools = crate::doctor::id_map_tools(&host, root, &os).await;
        assert_eq!(tools.package_installed, installed, "{os_release} {answer}");
        assert_eq!(tools.install.as_deref(), install, "{os_release} {answer}");
        assert_eq!(tools.command().as_deref(), install, "{os_release} {answer}");
        assert_eq!(tools.describe(), described, "{os_release} {answer}");
    }
    // Only read-only queries ran.
    let calls = shims.calls();
    assert!(
        calls
            .iter()
            .all(|call| call == "dpkg-query -W -f ${db:Status-Status} uidmap"
                || call == "rpm -q shadow-utils"
                || call == "pacman -Q shadow"),
        "{calls:?}"
    );
    assert!(calls.contains(&"dpkg-query -W -f ${db:Status-Status} uidmap".to_string()));
}

/// Step 3's steps left for a model too large for this computer, or whose
/// context Ollama keeps too small, name `axocoatl onboard` with the writer
/// (so Step 1 is not asked again, nor the writer switched), and
/// `--replace` when the configuration this run writes has an Agent on the
/// model: `--update` keeps a configuration's Agents and changes only
/// `run.models`.
#[cfg(unix)]
#[tokio::test]
async fn step_three_names_a_command_that_keeps_the_writer_and_moves_the_agents() {
    let shims = Shims::new();
    let found = facts(
        ubuntu(),
        12,
        no_podman(),
        &shims,
        "http://127.0.0.1:9",
        OllamaService::Manual,
    )
    .await;
    let mut o = onboarding(
        &shims,
        Ui::new(Mode::Yes, false, None),
        found,
        OnboardArgs::default(),
    );
    let because = " (--replace backs up the configuration and writes the team for the new \
                   models; --update would keep the Agents that run gpt-oss:20b)";
    // No configuration yet: this run writes the team, which runs it.
    assert_eq!(o.kept.agent_models, None);
    assert_eq!(
        ollama::pull_step(&o, "gpt-oss:20b", "", WriterChoice::ClaudeCode),
        format!(
            "gpt-oss:20b (13.8 GB) is too large for this computer's 12 GB, so it was not \
             pulled: choose a smaller reviewer with `axocoatl onboard --replace --writer \
             claude-code`, or OpenRouter for the reviewer with `axocoatl onboard --replace \
             --writer claude-code --reviewer openrouter:openai/gpt-oss-120b`{because}"
        )
    );
    assert_eq!(
        ollama::hosted_instead(
            &o,
            &[("gpt-oss:20b".to_string(), 4096)],
            WriterChoice::Ollama
        ),
        format!("for these models (`axocoatl onboard --replace --writer openrouter`){because}")
    );
    assert_eq!(
        ollama::hosted_instead(
            &o,
            &[("gpt-oss:20b".to_string(), 4096)],
            WriterChoice::Codex
        ),
        format!(
            "for the reviewer (`axocoatl onboard --replace --writer codex --reviewer \
             openrouter:openai/gpt-oss-120b`){because}"
        )
    );
    // A model that fits is only pulled.
    assert_eq!(
        ollama::pull_step(
            &o,
            "qwen3:4b",
            " once Ollama is ready",
            WriterChoice::Ollama
        ),
        "Pull qwen3:4b once Ollama is ready: ollama pull qwen3:4b"
    );

    // "Update this setup" of a configuration whose Agents run other
    // models: it keeps them, and run.models is all that names this one.
    let hosted = "agents:\n  - id: lead\n    name: Lead\n    provider: openrouter\n    model: \
                  \"qwen/qwen3-coder\"\nrun:\n  models:\n    reviewer: \"ollama:gpt-oss:20b\"\n";
    o.kept = Kept::from_config(hosted);
    assert_eq!(o.kept.agent_models, Some(Vec::new()));
    assert_eq!(
        ollama::pull_step(&o, "gpt-oss:20b", "", WriterChoice::Codex),
        "gpt-oss:20b (13.8 GB) is too large for this computer's 12 GB, so it was not pulled: \
         choose a smaller reviewer with `axocoatl onboard --update --writer codex`, or \
         OpenRouter for the reviewer with `axocoatl onboard --update --writer codex --reviewer \
         openrouter:openai/gpt-oss-120b`"
    );
    // Kept Agents that run it: --replace. An Agent without a model runs
    // providers.ollama.model, as doctor reads it.
    let local = "agents:\n  - id: lead\n    name: Lead\n    provider: ollama\nproviders:\n  \
                 ollama:\n    base_url: \"http://localhost:11434\"\n    model: \"gpt-oss:20b\"\n";
    o.kept = Kept::from_config(local);
    assert_eq!(o.kept.agent_models, Some(vec!["gpt-oss:20b".to_string()]));
    assert!(
        ollama::pull_step(&o, "gpt-oss:20b", "", WriterChoice::Ollama).contains(
            "choose a smaller model with `axocoatl onboard --replace --writer ollama`, or a \
             hosted writer and reviewer with `axocoatl onboard --replace --writer openrouter`"
        )
    );
    // A configuration without Agents gets onboarding's team.
    o.kept = Kept::from_config("agents: []\n");
    assert_eq!(o.kept.agent_models, None);
}

/// Only /etc/subgid lacks alex's range: onboarding adds a group range and
/// no user range.
#[cfg(unix)]
#[tokio::test]
async fn only_the_missing_group_range_is_added() {
    let shims = Shims::new();
    let etc = shims.root.path().join("etc");
    std::fs::write(etc.join("subgid"), "").unwrap();
    let sudo = format!(
        r#"case "$*" in
  "usermod --add-subgids 165536-231071 alex") echo "alex:165536:65536" >> "{}/subgid" ;;
  *) exit 1 ;;
esac"#,
        etc.display()
    );
    let mut o = linux_sandbox(
        &shims,
        &sudo,
        OnboardArgs {
            allow_sudo: true,
            ..yes(WriterChoice::Ollama)
        },
    )
    .await;
    assert!(
        sandbox::run(&mut o).await,
        "{:?}\n{}",
        o.steps,
        o.ui.output()
    );
    let calls = shims.calls();
    assert!(
        calls
            .iter()
            .any(|call| call == "sudo usermod --add-subgids 165536-231071 alex"),
        "{calls:?}"
    );
    assert!(
        !calls.iter().any(|call| call.contains("--add-subuids")),
        "{calls:?}"
    );
}

/// The files leave no free range from 100000 up: onboarding names no range
/// (one it named would overlap another user's) and runs nothing.
#[cfg(unix)]
#[tokio::test]
async fn no_free_range_leaves_a_step_and_runs_nothing() {
    let shims = Shims::new();
    shims.subordinate_ids(Some("other:100000:4294867295\n"));
    let mut o = linux_sandbox(
        &shims,
        "exit 0",
        OnboardArgs {
            allow_sudo: true,
            ..yes(WriterChoice::Ollama)
        },
    )
    .await;
    assert!(!sandbox::run(&mut o).await);
    let calls = shims.calls();
    assert!(
        !calls
            .iter()
            .any(|call| call.starts_with("sudo") || call.contains("system migrate")),
        "{calls:?}"
    );
    assert_eq!(o.steps.len(), 1, "{:?}", o.steps);
    assert!(
        o.steps[0].ends_with(
            "Choose 65536 IDs that no line of /etc/subuid or /etc/subgid uses (none is free from \
             100000 up to 4294967294 here) and add them: sudo usermod --add-subuids FIRST-LAST \
             --add-subgids FIRST-LAST alex, then: podman system migrate"
        ),
        "{:?}",
        o.steps
    );
}

/// Doctor judges a Podman reached through CONTAINER_HOST by its own `podman
/// info`, as before it read the files: no false FAIL for this computer's
/// /etc/subuid, and when that Podman maps one id, a FAIL whose fix runs
/// nothing here.
#[cfg(unix)]
#[tokio::test]
async fn doctor_judges_a_podman_reached_elsewhere_by_its_podman_info() {
    let shims = Shims::new();
    stale_podman(&shims);
    podman_state(&shims, &["machine", "running"]);
    shims.subordinate_ids(Some("newuser:100000:65536\n"));
    let env = crate::doctor::Environment {
        os: ubuntu(),
        host: shims.host(false),
        podman_connection: ConnectionOverride::Host(
            "ssh://builder/run/user/1000/podman/podman.sock".to_string(),
        ),
        ..doctor_env(&shims)
    };
    let config_path = shims.home.path().join("config.yaml");
    let report = crate::doctor::collect(&config_path, &env).await;
    assert!(
        !report
            .sections
            .sandbox
            .iter()
            .any(|item| item.name == "User ID ranges"),
        "{:?}",
        report.sections.sandbox
    );
    podman_state(&shims, &["stale"]);
    let report = crate::doctor::collect(&config_path, &env).await;
    let ranges = report
        .sections
        .sandbox
        .iter()
        .find(|item| item.name == "User ID ranges")
        .unwrap_or_else(|| panic!("{:?}", report.sections.sandbox));
    assert_eq!(ranges.status, crate::doctor::Status::Fail);
    let fix = ranges.fix.as_deref().unwrap();
    assert!(
        ranges
            .detail
            .contains("CONTAINER_HOST=ssh://builder/run/user/1000/podman/podman.sock"),
        "{ranges:?}"
    );
    assert!(
        fix.contains("then run `podman system migrate` as that user there")
            && !fix.contains("&& podman system migrate")
            && !fix.contains("sudo")
            && !fix.contains("alex"),
        "{fix}"
    );
    assert!(
        fix.ends_with("Or unset CONTAINER_HOST to use your own rootless Podman on this computer"),
        "{fix}"
    );
}

/// Ubuntu 22.04's Podman 3.4.4 is refused by name, before any install.
#[cfg(unix)]
#[tokio::test]
async fn ubuntu_22_04_is_refused_with_the_podman_floor() {
    let shims = Shims::new();
    shims.add("sudo", "exit 0");
    let facts = facts(
        Os::Linux {
            release: OsRelease::parse("ID=ubuntu\nID_LIKE=debian\nVERSION_ID=\"22.04\"\n"),
            wsl: None,
            systemd: true,
        },
        64,
        no_podman(),
        &shims,
        "http://127.0.0.1:9",
        OllamaService::Manual,
    )
    .await;
    let mut o = onboarding(
        &shims,
        Ui::new(Mode::Yes, true, None),
        facts,
        yes(WriterChoice::Openrouter),
    );
    assert!(!sandbox::run(&mut o).await);
    assert_eq!(
        o.steps,
        ["Ubuntu 22.04 ships Podman 3.4.4. Axocoatl runs need Podman 4.3 or later (4.9 tested). Use Ubuntu 24.04 or newer."]
    );
    assert!(shims.calls().is_empty());
}

/// A rootful machine (Podman Desktop's default): the switch is asked with
/// No as the default; yes runs stop, set --rootful=false and start. No
/// offers Axocoatl its own rootless machine instead (also No by default):
/// yes creates it and leaves the step to use it, no leaves a step with both
/// ways out.
#[cfg(unix)]
#[tokio::test]
async fn a_rootful_machine_is_switched_only_with_consent() {
    for answers in [
        vec![Answer::Yes, Answer::Yes],
        vec![Answer::No, Answer::No],
        vec![Answer::No, Answer::Yes],
    ] {
        let shims = Shims::new();
        podman_shim(&shims);
        podman_state(&shims, &["machine", "running", "rootful"]);
        let report = podman_report(&shims).await;
        assert_eq!(report.rootless(), Some(false));
        let found = facts(
            mac("26.6"),
            48,
            report,
            &shims,
            "http://127.0.0.1:9",
            OllamaService::Manual,
        )
        .await;
        let mut o = onboarding(
            &shims,
            Ui::scripted(answers.clone()),
            found,
            OnboardArgs::default(),
        );
        let ready = sandbox::run(&mut o).await;
        let calls = shims.calls();
        let output = o.ui.output();
        assert!(output.contains("would stop with exit 5"), "{output}");
        assert!(output.contains("[y/N]"), "{output}");
        match answers[..] {
            [Answer::Yes, _] => {
                assert!(ready, "{output}\n{:?}", o.steps);
                for expected in [
                    "podman machine stop",
                    "podman machine set --rootful=false",
                    "podman machine start",
                ] {
                    assert!(calls.iter().any(|call| call == expected), "{calls:?}");
                }
            }
            [Answer::No, Answer::No] => {
                assert!(!ready);
                assert!(
                    !calls.iter().any(|call| call.contains("--rootful")
                        || call.starts_with("podman machine init")),
                    "{calls:?}"
                );
                assert!(
                    o.steps[0].contains("podman machine set --rootful=false"),
                    "{:?}",
                    o.steps
                );
                assert!(
                    o.steps[0].contains("CONTAINER_CONNECTION=axocoatl"),
                    "{:?}",
                    o.steps
                );
            }
            _ => {
                assert!(!ready);
                assert!(
                    calls.iter().any(|call| call
                        == "podman machine init --cpus 6 --memory 8192 --disk-size 60 axocoatl"),
                    "{calls:?}"
                );
                assert!(!calls.iter().any(|call| call.contains("--rootful")));
                assert!(
                    o.steps[0].starts_with(
                        "Use Axocoatl's own rootless machine: stop \
                         podman-machine-default (`podman machine stop`), then start Axocoatl \
                         from a shell with `export CONTAINER_CONNECTION=axocoatl`"
                    ),
                    "{:?}",
                    o.steps
                );
            }
        }
    }
}

/// A stopped machine is started after asking; the proof container then
/// runs with a hardened Session's shape.
#[cfg(unix)]
#[tokio::test]
async fn a_stopped_machine_is_started_and_proved_with_one_container() {
    let shims = Shims::new();
    podman_shim(&shims);
    podman_state(&shims, &["machine"]);
    let report = podman_report(&shims).await;
    if !cfg!(target_os = "macos") {
        // The stopped state is a macOS (machine) state.
        return;
    }
    assert_eq!(report.readiness, PodmanReadiness::MachineStopped);
    let facts = facts(
        mac("26.6"),
        48,
        report,
        &shims,
        "http://127.0.0.1:9",
        OllamaService::Manual,
    )
    .await;
    let mut o = onboarding(
        &shims,
        Ui::new(Mode::Yes, false, None),
        facts,
        OnboardArgs::default(),
    );
    assert!(sandbox::run(&mut o).await, "{}", o.ui.output());
    let calls = shims.calls();
    assert!(
        calls.iter().any(|call| call == "podman machine start"),
        "{calls:?}"
    );
    assert!(
        calls.iter().any(|call| call
            == "podman run --rm --pull=never --userns=keep-id:uid=1000,gid=1000 --user 1000:1000 \
            --network=none --cap-drop=all --security-opt=no-new-privileges \
            docker.io/library/alpine:3.20 id -u"),
        "{calls:?}"
    );
    assert!(o
        .ui
        .output()
        .contains("Sandbox ready: Podman 5.8.2, applehv, rootless, 6 CPUs / 8 GiB"));
}

/// --plan runs only read-only commands and writes nothing.
#[cfg(unix)]
#[tokio::test]
async fn plan_changes_nothing() {
    let shims = Shims::new();
    shims.add(
        "brew",
        "case \"$*\" in --version) echo 'Homebrew 7.0.10';; esac",
    );
    ollama_shim(&shims);
    let ollama = FakeOllama::start("0.40.6", shims.home.path(), shims.dir.path(), &[]).await;
    let facts = facts(
        mac("26.6"),
        48,
        no_podman(),
        &shims,
        &ollama.url(),
        OllamaService::Brew,
    )
    .await;
    let before = tree(shims.home.path());
    let mut o = onboarding(
        &shims,
        Ui::new(Mode::Plan, false, None),
        facts,
        OnboardArgs {
            plan: true,
            writer: Some(WriterChoice::Ollama),
            ollama_url: Some(ollama.url()),
            ..OnboardArgs::default()
        },
    );
    assert_eq!(run(&mut o).await, exit::READY);
    assert_eq!(tree(shims.home.path()), before);
    assert!(shims.calls().is_empty(), "{:?}", shims.calls());
    let output = o.ui.output();
    for expected in [
        "would run: brew install podman",
        "would run: podman machine init --cpus 6 --memory 8192 --disk-size 60",
        "would write ",
        "Plan only: nothing was changed.",
    ] {
        assert!(output.contains(expected), "{expected}\n{output}");
    }
}

fn tree(directory: &Path) -> Vec<(PathBuf, Vec<u8>)> {
    let mut entries = Vec::new();
    for entry in std::fs::read_dir(directory).into_iter().flatten().flatten() {
        let path = entry.path();
        if path.is_dir() {
            entries.push((path.clone(), Vec::new()));
            entries.extend(tree(&path));
        } else {
            entries.push((path.clone(), std::fs::read(&path).unwrap_or_default()));
        }
    }
    entries.sort();
    entries
}

/// Below 48 GB no local pair is offered by default. At the terminal the
/// person is steered to OpenRouter (the menu's default) and nothing is
/// pulled; with --yes an explicit `--writer ollama` is never turned into a
/// hosted writer: naming the models is a step left.
#[cfg(unix)]
#[tokio::test]
async fn a_16_gb_computer_is_steered_to_openrouter_and_pulls_nothing() {
    let shims = Shims::new();
    ollama_shim(&shims);
    let ollama = FakeOllama::start("0.40.6", shims.home.path(), shims.dir.path(), &[]).await;
    let missing_variable = format!("AXOCOATL_ONBOARD_TEST_NO_KEY_{}", std::process::id());
    for interactive in [true, false] {
        let facts = facts(
            mac("26.6"),
            16,
            no_podman(),
            &shims,
            &ollama.url(),
            OllamaService::Brew,
        )
        .await;
        let ui = if interactive {
            // The menu's default, then yes to credits, the default models.
            Ui::scripted(vec![
                Answer::Enter,
                Answer::Yes,
                Answer::Enter,
                Answer::Enter,
            ])
        } else {
            Ui::new(Mode::Yes, false, None)
        };
        let mut o = onboarding(
            &shims,
            ui,
            facts,
            OnboardArgs {
                ollama_url: Some(ollama.url()),
                podman: Some(PodmanChoice::Skip),
                openrouter_key_env: Some(missing_variable.clone()),
                yes: !interactive,
                ..yes(WriterChoice::Ollama)
            },
        );
        let mut choice = Choice::default();
        ollama::writer_and_reviewer(&mut o, &mut choice).await;
        let output = o.ui.output();
        assert!(
            !shims.calls().iter().any(|call| call.contains("pull")),
            "{:?}",
            shims.calls()
        );
        assert!(choice.writer_model.is_none());
        if interactive {
            assert!(
                output.contains("No local pair has been tested on 16 GB of memory"),
                "{output}"
            );
            assert!(
                o.steps
                    .iter()
                    .any(|step| step.contains(&format!("Export {missing_variable}"))),
                "{:?}",
                o.steps
            );
        } else {
            assert_eq!(o.steps.len(), 1, "{:?}", o.steps);
            assert!(
                o.steps[0].starts_with("No local pair has been tested on 16 GB of memory")
                    && o.steps[0].contains("--writer-model ollama:MODEL"),
                "{:?}",
                o.steps
            );
            assert!(!output.contains("OpenRouter credits"), "{output}");
        }
    }
}

/// A pull that does not fit the disk is not started without a person:
/// under --yes it is a step left with the size it needs.
#[cfg(unix)]
#[tokio::test]
async fn yes_never_starts_a_pull_that_does_not_fit_the_disk() {
    let shims = Shims::new();
    ollama_shim(&shims);
    let ollama = FakeOllama::start("0.40.6", shims.home.path(), shims.dir.path(), &[]).await;
    std::fs::write(shims.dir.path().join("no-cloud"), "").unwrap();
    let mut facts = facts(
        mac("26.6"),
        48,
        no_podman(),
        &shims,
        &ollama.url(),
        OllamaService::Brew,
    )
    .await;
    facts.disk_free_bytes = Some(12_500_000_000);
    let mut o = onboarding(
        &shims,
        Ui::new(Mode::Yes, false, None),
        facts,
        OnboardArgs {
            ollama_url: Some(ollama.url()),
            ..yes(WriterChoice::Ollama)
        },
    );
    let mut choice = Choice::default();
    ollama::writer_and_reviewer(&mut o, &mut choice).await;
    let output = o.ui.output();
    assert!(
        output.contains("Needs 35.6 GB free; 12.5 GB free here. no: it does not fit this disk"),
        "{output}"
    );
    assert!(
        !shims
            .calls()
            .iter()
            .any(|call| call.starts_with("ollama pull")),
        "{:?}",
        shims.calls()
    );
    assert!(o.steps.contains(
        &"Free disk space for qwen3-coder:30b, then pull it: ollama pull qwen3-coder:30b"
            .to_string()
    ));
}

/// A failed pull shows Ollama's own error and leaves the pull as a step.
#[cfg(unix)]
#[tokio::test]
async fn a_failed_pull_is_reported_with_ollamas_message() {
    let shims = Shims::new();
    shims.add(
        "ollama",
        "case \"$1\" in pull) echo 'pulling manifest' ; echo 'Error: pull model manifest: file does not exist' >&2; exit 1;; esac",
    );
    let ollama = FakeOllama::start("0.40.6", shims.home.path(), shims.dir.path(), &[]).await;
    std::fs::create_dir_all(shims.home.path().join(".ollama")).unwrap();
    std::fs::write(
        shims.home.path().join(".ollama/server.json"),
        r#"{"disable_ollama_cloud":true}"#,
    )
    .unwrap();
    let facts = facts(
        mac("26.6"),
        48,
        no_podman(),
        &shims,
        &ollama.url(),
        OllamaService::Brew,
    )
    .await;
    let mut o = onboarding(
        &shims,
        Ui::new(Mode::Yes, false, None),
        facts,
        OnboardArgs {
            ollama_url: Some(ollama.url()),
            ..yes(WriterChoice::Ollama)
        },
    );
    let mut choice = Choice::default();
    ollama::writer_and_reviewer(&mut o, &mut choice).await;
    let output = o.ui.output();
    assert!(
        output.contains("ollama pull qwen3-coder:30b failed: `OLLAMA_HOST=")
            && output.contains("failed (exit 1): Error: pull model manifest: file does not exist"),
        "{output}"
    );
    assert!(o
        .steps
        .contains(&"Pull qwen3-coder:30b: ollama pull qwen3-coder:30b".to_string()));
}

/// OpenRouter with --yes and a key from an environment variable: separate
/// writer and reviewer models, the key checked and both models listed
/// before anything is written, the key named (not stored).
#[cfg(unix)]
#[tokio::test]
async fn openrouter_writes_a_separate_reviewer_after_checking_key_and_models() {
    let shims = Shims::new();
    let openrouter = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/key"))
        .and(wiremock::matchers::header(
            "authorization",
            "Bearer sk-or-test-onboard",
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": {"label": "x"}})))
        .mount(&openrouter)
        .await;
    Mock::given(method("GET"))
        .and(path("/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": [
            {"id": "qwen/qwen3-coder"}, {"id": "openai/gpt-oss-120b"}
        ]})))
        .mount(&openrouter)
        .await;
    let variable = format!("AXOCOATL_ONBOARD_TEST_KEY_{}", std::process::id());
    std::env::set_var(&variable, "sk-or-test-onboard");
    podman_shim(&shims);
    podman_state(&shims, &["machine", "running"]);
    let report = podman_report(&shims).await;
    let facts = facts(
        mac("26.6"),
        16,
        report,
        &shims,
        "http://127.0.0.1:9",
        OllamaService::Manual,
    )
    .await;
    shims.add("axocoatl", "exit 0");
    let mut o = onboarding(
        &shims,
        Ui::new(Mode::Yes, false, None),
        facts,
        OnboardArgs {
            openrouter_key_env: Some(variable.clone()),
            start: Some(StartChoice::Dev),
            ..yes(WriterChoice::Openrouter)
        },
    );
    o.openrouter_api = openrouter.uri();
    let code = run(&mut o).await;
    let output = o.ui.output();
    assert_eq!(code, exit::READY, "{output}\n{:?}", o.steps);
    // `axocoatl dev` runs as its own process, with the signals' defaults.
    assert!(
        shims.calls().contains(&format!(
            "axocoatl dev --config {}",
            o.paths.config_path.display()
        )),
        "{:?}",
        shims.calls()
    );
    let text = config_text(&o);
    let config = axocoatl_config::parse_config(&text, &o.paths.config_path).unwrap();
    let model = |id: &str| {
        config
            .agents
            .iter()
            .find(|a| a.id == id)
            .unwrap()
            .model
            .clone()
    };
    assert_eq!(model("lead"), "qwen/qwen3-coder");
    assert_eq!(model("reviewer"), "openai/gpt-oss-120b");
    assert_ne!(model("lead"), model("reviewer"));
    assert!(text.contains(&format!("api_key: \"${{{variable}}}\"")));
    assert!(!text.contains("sk-or-test-onboard"));
    assert!(!output.contains("sk-or-test-onboard"));
    assert!(output.contains("OpenRouter accepted the key"));
    assert!(output.contains("OpenRouter lists openai/gpt-oss-120b"));
    std::env::remove_var(&variable);
}

/// Without an OpenRouter key nothing is saved, and the step left repeats
/// the writer: Claude Code or Codex (with its --writer-model), a local
/// writer, or OpenRouter, so running it again does not ask Step 1 again.
#[cfg(unix)]
#[tokio::test]
async fn the_no_key_step_repeats_the_writer_chosen() {
    let variable = format!("AXOCOATL_ONBOARD_TEST_NO_KEY_W_{}", std::process::id());
    for (external, model, local, expected) in [
        (
            Some(WriterChoice::Codex),
            Some("openai:gpt-5.1-codex"),
            None,
            "--writer codex --writer-model openai:gpt-5.1-codex",
        ),
        (
            Some(WriterChoice::ClaudeCode),
            None,
            None,
            "--writer claude-code",
        ),
        (
            None,
            None,
            Some("qwen3-coder:30b"),
            "--writer ollama --writer-model ollama:qwen3-coder:30b",
        ),
    ] {
        let shims = Shims::new();
        let facts = facts(
            ubuntu(),
            16,
            no_podman(),
            &shims,
            "http://127.0.0.1:9",
            OllamaService::Manual,
        )
        .await;
        let mut o = onboarding(
            &shims,
            Ui::new(Mode::Yes, false, None),
            facts,
            OnboardArgs {
                openrouter_key_env: Some(variable.clone()),
                ..yes(external.unwrap_or(WriterChoice::Ollama))
            },
        );
        let mut choice = Choice {
            external_writer: external,
            external_writer_model: model.map(str::to_string),
            writer_model: local.map(|model| ModelRef::new(Provider::Ollama, model)),
            ..Choice::default()
        };
        openrouter::run(&mut o, &mut choice, false).await;
        assert!(choice.openrouter_key.is_none());
        assert_eq!(
            o.steps,
            [format!(
                "Export {variable} with your OpenRouter key in this shell, then run `axocoatl \
                 onboard {expected} --reviewer openrouter:openai/gpt-oss-120b \
                 --openrouter-key-env {variable}`"
            )]
        );
    }
}

/// The same model for writer and reviewer is refused on the command line.
#[test]
fn the_same_model_twice_is_a_usage_error() {
    let args = OnboardArgs {
        writer: Some(WriterChoice::Openrouter),
        writer_model: Some("openrouter:qwen/qwen3-coder".into()),
        reviewer: Some("openrouter:qwen/qwen3-coder".into()),
        ..OnboardArgs::default()
    };
    assert!(args.check().unwrap_err().contains("different model"));
    let anthropic = OnboardArgs {
        reviewer: Some("anthropic:opus".into()),
        ..OnboardArgs::default()
    };
    assert!(anthropic
        .check()
        .unwrap_err()
        .contains("cannot run native Agents"));
    let remote = OnboardArgs {
        ollama_url: Some("http://192.0.2.10:11434".into()),
        ..OnboardArgs::default()
    };
    assert!(remote
        .check()
        .unwrap_err()
        .contains("only an Ollama server on this computer"));
    assert!(OnboardArgs {
        openrouter_key_env: Some("sk-or-v1-abc".into()),
        ..OnboardArgs::default()
    }
    .check()
    .is_err());
    // An OpenRouter writer's reviewer runs on OpenRouter: a local one is
    // refused, never silently replaced.
    let mixed = OnboardArgs {
        writer: Some(WriterChoice::Openrouter),
        reviewer: Some("ollama:gpt-oss:20b".into()),
        ..OnboardArgs::default()
    };
    assert!(mixed
        .check()
        .unwrap_err()
        .contains("with --writer openrouter the reviewer runs on OpenRouter too"));
}

/// Claude Code with --yes: the CLI found, the subscription said, the image
/// built through `axocoatl recipe build`, sign-in left as a step (no
/// --connect), the reviewer from --reviewer, and the configuration with
/// only the reviewer default.
#[cfg(unix)]
#[tokio::test]
async fn claude_code_builds_the_image_and_leaves_sign_in_to_a_terminal() {
    let shims = Shims::new();
    shims.add("claude", "echo '2.1.292 (Claude Code)'");
    shims.add("axocoatl", "exit 0");
    podman_shim(&shims);
    podman_state(&shims, &["machine", "running"]);
    ollama_shim(&shims);
    let ollama = FakeOllama::start(
        "0.40.6",
        shims.home.path(),
        shims.dir.path(),
        &["gpt-oss:20b"],
    )
    .await;
    std::fs::write(shims.dir.path().join("no-cloud"), "").unwrap();
    std::fs::write(shims.dir.path().join("big-context"), "").unwrap();
    let report = podman_report(&shims).await;
    let mut facts = facts(
        mac("26.6"),
        48,
        report,
        &shims,
        &ollama.url(),
        OllamaService::Brew,
    )
    .await;
    facts.claude = Some((shims.dir.path().join("claude"), Some("2.1.292".into())));
    let mut o = onboarding(
        &shims,
        Ui::new(Mode::Yes, false, None),
        facts,
        OnboardArgs {
            ollama_url: Some(ollama.url()),
            reviewer: Some("ollama:gpt-oss:20b".into()),
            ..yes(WriterChoice::ClaudeCode)
        },
    );
    let code = run(&mut o).await;
    let output = o.ui.output();
    assert_eq!(code, exit::NOT_READY, "{output}");
    assert!(output.contains("needs a subscription"), "{output}");
    assert!(
        output.contains("Claude Code runs in your repository's language image"),
        "{output}"
    );
    assert!(!output.contains("Node repositories only"), "{output}");
    let calls = shims.calls();
    assert!(
        calls
            .iter()
            .any(|call| call.starts_with("axocoatl recipe build claude-code --config ")),
        "{calls:?}"
    );
    assert!(
        !calls
            .iter()
            .any(|call| call.starts_with("axocoatl connect")),
        "{calls:?}"
    );
    assert!(o
        .steps
        .contains(&"Sign in to Claude Code: axocoatl connect claude-code".to_string()));
    assert_eq!(
        config::run_models(&config_text(&o)),
        RunModels {
            writer: None,
            reviewer: Some("ollama:gpt-oss:20b".into()),
        }
    );
}

/// Codex: the billing is written explicitly on the run command, never
/// left to `auto`.
#[test]
fn codex_names_its_credential_in_the_next_command() {
    let choice = Choice {
        reviewer_model: Some(config::ModelRef::new(
            config::Provider::Ollama,
            "gpt-oss:20b",
        )),
        codex_credential: Some("codex-chatgpt"),
        ..Choice::default()
    };
    let next = next_command(WriterChoice::Codex, &choice);
    assert!(
        next.contains(
            "axocoatl run fix-codex --task \"Describe the bug\" --param credential=codex-chatgpt"
        ),
        "{next}"
    );
    assert!(next.contains("reviewer ollama:gpt-oss:20b comes from your setup"));
}

/// Step 1's default follows what is installed and the memory.
#[tokio::test]
async fn the_default_writer_follows_what_is_installed() {
    let shims = Shims::new();
    let mut facts = facts(
        mac("26.6"),
        48,
        no_podman(),
        &shims,
        "http://127.0.0.1:9",
        OllamaService::Manual,
    )
    .await;
    assert_eq!(default_writer(&facts), WriterChoice::Ollama);
    facts.memory_bytes = Some(16 * GIB);
    assert_eq!(default_writer(&facts), WriterChoice::Openrouter);
    facts.codex = Some(PathBuf::from("/bin/codex"));
    assert_eq!(default_writer(&facts), WriterChoice::Codex);
    facts.claude = Some((PathBuf::from("/bin/claude"), None));
    assert_eq!(default_writer(&facts), WriterChoice::ClaudeCode);
}

/// Running onboard again updates the existing file: backed up, comments
/// kept, only the run defaults and port changed.
#[cfg(unix)]
#[tokio::test]
async fn an_existing_configuration_is_backed_up_and_updated() {
    let shims = Shims::new();
    podman_shim(&shims);
    podman_state(&shims, &["machine", "running"]);
    ollama_shim(&shims);
    let ollama = FakeOllama::start(
        "0.40.6",
        shims.home.path(),
        shims.dir.path(),
        &["qwen3-coder:30b", "gpt-oss:20b"],
    )
    .await;
    std::fs::write(shims.dir.path().join("no-cloud"), "").unwrap();
    std::fs::write(shims.dir.path().join("big-context"), "").unwrap();
    let report = podman_report(&shims).await;
    let facts = facts(
        mac("26.6"),
        48,
        report,
        &shims,
        &ollama.url(),
        OllamaService::Brew,
    )
    .await;
    let mut o = onboarding(
        &shims,
        Ui::new(Mode::Yes, false, None),
        facts,
        OnboardArgs {
            ollama_url: Some(ollama.url()),
            update: true,
            ..yes(WriterChoice::Ollama)
        },
    );
    std::fs::create_dir_all(&o.paths.config_dir).unwrap();
    let old = format!(
        "# my own notes\nagents:\n  - id: lead\n    name: \"Lead\"\n    provider: ollama\n    model: \"qwen3-coder:30b\"\n    tools: [read_file]\n\nproviders:\n  ollama:\n    base_url: \"{}\"\n",
        ollama.url()
    );
    std::fs::write(&o.paths.config_path, &old).unwrap();
    let code = run(&mut o).await;
    let output = o.ui.output();
    assert_eq!(code, exit::READY, "{output}\n{:?}", o.steps);
    let new = config_text(&o);
    assert!(
        new.starts_with("# my own notes\nagents:\n  - id: lead\n"),
        "{new}"
    );
    assert_eq!(
        config::run_models(&new).reviewer.as_deref(),
        Some("ollama:gpt-oss:20b")
    );
    let backups: Vec<_> = std::fs::read_dir(&o.paths.config_dir)
        .unwrap()
        .flatten()
        .filter(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .starts_with("config.yaml.bak-")
        })
        .collect();
    assert_eq!(backups.len(), 1);
    assert_eq!(std::fs::read_to_string(backups[0].path()).unwrap(), old);
    assert!(output.contains("+ run:"), "{output}");
}

/// At the terminal, the same OpenRouter model for the reviewer is refused
/// and asked again.
#[tokio::test]
async fn an_interactive_reviewer_equal_to_the_writer_is_asked_again() {
    let shims = Shims::new();
    let openrouter = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/key"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": {}})))
        .mount(&openrouter)
        .await;
    Mock::given(method("GET"))
        .and(path("/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": [
            {"id": "qwen/qwen3-coder"}, {"id": "z-ai/glm-4.6"}
        ]})))
        .mount(&openrouter)
        .await;
    let variable = format!("AXOCOATL_ONBOARD_TEST_KEY_I_{}", std::process::id());
    std::env::set_var(&variable, "sk-or-interactive");
    let facts = facts(
        mac("26.6"),
        16,
        no_podman(),
        &shims,
        "http://127.0.0.1:9",
        OllamaService::Manual,
    )
    .await;
    let mut o = onboarding(
        &shims,
        Ui::scripted(vec![
            Answer::Yes,
            Answer::Text("qwen/qwen3-coder".into()),
            Answer::Text("qwen/qwen3-coder".into()),
            Answer::Text("z-ai/glm-4.6".into()),
        ]),
        facts,
        OnboardArgs {
            openrouter_key_env: Some(variable.clone()),
            ..OnboardArgs::default()
        },
    );
    o.openrouter_api = openrouter.uri();
    let mut choice = Choice::default();
    openrouter::run(&mut o, &mut choice, true).await;
    std::env::remove_var(&variable);
    assert!(o.steps.is_empty(), "{:?}", o.steps);
    assert!(o
        .ui
        .output()
        .contains("The reviewer must be a different model"));
    assert_eq!(choice.writer_model.unwrap().model, "qwen/qwen3-coder");
    assert_eq!(choice.reviewer_model.unwrap().model, "z-ai/glm-4.6");
}

/// An existing configuration can be left alone: nothing is written, and
/// it is checked as it is (exit 0 only when that check passes).
#[tokio::test]
async fn an_existing_configuration_can_be_left_unchanged() {
    let shims = Shims::new();
    let facts = facts(
        mac("26.6"),
        48,
        no_podman(),
        &shims,
        "http://127.0.0.1:9",
        OllamaService::Manual,
    )
    .await;
    let mut o = onboarding(
        &shims,
        Ui::scripted(vec![Answer::Pick(2)]),
        facts,
        OnboardArgs::default(),
    );
    std::fs::create_dir_all(&o.paths.config_dir).unwrap();
    std::fs::write(&o.paths.config_path, "agents: []\n").unwrap();
    assert_eq!(run(&mut o).await, exit::NOT_READY);
    assert_eq!(config_text(&o), "agents: []\n");
    let output = o.ui.output();
    assert!(output.contains("Configuration left unchanged"), "{output}");
    assert!(output.contains("Checking it as it is"), "{output}");
    assert!(output.contains("Not ready yet"), "{output}");
    assert!(
        o.steps
            .iter()
            .any(|step| step.contains("Podman is not installed")),
        "{:?}",
        o.steps
    );
    assert!(shims.calls().is_empty());
}

/// A connection that exists but does not answer is never replaced by a new
/// machine: onboarding stops with what to check.
#[cfg(unix)]
#[tokio::test]
async fn an_existing_connection_is_never_replaced_by_a_new_machine() {
    let shims = Shims::new();
    podman_shim(&shims);
    podman_state(&shims, &["connection"]);
    let report = podman_report(&shims).await;
    if !cfg!(target_os = "macos") {
        return;
    }
    assert_eq!(report.readiness, PodmanReadiness::MachineMissing);
    let facts = facts(
        mac("26.6"),
        48,
        report,
        &shims,
        "http://127.0.0.1:9",
        OllamaService::Manual,
    )
    .await;
    let mut o = onboarding(
        &shims,
        Ui::new(Mode::Yes, false, None),
        facts,
        OnboardArgs::default(),
    );
    assert!(!sandbox::run(&mut o).await);
    assert!(
        !shims
            .calls()
            .iter()
            .any(|call| call.contains("machine init")),
        "{:?}",
        shims.calls()
    );
    assert!(
        o.steps[0].contains("Podman has connections (podman-machine-default)"),
        "{:?}",
        o.steps
    );
}

#[test]
fn connection_names_are_read_from_podman() {
    assert_eq!(
        sandbox::connection_names(
            r#"[{"Name":"podman-machine-default","URI":"ssh://x"},{"Name":"remote"}]"#
        ),
        ["podman-machine-default", "remote"]
    );
    assert!(sandbox::connection_names("[]").is_empty());
    assert!(sandbox::connection_names("not json").is_empty());
}

/// Step 0 says what this computer has and what is missing.
#[tokio::test]
async fn step_0_names_the_computer_and_what_is_missing() {
    let shims = Shims::new();
    let mut facts = facts(
        mac("26.6"),
        48,
        no_podman(),
        &shims,
        "http://127.0.0.1:9",
        OllamaService::Manual,
    )
    .await;
    facts.brew = Some("7.0.10".into());
    let lines = detect::summary(&facts);
    assert_eq!(
        lines[0],
        "This computer: macOS 26.6, Apple M5 Pro, 48 GB memory, 18 cores, 400 GB free."
    );
    assert_eq!(lines[1], "Found: Homebrew 7.0.10, git 2.39.5.");
    assert_eq!(lines[2], "Not found: Podman, Ollama, claude, codex, gh.");
    facts.os = Os::Linux {
        release: OsRelease::parse("PRETTY_NAME=\"Ubuntu 24.04.5 LTS\"\nID=ubuntu\n"),
        wsl: Some(detect::Wsl::Two),
        systemd: false,
    };
    let lines = detect::summary(&facts);
    assert!(
        lines[0].starts_with("This computer: Ubuntu 24.04.5 LTS (WSL2), "),
        "{}",
        lines[0]
    );
    assert!(lines[0].contains("48 GB memory (by default WSL gives Linux half of Windows' memory)"));
}

/// --plan over an existing configuration shows the update it would make and
/// leaves the file as it was.
#[tokio::test]
async fn plan_over_an_existing_configuration_changes_nothing() {
    let shims = Shims::new();
    let facts = facts(
        mac("26.6"),
        16,
        no_podman(),
        &shims,
        "http://127.0.0.1:9",
        OllamaService::Manual,
    )
    .await;
    let mut o = onboarding(
        &shims,
        Ui::new(Mode::Plan, false, None),
        facts,
        OnboardArgs {
            plan: true,
            writer: Some(WriterChoice::Openrouter),
            podman: Some(PodmanChoice::Skip),
            ..OnboardArgs::default()
        },
    );
    std::fs::create_dir_all(&o.paths.config_dir).unwrap();
    std::fs::write(&o.paths.config_path, "agents: []\n").unwrap();
    let before = tree(shims.home.path());
    assert_eq!(run(&mut o).await, exit::READY, "{}", o.ui.output());
    assert_eq!(tree(shims.home.path()), before);
    assert!(
        o.ui.output().contains("would ask: Write these changes?"),
        "{}",
        o.ui.output()
    );
}

/// An Ollama that is installed but not running, and that only a person can
/// start (the app, or `ollama serve`), is a step left at once under --yes,
/// without waiting for it.
#[cfg(unix)]
#[tokio::test]
async fn a_stopped_ollama_nobody_can_start_is_a_step_without_waiting() {
    let shims = Shims::new();
    ollama_shim(&shims);
    let port = free_port();
    let url = format!("http://127.0.0.1:{port}");
    let facts = facts(
        mac("26.6"),
        48,
        no_podman(),
        &shims,
        &url,
        OllamaService::Manual,
    )
    .await;
    let mut o = onboarding(
        &shims,
        Ui::new(Mode::Yes, false, None),
        facts,
        OnboardArgs::default(),
    );
    let started = std::time::Instant::now();
    assert_eq!(ollama::server(&mut o).await, None);
    assert!(started.elapsed() < std::time::Duration::from_secs(10));
    assert!(
        o.steps[0].starts_with(&format!(
            "Start Ollama so it answers at {url}: run `OLLAMA_NO_CLOUD=1 ollama serve`"
        )),
        "{:?}",
        o.steps
    );
}

/// A machine with another name than `podman-machine-default`: onboarding
/// starts it and switches it to rootless by name (without one, Podman acts
/// on `podman-machine-default`).
#[cfg(target_os = "macos")]
#[tokio::test]
async fn a_machine_with_its_own_name_is_started_and_switched_by_name() {
    let shims = Shims::new();
    podman_shim_named(&shims, "dev");
    podman_state(&shims, &["machine"]);
    let report = podman_report(&shims).await;
    assert_eq!(report.readiness, PodmanReadiness::MachineStopped);
    let found = facts(
        mac("26.6"),
        48,
        report,
        &shims,
        "http://127.0.0.1:9",
        OllamaService::Manual,
    )
    .await;
    let mut o = onboarding(
        &shims,
        Ui::new(Mode::Yes, false, None),
        found,
        OnboardArgs::default(),
    );
    assert!(sandbox::run(&mut o).await, "{}", o.ui.output());
    let calls = shims.calls();
    assert!(
        calls.iter().any(|call| call == "podman machine start dev"),
        "{calls:?}"
    );
    assert!(!calls.iter().any(|call| call == "podman machine start"));
    assert!(o
        .ui
        .output()
        .contains("Start dev now? (`podman machine start dev`)"));

    let shims = Shims::new();
    podman_shim_named(&shims, "dev");
    podman_state(&shims, &["machine", "running", "rootful"]);
    let report = podman_report(&shims).await;
    let found = facts(
        mac("26.6"),
        48,
        report,
        &shims,
        "http://127.0.0.1:9",
        OllamaService::Manual,
    )
    .await;
    let mut o = onboarding(
        &shims,
        Ui::scripted(vec![Answer::Yes, Answer::Yes]),
        found,
        OnboardArgs::default(),
    );
    assert!(
        sandbox::run(&mut o).await,
        "{}\n{:?}",
        o.ui.output(),
        o.steps
    );
    let calls = shims.calls();
    for expected in [
        "podman machine stop dev",
        "podman machine set --rootful=false dev",
        "podman machine start dev",
    ] {
        assert!(calls.iter().any(|call| call == expected), "{calls:?}");
    }
}

/// Through a `CONTAINER_CONNECTION` that is not a machine onboarding
/// changes no machine: a rootful Podman there is a step that names the
/// connection.
#[cfg(unix)]
#[tokio::test]
async fn a_podman_reached_through_another_connection_is_never_changed() {
    let shims = Shims::new();
    podman_shim(&shims);
    podman_state(&shims, &["machine", "running", "rootful"]);
    let report = axocoatl_isolation::podman_report::inspect_through(
        &shims.dir.path().join("podman").display().to_string(),
        axocoatl_isolation::podman::ConnectionOverride::Connection("remote".into()),
    )
    .await;
    assert_eq!(report.readiness, PodmanReadiness::Ready);
    assert!(report.machine_in_use().is_none());
    let facts = facts(
        mac("26.6"),
        48,
        report,
        &shims,
        "http://127.0.0.1:9",
        OllamaService::Manual,
    )
    .await;
    let mut o = onboarding(
        &shims,
        Ui::new(Mode::Yes, false, None),
        facts,
        OnboardArgs::default(),
    );
    assert!(!sandbox::run(&mut o).await);
    assert!(
        !shims
            .calls()
            .iter()
            .any(|call| call.starts_with("podman machine stop")
                || call.starts_with("podman machine set")
                || call.starts_with("podman machine start")),
        "{:?}",
        shims.calls()
    );
    assert!(
        o.steps[0].contains("reached through CONTAINER_CONNECTION=remote"),
        "{:?}",
        o.steps
    );
    assert!(!o.steps[0].contains("  "), "{:?}", o.steps);
}

/// `--plan` asks every question the Podman setup raises (a rootful
/// machine here) and shows the service and the first run it would start,
/// running only read-only commands and writing nothing.
#[cfg(unix)]
#[tokio::test]
async fn plan_shows_the_rootful_switch_the_service_and_the_first_run() {
    let shims = Shims::new();
    podman_shim(&shims);
    podman_state(&shims, &["machine", "running", "rootful"]);
    let report = podman_report(&shims).await;
    ollama_shim(&shims);
    let ollama = FakeOllama::start(
        "0.40.6",
        shims.home.path(),
        shims.dir.path(),
        &["qwen3-coder:30b", "gpt-oss:20b"],
    )
    .await;
    std::fs::write(shims.dir.path().join("no-cloud"), "").unwrap();
    std::fs::write(shims.dir.path().join("big-context"), "").unwrap();
    let facts = facts(
        mac("26.6"),
        48,
        report,
        &shims,
        &ollama.url(),
        OllamaService::Brew,
    )
    .await;
    let read_only = shims.calls().len();
    let before = tree(shims.home.path());
    let mut o = onboarding(
        &shims,
        Ui::new(Mode::Plan, false, None),
        facts,
        OnboardArgs {
            plan: true,
            writer: Some(WriterChoice::Ollama),
            ollama_url: Some(ollama.url()),
            start: Some(StartChoice::Service),
            smoke_test: true,
            ..OnboardArgs::default()
        },
    );
    assert_eq!(run(&mut o).await, exit::READY);
    assert_eq!(tree(shims.home.path()), before);
    assert_eq!(shims.calls().len(), read_only, "{:?}", shims.calls());
    let output = o.ui.output();
    for expected in [
        "would ask: podman-machine-default runs containers as root",
        "Switch it to rootless? This runs podman machine stop && podman machine set \
         --rootful=false && podman machine start",
        "would ask: Install the Always-On Service",
        "would run: axocoatl service install && axocoatl service start",
        "would copy the sample repository to a temporary directory and run: axocoatl run fix",
        "Plan only: nothing was changed.",
    ] {
        assert!(output.contains(expected), "{expected}\n{output}");
    }
}

/// Running onboarding again keeps the writer and the models the setup
/// has: with `claude` installed, `--yes --update` on a local setup changes
/// only the port it was asked to.
#[cfg(unix)]
#[tokio::test]
async fn update_keeps_the_writer_and_the_models_the_setup_has() {
    let shims = Shims::new();
    shims.add("claude", "echo '2.1.292 (Claude Code)'");
    shims.add("axocoatl", "exit 0");
    podman_shim(&shims);
    podman_state(&shims, &["machine", "running"]);
    ollama_shim(&shims);
    let ollama = FakeOllama::start(
        "0.40.6",
        shims.home.path(),
        shims.dir.path(),
        &["qwen3-coder:30b", "gpt-oss:20b", "qwen3:8b"],
    )
    .await;
    std::fs::write(shims.dir.path().join("no-cloud"), "").unwrap();
    std::fs::write(shims.dir.path().join("big-context"), "").unwrap();
    let report = podman_report(&shims).await;
    let mut facts = facts(
        mac("26.6"),
        48,
        report,
        &shims,
        &ollama.url(),
        OllamaService::Brew,
    )
    .await;
    facts.claude = Some((shims.dir.path().join("claude"), Some("2.1.292".into())));
    let port = free_port();
    let mut o = onboarding(
        &shims,
        Ui::new(Mode::Yes, false, None),
        facts,
        OnboardArgs {
            ollama_url: Some(ollama.url()),
            update: true,
            port: Some(port),
            writer: None,
            ..yes(WriterChoice::Ollama)
        },
    );
    let old = config::render(&config::ConfigPlan {
        ollama_base_url: Some(ollama.url()),
        openrouter_key: None,
        lead: config::ModelRef::new(config::Provider::Ollama, "qwen3-coder:30b"),
        reviewer: config::ModelRef::new(config::Provider::Ollama, "gpt-oss:20b"),
        run_writer: Some(config::ModelRef::new(
            config::Provider::Ollama,
            "qwen3-coder:30b",
        )),
        run_reviewer: Some(config::ModelRef::new(
            config::Provider::Ollama,
            "gpt-oss:20b",
        )),
        port: free_port(),
        note: "local Ollama setup".into(),
        writer: "ollama",
        codex_credential: None,
    });
    std::fs::create_dir_all(&o.paths.config_dir).unwrap();
    std::fs::write(&o.paths.config_path, &old).unwrap();
    let code = run(&mut o).await;
    let output = o.ui.output();
    assert_eq!(code, exit::READY, "{output}\n{:?}", o.steps);
    assert!(
        output.contains("Writer: A local model on this computer (Ollama, no account, needs 48 GB memory or more) (kept from your configuration)"),
        "{output}"
    );
    assert!(
        !shims
            .calls()
            .iter()
            .any(|call| call.starts_with("axocoatl")),
        "{:?}",
        shims.calls()
    );
    let new = config_text(&o);
    let changes = config::diff(&old, &new);
    assert_eq!(changes.len(), 2, "{changes:?}");
    assert!(
        changes[1].ends_with(&format!("port: {port}")),
        "{changes:?}"
    );
}

/// Updating an OpenRouter setup uses the key it has: under --yes the
/// `${VARIABLE}` it names, with nothing to change; at the terminal, "no"
/// to keeping it asks for a key at the hidden prompt, which replaces the
/// reference in place and is never shown.
#[cfg(unix)]
#[tokio::test]
async fn an_openrouter_setup_keeps_its_key_or_replaces_it_in_place() {
    let openrouter = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/key"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": {}})))
        .mount(&openrouter)
        .await;
    Mock::given(method("GET"))
        .and(path("/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": [
            {"id": "qwen/qwen3-coder"}, {"id": "openai/gpt-oss-120b"}
        ]})))
        .mount(&openrouter)
        .await;
    let variable = format!("AXOCOATL_ONBOARD_TEST_KEY_U_{}", std::process::id());
    std::env::set_var(&variable, "sk-or-from-env");
    let old_port = free_port();
    let old = config::render(&config::ConfigPlan {
        ollama_base_url: None,
        openrouter_key: Some(config::KeyRef::Env(variable.clone())),
        lead: config::ModelRef::new(config::Provider::OpenRouter, "qwen/qwen3-coder"),
        reviewer: config::ModelRef::new(config::Provider::OpenRouter, "openai/gpt-oss-120b"),
        run_writer: Some(config::ModelRef::new(
            config::Provider::OpenRouter,
            "qwen/qwen3-coder",
        )),
        run_reviewer: Some(config::ModelRef::new(
            config::Provider::OpenRouter,
            "openai/gpt-oss-120b",
        )),
        port: old_port,
        note: "OpenRouter setup".into(),
        writer: "openrouter",
        codex_credential: None,
    });
    for interactive in [false, true] {
        let shims = Shims::new();
        podman_shim(&shims);
        podman_state(&shims, &["machine", "running"]);
        let report = podman_report(&shims).await;
        let facts = facts(
            mac("26.6"),
            16,
            report,
            &shims,
            "http://127.0.0.1:9",
            OllamaService::Manual,
        )
        .await;
        let ui = if interactive {
            let mut answers = vec![
                Answer::Pick(0), // update this setup
                Answer::Enter,   // the writer it has
                Answer::Yes,     // the sandbox check container
                Answer::Yes,     // credits, no BYOK
                Answer::No,      // keep reading the key from the variable
            ];
            if std::env::var("OPENROUTER_API_KEY").is_ok_and(|value| !value.trim().is_empty()) {
                answers.push(Answer::No);
            }
            answers.extend([
                Answer::Text("sk-or-typed-at-the-prompt".into()),
                Answer::Yes,     // write the change
                Answer::Pick(2), // start it later
                Answer::No,      // no first run
            ]);
            Ui::scripted(answers)
        } else {
            Ui::new(Mode::Yes, false, None)
        };
        let mut o = onboarding(
            &shims,
            ui,
            facts,
            OnboardArgs {
                yes: !interactive,
                update: !interactive,
                port: Some(old_port),
                ..OnboardArgs::default()
            },
        );
        o.openrouter_api = openrouter.uri();
        std::fs::create_dir_all(&o.paths.config_dir).unwrap();
        std::fs::write(&o.paths.config_path, &old).unwrap();
        let code = run(&mut o).await;
        let output = o.ui.output();
        assert_eq!(code, exit::READY, "{output}\n{:?}", o.steps);
        let new = config_text(&o);
        if interactive {
            let config = axocoatl_config::parse_config(&new, &o.paths.config_path).unwrap();
            assert_eq!(
                config.providers.openrouter.unwrap().api_key.expose_secret(),
                "sk-or-typed-at-the-prompt"
            );
            assert_eq!(new.matches("api_key:").count(), 1);
            assert!(output.contains("+     api_key: (hidden)"), "{output}");
            assert!(!output.contains("sk-or-typed-at-the-prompt"), "{output}");
        } else {
            assert_eq!(new, old);
            assert!(output.contains("already has this setup"), "{output}");
            assert!(output.contains(&format!("OpenRouter key: from ${variable}")));
        }
        assert!(!output.contains("sk-or-from-env"));
    }
    std::env::remove_var(&variable);
}

/// The first run never starts a second daemon beside one that is starting
/// (the service Step 6 just started) or already running: it uses that one
/// once it answers, or says it does not.
#[cfg(unix)]
#[tokio::test]
async fn the_first_run_uses_the_running_daemon_and_never_starts_another() {
    let shims = Shims::new();
    shims.add(
        "axocoatl",
        "case \"$1 $2\" in \"receipt key\") echo k-test ;; esac; exit 0",
    );
    let health = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/health"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"status": "ok"})))
        .mount(&health)
        .await;
    let facts = facts(
        mac("26.6"),
        48,
        no_podman(),
        &shims,
        "http://127.0.0.1:9",
        OllamaService::Manual,
    )
    .await;
    let mut o = onboarding(
        &shims,
        Ui::new(Mode::Yes, false, None),
        facts,
        OnboardArgs::default(),
    );
    let write = |o: &Onboarding, port: u16| {
        std::fs::create_dir_all(&o.paths.config_dir).unwrap();
        std::fs::write(
            &o.paths.config_path,
            format!("agents: []\nserver:\n  port: {port}\n  host: \"127.0.0.1\"\n"),
        )
        .unwrap();
    };
    write(&o, health.address().port());
    let choice = Choice {
        writer_model: Some(config::ModelRef::new(
            config::Provider::Ollama,
            "qwen3-coder:30b",
        )),
        reviewer_model: Some(config::ModelRef::new(
            config::Provider::Ollama,
            "gpt-oss:20b",
        )),
        ..Choice::default()
    };
    assert!(
        smoke::run(&mut o, WriterChoice::Ollama, &choice, true).await,
        "{}\n{:?}",
        o.ui.output(),
        o.steps
    );
    let calls = shims.calls();
    assert!(calls
        .iter()
        .any(|call| call.starts_with("axocoatl run fix --task")));
    assert!(calls
        .iter()
        .any(|call| call.starts_with("axocoatl receipt verify")));
    assert!(o.ui.output().contains("Using the daemon running on port"));

    // A daemon answers on the IPC socket but not on this configuration's
    // port: no second daemon, a step to restart it.
    let _ipc = std::os::unix::net::UnixListener::bind(&o.socket_path).unwrap();
    write(&o, free_port());
    assert!(!smoke::run(&mut o, WriterChoice::Ollama, &choice, false).await);
    assert!(
        o.steps
            .iter()
            .any(|step| step.starts_with("An Axocoatl daemon is running but does not answer")),
        "{:?}",
        o.steps
    );
    assert!(
        !shims
            .calls()
            .iter()
            .any(|call| call.starts_with("axocoatl serve")),
        "{:?}",
        shims.calls()
    );
}

/// Replacing a configuration never shows or logs the old file's secrets;
/// the new file is shown with its key hidden.
#[tokio::test]
async fn replace_shows_the_new_file_and_none_of_the_old_secrets() {
    let shims = Shims::new();
    let facts = facts(
        mac("26.6"),
        48,
        no_podman(),
        &shims,
        "http://127.0.0.1:9",
        OllamaService::Manual,
    )
    .await;
    let mut o = onboarding(
        &shims,
        Ui::new(Mode::Yes, false, None),
        facts,
        OnboardArgs::default(),
    );
    let old = "webhooks:\n  - name: ci\n    url: \"https://example.test/hook\"\n    secret: \"whsec-old\"\n    headers:\n      Authorization: \"Bearer tok-old\"\nserver:\n  auth:\n    api_keys:\n      - \"key-old\"\n    bearer_tokens: [\"tok-two\"]\n";
    std::fs::create_dir_all(&o.paths.config_dir).unwrap();
    std::fs::write(&o.paths.config_path, old).unwrap();
    let plan = config::ConfigPlan {
        ollama_base_url: Some("http://localhost:11434".into()),
        openrouter_key: Some(config::KeyRef::Value(zeroize::Zeroizing::new(
            "sk-or-new-value".to_string(),
        ))),
        lead: config::ModelRef::new(config::Provider::Ollama, "qwen3-coder:30b"),
        reviewer: config::ModelRef::new(config::Provider::OpenRouter, "openai/gpt-oss-120b"),
        run_writer: Some(config::ModelRef::new(
            config::Provider::Ollama,
            "qwen3-coder:30b",
        )),
        run_reviewer: Some(config::ModelRef::new(
            config::Provider::OpenRouter,
            "openai/gpt-oss-120b",
        )),
        port: 8231,
        note: "local Ollama setup".into(),
        writer: "ollama",
        codex_credential: None,
    };
    assert!(write_config(
        &mut o,
        &plan,
        &ExistingConfig::Replace(old.to_string())
    ));
    let output = o.ui.output();
    for secret in [
        "whsec-old",
        "tok-old",
        "key-old",
        "tok-two",
        "sk-or-new-value",
    ] {
        assert!(!output.contains(secret), "{secret}\n{output}");
    }
    assert!(output.contains("is replaced (the current file is kept as config.yaml.bak-"));
    assert!(output.contains("+     api_key: (hidden)"), "{output}");
    assert!(config_text(&o).contains("sk-or-new-value"));
}

/// Step 5 does not list again a problem an earlier step left: two models
/// not pulled are two steps, not four.
#[cfg(unix)]
#[tokio::test]
async fn a_problem_is_listed_once_in_the_steps_left() {
    let shims = Shims::new();
    podman_shim(&shims);
    podman_state(&shims, &["machine", "running"]);
    ollama_shim(&shims);
    let ollama = FakeOllama::start("0.40.6", shims.home.path(), shims.dir.path(), &[]).await;
    std::fs::write(shims.dir.path().join("no-cloud"), "").unwrap();
    std::fs::write(shims.dir.path().join("big-context"), "").unwrap();
    let report = podman_report(&shims).await;
    let facts = facts(
        mac("26.6"),
        48,
        report,
        &shims,
        &ollama.url(),
        OllamaService::Brew,
    )
    .await;
    let mut o = onboarding(
        &shims,
        Ui::new(Mode::Yes, false, None),
        facts,
        OnboardArgs {
            ollama_url: Some(ollama.url()),
            no_pull: true,
            ..yes(WriterChoice::Ollama)
        },
    );
    assert_eq!(run(&mut o).await, exit::NOT_READY);
    assert_eq!(
        o.steps,
        [
            "Pull qwen3-coder:30b: ollama pull qwen3-coder:30b",
            "Pull gpt-oss:20b: ollama pull gpt-oss:20b"
        ],
        "{}",
        o.ui.output()
    );
    assert!(o.ui.output().contains("Not ready yet, 2 steps left:"));
}

/// Enter leaves an existing Ollama `server.json` alone: turning cloud
/// models off there is asked with No as the default.
#[cfg(unix)]
#[tokio::test]
async fn enter_leaves_an_existing_ollama_configuration_alone() {
    let shims = Shims::new();
    shims.add("brew", "exit 0");
    ollama_shim(&shims);
    let ollama = FakeOllama::start("0.40.6", shims.home.path(), shims.dir.path(), &[]).await;
    let server_json = shims.home.path().join(".ollama/server.json");
    std::fs::create_dir_all(server_json.parent().unwrap()).unwrap();
    std::fs::write(&server_json, "{\"other\": 1}\n").unwrap();
    let facts = facts(
        mac("26.6"),
        48,
        no_podman(),
        &shims,
        &ollama.url(),
        OllamaService::Brew,
    )
    .await;
    let mut o = onboarding(
        &shims,
        Ui::scripted(vec![Answer::Enter]),
        facts,
        OnboardArgs::default(),
    );
    assert_eq!(ollama::server(&mut o).await, None);
    let output = o.ui.output();
    assert!(
        output.contains("The current file is backed up first. [y/N] Enter"),
        "{output}"
    );
    assert_eq!(
        std::fs::read_to_string(&server_json).unwrap(),
        "{\"other\": 1}\n"
    );
    assert!(
        o.steps[0].starts_with("Turn Ollama's cloud models off"),
        "{:?}",
        o.steps
    );
    assert!(!shims
        .calls()
        .iter()
        .any(|call| call.starts_with("brew services")));
}

/// Doctor on a 16 GB computer with a Claude Code setup whose local
/// reviewer, gpt-oss:20b, is not pulled: too large to pull here, so the fix
/// moves the reviewer to OpenRouter and keeps Claude Code (it used to say
/// `--writer openrouter`).
#[cfg(unix)]
#[tokio::test]
async fn doctor_moves_a_too_large_reviewer_not_the_claude_code_writer() {
    let shims = Shims::new();
    podman_shim(&shims);
    podman_state(&shims, &["machine", "running"]);
    std::fs::write(shims.dir.path().join("no-cloud"), "").unwrap();
    let ollama = FakeOllama::start("0.40.6", shims.home.path(), shims.dir.path(), &[]).await;
    let config_path = shims.home.path().join("config.yaml");
    let reviewer = config::ModelRef::new(config::Provider::Ollama, "gpt-oss:20b");
    std::fs::write(
        &config_path,
        config::render(&config::ConfigPlan {
            ollama_base_url: Some(ollama.url()),
            openrouter_key: None,
            lead: reviewer.clone(),
            reviewer: reviewer.clone(),
            run_writer: None,
            run_reviewer: Some(reviewer),
            port: free_port(),
            note: "Claude Code writes".into(),
            writer: "claude-code",
            codex_credential: None,
        }),
    )
    .unwrap();
    let env = crate::doctor::Environment {
        memory_bytes: Some(16 * GIB),
        ..doctor_env(&shims)
    };
    let report = crate::doctor::collect(&config_path, &env).await;
    let item = report
        .sections
        .models
        .iter()
        .find(|item| item.name == "Model gpt-oss:20b")
        .unwrap_or_else(|| panic!("{:?}", report.sections.models));
    assert_eq!(item.status, crate::doctor::Status::Fail);
    assert_eq!(
        item.detail,
        "gpt-oss:20b is not pulled, and at 13.8 GB it is too large for this computer's 16 GB"
    );
    assert_eq!(
        item.fix.as_deref(),
        Some(
            "Use OpenRouter for the reviewer (`axocoatl onboard --replace --writer claude-code \
             --reviewer openrouter:openai/gpt-oss-120b`), or choose a smaller reviewer (`axocoatl \
             onboard --replace --writer claude-code`) (`--update` would keep the Agents that run \
             gpt-oss:20b; `--replace` backs this configuration up first)"
        )
    );
}

/// Doctor knows which writer the setup chose: a Claude Code setup without
/// its image or its sign-in is not ready, and no `axocoatl run fix` line
/// tells it to switch to a local writer.
#[cfg(unix)]
#[tokio::test]
async fn doctor_fails_a_chosen_claude_code_writer_that_cannot_run() {
    let shims = Shims::new();
    podman_shim(&shims);
    podman_state(&shims, &["machine", "running"]);
    let home = shims.home.path();
    let config_path = home.join("config.yaml");
    std::fs::write(
        &config_path,
        config::render(&config::ConfigPlan {
            ollama_base_url: None,
            openrouter_key: Some(config::KeyRef::Value(zeroize::Zeroizing::new(
                "sk-or-test".to_string(),
            ))),
            lead: config::ModelRef::new(config::Provider::OpenRouter, "openai/gpt-oss-120b"),
            reviewer: config::ModelRef::new(config::Provider::OpenRouter, "openai/gpt-oss-120b"),
            run_writer: None,
            run_reviewer: Some(config::ModelRef::new(
                config::Provider::OpenRouter,
                "openai/gpt-oss-120b",
            )),
            port: free_port(),
            note: "Claude Code writes".into(),
            writer: "claude-code",
            codex_credential: None,
        }),
    )
    .unwrap();
    let env = doctor_env(&shims);
    let report = crate::doctor::collect(&config_path, &env).await;
    assert!(!report.ready);
    let claude: Vec<_> = report
        .sections
        .writers
        .iter()
        .filter(|item| item.name == "Claude Code")
        .collect();
    assert_eq!(claude.len(), 2, "{claude:?}");
    assert!(claude
        .iter()
        .all(|item| item.status == crate::doctor::Status::Fail));
    assert!(
        !report
            .ready_for
            .iter()
            .any(|path| path.command == "axocoatl run fix"),
        "{:?}",
        report.ready_for
    );
    let claude_path = report
        .ready_for
        .iter()
        .find(|path| path.command == "axocoatl run fix-claude-code")
        .unwrap();
    assert!(!claude_path.ok);
    // A writer not set up is listed with the command that sets it up.
    let codex_path = report
        .ready_for
        .iter()
        .find(|path| path.command == "axocoatl run fix-codex")
        .unwrap();
    assert!(!codex_path.ok);
    // Codex's sign-in builds its image the first time, and neither changes
    // the configuration (`onboard --update --writer codex` would make it the
    // writer here).
    assert_eq!(
        codex_path.detail,
        "not set up: run `axocoatl connect codex`"
    );
}

/// "OpenRouter is not configured": the fix is the browser sign-in, which
/// adds providers.openrouter and changes nothing else (not `onboard
/// --update --writer openrouter`, which would also switch the writer).
/// Doctor then reads the key that sign-in stores (`api_key_secret`) as
/// present; a named secret that is not stored says how to store it.
#[cfg(unix)]
#[tokio::test]
async fn doctor_names_connect_openrouter_and_reads_the_key_it_stores() {
    let shims = Shims::new();
    podman_shim(&shims);
    podman_state(&shims, &["machine", "running"]);
    let home = shims.home.path();
    let config_path = home.join("config.yaml");
    let agents = "agents:\n  - id: lead\n    name: Lead\n    provider: openrouter\n    model: \
                  \"qwen/qwen3-coder\"\n    tools: [read_file]\n";
    let server = format!("server:\n  port: {}\n", free_port());
    std::fs::write(&config_path, format!("{agents}{server}")).unwrap();
    let env = doctor_env(&shims);
    let openrouter = |report: &crate::doctor::Report| {
        report
            .sections
            .models
            .iter()
            .find(|item| item.name == "OpenRouter")
            .cloned()
            .unwrap_or_else(|| panic!("{:?}", report.sections.models))
    };
    let item = openrouter(&crate::doctor::collect(&config_path, &env).await);
    assert_eq!(item.status, crate::doctor::Status::Fail);
    assert_eq!(
        item.fix,
        Some(format!(
            "Run `axocoatl connect openrouter --config {}`: it signs you in to OpenRouter in the \
             browser, stores the key it issues and adds providers.openrouter to this \
             configuration, changing nothing else in it",
            crate::onboard::host::shell_quote(&config_path.display().to_string())
        ))
    );
    // What that command writes: the secret's name, never the key.
    let connected = format!(
        "{agents}providers:\n{}{server}",
        crate::connect_cmd::openrouter::provider_yaml("openrouter")
    );
    std::fs::write(&config_path, &connected).unwrap();
    let item = openrouter(&crate::doctor::collect(&config_path, &env).await);
    assert_eq!(item.status, crate::doctor::Status::Fail);
    assert_eq!(
        item.detail,
        "OpenRouter API key secret openrouter is not stored, and an Agent or run default uses \
         OpenRouter"
    );
    // The configuration doctor checks is not the default one: the command
    // names it, as the "not configured" fix does, so the key goes to its
    // data directory.
    assert_eq!(
        item.fix,
        Some(format!(
            "Run `axocoatl connect openrouter --config {}`, which signs you in to OpenRouter in \
             the browser and stores the key it issues without showing it",
            crate::onboard::host::shell_quote(&config_path.display().to_string())
        ))
    );
    crate::paste_safe::assert_paste_safe(item.fix.as_deref().unwrap());
    let data_dir = env.data_dir.as_deref().unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::create_dir_all(data_dir).unwrap();
        std::fs::set_permissions(data_dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    axocoatl_daemon::secret_store::set_secret(data_dir, "openrouter", b"sk-or-v1-0123456789abcdef")
        .unwrap();
    let report = crate::doctor::collect(&config_path, &env).await;
    let item = openrouter(&report);
    assert_eq!(item.status, crate::doctor::Status::Ok, "{item:?}");
    assert_eq!(
        item.detail,
        "OpenRouter API key present (secret openrouter, never shown)"
    );
    assert!(
        report
            .ready_for
            .iter()
            .any(|path| path.command == "workbench Sessions" && path.ok),
        "{:?}",
        report.ready_for
    );
    // The file is as the sign-in left it.
    assert_eq!(std::fs::read_to_string(&config_path).unwrap(), connected);
}

/// With E2B for workbench Sessions, `axocoatl run` still needs Podman:
/// doctor never says it is ready to run without it.
#[cfg(unix)]
#[tokio::test]
async fn doctor_never_readies_runs_without_podman_under_e2b() {
    let shims = Shims::new();
    let home = shims.home.path();
    let config_path = home.join("config.yaml");
    let text = config::render(&config::ConfigPlan {
        ollama_base_url: None,
        openrouter_key: Some(config::KeyRef::Value(zeroize::Zeroizing::new(
            "sk-or-test".to_string(),
        ))),
        lead: config::ModelRef::new(config::Provider::OpenRouter, "qwen/qwen3-coder"),
        reviewer: config::ModelRef::new(config::Provider::OpenRouter, "openai/gpt-oss-120b"),
        run_writer: Some(config::ModelRef::new(
            config::Provider::OpenRouter,
            "qwen/qwen3-coder",
        )),
        run_reviewer: Some(config::ModelRef::new(
            config::Provider::OpenRouter,
            "openai/gpt-oss-120b",
        )),
        port: free_port(),
        note: "OpenRouter setup".into(),
        writer: "openrouter",
        codex_credential: None,
    });
    std::fs::write(
        &config_path,
        format!("{text}\nsandbox:\n  backend: e2b\n  network: bridge\n"),
    )
    .unwrap();
    let report = crate::doctor::collect(&config_path, &doctor_env(&shims)).await;
    let podman = &report.sections.sandbox[0];
    assert_eq!(podman.status, crate::doctor::Status::Warn, "{podman:?}");
    let run_fix = report
        .ready_for
        .iter()
        .find(|path| path.command == "axocoatl run fix")
        .unwrap_or_else(|| panic!("{:?}", report.ready_for));
    assert!(!run_fix.ok, "{run_fix:?}");
    assert!(run_fix.detail.contains("the sandbox is not ready"));
}

/// What doctor reads about this computer, in a test's home with its shims.
fn doctor_env(shims: &Shims) -> crate::doctor::Environment {
    let home = shims.home.path();
    crate::doctor::Environment {
        os: mac("26.6"),
        user: "alex".to_string(),
        memory_bytes: Some(48 * GIB),
        cpus: 18,
        home: home.to_path_buf(),
        xdg_config_home: None,
        host: shims.host(true),
        load_models: false,
        contexts_read: Default::default(),
        socket_path: home.join("no-daemon.sock"),
        podman: shims.dir.path().join("podman").display().to_string(),
        podman_connection: ConnectionOverride::None,
        id_root: shims.root.path().to_path_buf(),
        data_dir: Some(home.join("data")),
        landlock_abi: Some(6),
    }
}

/// A Mac with neither Podman nor Homebrew: onboarding, --plan included,
/// points to the installer from podman.io and asks for onboard to run again;
/// it runs nothing and writes nothing.
#[cfg(unix)]
#[tokio::test]
async fn a_mac_without_homebrew_is_pointed_to_the_podman_installer() {
    for mode in [Mode::Plan, Mode::Yes] {
        let shims = Shims::new();
        ollama_shim(&shims);
        let ollama = FakeOllama::start("0.40.6", shims.home.path(), shims.dir.path(), &[]).await;
        let facts = facts(
            mac("26.6"),
            48,
            no_podman(),
            &shims,
            &ollama.url(),
            OllamaService::Manual,
        )
        .await;
        assert_eq!(facts.brew, None);
        let plan = mode == Mode::Plan;
        let mut o = onboarding(
            &shims,
            Ui::new(mode, false, None),
            facts,
            OnboardArgs {
                plan,
                yes: !plan,
                writer: Some(WriterChoice::Ollama),
                ollama_url: Some(ollama.url()),
                start: Some(StartChoice::None),
                ..OnboardArgs::default()
            },
        );
        let before = tree(shims.home.path());
        assert!(!sandbox::run(&mut o).await);
        let output = o.ui.output();
        assert_eq!(
            o.steps,
            ["Install Podman with the installer from https://podman.io (or install Homebrew and \
              run `brew install podman`), then run `axocoatl onboard` again"],
            "{output}"
        );
        assert!(output.contains("https://podman.io"), "{output}");
        assert!(shims.calls().is_empty(), "{:?}", shims.calls());
        assert_eq!(tree(shims.home.path()), before);
    }
}

mod interactive;
