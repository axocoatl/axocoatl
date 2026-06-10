use std::path::PathBuf;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "axocoatl")]
#[command(about = "Axocoatl — The agentic AI framework that doesn't waste tokens")]
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

    /// Validate a axocoatl.yaml configuration file
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

        /// Resume a previous session
        #[arg(long)]
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
    let exe = match std::env::current_exe() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("✗ could not locate the axocoatl binary: {e}");
            std::process::exit(1);
        }
    };
    let config_abs = std::path::Path::new(config)
        .canonicalize()
        .unwrap_or_else(|_| std::path::PathBuf::from(config));
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
                 own — are configured separately in axocoatl.yaml."
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

fn print_next_steps(project_name: &str) {
    println!();
    println!("Created Axocoatl project: {project_name}/");
    println!("  axocoatl.yaml    — agent configuration");
    println!("  .env.example     — API key template");
    println!("  data/            — runtime data directory");
    println!();
    println!("Next steps — copy/paste:");
    println!();
    println!("  cd {project_name}");
    println!("  cp .env.example .env       # then edit .env with your API keys");
    println!("  axocoatl doctor            # verify your environment");
    println!("  axocoatl validate          # check the config");
    println!("  axocoatl dev               # start the daemon + API server");
    println!("  axocoatl chat -a assistant # chat with your agent");
    println!();
}

