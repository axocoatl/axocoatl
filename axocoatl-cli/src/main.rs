use std::io::Write;
use std::path::PathBuf;

use axocoatl_core::SecureDir;
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "axocoatl")]
#[command(about = "Axocoatl - local-first coding workbench and agent runtime")]
#[command(version)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Initialize a new Axocoatl project
    Init {
        /// Project name / directory
        name: Option<String>,
    },

    /// Interactive setup wizard — provider, model, project scaffold
    Onboard {
        /// Also install a background daemon service unit
        #[arg(long)]
        install_daemon: bool,
    },

    /// Check environment, dependencies, and config health
    Doctor {
        /// Path to config file
        #[arg(short, long, default_value = "axocoatl.yaml")]
        config: PathBuf,
    },

    /// Validate an axocoatl.yaml configuration file
    Validate {
        /// Path to config file (default: axocoatl.yaml)
        #[arg(default_value = "axocoatl.yaml")]
        config: PathBuf,
    },

    /// Start in development mode (verbose logging, no daemonization)
    Dev {
        /// Path to config file
        #[arg(short, long, default_value = "axocoatl.yaml")]
        config: PathBuf,
    },

    /// Start the daemon + API server in production mode
    Serve {
        /// Path to config file
        #[arg(short, long, default_value = "axocoatl.yaml")]
        config: PathBuf,
    },

    /// Interactive chat with an agent
    Chat {
        /// Agent ID to chat with
        #[arg(short, long, default_value = "assistant")]
        agent: String,

        /// Config file
        #[arg(short, long, default_value = "axocoatl.yaml")]
        config: PathBuf,

        /// Legacy display label only; does not select or resume stored chat history
        #[arg(long, value_name = "LABEL")]
        session: Option<String>,
    },

    /// Directory Sessions — pick a directory, an agent builds in it
    Session {
        #[command(subcommand)]
        command: SessionCommands,
    },

    /// Agent management commands
    Agents {
        #[command(subcommand)]
        command: AgentCommands,
    },

    /// Skill management commands
    Skills {
        #[command(subcommand)]
        command: SkillCommands,
    },

    /// MCP server management
    Mcp {
        #[command(subcommand)]
        command: McpCommands,
    },

    /// Token usage reporting
    Tokens {
        #[command(subcommand)]
        command: TokenCommands,
    },

    /// Workflow management and execution
    Workflow {
        #[command(subcommand)]
        command: WorkflowCommands,
    },

    /// Run benchmarks
    Benchmark {
        /// Benchmark to run (token, routing, isolation, actor, all)
        #[arg(default_value = "all")]
        name: String,
    },

    /// Always-On Service — run the daemon 24/7 as an OS background service
    Service {
        #[command(subcommand)]
        command: ServiceCommands,
    },
}

#[derive(Subcommand)]
enum ServiceCommands {
    /// Install the daemon as an OS background service (systemd / launchd)
    Install {
        /// Config file the service will run with
        #[arg(short, long, default_value = "axocoatl.yaml")]
        config: String,
    },
    /// Start the Always-On Service now (and enable it at login)
    Start,
    /// Stop the Always-On Service
    Stop,
    /// Show whether the Always-On Service is installed and running
    Status,
    /// Uninstall the Always-On Service
    Uninstall,
}

#[derive(Subcommand)]
enum SessionCommands {
    /// Create a new directory session
    New {
        /// Working directory the agent will build in
        directory: String,
        /// Agent to run in the session
        #[arg(short, long, default_value = "assistant")]
        agent: String,
        /// Session name (defaults to the directory name)
        #[arg(short, long)]
        name: Option<String>,
    },
    /// List directory sessions
    List,
    /// Send an instruction to a session
    Exec {
        /// Session id
        session_id: String,
        /// Instruction for the agent
        input: String,
    },
    /// Close a directory session
    Close {
        /// Session id
        session_id: String,
    },
}

#[derive(Subcommand)]
enum AgentCommands {
    /// List all configured agents
    List {
        #[arg(short, long, default_value = "axocoatl.yaml")]
        config: PathBuf,
    },
    /// Show agent status
    Status {
        #[arg(short, long, default_value = "axocoatl.yaml")]
        config: PathBuf,
    },
    /// Restart an agent
    Restart {
        /// Agent ID
        agent_id: String,
        #[arg(short, long, default_value = "axocoatl.yaml")]
        config: PathBuf,
    },
}

#[derive(Subcommand)]
enum SkillCommands {
    /// List available skills
    List,
    /// Run a skill
    Run {
        /// Skill name
        name: String,
        /// Parameters as key=value pairs
        #[arg(trailing_var_arg = true)]
        params: Vec<String>,
    },
}

#[derive(Subcommand)]
enum McpCommands {
    /// List connected MCP servers
    Servers {
        #[arg(short, long, default_value = "axocoatl.yaml")]
        config: PathBuf,
    },
    /// List available MCP tools
    Tools {
        #[arg(short, long, default_value = "axocoatl.yaml")]
        config: PathBuf,
        /// Filter by server name
        #[arg(short, long)]
        server: Option<String>,
    },
    /// Run Axocoatl AS an MCP server over stdio, exposing each agent as an MCP
    /// tool (`agent_<id>`). Point any MCP client (Claude Desktop, etc.) at
    /// `axocoatl mcp serve`.
    Serve {
        #[arg(short, long, default_value = "axocoatl.yaml")]
        config: PathBuf,
    },
}

#[derive(Subcommand)]
enum WorkflowCommands {
    /// List configured workflows
    List {
        #[arg(short, long, default_value = "axocoatl.yaml")]
        config: PathBuf,
    },
    /// Run a workflow
    Run {
        /// Workflow ID
        workflow_id: String,

        /// Input text for the workflow
        #[arg(short, long)]
        input: String,

        /// Config file
        #[arg(short, long, default_value = "axocoatl.yaml")]
        config: PathBuf,
    },
}

#[derive(Subcommand)]
enum TokenCommands {
    /// Show token usage report
    Report {
        #[arg(short, long, default_value = "axocoatl.yaml")]
        config: PathBuf,
    },
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();

    // Initialize tracing. Logs go to stderr so they never collide with a
    // command's stdout output — in particular `mcp serve`, whose stdout is the
    // MCP JSON-RPC channel.
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    match cli.command {
        Commands::Init { name } => cmd_init(name).await,
        Commands::Onboard { install_daemon } => cmd_onboard(install_daemon).await,
        Commands::Doctor { config } => cmd_doctor(&config).await,
        Commands::Validate { config } => cmd_validate(&config).await,
        Commands::Dev { config } => cmd_dev(&config).await,
        Commands::Serve { config } => cmd_serve(&config).await,
        Commands::Chat {
            agent,
            config,
            session,
        } => cmd_chat(&config, &agent, session).await,
        Commands::Session { command } => match command {
            SessionCommands::New {
                directory,
                agent,
                name,
            } => cmd_session_new(&directory, &agent, name).await,
            SessionCommands::List => cmd_session_list().await,
            SessionCommands::Exec { session_id, input } => {
                cmd_session_exec(&session_id, &input).await
            }
            SessionCommands::Close { session_id } => cmd_session_close(&session_id).await,
        },
        Commands::Agents { command } => match command {
            AgentCommands::List { config } => cmd_agents_list(&config).await,
            AgentCommands::Status { config } => cmd_agents_status(&config).await,
            AgentCommands::Restart { agent_id, config } => {
                cmd_agents_restart(&config, &agent_id).await
            }
        },
        Commands::Skills { command } => match command {
            SkillCommands::List => cmd_skills_list().await,
            SkillCommands::Run { name, params } => cmd_skills_run(&name, params).await,
        },
        Commands::Mcp { command } => match command {
            McpCommands::Servers { config } => cmd_mcp_servers(&config).await,
            McpCommands::Tools { config, server } => cmd_mcp_tools(&config, server).await,
            McpCommands::Serve { config } => cmd_mcp_serve(&config).await,
        },
        Commands::Tokens { command } => match command {
            TokenCommands::Report { config } => cmd_tokens_report(&config).await,
        },
        Commands::Workflow { command } => match command {
            WorkflowCommands::List { config } => cmd_workflow_list(&config).await,
            WorkflowCommands::Run {
                workflow_id,
                input,
                config,
            } => cmd_workflow_run(&config, &workflow_id, &input).await,
        },
        Commands::Benchmark { name } => cmd_benchmark(&name).await,
        Commands::Service { command } => match command {
            ServiceCommands::Install { config } => cmd_service_install(&config),
            ServiceCommands::Start => cmd_service_start(),
            ServiceCommands::Stop => cmd_service_stop(),
            ServiceCommands::Status => cmd_service_status(),
            ServiceCommands::Uninstall => cmd_service_uninstall(),
        },
    }
}

// ── Always-On Service ───────────────────────────────────────────────────
//
// The "Always-On Service" keeps the daemon *process* running 24/7 as an OS
// service. It is distinct from "Proactive Agents" — agents that act on their
// own while the daemon runs. Service management is synchronous (it shells out
// to systemctl / launchctl), so these are plain functions.

fn cmd_service_install(config: &str) {
    let mgr = match axocoatl_service::manager() {
        Ok(m) => m,
        Err(e) => {
            eprintln!("✗ {e}");
            std::process::exit(1);
        }
    };
    // The service runs an absolute path — relative paths in a unit file are
    // the #1 failure mode.
    let exe = match std::env::current_exe().and_then(|path| path.canonicalize()) {
        Ok(path) if path.is_file() => path,
        Ok(path) => {
            eprintln!(
                "✗ the resolved axocoatl executable is not a file: {}",
                path.display()
            );
            std::process::exit(1);
        }
        Err(e) => {
            eprintln!("✗ could not resolve the axocoatl binary: {e}");
            std::process::exit(1);
        }
    };
    let config_abs = match std::path::Path::new(config).canonicalize() {
        Ok(path) if path.is_file() => path,
        Ok(path) => {
            eprintln!("✗ the service config is not a file: {}", path.display());
            std::process::exit(1);
        }
        Err(error) => {
            eprintln!("✗ could not resolve service config '{config}': {error}");
            std::process::exit(1);
        }
    };
    match mgr.install(&exe, &config_abs) {
        Ok(()) => {
            println!("✓ Always-On Service installed ({} backend)", mgr.backend());
            println!("  config: {}", config_abs.display());
            if let Some(hint) = mgr.post_install_hint() {
                println!("\n{hint}");
            }
            println!("\nStart it with:  axocoatl service start");
        }
        Err(e) => {
            eprintln!("✗ install failed: {e}");
            std::process::exit(1);
        }
    }
}

fn cmd_service_start() {
    with_manager(
        |m| m.start(),
        "Always-On Service started — the daemon now runs 24/7",
    );
}

