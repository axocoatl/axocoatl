//! Onboarding end to end with shim programs (podman, brew, ollama, sudo,
//! systemctl, axocoatl) that log their arguments and change nothing, a fake
//! Ollama server whose cloud mode and context follow the files onboarding
//! writes, and temporary homes. Nothing on the computer running the tests
//! is read or changed: no real Podman, Homebrew, Ollama, service or port
//! 8080.

use std::path::{Path, PathBuf};

use axocoatl_isolation::podman::PodmanReadiness;
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
    let dir = shims.dir.path().display().to_string();
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
      echo "[{{\"Name\":\"podman-machine-default\",\"Default\":true,\"Running\":$r,\"VMType\":\"applehv\",\"CPUs\":6,\"Memory\":\"8589934592\"}}]"
    else echo "[]"; fi ;;
  "machine inspect podman-machine-default")
    rf=false; [ -f "$D/rootful" ] && rf=true
    echo "[{{\"Name\":\"podman-machine-default\",\"Rootful\":$rf,\"Resources\":{{\"CPUs\":6,\"Memory\":8192}}}}]" ;;
  "machine info --format json") echo '{{"Host":{{"VMType":"applehv"}}}}' ;;
  "system connection list --format json")
    if [ -f "$D/machine" ] || [ -f "$D/connection" ]; then echo '[{{"Name":"podman-machine-default"}}]'; else echo '[]'; fi ;;
  machine\ init*) touch "$D/machine" ;;
  "machine start") touch "$D/running" ;;
  "machine stop") rm -f "$D/running" ;;
  "machine set --rootful=false") rm -f "$D/rootful" ;;
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

async fn podman_report(shims: &Shims) -> PodmanReport {
    axocoatl_isolation::podman_report::inspect_with(
        &shims.dir.path().join("podman").display().to_string(),
    )
    .await
}

fn no_podman() -> PodmanReport {
    PodmanReport {
        readiness: PodmanReadiness::NotInstalled,
        client_version: None,
        runtime: None,
        machines: Vec::new(),
        default_provider: None,
    }
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
/// environment names it).
fn brew_service_on(shims: &Shims, url: &str) {
    let agents = shims.home.path().join("Library/LaunchAgents");
    std::fs::create_dir_all(&agents).unwrap();
    std::fs::write(agents.join("homebrew.mxcl.ollama.plist"), "<plist/>").unwrap();
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
    assert!(output
        .contains("Pull qwen3-coder:30b (18.6 GB) and gpt-oss:20b (13.8 GB)? Needs 32.3 GB free"));
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
    let dir = shims.dir.path().display().to_string();
    shims.add(
        "sudo",
        &format!(
            r#"case "$*" in
  "apt-get install -y podman") cp "{dir}/podman.real" "{dir}/podman"; chmod +x "{dir}/podman"; touch "{dir}/machine" "{dir}/running" "{dir}/no-ids" ;;
  usermod*) rm -f "{dir}/no-ids" ;;
  tee*) cat > "{dir}/drop-in" ;;
  "systemctl restart ollama") [ -f "{dir}/drop-in" ] && grep -q OLLAMA_NO_CLOUD "{dir}/drop-in" && touch "{dir}/no-cloud" ;;
esac"#
        ),
    );
    podman_shim(&shims);
    // Without subordinate ids until usermod runs.
    let script = std::fs::read_to_string(shims.dir.path().join("podman")).unwrap();
    std::fs::write(
        shims.dir.path().join("podman"),
        script.replace(
            r#"if [ -f "$D/rootful" ]; then"#,
            r#"if [ -f "$D/no-ids" ]; then echo "$info" | sed 's/{"container_id":1,"host_id":100000,"size":65536}//g; s/},\]/}]/g'; elif [ -f "$D/rootful" ]; then"#,
        ),
    )
    .unwrap();
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
/// No as the default; yes runs stop, set --rootful=false and start; no
/// leaves a step with both ways out.
#[cfg(unix)]
#[tokio::test]
async fn a_rootful_machine_is_switched_only_with_consent() {
    for answer in [Answer::Yes, Answer::No] {
        let shims = Shims::new();
        podman_shim(&shims);
        podman_state(&shims, &["machine", "running", "rootful"]);
        let report = podman_report(&shims).await;
        assert_eq!(report.rootless(), Some(false));
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
            Ui::scripted(vec![answer.clone(), Answer::Yes]),
            facts,
            OnboardArgs::default(),
        );
        let ready = sandbox::run(&mut o).await;
        let calls = shims.calls();
        let output = o.ui.output();
        assert!(output.contains("would stop with exit 5"), "{output}");
        assert!(output.contains("[y/N]"), "{output}");
        if answer == Answer::Yes {
            assert!(ready, "{output}\n{:?}", o.steps);
            for expected in [
                "podman machine stop",
                "podman machine set --rootful=false",
                "podman machine start",
            ] {
                assert!(calls.iter().any(|call| call == expected), "{calls:?}");
            }
        } else {
            assert!(!ready);
            assert!(
                !calls.iter().any(|call| call.contains("--rootful")),
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

/// Below 48 GB no local pair is offered by default: the person is steered
/// to OpenRouter, and nothing is pulled.
#[cfg(unix)]
#[tokio::test]
async fn a_16_gb_computer_is_steered_to_openrouter_and_pulls_nothing() {
    let shims = Shims::new();
    ollama_shim(&shims);
    let ollama = FakeOllama::start("0.40.6", shims.home.path(), shims.dir.path(), &[]).await;
    let facts = facts(
        mac("26.6"),
        16,
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
            podman: Some(PodmanChoice::Skip),
            ..yes(WriterChoice::Ollama)
        },
    );
    let mut choice = Choice::default();
    ollama::writer_and_reviewer(&mut o, &mut choice).await;
    let output = o.ui.output();
    assert!(
        output.contains("No local pair has been tested on 16 GB of memory"),
        "{output}"
    );
    assert!(
        !shims.calls().iter().any(|call| call.contains("pull")),
        "{:?}",
        shims.calls()
    );
    assert!(choice.writer_model.is_none());
    assert!(
        o.steps
            .iter()
            .any(|step| step.contains("OPENROUTER_API_KEY")),
        "{:?}",
        o.steps
    );
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
    let mut o = onboarding(
        &shims,
        Ui::new(Mode::Yes, false, None),
        facts,
        OnboardArgs {
            openrouter_key_env: Some(variable.clone()),
            ..yes(WriterChoice::Openrouter)
        },
    );
    o.openrouter_api = openrouter.uri();
    let code = run(&mut o).await;
    let output = o.ui.output();
    assert_eq!(code, exit::READY, "{output}\n{:?}", o.steps);
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
    assert!(output.contains("Node repositories only for now"));
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

/// An existing configuration can be left alone: nothing is written.
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
    assert_eq!(run(&mut o).await, exit::READY);
    assert_eq!(config_text(&o), "agents: []\n");
    assert!(o.ui.output().contains("Configuration left unchanged"));
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
