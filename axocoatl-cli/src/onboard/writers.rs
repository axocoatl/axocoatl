//! Steps 3B and 3C: Claude Code or Codex writes the code.
//!
//! Onboarding finds the `claude` CLI, says what each needs (a Claude Pro or
//! Max plan; a ChatGPT plan or an OpenAI Platform organization), builds the
//! pinned sandbox image and signs in through the existing `axocoatl recipe
//! build` and `axocoatl connect` commands after asking, then sets up the
//! reviewer alone, on Ollama or OpenRouter. Sign-ins need a terminal: with
//! `--yes` they run only with `--connect`, and otherwise become steps left.

use std::io::IsTerminal;

use axocoatl_daemon::external_agent::claude_code::CLAUDE_CODE_SECRET;
use axocoatl_daemon::external_agent::codex::{CODEX_CHATGPT_SECRET, CODEX_SECRET};

use super::config::Provider;
use super::host::Cmd;
use super::ui::Mode;
use super::{Choice, Onboarding, WriterChoice};

/// What fix-claude-code and fix-codex run in: the repository's toolchain.
pub(crate) const TOOLCHAIN_NOTE: &str =
    "runs in your repository's language image (Node.js, Python, Go or Rust, detected from \
     package.json, pyproject.toml or requirements, go.mod or Cargo.toml), with the \
     dependencies installed first and only the package registries open for that step \
     (https://docs.axocoatl.ai/configure/toolchains/)";

fn stored(o: &Onboarding, names: &[&str]) -> Option<String> {
    let secrets = super::stored_secret_names(o);
    names
        .iter()
        .find(|name| secrets.iter().any(|secret| secret == **name))
        .map(|name| name.to_string())
}

fn image_built(o: &Onboarding, recipe: &str) -> bool {
    axocoatl_daemon::external_agent::recipe_images::recorded_images_at(&o.data_dir())
        .map(|images| {
            images
                .iter()
                .any(|image| image.recipes.len() == 1 && image.recipes[0] == recipe)
        })
        .unwrap_or(false)
}

/// Run `axocoatl <args>` (this executable) with the person's terminal.
async fn axocoatl(o: &Onboarding, args: &[&str]) -> Result<(), String> {
    let cmd = Cmd::new(&o.host.exe.display().to_string(), args);
    if o.ui.plan() {
        o.ui.say(format!("    would run: axocoatl {}", args.join(" ")));
        return Ok(());
    }
    o.ui.say(format!("  $ axocoatl {}", args.join(" ")));
    o.host.run(&cmd).await
}

/// A sign-in runs when there is a terminal and the person agrees; with
/// `--yes`, only with `--connect`.
fn may_sign_in(o: &Onboarding, question: &str) -> bool {
    let terminal = std::io::stdin().is_terminal();
    match (o.args.connect(), o.ui.mode) {
        (Some(false), _) => false,
        (_, Mode::Plan) => o.ui.consent(question, true, false),
        (Some(true), _) if terminal => {
            o.ui.say(format!("  {question} yes (--connect)"));
            true
        }
        (None, Mode::Interactive) if terminal => o.ui.consent(question, true, false),
        _ => false,
    }
}

/// Build the recipe image unless it is built, after asking.
async fn build_image(o: &mut Onboarding, recipe: &str, size: &str, sandbox_ready: bool) -> bool {
    if image_built(o, recipe) {
        o.ui.ok(format!("The {recipe} sandbox image is built"));
        return true;
    }
    let command = format!("axocoatl recipe build {recipe}");
    if !sandbox_ready && !o.ui.plan() {
        o.step_left(format!(
            "Build the {recipe} sandbox image once Podman is ready: {command}"
        ));
        return false;
    }
    let build = match o.args.recipe_build() {
        Some(choice) => {
            o.ui.say(format!(
                "  Build the {recipe} sandbox image ({size}): {}",
                if choice {
                    "yes (--recipe-build)"
                } else {
                    "no (--no-recipe-build)"
                }
            ));
            choice
        }
        None => o.ui.consent(
            &format!("Build the {recipe} sandbox image now (`{command}`, about {size})?"),
            true,
            false,
        ),
    };
    if !build {
        o.step_left(format!("Build the {recipe} sandbox image: {command}"));
        return false;
    }
    let config = o.paths.config_path.display().to_string();
    match axocoatl(o, &["recipe", "build", recipe, "--config", &config]).await {
        Ok(()) => true,
        Err(error) => {
            o.step_left(format!("{error}; build it with: {command}"));
            false
        }
    }
}

/// Step 3's heading for Claude Code or Codex.
pub(crate) fn heading(writer: WriterChoice) -> &'static str {
    if writer == WriterChoice::ClaudeCode {
        "Step 3 · Claude Code"
    } else {
        "Step 3 · Codex"
    }
}