fn cmd_service_stop() {
    with_manager(|m| m.stop(), "Always-On Service stopped");
}

fn cmd_service_uninstall() {
    with_manager(|m| m.uninstall(), "Always-On Service uninstalled");
}

fn cmd_service_status() {
    let mgr = match axocoatl_service::manager() {
        Ok(m) => m,
        Err(e) => {
            eprintln!("✗ {e}");
            std::process::exit(1);
        }
    };
    match mgr.status() {
        Ok(s) => {
            println!("Always-On Service ({} backend)", mgr.backend());
            println!("  installed: {}", if s.installed { "yes" } else { "no" });
            println!("  running:   {}", if s.running { "yes" } else { "no" });
            println!("  at login:  {}", if s.enabled { "yes" } else { "no" });
            println!("  detail:    {}", s.detail);
            println!(
                "\nNote: this is the Always-On *Service* (keeps the daemon \
                 process alive).\nProactive Agents — agents that act on their \
                 own — are managed as Automations in Settings."
            );
        }
        Err(e) => {
            eprintln!("✗ {e}");
            std::process::exit(1);
        }
    }
}

/// Run a service action and report success/failure uniformly.
fn with_manager(
    action: impl FnOnce(
        &dyn axocoatl_service::ServiceManager,
    ) -> Result<(), axocoatl_service::ServiceError>,
    ok_msg: &str,
) {
    let mgr = match axocoatl_service::manager() {
        Ok(m) => m,
        Err(e) => {
            eprintln!("✗ {e}");
            std::process::exit(1);
        }
    };
    match action(mgr.as_ref()) {
        Ok(()) => println!("✓ {ok_msg}"),
        Err(e) => {
            eprintln!("✗ {e}");
            std::process::exit(1);
        }
    }
}

/// Scaffold a project directory: `dir/`, `dir/data/`, `axocoatl.yaml`, `.env.example`.
fn scaffold_project(
    dir: &std::path::Path,
    config_yaml: &str,
    env_example: &str,
) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    std::fs::create_dir_all(dir.join("data"))?;
    std::fs::write(dir.join("axocoatl.yaml"), config_yaml)?;
    std::fs::write(dir.join(".env.example"), env_example)?;
    Ok(())
}

/// Default OpenAI-based template used by `init`.
const TEMPLATE_OPENAI: &str = r#"# Axocoatl Agent Configuration
# See: https://github.com/axocoatl/axocoatl for full reference

agents:
  - id: assistant
    name: "Assistant Agent"
    provider: openai
    model: gpt-4o
    system_prompt: "You are a helpful assistant."
    token_budget:
      per_execution: 20000
      per_call: 8192
      overflow_policy: summarize

providers:
  openai:
    api_key: "${OPENAI_API_KEY}"

server:
  port: 8080
  host: "127.0.0.1"
"#;

const ENV_EXAMPLE: &str =
    "OPENAI_API_KEY=sk-your-key-here\nANTHROPIC_API_KEY=sk-ant-your-key-here\n";

fn next_steps_text(project_name: &str) -> String {
    format!(
        r#"
Created Axocoatl project: {project_name}/
  axocoatl.yaml    — Agent configuration
  .env.example     — process-environment template
  data/            — runtime data directory

Next steps — copy/paste:

  cd {project_name}
  mv .env.example .env       # edit provider keys; never commit this file
  chmod 600 .env
  set -a
  . ./.env
  set +a
  axocoatl doctor            # verify your environment
  axocoatl validate          # check the config
  axocoatl dev               # start the daemon + API server
  axocoatl chat -a assistant # chat with your Agent

Axocoatl reads provider keys from its process environment and does not load
.env automatically. Source it again in each new shell, or configure equivalent
environment injection for the service.

"#
    )
}

fn print_next_steps(project_name: &str) {
    let text = next_steps_text(project_name);
    print!("{text}");
}

fn hosted_key_prompt(provider: &str) -> String {
    format!("{provider} API key (leave blank to set in the process environment later)")
}

fn missing_provider_key_hint(provider: &str) -> String {
    format!(
        "Export it in the process environment that starts Axocoatl before using {provider} Agents."
    )
}

async fn cmd_init(name: Option<String>) {
    let project_name = name.unwrap_or_else(|| "my-axocoatl-project".to_string());
    let dir = PathBuf::from(&project_name);

    if dir.exists() {
        eprintln!("Error: Directory '{project_name}' already exists");
        std::process::exit(1);
    }

    if let Err(e) = scaffold_project(&dir, TEMPLATE_OPENAI, ENV_EXAMPLE) {
        eprintln!("Failed to scaffold project: {e}");
        std::process::exit(1);
    }

    print_next_steps(&project_name);
    println!("Tip: `axocoatl onboard` runs an interactive setup wizard instead.");
}

/// Ping an Ollama server; returns the list of installed model names on success.
async fn ollama_models(base_url: &str) -> Result<Vec<String>, String> {
    let url = format!("{}/api/tags", base_url.trim_end_matches('/'));
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()
        .map_err(|e| e.to_string())?;
    let resp = client.get(&url).send().await.map_err(|e| e.to_string())?;
    let json: serde_json::Value = resp.json().await.map_err(|e| e.to_string())?;
    let models = json
        .get("models")
        .and_then(|m| m.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|m| m.get("name").and_then(|n| n.as_str()).map(String::from))
                .collect()
        })
        .unwrap_or_default();
    Ok(models)
}

/// Run environment health checks. Returns true if all *hard* checks passed.
async fn run_doctor_checks(config_path: &std::path::Path) -> bool {
    let mut hard_ok = true;
    let pass = |label: &str| println!("  [ OK ] {label}");
    let warn = |label: &str, hint: &str| println!("  [WARN] {label}\n         → {hint}");
    let fail = |label: &str, hint: &str| {
        println!("  [FAIL] {label}\n         → {hint}");
    };

    println!("Axocoatl environment check:\n");

    // 1. Rust toolchain (informational)
    match std::process::Command::new("rustc")
        .arg("--version")
        .output()
    {
        Ok(o) if o.status.success() => {
            pass(&format!(
                "Rust toolchain: {}",
                String::from_utf8_lossy(&o.stdout).trim()
            ));
        }
        _ => warn(
            "Rust toolchain not found",
            "Only needed to build from source; prebuilt binaries don't require it.",
        ),
    }

    // 2. Config file exists & validates
    let config = match axocoatl_config::load_config(config_path).await {
        Ok(c) => {
            pass(&format!("Config valid: {}", config_path.display()));
            Some(c)
        }
        Err(e) => {
            hard_ok = false;
            fail(
                &format!("Config invalid: {}", config_path.display()),
                &format!("{e}"),
            );
            None
        }
    };

    if let Some(config) = &config {
        // 3. Provider reachability / credentials
        if let Some(ollama) = &config.providers.ollama {
            match ollama_models(&ollama.base_url).await {
                Ok(models) => {
                    pass(&format!("Ollama reachable at {}", ollama.base_url));
                    // 4. Are the configured models pulled?
                    let wanted: std::collections::HashSet<String> = config
                        .agents
                        .iter()
                        .filter(|a| a.provider == "ollama")
                        .map(|a| {
                            if a.model.is_empty() {
                                ollama
                                    .model
                                    .clone()
                                    .unwrap_or_else(|| "llama3.2".to_string())
                            } else {
                                a.model.clone()
                            }
                        })
                        .collect();
                    for m in wanted {
                        let have = models.iter().any(|installed| {
                            installed == &m || installed.starts_with(&format!("{m}:"))
                        });
                        if have {
                            pass(&format!("Model '{m}' is pulled"));
                        } else {
                            hard_ok = false;
                            fail(
                                &format!("Model '{m}' not pulled"),
                                &format!("Run: ollama pull {m}"),
                            );
                        }
                    }
                }
                Err(e) => {
                    hard_ok = false;
                    fail(
                        &format!("Ollama not reachable at {}", ollama.base_url),
                        &format!("Start it with `ollama serve` ({e})"),
                    );
                }
            }
        }
        let cred_check = |name: &str, key: &str| {
            if key.is_empty() || key.contains("your-key") || key.starts_with("${") {
                warn(
                    &format!("{name} API key not set"),
                    &missing_provider_key_hint(name),
                );
            } else {
                pass(&format!("{name} API key present"));
            }
        };
        if let Some(c) = &config.providers.openai {
            cred_check("OpenAI", c.api_key.expose_secret());
        }
        if let Some(c) = &config.providers.anthropic {
            cred_check("Anthropic", c.api_key.expose_secret());
        }
        if let Some(c) = &config.providers.gemini {
            cred_check("Gemini", c.api_key.expose_secret());
        }
        if let Some(c) = &config.providers.mistral {
            cred_check("Mistral", c.api_key.expose_secret());
        }
        if let Some(c) = &config.providers.openrouter {
            cred_check("OpenRouter", c.api_key.expose_secret());
        }
    }

    // 5. Data dir writable
    let data_dir = std::env::var("AXOCOATL_DATA_DIR").unwrap_or_else(|_| "./data".to_string());
    match probe_data_dir(std::path::Path::new(&data_dir)) {
        Ok(()) => pass(&format!("Data dir writable: {data_dir}")),
        Err(e) => {
            hard_ok = false;
            fail(
                &format!("Data dir not writable: {data_dir}"),
                &format!("{e}"),
            );
        }
    }

    // 6. Daemon running?
    let socket_path = axocoatl_daemon::ipc::default_socket_path();
    if axocoatl_daemon::ipc::IpcClient::connect(&socket_path)
        .await
        .is_ok()
    {
        pass("Daemon is running (IPC reachable)");
    } else {
        warn(
            "No running daemon",
            "Start one with `axocoatl dev` or `axocoatl serve` (not required for one-shot commands).",
        );
    }

    // 7. Podman — the sandbox runtime for Directory Sessions.
    match axocoatl_isolation::podman::detect().await {
        axocoatl_isolation::podman::PodmanReadiness::Ready => {
            pass("Podman ready — Directory Sessions run sandboxed");
        }
        other => warn(
            "Podman not ready (needed for Directory Sessions)",
            &other.summary(),
        ),
    }

    // Outbound egress transparency — always surface what leaves the box.
    if let Some(cfg) = &config {
        if cfg.webhooks.is_empty() {
            pass("Outbound webhooks: none (no event egress)");
        } else {
            let names: Vec<&str> = cfg
                .webhooks
                .iter()
                .filter(|w| w.enabled)
                .map(|w| w.name.as_str())
                .collect();
            println!(
                "  [EGRESS] {} webhook(s) active — lattice events leave the box to: {}",
                names.len(),
                names.join(", ")
            );
        }
    }

    println!();
    if hard_ok {
        println!("All required checks passed.");
    } else {
        println!("Some required checks FAILED — see hints above.");
    }
    hard_ok
}