async fn cmd_init(name: Option<String>) {
    let project_name = name.unwrap_or_else(|| "my-axocoatl-project".to_string());
    let dir = PathBuf::from(&project_name);

    if dir.exists() {
        eprintln!("Error: Directory '{}' already exists", project_name);
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
                    &format!("Set it in .env or the config before using {name} agents."),
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
    }

    // 5. Data dir writable
    let data_dir = std::env::var("AXOCOATL_DATA_DIR").unwrap_or_else(|_| "./data".to_string());
    match std::fs::create_dir_all(&data_dir).and_then(|_| {
        let probe = std::path::Path::new(&data_dir).join(".write_probe");
        std::fs::write(&probe, b"ok").and_then(|_| std::fs::remove_file(&probe))
    }) {
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

    println!();
    if hard_ok {
        println!("All required checks passed.");
    } else {
        println!("Some required checks FAILED — see hints above.");
    }
    hard_ok
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
        "OpenRouter (cloud, every model behind one key)",
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
            // OpenRouter — every model behind one key.
            let key: String = Input::new()
                .with_prompt("OpenRouter API key (leave blank to set later in .env)")
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
# OpenRouter is OpenAI-compatible; every model on openrouter.ai is
# reachable through this single key. See https://openrouter.ai/models.
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
                .with_prompt("Anthropic API key (leave blank to set later in .env)")
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
                .with_prompt("OpenAI API key (leave blank to set later in .env)")
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

    // Start the schedule runner (cron tasks).
    let (schedule_table, schedules) = {
        let d = state.read().await;
        (d.schedule_table.clone(), d.config.schedules.clone())
    };
    let sched_count = schedules.len();
    axocoatl_daemon::scheduler::start_scheduler(schedule_table, state.clone(), schedules);
    if sched_count > 0 {
        println!("  Schedules: {sched_count} loaded");
    }

    // Start the proactive agents (act-on-their-own from a timer or event).
    let (proactive_table, proactive, lattice) = {
        let d = state.read().await;
        (
            d.proactive_table.clone(),
            d.config.proactive.clone(),
            d.event_lattice.clone(),
        )
    };
    let proactive_count = proactive.len();
    axocoatl_daemon::proactive::start_proactive_runners(
        proactive_table,
        state.clone(),
        proactive,
        lattice,
    );
    if proactive_count > 0 {
        println!("  Proactive agents: {proactive_count} active");
    }

    // Supervise agents: restart any that crash, from their last checkpoint.
    axocoatl_daemon::supervision::start_supervision(state.clone());

    // Background memory consolidation (sleep-time): idle agents promote durable
    // facts from semantic memory into their curated core-memory blocks.
    let consolidation = { state.read().await.config.consolidation.clone() };
    axocoatl_daemon::consolidation::start_consolidation(state.clone(), consolidation);

    // Start IPC server for CLI clients
    let socket_path = axocoatl_daemon::ipc::default_socket_path();
    match axocoatl_daemon::ipc::start_ipc_server(state.clone(), &socket_path).await {
        Ok(_handle) => {
            println!("  IPC:    {}", socket_path.display());
        }
        Err(e) => {
            eprintln!("  IPC:    failed to start ({e})");
        }
    }

    println!("  Server: http://{host}:{port}");
    println!("  Health: http://{host}:{port}/health");
    println!();
    println!("Axocoatl is running. Press Ctrl+C to stop.");

    // Start the HTTP server (blocks until shutdown) — shares state with IPC
    if let Err(e) = axocoatl_server::serve_shared(state, &host, port).await {
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

    let daemon = match axocoatl_daemon::AxocoatlDaemon::bootstrap(config).await {
        Ok(d) => d,
        Err(e) => {
            eprintln!("Failed to bootstrap daemon: {e}");
            std::process::exit(1);
        }
    };

    println!("Axocoatl server starting on {host}:{port}");

    // Wrap in Arc<RwLock> so the scheduler can call execute_workflow.
    let state: std::sync::Arc<tokio::sync::RwLock<axocoatl_daemon::AxocoatlDaemon>> =
        std::sync::Arc::new(tokio::sync::RwLock::new(daemon));
    let (schedule_table, schedules) = {
        let d = state.read().await;
        (d.schedule_table.clone(), d.config.schedules.clone())
    };
    axocoatl_daemon::scheduler::start_scheduler(schedule_table, state.clone(), schedules);

    // Start the proactive agents (act-on-their-own from a timer or event).
    let (proactive_table, proactive, lattice) = {
        let d = state.read().await;
        (
            d.proactive_table.clone(),
            d.config.proactive.clone(),
            d.event_lattice.clone(),
        )
    };
    axocoatl_daemon::proactive::start_proactive_runners(
        proactive_table,
        state.clone(),
        proactive,
        lattice,
    );

    // Supervise agents: restart any that crash, from their last checkpoint.
    axocoatl_daemon::supervision::start_supervision(state.clone());

    // Background memory consolidation (sleep-time): idle agents promote durable
    // facts from semantic memory into their curated core-memory blocks.
    let consolidation = { state.read().await.config.consolidation.clone() };
    axocoatl_daemon::consolidation::start_consolidation(state.clone(), consolidation);

    if let Err(e) = axocoatl_server::serve_shared(state, &host, port).await {
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

    let daemon = match axocoatl_daemon::AxocoatlDaemon::bootstrap(config).await {
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
        let daemon = match axocoatl_daemon::AxocoatlDaemon::bootstrap(config).await {
            Ok(d) => d,
            Err(e) => {
                eprintln!("Failed to bootstrap daemon: {e}");
                std::process::exit(1);
            }
        };
        let ids = daemon.agent_registry.list_ids().await;
        let mut per_agent = Vec::new();
        let mut total_in = 0;
        let mut total_out = 0;
        for id in ids {
            if let Some(actor) = daemon.agent_registry.get(&id).await {
                if let Ok(u) = axocoatl_actor::get_agent_token_usage(&actor).await {
                    total_in += u.input_tokens;
                    total_out += u.output_tokens;
                    per_agent.push(axocoatl_daemon::ipc::IpcTokenUsage {
                        agent_id: id.to_string(),
                        input_tokens: u.input_tokens,
                        output_tokens: u.output_tokens,
                        reasoning_tokens: u.reasoning_tokens,
                    });
                }
            }
        }
        daemon.shutdown().await;
        Ok(IpcResponse::TokenUsage {
            per_agent,
            total_input: total_in,
            total_output: total_out,
        })
    };

    match resp {
        Ok(IpcResponse::TokenUsage {
            per_agent,
            total_input,
            total_output,
        }) => {
            println!(
                "{:<20} {:>10} {:>10} {:>10}",
                "AGENT", "INPUT", "OUTPUT", "TOTAL"
            );
            println!("{}", "-".repeat(54));
            for u in &per_agent {
                println!(
                    "{:<20} {:>10} {:>10} {:>10}",
                    u.agent_id,
                    u.input_tokens,
                    u.output_tokens,
                    u.input_tokens + u.output_tokens
                );
            }
            println!("{}", "-".repeat(54));
            println!(
                "{:<20} {:>10} {:>10} {:>10}",
                "TOTAL",
                total_input,
                total_output,
                total_input + total_output
            );
        }
        Ok(IpcResponse::Error { message }) => {
            eprintln!("Error: {message}");
            std::process::exit(1);
        }
        Ok(_) => eprintln!("Unexpected response from daemon"),
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
        let args_display = if args_summary.len() > 80 {
            format!("{}...", &args_summary[..77])
        } else {
            args_summary
        };
        if let Some(result) = &tc.result {
            let result_str = result.to_string();
            let result_display = if result_str.len() > 60 {
                format!("{}...", &result_str[..57])
            } else {
                result_str
            };
            println!(
                "  [tool: {}({})] -> {}",
                tc.tool_name, args_display, result_display
            );
        } else {
            println!("  [tool: {}({})]", tc.tool_name, args_display);
        }
    }
}

async fn cmd_chat(config_path: &std::path::Path, agent_id: &str, session_id: Option<String>) {
    use std::io::{self, BufRead, Write};

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

    let session = session_id.unwrap_or_else(|| uuid::Uuid::new_v4().to_string());

    // Try connecting to a running daemon via IPC first
    let socket_path = axocoatl_daemon::ipc::default_socket_path();
    let ipc_client = axocoatl_daemon::ipc::IpcClient::connect(&socket_path)
        .await
        .ok();

    let using_ipc = ipc_client.is_some();

    // If no daemon running, bootstrap in-process
    let daemon = if ipc_client.is_none() {
        Some(
            match axocoatl_daemon::AxocoatlDaemon::bootstrap(config).await {
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
    println!("  Session: {session}");
    if using_ipc {
        println!("  Mode:    connected to daemon (IPC)");
    } else {
        println!("  Mode:    in-process");
    }
    println!("  Type 'exit' or Ctrl+D to quit.\n");

    let stdin = io::stdin();
    let mut stdout = io::stdout();

    let mut total_input_tokens: usize = 0;
    let mut total_output_tokens: usize = 0;
    let mut turn_count: usize = 0;

    // Mutable IPC client (needs to be mutable for requests)
    let mut ipc = ipc_client;

    loop {
        print!("you> ");
        stdout.flush().unwrap();

        let mut line = String::new();
        if stdin.lock().read_line(&mut line).unwrap() == 0 {
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
            let req = axocoatl_daemon::ipc::IpcRequest::Execute {
                agent_id: agent_id.to_string(),
                input: input.to_string(),
                session_id: session.clone(),
            };
            match client.request(&req).await {
                Ok(axocoatl_daemon::ipc::IpcResponse::Response {
                    content,
                    tool_calls,
                    input_tokens,
                    output_tokens,
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

                    total_input_tokens += input_tokens;
                    total_output_tokens += output_tokens;
                    println!(
                        "  (tokens: {} in / {} out | session total: {} in / {} out)",
                        input_tokens, output_tokens, total_input_tokens, total_output_tokens,
                    );
                    println!();
                }
                Ok(axocoatl_daemon::ipc::IpcResponse::Error { message }) => {
                    eprintln!("\nerror> {message}\n");
                }
                Ok(_) => {
                    eprintln!("\nerror> unexpected response from daemon\n");
                }
                Err(e) => {
                    eprintln!("\nerror> IPC error: {e}\n");
                }
            }
        } else if let Some(ref daemon) = daemon {
            match daemon.execute_agent(agent_id, input).await {
                Ok(output) => {
                    turn_count += 1;
                    display_tool_calls(&output.tool_calls);

                    println!("\nagent> {}\n", output.content);

                    total_input_tokens += output.token_usage.input_tokens;
                    total_output_tokens += output.token_usage.output_tokens;
                    println!(
                        "  (tokens: {} in / {} out | session total: {} in / {} out)",
                        output.token_usage.input_tokens,
                        output.token_usage.output_tokens,
                        total_input_tokens,
                        total_output_tokens,
                    );
                    println!();
                }
                Err(e) => {
                    eprintln!("\nerror> {e}\n");
                }
            }
        }
    }

    println!();
    println!("Session summary:");
    println!("  Turns:  {turn_count}");
    println!(
        "  Tokens: {total_input_tokens} in / {total_output_tokens} out ({} total)",
        total_input_tokens + total_output_tokens
    );
    println!("  Session ID: {session} (use --session {session} to resume)");
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
            println!(
                "\nSend it work:  axocoatl session exec {} \"<instruction>\"",
                session.id
            );
        }
        Ok(axocoatl_daemon::ipc::IpcResponse::Error { message }) => {
            eprintln!("✗ {message}");
            std::process::exit(1);
        }
        Ok(_) => eprintln!("✗ unexpected daemon response"),
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
                "{:<40} {:<18} {:<10} {}",
                "ID", "NAME", "STATUS", "DIRECTORY"
            );
            println!("{}", "-".repeat(96));
            for s in sessions {
                println!(
                    "{:<40} {:<18} {:<10} {}",
                    s.id, s.name, s.status, s.working_dir
                );
            }
        }
        Ok(axocoatl_daemon::ipc::IpcResponse::Error { message }) => {
            eprintln!("✗ {message}");
            std::process::exit(1);
        }
        Ok(_) => eprintln!("✗ unexpected daemon response"),
        Err(e) => {
            eprintln!("✗ {e}");
            std::process::exit(1);
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
            ..
        }) => {
            println!("{content}");
            println!("\n[{input_tokens} in / {output_tokens} out tokens]");
        }
        Ok(axocoatl_daemon::ipc::IpcResponse::Error { message }) => {
            eprintln!("✗ {message}");
            std::process::exit(1);
        }
        Ok(_) => eprintln!("✗ unexpected daemon response"),
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
        Ok(axocoatl_daemon::ipc::IpcResponse::Error { message }) => {
            eprintln!("✗ {message}");
            std::process::exit(1);
        }
        Ok(_) => eprintln!("✗ unexpected daemon response"),
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
            Ok(IpcResponse::Error { message }) => {
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
        let daemon = match axocoatl_daemon::AxocoatlDaemon::bootstrap(config).await {
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
        Ok(IpcResponse::Error { message }) => {
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
    let daemon = match axocoatl_daemon::AxocoatlDaemon::bootstrap(config).await {
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
        IpcResponse::Error { message } => {
            eprintln!("Error: {message}");
            std::process::exit(1);
        }
        _ => eprintln!("Unexpected response from daemon"),
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
        IpcResponse::Error { message } => {
            eprintln!("Error: {message}");
            std::process::exit(1);
        }
        _ => eprintln!("Unexpected response from daemon"),
    }
}

async fn cmd_workflow_list(config_path: &std::path::Path) {
    let config = match axocoatl_config::load_config(config_path).await {
        Ok(c) => c,
        Err(e) => {
            eprintln!("Configuration error:\n{e}");
            std::process::exit(1);
        }
    };

    if config.workflows.is_empty() {
        println!("No workflows configured.");
        println!("Add a 'workflows:' section to your axocoatl.yaml.");
        return;
    }

    println!(
        "{:<25} {:<25} {:<20} {:<15}",
        "ID", "NAME", "AGENTS", "ENTRY POINT"
    );
    println!("{}", "-".repeat(85));
    for w in &config.workflows {
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
                completed_agents,
                failed_agents,
            }) => {
                println!("Workflow '{workflow_id}' completed.\n");
                println!("Agent outputs:");
                for output in &agent_outputs {
                    println!(
                        "  [{}] ({} in / {} out tokens)",
                        output.agent_id, output.input_tokens, output.output_tokens
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
                    "Total tokens: {} in / {} out",
                    total_input_tokens, total_output_tokens
                );
            }
            Ok(axocoatl_daemon::ipc::IpcResponse::Error { message }) => {
                eprintln!("Workflow error: {message}");
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
    let daemon = match axocoatl_daemon::AxocoatlDaemon::bootstrap(config).await {
        Ok(d) => d,
        Err(e) => {
            eprintln!("Failed to bootstrap daemon: {e}");
            std::process::exit(1);
        }
    };

    println!("Running workflow '{workflow_id}'...\n");

    match daemon.execute_workflow(workflow_id, input).await {
        Ok(output) => {
            println!("Workflow '{}' completed.\n", output.workflow_id);
            println!("Agent outputs:");
            for (agent_id, agent_output) in &output.agent_outputs {
                println!(
                    "  [{}] ({} in / {} out tokens)",
                    agent_id,
                    agent_output.token_usage.input_tokens,
                    agent_output.token_usage.output_tokens
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
                "Total tokens: {} in / {} out",
                output.total_token_usage.input_tokens, output.total_token_usage.output_tokens
            );
        }
        Err(e) => {
            eprintln!("Workflow error: {e}");
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