/// Steps 3B (Claude Code) and 3C (Codex).
pub(crate) async fn run(
    o: &mut Onboarding,
    writer: WriterChoice,
    sandbox_ready: bool,
    choice: &mut Choice,
) {
    let config = o.paths.config_path.display().to_string();
    choice.external_writer = Some(writer);
    choice.external_writer_model = o.args.writer_model.clone();
    if writer == WriterChoice::ClaudeCode {
        match &o.facts.claude {
            Some((path, version)) => o.ui.ok(format!(
                "claude {} at {}",
                version.as_deref().unwrap_or("(version unknown)"),
                path.display()
            )),
            None => o.step_left(
                "Install Claude Code (https://docs.anthropic.com/en/docs/claude-code/setup) and \
                 run `axocoatl connect claude-code` (or pass --claude PATH to it)",
            ),
        }
        o.ui.say(
            "  Claude Code runs on your Claude Pro or Max plan: `claude setup-token` needs a \
             subscription. Anthropic API keys, Bedrock and Vertex are not supported by \
             fix-claude-code.",
        );
        build_image(o, "claude-code", "0.6 to 0.9 GB", sandbox_ready).await;
        match stored(o, &[CLAUDE_CODE_SECRET]) {
            Some(_) => {
                o.ui.ok(format!("Signed in: {CLAUDE_CODE_SECRET} is stored"))
            }
            None if o.facts.claude.is_some() => {
                if may_sign_in(
                    o,
                    "Sign in now? `axocoatl connect claude-code` runs `claude setup-token` and \
                     opens your browser.",
                ) {
                    if let Err(error) =
                        axocoatl(o, &["connect", "claude-code", "--config", &config]).await
                    {
                        o.step_left(format!(
                            "{error}; sign in with: axocoatl connect claude-code"
                        ));
                    }
                } else {
                    o.step_left("Sign in to Claude Code: axocoatl connect claude-code");
                }
            }
            None => {}
        }
    } else {
        if let Some(path) = &o.facts.codex {
            o.ui.ok(format!(
                "codex at {} (not needed: Axocoatl runs the pinned Codex in its sandbox)",
                path.display()
            ));
        }
        let items = vec![
            "your ChatGPT plan (recommended)".to_string(),
            "an OpenAI API key (needs an OpenAI Platform organization; billed at API rates)"
                .to_string(),
        ];
        let api_key = match o.kept.codex_credential.as_deref() {
            Some(kept @ (CODEX_SECRET | CODEX_CHATGPT_SECRET)) => {
                o.ui.say(format!(
                    "  Codex runs bill to {} (kept from your configuration: {kept})",
                    if kept == CODEX_SECRET {
                        "an OpenAI API key"
                    } else {
                        "your ChatGPT plan"
                    }
                ));
                kept == CODEX_SECRET
            }
            _ => o.ui.select("Bill Codex runs to:", &items, 0) == 1,
        };
        let (credential, connect_args): (&'static str, Vec<&str>) = if api_key {
            (CODEX_SECRET, vec!["connect", "codex", "--api-key-only"])
        } else {
            (CODEX_CHATGPT_SECRET, vec!["connect", "codex"])
        };
        // Named on every run's command line, not `auto`: storing a key later
        // would otherwise move a ChatGPT-plan setup to API billing.
        choice.codex_credential = Some(credential);
        match stored(o, &[credential]) {
            Some(_) => {
                o.ui.ok(format!("Signed in: {credential} is stored"));
                build_image(o, "codex", "0.8 to 1.2 GB", sandbox_ready).await;
            }
            None => {
                let question = format!(
                    "Sign in now? `axocoatl {}` builds the Codex sandbox image once (about 0.8 to \
                     1.2 GB) and opens your browser to sign in{}.",
                    connect_args.join(" "),
                    if api_key {
                        " and take an OpenAI API key"
                    } else {
                        " with your ChatGPT account"
                    }
                );
                if !sandbox_ready && !o.ui.plan() {
                    o.step_left(format!(
                        "Sign in to Codex once Podman is ready: axocoatl {}",
                        connect_args.join(" ")
                    ));
                } else if may_sign_in(o, &question) {
                    let mut args = connect_args.clone();
                    args.extend(["--config", config.as_str()]);
                    if let Err(error) = axocoatl(o, &args).await {
                        o.step_left(format!(
                            "{error}; sign in with: axocoatl {}",
                            connect_args.join(" ")
                        ));
                    }
                } else {
                    build_image(o, "codex", "0.8 to 1.2 GB", sandbox_ready).await;
                    o.step_left(format!(
                        "Sign in to Codex: axocoatl {}",
                        connect_args.join(" ")
                    ));
                }
            }
        }
    }
    let name = if writer == WriterChoice::ClaudeCode {
        "Claude Code"
    } else {
        "Codex"
    };
    o.ui.say(format!("  {name} {TOOLCHAIN_NOTE}."));
    reviewer(o, choice).await;
}

/// The reviewer alone: a local Ollama model or OpenRouter.
async fn reviewer(o: &mut Onboarding, choice: &mut Choice) {
    o.ui.say("\n  The reviewer: a different model that only reads the change.");
    let provider = match o.args.reviewer.as_deref() {
        Some(spec) if spec.starts_with("openrouter:") => Provider::OpenRouter,
        Some(_) => Provider::Ollama,
        None => {
            let big_enough = o
                .facts
                .memory_bytes
                .is_some_and(|bytes| axocoatl_isolation::host::at_least_gb(bytes, 48));
            let local = match super::ollama::too_large(o, "gpt-oss:20b") {
                Some(why) => format!("Local Ollama gpt-oss:20b (13.8 GB, {why})"),
                None => "Local Ollama gpt-oss:20b (13.8 GB, needs 48 GB memory)".to_string(),
            };
            let items = vec![local, "OpenRouter (hosted, credits)".to_string()];
            if o.ui.select(
                "Choose the reviewer",
                &items,
                if big_enough { 0 } else { 1 },
            ) == 0
            {
                Provider::Ollama
            } else {
                Provider::OpenRouter
            }
        }
    };
    match provider {
        Provider::Ollama => super::ollama::reviewer_only(o, choice).await,
        Provider::OpenRouter => super::openrouter::run(o, choice, false).await,
    }
}