fn probe_data_dir(path: &std::path::Path) -> std::io::Result<()> {
    let data = SecureDir::open_or_create_all(path)?;
    #[cfg(unix)]
    {
        unsafe extern "C" {
            fn geteuid() -> u32;
        }
        // SAFETY: geteuid takes no arguments and has no failure sentinel.
        data.require_owner_and_private_writes(unsafe { geteuid() })?;
    }
    data.restrict_owner_only()?;
    let name = format!(".write-probe-{}", uuid::Uuid::new_v4());
    let mut probe = data.create_new(&name)?;
    probe.write_all(b"ok")?;
    probe.sync_all()?;
    drop(probe);
    data.remove_file(name)
}

async fn cmd_doctor(config_path: &std::path::Path) {
    let ok = run_doctor_checks(config_path).await;
    if !ok {
        std::process::exit(1);
    }
}

async fn cmd_onboard(install_daemon: bool) {
    use dialoguer::{Confirm, Input, Select};

    println!("┌─────────────────────────────────────────┐");
    println!("│   Axocoatl — interactive setup wizard    │");
    println!("└─────────────────────────────────────────┘\n");

    // 1. Provider
    let providers = [
        "Ollama (local, no API key)",
        "OpenRouter (cloud, models available to your account)",
        "Anthropic",
        "OpenAI",
    ];
    let provider_idx = Select::new()
        .with_prompt("Choose your LLM provider")
        .items(&providers)
        .default(0)
        .interact()
        .unwrap_or(0);

    // 2. Project name
    let project_name: String = Input::new()
        .with_prompt("Project directory name")
        .default("my-axocoatl-project".to_string())
        .interact_text()
        .unwrap_or_else(|_| "my-axocoatl-project".to_string());

    let dir = PathBuf::from(&project_name);
    if dir.exists() {
        eprintln!("Error: Directory '{project_name}' already exists");
        std::process::exit(1);
    }

    // 3. Provider-specific config
    let (config_yaml, env_example) = match provider_idx {
        0 => {
            // Ollama
            if which_ollama().is_none() {
                println!("\nOllama is not installed.");
                println!("Install it from https://ollama.com/download, then re-run onboard.");
                if !Confirm::new()
                    .with_prompt("Continue scaffolding anyway?")
                    .default(true)
                    .interact()
                    .unwrap_or(true)
                {
                    std::process::exit(1);
                }
            }
            let model: String = Input::new()
                .with_prompt("Ollama model")
                .default("llama3.2".to_string())
                .interact_text()
                .unwrap_or_else(|_| "llama3.2".to_string());

            if which_ollama().is_some()
                && Confirm::new()
                    .with_prompt(format!("Pull '{model}' now with `ollama pull`?"))
                    .default(true)
                    .interact()
                    .unwrap_or(false)
            {
                println!("Pulling {model} (this can take a few minutes)...");
                let _ = std::process::Command::new("ollama")
                    .arg("pull")
                    .arg(&model)
                    .status();
            }

            let cfg = format!(
                r#"# Axocoatl — local Ollama setup
agents:
  - id: assistant
    name: "Assistant"
    provider: ollama
    model: {model}
    system_prompt: "You are a helpful assistant powered by Axocoatl."
    token_budget:
      per_execution: 16000
      per_call: 8192
      overflow_policy: warn

  - id: researcher
    name: "Researcher"
    provider: ollama
    model: {model}
    system_prompt: "You are a research assistant. Provide detailed, factual answers."
    depends_on: []

  - id: summarizer
    name: "Summarizer"
    provider: ollama
    model: {model}
    system_prompt: "Summarize the input in 1-2 sentences."
    depends_on: [researcher]

workflows:
  - id: research-and-summarize
    name: "Research and Summarize"
    agents: [researcher, summarizer]
    entry_point: researcher

providers:
  ollama:
    base_url: "http://localhost:11434"

server:
  port: 8080
  host: "127.0.0.1"
"#
            );
            (cfg, String::from("# No API keys needed for local Ollama\n"))
        }
        1 => {
            // OpenRouter — one key for the model IDs available to the account.
            let key: String = Input::new()
                .with_prompt(hosted_key_prompt("OpenRouter"))
                .allow_empty(true)
                .interact_text()
                .unwrap_or_default();
            let model: String = Input::new()
                .with_prompt("Default model (vendor/model)")
                .default("openai/gpt-4o-mini".to_string())
                .interact_text()
                .unwrap_or_else(|_| "openai/gpt-4o-mini".to_string());
            let cfg = format!(
                r#"# Axocoatl — OpenRouter setup
# OpenRouter is OpenAI-compatible. Choose a model ID available to your
# OpenRouter account; browse the catalog at https://openrouter.ai/models.
agents:
  - id: assistant
    name: "Assistant"
    provider: openrouter
    model: "{model}"
    system_prompt: "You are a helpful assistant."
    token_budget:
      per_execution: 16000
      per_call: 8192
      overflow_policy: warn

providers:
  openrouter:
    api_key: "${{OPENROUTER_API_KEY}}"

server:
  port: 8080
  host: "127.0.0.1"
"#
            );
            let env = if key.is_empty() {
                "OPENROUTER_API_KEY=sk-or-your-key-here\n".to_string()
            } else {
                format!("OPENROUTER_API_KEY={key}\n")
            };
            (cfg, env)
        }
        2 => {
            // Anthropic
            let key: String = Input::new()
                .with_prompt(hosted_key_prompt("Anthropic"))
                .allow_empty(true)
                .interact_text()
                .unwrap_or_default();
            let cfg = r#"# Axocoatl — Anthropic setup
agents:
  - id: assistant
    name: "Assistant"
    provider: anthropic
    model: claude-sonnet-4-6
    system_prompt: "You are a helpful assistant."
    token_budget:
      per_execution: 20000
      per_call: 8192
      overflow_policy: summarize

providers:
  anthropic:
    api_key: "${ANTHROPIC_API_KEY}"

server:
  port: 8080
  host: "127.0.0.1"
"#
            .to_string();
            let env = if key.is_empty() {
                "ANTHROPIC_API_KEY=sk-ant-your-key-here\n".to_string()
            } else {
                format!("ANTHROPIC_API_KEY={key}\n")
            };
            (cfg, env)
        }
        _ => {
            // OpenAI
            let key: String = Input::new()
                .with_prompt(hosted_key_prompt("OpenAI"))
                .allow_empty(true)
                .interact_text()
                .unwrap_or_default();
            let env = if key.is_empty() {
                "OPENAI_API_KEY=sk-your-key-here\n".to_string()
            } else {
                format!("OPENAI_API_KEY={key}\n")
            };
            (TEMPLATE_OPENAI.to_string(), env)
        }
    };

    if let Err(e) = scaffold_project(&dir, &config_yaml, &env_example) {
        eprintln!("Failed to scaffold project: {e}");
        std::process::exit(1);
    }

    if install_daemon {
        write_daemon_unit(&dir, &project_name);
    }

    println!("\n✓ Project scaffolded.\n");

    // Run doctor inline against the new config
    let cfg_path = dir.join("axocoatl.yaml");
    let _ = run_doctor_checks(&cfg_path).await;

    print_next_steps(&project_name);
}

/// Locate the `ollama` binary on PATH.
fn which_ollama() -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|p| p.join("ollama"))
        .find(|p| p.is_file())
}

/// Drop a service unit template for `--install-daemon`.
fn write_daemon_unit(dir: &std::path::Path, project_name: &str) {
    #[cfg(target_os = "macos")]
    {
        let plist = format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>Label</key><string>dev.axocoatl.daemon</string>
  <key>ProgramArguments</key><array>
    <string>axocoatl</string><string>serve</string>
    <string>-c</string><string>{project_name}/axocoatl.yaml</string>
  </array>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><true/>
</dict></plist>
"#
        );
        let _ = std::fs::write(dir.join("dev.axocoatl.daemon.plist"), plist);
        println!("Wrote launchd unit: {project_name}/dev.axocoatl.daemon.plist");
        println!("Enable: cp {project_name}/dev.axocoatl.daemon.plist ~/Library/LaunchAgents/ && launchctl load ~/Library/LaunchAgents/dev.axocoatl.daemon.plist");
    }
    #[cfg(not(target_os = "macos"))]
    {
        let unit = format!(
            r#"[Unit]
Description=Axocoatl daemon
After=network.target

[Service]
ExecStart=axocoatl serve -c %h/{project_name}/axocoatl.yaml
Restart=on-failure

[Install]
WantedBy=default.target
"#
        );
        let _ = std::fs::write(dir.join("axocoatl.service"), unit);
        println!("Wrote systemd unit: {project_name}/axocoatl.service");
        println!("Enable: mkdir -p ~/.config/systemd/user && cp {project_name}/axocoatl.service ~/.config/systemd/user/ && systemctl --user enable --now axocoatl");
    }
}

async fn cmd_validate(config_path: &std::path::Path) {
    match axocoatl_config::load_config(config_path).await {
        Ok(config) => {
            println!("Config is valid.");
            println!("  Agents: {}", config.agents.len());
            for agent in &config.agents {
                let budget = agent
                    .token_budget
                    .as_ref()
                    .map(|b| format!("{} tokens/exec", b.per_execution))
                    .unwrap_or_else(|| "unlimited".to_string());
                println!(
                    "    - {} ({}/{}) [{}]",
                    agent.id, agent.provider, agent.model, budget
                );
            }
            println!("  Workflows: {}", config.workflows.len());
            println!("  MCP servers: {}", config.mcp_servers.len());
        }
        Err(e) => {
            eprintln!("Configuration error:\n{e}");
            std::process::exit(1);
        }
    }
}

/// Singleton reservation acquired before daemon bootstrap. Development and
/// service/serve mode share this exact boundary, so a second daemon exits
/// before it can spawn actors or touch runtime state.
struct CliIpcReservation {
    socket_path: std::path::PathBuf,
    listener: tokio::net::UnixListener,
}

async fn reserve_cli_ipc() -> CliIpcReservation {
    let socket_path = axocoatl_daemon::ipc::default_socket_path();
    let listener = match axocoatl_daemon::ipc::bind_ipc_listener(&socket_path).await {
        Ok(listener) => listener,
        Err(error) => {
            eprintln!("Failed to reserve the CLI IPC socket: {error}");
            eprintln!("Another Axocoatl daemon may already be running.");
            std::process::exit(1);
        }
    };
    CliIpcReservation {
        socket_path,
        listener,
    }
}

/// Attach initialized state to the socket reserved before bootstrap.
fn start_cli_ipc(
    reservation: CliIpcReservation,
    state: std::sync::Arc<tokio::sync::RwLock<axocoatl_daemon::AxocoatlDaemon>>,
) -> tokio::task::JoinHandle<()> {
    println!("  IPC:    {}", reservation.socket_path.display());
    axocoatl_daemon::ipc::serve_ipc_listener(reservation.listener, state)
}

async fn cmd_dev(config_path: &std::path::Path) {
    let config = match axocoatl_config::load_config(config_path).await {
        Ok(c) => c,
        Err(e) => {
            eprintln!("Configuration error:\n{e}");
            std::process::exit(1);
        }
    };

    let host = config.server.host.clone();
    let port = config.server.port;
    let ipc_reservation = reserve_cli_ipc().await;

    println!("Axocoatl dev mode");
    println!("  Config: {}", config_path.display());
    println!("  Agents: {}", config.agents.len());

    let daemon = match axocoatl_daemon::AxocoatlDaemon::bootstrap(config).await {
        Ok(d) => {
            println!("  Runtime: {} agents spawned", d.agent_count().await);
            d
        }
        Err(e) => {
            eprintln!("Failed to bootstrap daemon: {e}");
            std::process::exit(1);
        }
    };

    // Shared state for both IPC and HTTP
    let state: std::sync::Arc<tokio::sync::RwLock<axocoatl_daemon::AxocoatlDaemon>> =
        std::sync::Arc::new(tokio::sync::RwLock::new(daemon));
    let ipc_handle = start_cli_ipc(ipc_reservation, state.clone());

    // One live runtime dispatches every canonical Automation trigger. Legacy
    // schedules/proactive YAML was already used (once) to seed that store.
    let automatic_count = {
        use axocoatl_config::AutomationTrigger;
        let d = state.read().await;
        d.list_automations()
            .await
            .iter()
            .filter(|automation| !matches!(&automation.trigger, AutomationTrigger::Manual))
            .count()
    };
    axocoatl_daemon::start_automation_runtime(state.clone()).await;
    if automatic_count > 0 {
        println!("  Automations: {automatic_count} automatic triggers active");
    }

    // Supervise agents: restart any that crash, from their last checkpoint.
    axocoatl_daemon::supervision::start_supervision(state.clone());

    // Background memory consolidation (sleep-time): idle agents promote durable
    // facts from semantic memory into their curated core-memory blocks.
    let consolidation = { state.read().await.config.consolidation.clone() };
    axocoatl_daemon::consolidation::start_consolidation(state.clone(), consolidation);

    println!("  Server: http://{host}:{port}");
    println!("  Health: http://{host}:{port}/health");
    println!();
    println!("Axocoatl is running. Press Ctrl+C to stop.");

    // Start the HTTP server (blocks until shutdown) — shares state with IPC
    let server = axocoatl_server::serve_shared(state, &host, port).await;
    ipc_handle.abort();
    let _ = ipc_handle.await;
    if let Err(e) = server {
        eprintln!("Server error: {e}");
        std::process::exit(1);
    }
}

async fn cmd_serve(config_path: &std::path::Path) {
    let config = match axocoatl_config::load_config(config_path).await {
        Ok(c) => c,
        Err(e) => {
            eprintln!("Configuration error:\n{e}");
            std::process::exit(1);
        }
    };

    let host = config.server.host.clone();
    let port = config.server.port;
    let ipc_reservation = reserve_cli_ipc().await;

    let daemon = match axocoatl_daemon::AxocoatlDaemon::bootstrap(config).await {
        Ok(d) => d,
        Err(e) => {
            eprintln!("Failed to bootstrap daemon: {e}");
            std::process::exit(1);
        }
    };

    println!("Axocoatl server starting on {host}:{port}");

    // Shared runtime state for the HTTP server and background services.
    let state: std::sync::Arc<tokio::sync::RwLock<axocoatl_daemon::AxocoatlDaemon>> =
        std::sync::Arc::new(tokio::sync::RwLock::new(daemon));
    let ipc_handle = start_cli_ipc(ipc_reservation, state.clone());
    axocoatl_daemon::start_automation_runtime(state.clone()).await;

    // Supervise agents: restart any that crash, from their last checkpoint.
    axocoatl_daemon::supervision::start_supervision(state.clone());

    // Background memory consolidation (sleep-time): idle agents promote durable
    // facts from semantic memory into their curated core-memory blocks.
    let consolidation = { state.read().await.config.consolidation.clone() };
    axocoatl_daemon::consolidation::start_consolidation(state.clone(), consolidation);

    let server = axocoatl_server::serve_shared(state, &host, port).await;
    ipc_handle.abort();
    let _ = ipc_handle.await;
    if let Err(e) = server {
        eprintln!("Server error: {e}");
        std::process::exit(1);
    }
}

/// Run Axocoatl AS an MCP server over stdio. Bootstraps the daemon so agents
/// exist, then speaks the MCP protocol on stdin/stdout, exposing each agent as
/// an `agent_<id>` tool any MCP client can list and call.
async fn cmd_mcp_serve(config_path: &std::path::Path) {
    use rmcp::ServiceExt;

    let config = match axocoatl_config::load_config(config_path).await {
        Ok(c) => c,
        Err(e) => {
            eprintln!("Configuration error:\n{e}");
            std::process::exit(1);
        }
    };

    let daemon = match axocoatl_daemon::AxocoatlDaemon::bootstrap_headless(config).await {
        Ok(d) => d,
        Err(e) => {
            eprintln!("Failed to bootstrap daemon: {e}");
            std::process::exit(1);
        }
    };

    let state = std::sync::Arc::new(tokio::sync::RwLock::new(daemon));
    let executor = std::sync::Arc::new(DaemonAgentExecutor { state });
    let server = axocoatl_mcp::AxocoatlMcpServer::new(executor);

    eprintln!("Axocoatl MCP server ready on stdio — exposing agents as tools.");

    // stdout is the MCP JSON-RPC channel (logs are on stderr — see main()).
    let running = match server
        .serve((tokio::io::stdin(), tokio::io::stdout()))
        .await
    {
        Ok(r) => r,
        Err(e) => {
            eprintln!("MCP serve error: {e}");
            std::process::exit(1);
        }
    };
    if let Err(e) = running.waiting().await {
        eprintln!("MCP server stopped: {e}");
        std::process::exit(1);
    }
}

/// Bridges MCP tool calls to the daemon's agents.
struct DaemonAgentExecutor {
    state: std::sync::Arc<tokio::sync::RwLock<axocoatl_daemon::AxocoatlDaemon>>,
}

#[async_trait::async_trait]
impl axocoatl_mcp::AgentExecutor for DaemonAgentExecutor {
    async fn list_agent_ids(&self) -> Vec<String> {
        let d = self.state.read().await;
        d.config.agents.iter().map(|a| a.id.clone()).collect()
    }

    async fn execute_agent(&self, agent_id: &str, input: &str) -> Result<String, String> {
        let d = self.state.read().await;
        d.execute_agent(agent_id, input)
            .await
            .map(|o| o.content)
            .map_err(|e| e.to_string())
    }
}

async fn cmd_agents_list(config_path: &std::path::Path) {
    let config = match axocoatl_config::load_config(config_path).await {
        Ok(c) => c,
        Err(e) => {
            eprintln!("Configuration error:\n{e}");
            std::process::exit(1);
        }
    };

    if config.agents.is_empty() {
        println!("No agents configured.");
        return;
    }

    println!(
        "{:<15} {:<20} {:<12} {:<15}",
        "ID", "NAME", "PROVIDER", "MODEL"
    );
    println!("{}", "-".repeat(62));
    for agent in &config.agents {
        println!(
            "{:<15} {:<20} {:<12} {:<15}",
            agent.id, agent.name, agent.provider, agent.model
        );
    }
}

async fn cmd_tokens_report(config_path: &std::path::Path) {
    use axocoatl_daemon::ipc::{IpcClient, IpcRequest, IpcResponse};

    let socket_path = axocoatl_daemon::ipc::default_socket_path();
    let resp = if let Ok(mut client) = IpcClient::connect(&socket_path).await {
        client
            .request(&IpcRequest::GetTokenUsage { agent_id: None })
            .await
    } else {
        // No running daemon: bootstrap in-process. Fresh agents report
        // their restored-from-checkpoint usage (often zero on first run).
        let config = match axocoatl_config::load_config(config_path).await {
            Ok(c) => c,
            Err(e) => {
                eprintln!("Configuration error:\n{e}");
                std::process::exit(1);
            }
        };
        let daemon = match axocoatl_daemon::AxocoatlDaemon::bootstrap_headless(config).await {
            Ok(d) => d,
            Err(e) => {
                eprintln!("Failed to bootstrap daemon: {e}");
                std::process::exit(1);
            }
        };
        let ids = daemon.agent_registry.list_ids().await;
        let mut per_agent = Vec::new();
        let mut total_in: usize = 0;
        let mut total_out: usize = 0;
        let mut total_reasoning: usize = 0;
        let mut token_usage_known = true;
        for id in ids {
            if let Some(actor) = daemon.agent_registry.get(&id).await {
                if let Ok(measured) = axocoatl_actor::get_agent_measured_token_usage(&actor).await {
                    let u = measured.usage;
                    token_usage_known &= measured.complete;
                    total_in = total_in.saturating_add(u.input_tokens);
                    total_out = total_out.saturating_add(u.output_tokens);
                    total_reasoning =
                        total_reasoning.saturating_add(u.reasoning_tokens.unwrap_or(0));
                    per_agent.push(axocoatl_daemon::ipc::IpcTokenUsage {
                        agent_id: id.to_string(),
                        input_tokens: u.input_tokens,
                        output_tokens: u.output_tokens,
                        reasoning_tokens: u.reasoning_tokens,
                        token_usage_known: measured.complete,
                    });
                }
            }
        }
        daemon.shutdown().await;
        Ok(IpcResponse::TokenUsage {
            per_agent,
            total_input: total_in,
            total_output: total_out,
            total_reasoning,
            token_usage_known,
        })
    };

    match resp {
        Ok(IpcResponse::TokenUsage {
            per_agent,
            total_input,
            total_output,
            total_reasoning,
            token_usage_known,
        }) => {
            println!(
                "{:<20} {:>10} {:>10} {:>10} {:>12}",
                "AGENT", "INPUT", "OUTPUT", "REASONING", "TOTAL / SUBTOTAL"
            );
            println!("{}", "-".repeat(69));
            for u in &per_agent {
                let reasoning = u.reasoning_tokens.unwrap_or(0);
                println!(
                    "{:<20} {:>10} {:>10} {:>10} {:>12}",
                    u.agent_id,
                    u.input_tokens,
                    u.output_tokens,
                    reasoning,
                    format!(
                        "{}{}",
                        if u.token_usage_known { "" } else { "≥" },
                        u.input_tokens
                            .saturating_add(u.output_tokens)
                            .saturating_add(reasoning)
                    )
                );
            }
            println!("{}", "-".repeat(69));
            println!(
                "{:<20} {:>10} {:>10} {:>10} {:>12}",
                if token_usage_known {
                    "TOTAL"
                } else {
                    "KNOWN SUBTOTAL"
                },
                total_input,
                total_output,
                total_reasoning,
                format!(
                    "{}{}",
                    if token_usage_known { "" } else { "≥" },
                    total_input
                        .saturating_add(total_output)
                        .saturating_add(total_reasoning)
                )
            );
            if !token_usage_known {
                println!("At least one dispatched call lacks terminal usage; the subtotal is a lower bound.");
            }
        }
        Ok(IpcResponse::Error { message, .. }) => {
            eprintln!("Error: {message}");
            std::process::exit(1);
        }
        Ok(_) => {
            eprintln!("Unexpected response from daemon");
            std::process::exit(1);
        }
        Err(e) => {
            eprintln!("Failed to query token usage: {e}");
            std::process::exit(1);
        }
    }
}

/// Display tool calls inline in the chat output.
fn display_tool_calls(tool_calls: &[axocoatl_core::ToolCallRecord]) {
    for tc in tool_calls {
        let args_summary = tc.arguments.to_string();
        let args_display = truncate_for_display(&args_summary, 80);
        if let Some(result) = &tc.result {
            let result_str = result.to_string();
            let result_display = truncate_for_display(&result_str, 60);
            println!(
                "  [tool: {}({})] -> {}",
                tc.tool_name, args_display, result_display
            );
        } else {
            println!("  [tool: {}({})]", tc.tool_name, args_display);
        }
    }
}

fn truncate_for_display(value: &str, max_bytes: usize) -> String {
    if value.len() <= max_bytes {
        return value.to_string();
    }
    let mut end = max_bytes.saturating_sub(3).min(value.len());
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}...", &value[..end])
}

const LEGACY_CHAT_SESSION_WARNING: &str = "warning: `chat --session` is a legacy display label only; it does not select or resume stored chat history";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct CliTokenMeasurement {
    input: usize,
    output: usize,
    reasoning: usize,
    complete: bool,
}

impl CliTokenMeasurement {
    fn known_zero() -> Self {
        Self {
            input: 0,
            output: 0,
            reasoning: 0,
            complete: true,
        }
    }

    fn merge(&mut self, other: Self) {
        self.input = self.input.saturating_add(other.input);
        self.output = self.output.saturating_add(other.output);
        self.reasoning = self.reasoning.saturating_add(other.reasoning);
        self.complete &= other.complete;
    }

    fn total(self) -> usize {
        self.input
            .saturating_add(self.output)
            .saturating_add(self.reasoning)
    }

    fn clause(self, scope: &str) -> String {
        if self.complete {
            format!(
                "{scope} tokens: {} in / {} out / {} reasoning ({} total)",
                self.input,
                self.output,
                self.reasoning,
                self.total()
            )
        } else {
            format!(
                "{scope} known token subtotal (lower bound): {} in / {} out / {} reasoning (≥{} total)",
                self.input,
                self.output,
                self.reasoning,
                self.total()
            )
        }
    }
}

fn ipc_error_measurement(
    input: Option<usize>,
    output: Option<usize>,
    reasoning: Option<usize>,
    complete: Option<bool>,
) -> Option<CliTokenMeasurement> {
    Some(CliTokenMeasurement {
        input: input?,
        output: output?,
        reasoning: reasoning?,
        complete: complete?,
    })
}

fn workflow_response_is_failure(failed_agents: &[(String, String)]) -> bool {
    !failed_agents.is_empty()
}

fn resolve_chat_label(session_label: Option<String>) -> (String, Option<&'static str>) {
    let warning = session_label.as_ref().map(|_| LEGACY_CHAT_SESSION_WARNING);
    let label = session_label.unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    (label, warning)
}

async fn cmd_chat(config_path: &std::path::Path, agent_id: &str, session_id: Option<String>) {
    use std::io::{self, BufRead, Write};

    let (chat_label, legacy_session_warning) = resolve_chat_label(session_id);
    if let Some(warning) = legacy_session_warning {
        eprintln!("{warning}");
    }

    let config = match axocoatl_config::load_config(config_path).await {
        Ok(c) => c,
        Err(e) => {
            eprintln!("Configuration error:\n{e}");
            std::process::exit(1);
        }
    };

    // Find agent model for display
    let agent_model = config
        .agents
        .iter()
        .find(|a| a.id == agent_id)
        .map(|a| a.model.clone())
        .unwrap_or_else(|| "unknown".to_string());

    // Try connecting to a running daemon via IPC first
    let socket_path = axocoatl_daemon::ipc::default_socket_path();
    let ipc_client = axocoatl_daemon::ipc::IpcClient::connect(&socket_path)
        .await
        .ok();

    let using_ipc = ipc_client.is_some();

    // If no daemon running, bootstrap in-process
    let daemon = if ipc_client.is_none() {
        Some(
            match axocoatl_daemon::AxocoatlDaemon::bootstrap_headless(config).await {
                Ok(d) => d,
                Err(e) => {
                    eprintln!("Failed to bootstrap daemon: {e}");
                    std::process::exit(1);
                }
            },
        )
    } else {
        None
    };

    println!("Axocoatl Chat");
    println!("  Agent:   {agent_id} ({agent_model})");
    println!("  Label:   {chat_label}");
    if using_ipc {
        println!("  Mode:    connected to daemon (IPC)");
    } else {
        println!("  Mode:    in-process");
    }
    println!("  Type 'exit' or Ctrl+D to quit.\n");

    let stdin = io::stdin();
    let mut stdout = io::stdout();

    let mut chat_usage = CliTokenMeasurement::known_zero();
    let mut turn_count: usize = 0;

    // Mutable IPC client (needs to be mutable for requests)
    let mut ipc = ipc_client;

    loop {
        print!("you> ");
        if let Err(error) = stdout.flush() {
            eprintln!("chat output error: {error}");
            if let Some(daemon) = daemon {
                daemon.shutdown().await;
            }
            std::process::exit(1);
        }

        let mut line = String::new();
        let bytes_read = match stdin.lock().read_line(&mut line) {
            Ok(bytes_read) => bytes_read,
            Err(error) => {
                eprintln!("chat input error: {error}");
                if let Some(daemon) = daemon {
                    daemon.shutdown().await;
                }
                std::process::exit(1);
            }
        };
        if bytes_read == 0 {
            break; // EOF
        }

        let input = line.trim();
        if input.is_empty() {
            continue;
        }
        if input == "exit" || input == "quit" {
            break;
        }

        // Execute via IPC or in-process
        if let Some(ref mut client) = ipc {
            // Keep carrying the legacy field for wire compatibility. It is a
            // display label here, not conversation ownership or history.
            let req = axocoatl_daemon::ipc::IpcRequest::Execute {
                agent_id: agent_id.to_string(),
                input: input.to_string(),
                session_id: chat_label.clone(),
            };
            match client.request(&req).await {
                Ok(axocoatl_daemon::ipc::IpcResponse::Response {
                    content,
                    tool_calls,
                    input_tokens,
                    output_tokens,
                    reasoning_tokens,
                    token_usage_known,
                }) => {
                    turn_count += 1;

                    // Convert IPC tool calls for display
                    let records: Vec<axocoatl_core::ToolCallRecord> = tool_calls
                        .into_iter()
                        .map(|tc| axocoatl_core::ToolCallRecord {
                            tool_name: tc.tool_name,
                            arguments: tc.arguments,
                            result: tc.result,
                        })
                        .collect();
                    display_tool_calls(&records);

                    println!("\nagent> {content}\n");

                    let turn_usage = CliTokenMeasurement {
                        input: input_tokens,
                        output: output_tokens,
                        reasoning: reasoning_tokens,
                        complete: token_usage_known,
                    };
                    chat_usage.merge(turn_usage);
                    println!(
                        "  ({} | {})",
                        turn_usage.clause("turn"),
                        chat_usage.clause("chat")
                    );
                    println!();
                }
                Ok(axocoatl_daemon::ipc::IpcResponse::Error {
                    message,
                    input_tokens,
                    output_tokens,
                    reasoning_tokens,
                    token_usage_known,
                }) => {
                    eprintln!("\nerror> {message}\n");
                    if let Some(usage) = ipc_error_measurement(
                        input_tokens,
                        output_tokens,
                        reasoning_tokens,
                        token_usage_known,
                    ) {
                        chat_usage.merge(usage);
                        eprintln!("  {}", usage.clause("failed turn"));
                    }
                }
                Ok(_) => {
                    eprintln!("\nerror> unexpected response from daemon\n");
                }
                Err(e) => {
                    eprintln!("\nerror> IPC error: {e}\n");
                }
            }
        } else if let Some(ref daemon) = daemon {
            match daemon.execute_agent_measured(agent_id, input).await {
                Ok(measured) => {
                    let output = measured.output;
                    turn_count += 1;
                    display_tool_calls(&output.tool_calls);

                    println!("\nagent> {}\n", output.content);

                    let turn_usage = CliTokenMeasurement {
                        input: output.token_usage.input_tokens,
                        output: output.token_usage.output_tokens,
                        reasoning: output.token_usage.reasoning_tokens.unwrap_or(0),
                        complete: measured.token_usage_known,
                    };
                    chat_usage.merge(turn_usage);
                    println!(
                        "  ({} | {})",
                        turn_usage.clause("turn"),
                        chat_usage.clause("chat")
                    );
                    println!();
                }
                Err(failure) => {
                    eprintln!("\nerror> {}\n", failure.error);
                    let usage = CliTokenMeasurement {
                        input: failure.token_usage.input_tokens,
                        output: failure.token_usage.output_tokens,
                        reasoning: failure.token_usage.reasoning_tokens.unwrap_or(0),
                        complete: failure.token_usage_known,
                    };
                    chat_usage.merge(usage);
                    eprintln!("  {}", usage.clause("failed turn"));
                }
            }
        }
    }

    println!();
    println!("Chat summary:");
    println!("  Turns:  {turn_count}");
    println!("  {}", chat_usage.clause("chat"));
    println!("  Label:  {chat_label}");
    println!();
    println!("Goodbye!");
    if let Some(daemon) = daemon {
        daemon.shutdown().await;
    }
}

// ── Directory Sessions ──────────────────────────────────────────────────
//
// Sessions talk to the running daemon over its IPC socket. Each session is a
// working directory + an agent that builds in it, inside a sandboxed
// container.

/// Connect to the running daemon's IPC socket, or print guidance and exit.
async fn session_ipc_client() -> axocoatl_daemon::ipc::IpcClient {
    let socket_path = axocoatl_daemon::ipc::default_socket_path();
    match axocoatl_daemon::ipc::IpcClient::connect(&socket_path).await {
        Ok(c) => c,
        Err(_) => {
            eprintln!("✗ Could not reach the Axocoatl daemon.");
            eprintln!("  Start it with `axocoatl dev`, or install the Always-On Service.");
            std::process::exit(1);
        }
    }
}

async fn cmd_session_new(directory: &str, agent: &str, name: Option<String>) {
    let name = name.unwrap_or_else(|| {
        std::path::Path::new(directory)
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| directory.to_string())
    });
    let mut client = session_ipc_client().await;
    let req = axocoatl_daemon::ipc::IpcRequest::CreateSession {
        name,
        working_dir: directory.to_string(),
        agent: agent.to_string(),
    };
    match client.request(&req).await {
        Ok(axocoatl_daemon::ipc::IpcResponse::Session { session }) => {
            println!("✓ Session created");
            println!("  id:        {}", session.id);
            println!("  name:      {}", session.name);
            println!("  directory: {}", session.working_dir);
            println!("  mode:      {}", session.mode);
            println!("\n{}", session_readiness_guidance(&session));
        }
        Ok(axocoatl_daemon::ipc::IpcResponse::Error { message, .. }) => {
            eprintln!("✗ {message}");
            std::process::exit(1);
        }
        Ok(_) => {
            eprintln!("✗ unexpected daemon response");
            std::process::exit(1);
        }
        Err(e) => {
            eprintln!("✗ {e}");
            std::process::exit(1);
        }
    }
}

async fn cmd_session_list() {
    let mut client = session_ipc_client().await;
    match client
        .request(&axocoatl_daemon::ipc::IpcRequest::ListSessions)
        .await
    {
        Ok(axocoatl_daemon::ipc::IpcResponse::Sessions { sessions }) => {
            if sessions.is_empty() {
                println!("No directory sessions yet.");
                println!("Create one:  axocoatl session new <directory>");
                return;
            }
            println!(
                "{:<40} {:<18} {:<10} {:<18} DIRECTORY",
                "ID", "NAME", "STATUS", "ENVIRONMENT"
            );
            println!("{}", "-".repeat(115));
            for s in sessions {
                println!(
                    "{:<40} {:<18} {:<10} {:<18} {}",
                    s.id,
                    s.name,
                    s.status,
                    display_environment_state(&s.environment_state),
                    s.working_dir
                );
            }
        }
        Ok(axocoatl_daemon::ipc::IpcResponse::Error { message, .. }) => {
            eprintln!("✗ {message}");
            std::process::exit(1);
        }
        Ok(_) => {
            eprintln!("✗ unexpected daemon response");
            std::process::exit(1);
        }
        Err(e) => {
            eprintln!("✗ {e}");
            std::process::exit(1);
        }
    }
}

fn display_environment_state(state: &str) -> &str {
    match state {
        "ready" => "ready",
        "awaiting_approval" => "needs setup review",
        "preparing" => "preparing",
        "failed" => "setup failed",
        "unprepared" | "" => "needs setup review",
        other => other,
    }
}

fn session_readiness_guidance(session: &axocoatl_daemon::ipc::IpcSessionInfo) -> String {
    match session.environment_state.as_str() {
        "ready" => format!(
            "Send it work:  axocoatl session exec {} \"<instruction>\"",
            session.id
        ),
        "preparing" => concat!(
            "Environment preparation is still running.\n",
            "Open Axocoatl in your browser and select this Session to monitor it."
        )
        .to_string(),
        "failed" => {
            let mut guidance = String::from("Environment preparation failed.");
            if let Some(error) = session
                .environment_error
                .as_deref()
                .filter(|error| !error.trim().is_empty())
            {
                guidance.push_str("\n  Error: ");
                guidance.push_str(error.trim());
            }
            guidance.push_str(
                "\nOpen Axocoatl in your browser, select this Session, and choose Review setup.",
            );
            guidance
        }
        _ => {
            let mut guidance = String::from(
                "Setup review is required before this Session can run project commands.",
            );
            if let Some(command) = session
                .setup_command
                .as_deref()
                .filter(|command| !command.trim().is_empty())
            {
                guidance.push_str("\n  Proposed command (not run): ");
                guidance.push_str(command.trim());
            }
            guidance.push_str(
                "\nOpen Axocoatl in your browser, select this Session, and choose Review setup.",
            );
            guidance
        }
    }
}

async fn cmd_session_exec(session_id: &str, input: &str) {
    let mut client = session_ipc_client().await;
    println!("Running in session {session_id}...\n");
    let req = axocoatl_daemon::ipc::IpcRequest::ExecuteSession {
        session_id: session_id.to_string(),
        input: input.to_string(),
    };
    match client.request(&req).await {
        Ok(axocoatl_daemon::ipc::IpcResponse::SessionResponse {
            content,
            input_tokens,
            output_tokens,
            reasoning_tokens,
            token_usage_known,
            ..
        }) => {
            println!("{content}");
            println!(
                "\n[{}]",
                CliTokenMeasurement {
                    input: input_tokens,
                    output: output_tokens,
                    reasoning: reasoning_tokens,
                    complete: token_usage_known,
                }
                .clause("session")
            );
        }
        Ok(axocoatl_daemon::ipc::IpcResponse::Error {
            message,
            input_tokens,
            output_tokens,
            reasoning_tokens,
            token_usage_known,
        }) => {
            eprintln!("✗ {message}");
            if let Some(usage) = ipc_error_measurement(
                input_tokens,
                output_tokens,
                reasoning_tokens,
                token_usage_known,
            ) {
                eprintln!("  {}", usage.clause("failed session"));
            }
            std::process::exit(1);
        }
        Ok(_) => {
            eprintln!("✗ unexpected daemon response");
            std::process::exit(1);
        }
        Err(e) => {
            eprintln!("✗ {e}");
            std::process::exit(1);
        }
    }
}

async fn cmd_session_close(session_id: &str) {
    let mut client = session_ipc_client().await;
    let req = axocoatl_daemon::ipc::IpcRequest::CloseSession {
        session_id: session_id.to_string(),
    };
    match client.request(&req).await {
        Ok(axocoatl_daemon::ipc::IpcResponse::SessionClosed { .. }) => {
            println!("✓ Session closed")
        }
        Ok(axocoatl_daemon::ipc::IpcResponse::Error { message, .. }) => {
            eprintln!("✗ {message}");
            std::process::exit(1);
        }
        Ok(_) => {
            eprintln!("✗ unexpected daemon response");
            std::process::exit(1);
        }
        Err(e) => {
            eprintln!("✗ {e}");
            std::process::exit(1);
        }
    }
}

async fn cmd_agents_status(config_path: &std::path::Path) {
    use axocoatl_daemon::ipc::{IpcClient, IpcRequest, IpcResponse};

    let socket_path = axocoatl_daemon::ipc::default_socket_path();
    let (statuses, source) = if let Ok(mut client) = IpcClient::connect(&socket_path).await {
        match client
            .request(&IpcRequest::GetAgentStatus { agent_id: None })
            .await
        {
            Ok(IpcResponse::AgentStatuses { statuses }) => (statuses, "daemon (IPC)"),
            Ok(IpcResponse::Error { message, .. }) => {
                eprintln!("Error: {message}");
                std::process::exit(1);
            }
            Ok(_) => {
                eprintln!("Unexpected response from daemon");
                std::process::exit(1);
            }
            Err(e) => {
                eprintln!("IPC error: {e}");
                std::process::exit(1);
            }
        }
    } else {
        let config = match axocoatl_config::load_config(config_path).await {
            Ok(c) => c,
            Err(e) => {
                eprintln!("Configuration error:\n{e}");
                std::process::exit(1);
            }
        };
        let daemon = match axocoatl_daemon::AxocoatlDaemon::bootstrap_headless(config).await {
            Ok(d) => d,
            Err(e) => {
                eprintln!("Failed to bootstrap daemon: {e}");
                std::process::exit(1);
            }
        };
        let mut statuses = Vec::new();
        for id in daemon.agent_registry.list_ids().await {
            if let Some(actor) = daemon.agent_registry.get(&id).await {
                let status = axocoatl_actor::get_agent_status(&actor)
                    .await
                    .map(|s| format!("{s:?}"))
                    .unwrap_or_else(|e| format!("Unreachable ({e})"));
                statuses.push(axocoatl_daemon::ipc::IpcAgentStatus {
                    agent_id: id.to_string(),
                    status,
                });
            }
        }
        daemon.shutdown().await;
        (statuses, "in-process")
    };

    println!("Agent status ({source}):\n");
    println!("{:<20} {:<20}", "AGENT", "STATUS");
    println!("{}", "-".repeat(40));
    for s in &statuses {
        println!("{:<20} {:<20}", s.agent_id, s.status);
    }
}

async fn cmd_agents_restart(_config_path: &std::path::Path, agent_id: &str) {
    use axocoatl_daemon::ipc::{IpcClient, IpcRequest, IpcResponse};

    let socket_path = axocoatl_daemon::ipc::default_socket_path();
    let Ok(mut client) = IpcClient::connect(&socket_path).await else {
        eprintln!("Agent restart requires a running daemon.");
        eprintln!("Start one with 'axocoatl dev' or 'axocoatl serve', then retry.");
        std::process::exit(1);
    };

    match client
        .request(&IpcRequest::RestartAgent {
            agent_id: agent_id.to_string(),
        })
        .await
    {
        Ok(IpcResponse::RestartAck { agent_id }) => {
            println!("Agent '{agent_id}' restarted (session restored from checkpoint).");
        }
        Ok(IpcResponse::Error { message, .. }) => {
            eprintln!("Restart failed: {message}");
            std::process::exit(1);
        }
        Ok(_) => {
            eprintln!("Unexpected response from daemon");
            std::process::exit(1);
        }
        Err(e) => {
            eprintln!("IPC error: {e}");
            std::process::exit(1);
        }
    }
}

async fn cmd_skills_list() {
    let mut registry = axocoatl_core::SkillRegistry::new();
    registry.register_builtins();

    println!("{:<15} DESCRIPTION", "NAME");
    println!("{}", "-".repeat(60));
    for name in registry.names() {
        if let Some(skill) = registry.get(&name) {
            println!("{:<15} {}", skill.name, skill.description);
        }
    }
}

async fn cmd_skills_run(name: &str, params: Vec<String>) {
    let mut registry = axocoatl_core::SkillRegistry::new();
    registry.register_builtins();

    let skill = match registry.get(name) {
        Some(s) => s.clone(),
        None => {
            eprintln!("Skill not found: {name}");
            eprintln!("Available skills: {:?}", registry.names());
            std::process::exit(1);
        }
    };

    let mut param_map = std::collections::HashMap::new();
    for p in params {
        if let Some((k, v)) = p.split_once('=') {
            param_map.insert(k.to_string(), v.to_string());
        }
    }

    match skill.render(&param_map) {
        Ok(prompt) => {
            println!("Rendered skill prompt:\n");
            println!("{prompt}");
            println!("\n(To execute, pipe this to an agent via 'axocoatl chat')");
        }
        Err(e) => {
            eprintln!("Skill error: {e}");
            std::process::exit(1);
        }
    }
}

/// Resolve an IpcResponse either from a running daemon or by in-process bootstrap.
async fn mcp_query(
    config_path: &std::path::Path,
    request: axocoatl_daemon::ipc::IpcRequest,
) -> axocoatl_daemon::ipc::IpcResponse {
    use axocoatl_daemon::ipc::{IpcClient, IpcRequest, IpcResponse};

    let socket_path = axocoatl_daemon::ipc::default_socket_path();
    if let Ok(mut client) = IpcClient::connect(&socket_path).await {
        return client.request(&request).await.unwrap_or_else(|e| {
            eprintln!("IPC error: {e}");
            std::process::exit(1);
        });
    }

    let config = match axocoatl_config::load_config(config_path).await {
        Ok(c) => c,
        Err(e) => {
            eprintln!("Configuration error:\n{e}");
            std::process::exit(1);
        }
    };
    let daemon = match axocoatl_daemon::AxocoatlDaemon::bootstrap_headless(config).await {
        Ok(d) => d,
        Err(e) => {
            eprintln!("Failed to bootstrap daemon: {e}");
            std::process::exit(1);
        }
    };
    let reg = daemon.mcp_registry.read().await;
    let resp = match request {
        IpcRequest::ListMcpServers => IpcResponse::McpServers {
            servers: reg
                .servers()
                .into_iter()
                .map(|s| axocoatl_daemon::ipc::IpcMcpServer {
                    name: s.name.clone(),
                    transport: s.transport_type.clone(),
                    tool_count: s.tool_count,
                })
                .collect(),
        },
        IpcRequest::ListMcpTools { server } => IpcResponse::McpTools {
            tools: reg
                .tool_entries()
                .into_iter()
                .filter(|(_, srv, _)| server.as_ref().is_none_or(|s| s == srv))
                .map(|(name, srv, desc)| axocoatl_daemon::ipc::IpcMcpTool {
                    name,
                    server: srv,
                    description: desc,
                })
                .collect(),
        },
        _ => IpcResponse::Error {
            message: "unsupported in-process request".to_string(),
            input_tokens: None,
            output_tokens: None,
            reasoning_tokens: None,
            token_usage_known: None,
        },
    };
    drop(reg); // release the read lock before shutting down the daemon
    daemon.shutdown().await;
    resp
}

async fn cmd_mcp_servers(config_path: &std::path::Path) {
    use axocoatl_daemon::ipc::{IpcRequest, IpcResponse};

    match mcp_query(config_path, IpcRequest::ListMcpServers).await {
        IpcResponse::McpServers { servers } => {
            if servers.is_empty() {
                println!("No MCP servers connected.");
                println!("Add an 'mcp_servers:' section to your axocoatl.yaml.");
                return;
            }
            println!("{:<20} {:<18} {:>10}", "SERVER", "TRANSPORT", "TOOLS");
            println!("{}", "-".repeat(50));
            for s in &servers {
                println!("{:<20} {:<18} {:>10}", s.name, s.transport, s.tool_count);
            }
        }
        IpcResponse::Error { message, .. } => {
            eprintln!("Error: {message}");
            std::process::exit(1);
        }
        _ => {
            eprintln!("Unexpected response from daemon");
            std::process::exit(1);
        }
    }
}

async fn cmd_mcp_tools(config_path: &std::path::Path, server: Option<String>) {
    use axocoatl_daemon::ipc::{IpcRequest, IpcResponse};

    match mcp_query(config_path, IpcRequest::ListMcpTools { server }).await {
        IpcResponse::McpTools { tools } => {
            if tools.is_empty() {
                println!("No MCP tools discovered.");
                return;
            }
            println!("{:<24} {:<16} DESCRIPTION", "TOOL", "SERVER");
            println!("{}", "-".repeat(70));
            for t in &tools {
                let desc: String = t.description.chars().take(40).collect();
                println!("{:<24} {:<16} {}", t.name, t.server, desc);
            }
        }
        IpcResponse::Error { message, .. } => {
            eprintln!("Error: {message}");
            std::process::exit(1);
        }
        _ => {
            eprintln!("Unexpected response from daemon");
            std::process::exit(1);
        }
    }
}

async fn cmd_workflow_list(config_path: &std::path::Path) {
    use axocoatl_config::{AutomationNodeKind, AutomationTrigger};
    use axocoatl_daemon::ipc::{IpcRequest, IpcResponse, IpcWorkflowInfo};

    let socket_path = axocoatl_daemon::ipc::default_socket_path();
    let workflows =
        if let Ok(mut client) = axocoatl_daemon::ipc::IpcClient::connect(&socket_path).await {
            match client.request(&IpcRequest::ListWorkflows).await {
                Ok(IpcResponse::Workflows { workflows }) => workflows,
                Ok(IpcResponse::Error { message, .. }) => {
                    eprintln!("Workflow error: {message}");
                    std::process::exit(1);
                }
                Ok(_) => {
                    eprintln!("Unexpected response from daemon");
                    std::process::exit(1);
                }
                Err(error) => {
                    eprintln!("IPC error: {error}");
                    std::process::exit(1);
                }
            }
        } else {
            // No daemon is running. Use the lease-owned offline read so this
            // fallback performs the same legacy-runtime preflight and retained
            // storage-capability checks without spawning providers, actors, MCP
            // clients, webhooks, or other daemon services. A direct store open here
            // could race a live daemon during a transient IPC outage.
            let config = match axocoatl_config::load_config(config_path).await {
                Ok(config) => config,
                Err(error) => {
                    eprintln!("Configuration error:\n{error}");
                    std::process::exit(1);
                }
            };
            let automations =
                match axocoatl_daemon::AxocoatlDaemon::list_automations_offline(config).await {
                    Ok(automations) => automations,
                    Err(error) => {
                        eprintln!("Failed to read Automations: {error}");
                        std::process::exit(1);
                    }
                };
            automations
                .into_iter()
                .filter(|automation| matches!(&automation.trigger, AutomationTrigger::Manual))
                .map(|automation| {
                    let agents = automation
                        .nodes
                        .iter()
                        .filter_map(|node| match &node.kind {
                            AutomationNodeKind::Agent { agent_id, .. } => Some(agent_id.clone()),
                            _ => None,
                        })
                        .collect();
                    let entry_point = automation.nodes.iter().find_map(|node| {
                        let has_incoming = automation.edges.iter().any(|edge| edge.to == node.id);
                        match (&node.kind, has_incoming) {
                            (AutomationNodeKind::Agent { agent_id, .. }, false) => {
                                Some(agent_id.clone())
                            }
                            _ => None,
                        }
                    });
                    IpcWorkflowInfo {
                        id: automation.id,
                        name: automation.name,
                        agents,
                        entry_point,
                    }
                })
                .collect()
        };

    if workflows.is_empty() {
        println!("No manual Automations configured.");
        println!("Create one in Settings → Automations.");
        return;
    }

    println!(
        "{:<25} {:<25} {:<20} {:<15}",
        "ID", "NAME", "AGENTS", "ENTRY POINT"
    );
    println!("{}", "-".repeat(85));
    for w in &workflows {
        println!(
            "{:<25} {:<25} {:<20} {:<15}",
            w.id,
            w.name,
            w.agents.join(", "),
            w.entry_point.as_deref().unwrap_or("-"),
        );
    }
}

async fn cmd_workflow_run(config_path: &std::path::Path, workflow_id: &str, input: &str) {
    let config = match axocoatl_config::load_config(config_path).await {
        Ok(c) => c,
        Err(e) => {
            eprintln!("Configuration error:\n{e}");
            std::process::exit(1);
        }
    };

    // Try IPC first
    let socket_path = axocoatl_daemon::ipc::default_socket_path();
    if let Ok(mut client) = axocoatl_daemon::ipc::IpcClient::connect(&socket_path).await {
        println!("Connected to daemon via IPC.");
        println!("Running workflow '{workflow_id}'...\n");

        let req = axocoatl_daemon::ipc::IpcRequest::ExecuteWorkflow {
            workflow_id: workflow_id.to_string(),
            input: input.to_string(),
        };

        match client.request(&req).await {
            Ok(axocoatl_daemon::ipc::IpcResponse::WorkflowResponse {
                workflow_id,
                content,
                agent_outputs,
                total_input_tokens,
                total_output_tokens,
                total_reasoning_tokens,
                token_usage_known,
                completed_agents,
                failed_agents,
            }) => {
                let workflow_failed = workflow_response_is_failure(&failed_agents);
                if workflow_failed {
                    eprintln!("Workflow '{workflow_id}' finished with failed steps.\n");
                } else {
                    println!("Workflow '{workflow_id}' completed.\n");
                }
                println!("Agent outputs:");
                for output in &agent_outputs {
                    println!(
                        "  [{}] ({} in / {} out / {} reasoning tokens; {} total)",
                        output.agent_id,
                        output.input_tokens,
                        output.output_tokens,
                        output.reasoning_tokens,
                        output
                            .input_tokens
                            .saturating_add(output.output_tokens)
                            .saturating_add(output.reasoning_tokens)
                    );
                    println!("    {}\n", output.content);
                }
                println!("Final output:\n  {content}\n");
                println!("Completed: {}", completed_agents.join(", "));
                if !failed_agents.is_empty() {
                    println!(
                        "Failed: {}",
                        failed_agents
                            .iter()
                            .map(|(id, e)| format!("{id}: {e}"))
                            .collect::<Vec<_>>()
                            .join(", ")
                    );
                }
                println!(
                    "{}",
                    CliTokenMeasurement {
                        input: total_input_tokens,
                        output: total_output_tokens,
                        reasoning: total_reasoning_tokens,
                        complete: token_usage_known,
                    }
                    .clause("workflow")
                );
                if workflow_failed {
                    std::process::exit(1);
                }
            }
            Ok(axocoatl_daemon::ipc::IpcResponse::Error {
                message,
                input_tokens,
                output_tokens,
                reasoning_tokens,
                token_usage_known,
            }) => {
                eprintln!("Workflow error: {message}");
                if let Some(usage) = ipc_error_measurement(
                    input_tokens,
                    output_tokens,
                    reasoning_tokens,
                    token_usage_known,
                ) {
                    eprintln!("{}", usage.clause("failed workflow"));
                }
                std::process::exit(1);
            }
            Ok(_) => {
                eprintln!("Unexpected response from daemon");
                std::process::exit(1);
            }
            Err(e) => {
                eprintln!("IPC error: {e}");
                std::process::exit(1);
            }
        }
        return;
    }

    // Fall back to in-process execution
    println!("No running daemon, bootstrapping in-process...");
    let daemon = match axocoatl_daemon::AxocoatlDaemon::bootstrap_headless(config).await {
        Ok(d) => d,
        Err(e) => {
            eprintln!("Failed to bootstrap daemon: {e}");
            std::process::exit(1);
        }
    };

    println!("Running workflow '{workflow_id}'...\n");

    let context =
        axocoatl_daemon::automation_executor::AutomationExecutionContext::from_daemon(&daemon);
    let result = match context.get_automation(workflow_id).await {
        Some(automation)
            if matches!(
                &automation.trigger,
                axocoatl_config::AutomationTrigger::Manual
            ) =>
        {
            let result = axocoatl_daemon::automation_executor::execute_automation_in_context(
                &context,
                &automation,
                input,
            )
            .await;
            axocoatl_daemon::record_automation_outcome(&context, &automation, &result);
            result
        }
        _ => Err(axocoatl_daemon::DaemonError::WorkflowNotFound(
            workflow_id.to_string(),
        )),
    };

    match result {
        Ok(output) => {
            let terminal_error = output.terminal_error();
            if terminal_error.is_some() {
                eprintln!(
                    "Workflow '{}' finished with failed steps.\n",
                    output.workflow_id
                );
            } else {
                println!("Workflow '{}' completed.\n", output.workflow_id);
            }
            println!("Agent outputs:");
            for (agent_id, agent_output) in &output.agent_outputs {
                println!(
                    "  [{}] ({} in / {} out / {} reasoning tokens; {} total)",
                    agent_id,
                    agent_output.token_usage.input_tokens,
                    agent_output.token_usage.output_tokens,
                    agent_output.token_usage.reasoning_tokens.unwrap_or(0),
                    agent_output.token_usage.total()
                );
                println!("    {}\n", agent_output.content);
            }
            println!("Final output:\n  {}\n", output.final_content);
            println!("Completed: {}", output.completed_agents.join(", "));
            if !output.failed_agents.is_empty() {
                println!(
                    "Failed: {}",
                    output
                        .failed_agents
                        .iter()
                        .map(|(id, e)| format!("{id}: {e}"))
                        .collect::<Vec<_>>()
                        .join(", ")
                );
            }
            println!(
                "{}",
                CliTokenMeasurement {
                    input: output.total_token_usage.input_tokens,
                    output: output.total_token_usage.output_tokens,
                    reasoning: output.total_token_usage.reasoning_tokens.unwrap_or(0),
                    complete: output.token_usage_known,
                }
                .clause("workflow")
            );
            if let Some(error) = terminal_error {
                eprintln!("Workflow error: {error}");
                daemon.shutdown().await;
                std::process::exit(1);
            }
        }
        Err(e) => {
            eprintln!("Workflow error: {e}");
            if let Some((usage, known)) = e.workflow_token_usage() {
                eprintln!(
                    "{}",
                    CliTokenMeasurement {
                        input: usage.input_tokens,
                        output: usage.output_tokens,
                        reasoning: usage.reasoning_tokens.unwrap_or(0),
                        complete: known,
                    }
                    .clause("failed workflow")
                );
            }
            std::process::exit(1);
        }
    }

    daemon.shutdown().await;
}

async fn cmd_benchmark(name: &str) {
    println!("Running benchmark: {name}");
    println!("Use 'cargo bench' for detailed benchmarks.");
    println!("Available: token, routing, isolation, actor, all");
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::{CommandFactory, Parser};

    #[cfg(unix)]
    #[test]
    fn doctor_probe_never_follows_the_predictable_legacy_probe_name() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(outside.path(), b"safe").unwrap();
        symlink(outside.path(), root.path().join(".write_probe")).unwrap();
        probe_data_dir(root.path()).unwrap();
        assert_eq!(std::fs::read(outside.path()).unwrap(), b"safe");
    }

    #[test]
    fn legacy_chat_session_flag_is_only_a_display_label() {
        let cli = Cli::try_parse_from(["axocoatl", "chat", "--session", "legacy-label"])
            .expect("legacy flag should remain parseable");
        let Commands::Chat { session, .. } = cli.command else {
            panic!("expected chat command");
        };

        let (label, warning) = resolve_chat_label(session);
        assert_eq!(label, "legacy-label");
        assert_eq!(warning, Some(LEGACY_CHAT_SESSION_WARNING));
    }

    #[test]
    fn chat_help_says_the_legacy_label_does_not_resume_history() {
        let mut command = Cli::command();
        let help = command
            .find_subcommand_mut("chat")
            .expect("chat subcommand should exist")
            .render_long_help()
            .to_string();

        assert!(help.contains("Legacy display label only"));
        assert!(help.contains("does not select or resume stored chat history"));
    }

    #[test]
    fn generated_chat_label_does_not_emit_the_legacy_warning() {
        let (label, warning) = resolve_chat_label(None);

        assert!(uuid::Uuid::parse_str(&label).is_ok());
        assert_eq!(warning, None);
    }

    #[test]
    fn measured_usage_keeps_numeric_subtotal_and_sticky_lower_bound_wording() {
        let mut total = CliTokenMeasurement::known_zero();
        total.merge(CliTokenMeasurement {
            input: 10,
            output: 5,
            reasoning: 7,
            complete: true,
        });
        total.merge(CliTokenMeasurement {
            input: 3,
            output: 2,
            reasoning: 1,
            complete: false,
        });
        total.merge(CliTokenMeasurement {
            input: 4,
            output: 1,
            reasoning: 0,
            complete: true,
        });

        assert_eq!(total.total(), 33);
        assert!(
            !total.complete,
            "a later exact turn cannot restore completeness"
        );
        let rendered = total.clause("chat");
        assert!(rendered.contains("known token subtotal (lower bound)"));
        assert!(rendered.contains("≥33 total"));
        assert!(rendered.contains("8 reasoning"));
    }

    #[test]
    fn ipc_error_measurement_distinguishes_legacy_error_from_paid_failure() {
        assert_eq!(ipc_error_measurement(None, None, None, None), None);

        let measured = ipc_error_measurement(Some(13), Some(8), Some(3), Some(false))
            .expect("measured execution error");
        assert_eq!(measured.total(), 24);
        assert!(!measured.complete);
        assert!(measured.clause("failed session").contains("lower bound"));
    }

    #[test]
    fn workflow_with_handled_failed_steps_is_terminal_failure() {
        assert!(!workflow_response_is_failure(&[]));
        assert!(workflow_response_is_failure(&[(
            "writer".to_string(),
            "provider timeout".to_string(),
        )]));
    }

    #[test]
    fn tool_display_truncation_is_utf8_safe_and_bounded() {
        let rendered = truncate_for_display(&"€".repeat(40), 80);
        assert!(rendered.ends_with("..."));
        assert!(rendered.len() <= 80);
        assert!(rendered.is_char_boundary(rendered.len()));
        assert_eq!(truncate_for_display("short", 80), "short");
    }

    #[test]
    fn generated_next_steps_explicitly_load_the_process_environment() {
        let steps = next_steps_text("demo-project");

        assert!(steps.contains("cd demo-project"));
        assert!(steps.contains("mv .env.example .env"));
        assert!(steps.contains("never commit this file"));
        assert!(steps.contains("set -a\n  . ./.env\n  set +a"));
        assert!(steps.contains("process environment"));
        assert!(steps.contains("does not load\n.env automatically"));
        assert!(!steps.contains("cp .env.example .env"));
    }

    #[test]
    fn hosted_provider_copy_never_implies_automatic_dotenv_loading() {
        for provider in ["OpenRouter", "Anthropic", "OpenAI"] {
            let prompt = hosted_key_prompt(provider);
            let hint = missing_provider_key_hint(provider);

            assert!(prompt.contains("process environment"));
            assert!(!prompt.contains(".env"));
            assert!(hint.contains("process environment that starts Axocoatl"));
            assert!(!hint.contains(".env"));
        }
    }

    fn ipc_session_with_environment(
        state: &str,
        setup_command: Option<&str>,
        environment_error: Option<&str>,
    ) -> axocoatl_daemon::ipc::IpcSessionInfo {
        axocoatl_daemon::ipc::IpcSessionInfo {
            id: "ses-demo".to_string(),
            name: "Demo".to_string(),
            workspace_id: "wsp-demo".to_string(),
            working_dir: "/tmp/demo".to_string(),
            mode: "single-agent (coder)".to_string(),
            status: "active".to_string(),
            environment_state: state.to_string(),
            setup_command: setup_command.map(str::to_string),
            environment_error: environment_error.map(str::to_string),
        }
    }

    #[test]
    fn session_new_only_suggests_exec_after_the_environment_is_ready() {
        let ready = ipc_session_with_environment("ready", Some("npm ci"), None);
        let ready_guidance = session_readiness_guidance(&ready);
        assert!(ready_guidance.contains("axocoatl session exec ses-demo"));
        assert!(!ready_guidance.contains("Review setup"));

        let awaiting = ipc_session_with_environment("awaiting_approval", Some("npm ci"), None);
        let awaiting_guidance = session_readiness_guidance(&awaiting);
        assert!(awaiting_guidance.contains("Proposed command (not run): npm ci"));
        assert!(awaiting_guidance.contains("Review setup"));
        assert!(!awaiting_guidance.contains("session exec"));

        let failed = ipc_session_with_environment(
            "failed",
            Some("npm ci"),
            Some("package manager unavailable"),
        );
        let failed_guidance = session_readiness_guidance(&failed);
        assert!(failed_guidance.contains("package manager unavailable"));
        assert!(failed_guidance.contains("Review setup"));
        assert!(!failed_guidance.contains("session exec"));
    }

    #[test]
    fn session_list_environment_labels_are_actionable() {
        assert_eq!(
            display_environment_state("awaiting_approval"),
            "needs setup review"
        );
        assert_eq!(display_environment_state("failed"), "setup failed");
        assert_eq!(display_environment_state("ready"), "ready");
        assert_eq!(display_environment_state(""), "needs setup review");
    }
}
