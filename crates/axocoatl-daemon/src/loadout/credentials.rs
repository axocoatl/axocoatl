//! The stored credential each external writer's route sends, chosen when
//! the run is admitted. Owner: workstream `agents`.
//!
//! A loadout's external writer names a credential (`credential:
//! codex-chatgpt`), names `auto` (the built-in `fix-codex`'s default), or
//! names none (its runtime's default, `codex-openai` or
//! `claude-code-oauth`); a `text` parameter can carry any of these, so
//! `--param credential=…` chooses one. `auto` is the first of its runtime's
//! credentials ([`AgentRuntime::credentials`]) that is stored: for Codex
//! `codex-openai` when it is stored, else `codex-chatgpt`. When none is,
//! the run is refused before anything is created (exit code 3), saying how
//! to connect.
//!
//! The choice is recorded so it is never ambiguous which credential ran:
//! the run's manifest keeps it (`RunManifest::credentials`; a restart
//! applies it again rather than choosing anew), a `note` event says it when
//! the run starts (`axocoatl run` prints it), and the Outcome's notes say
//! it (the summary, the JUnit file and the record).

use std::collections::BTreeMap;

use axocoatl_config::loadout::{AgentRuntime, ResolvedLoadout, AUTO_CREDENTIAL};

use super::RunError;

/// What the credential `name` is and where its route sends it.
pub fn credential_words(name: &str) -> &'static str {
    match name {
        "claude-code-oauth" => "Claude Code's OAuth token, sent to api.anthropic.com",
        "codex-openai" => "an OpenAI API key, sent to api.openai.com",
        "codex-chatgpt" => "a ChatGPT sign-in, sent to chatgpt.com",
        _ => "a stored credential",
    }
}

/// How to store one of `runtime`'s credentials, for a refused `auto`.
fn how_to_connect(runtime: AgentRuntime) -> String {
    match runtime {
        AgentRuntime::Codex => "run `axocoatl connect codex` to sign in with your ChatGPT plan \
             (stored as codex-chatgpt), or `axocoatl connect codex --api-key` to also store the \
             OpenAI API key OpenAI issues with that sign-in (as codex-openai), then run again"
            .into(),
        _ => runtime
            .credentials()
            .first()
            .map(|name| crate::secret_store::how_to_store(name))
            .unwrap_or_default(),
    }
}

/// Choose the credential of each external writer of `resolved` whose
/// loadout says `auto`: the first of its runtime's credentials for which
/// `stored` is true. Every external writer then has the credential its route
/// sends in `resolved.agent_credentials`, and a note in `resolved.notes`
/// says which, and why when `auto` chose it. Returns the credentials by
/// Agent id, as the run's manifest records them. A writer whose `auto`
/// finds nothing stored is a usage error naming how to connect.
pub fn choose(
    resolved: &mut ResolvedLoadout,
    stored: impl Fn(&str) -> bool,
) -> Result<BTreeMap<String, String>, RunError> {
    let mut chosen = BTreeMap::new();
    let agents: Vec<(String, AgentRuntime)> = resolved
        .loadout
        .file
        .agents
        .iter()
        .map(|agent| (agent.id.clone(), agent.runtime))
        .collect();
    for (id, runtime) in agents {
        let Some(asked) = resolved.agent_credentials.get(&id).cloned() else {
            continue;
        };
        let names = runtime.credentials();
        let note = if asked == AUTO_CREDENTIAL {
            let Some(name) = names.iter().find(|name| stored(name)) else {
                return Err(RunError::Usage(format!(
                    "Agent {id} ({}) has credential {AUTO_CREDENTIAL}, and {} stored: {}",
                    runtime.id(),
                    match names {
                        [only] => format!("{only} is not"),
                        [first, second] => format!("neither {first} nor {second} is"),
                        _ => format!("none of {} is", names.join(", ")),
                    },
                    how_to_connect(runtime)
                )));
            };
            let why = match names {
                [_] => format!("the only credential a {} writer sends", runtime.id()),
                _ => format!("the first of {} that is stored", names.join(" and ")),
            };
            resolved
                .agent_credentials
                .insert(id.clone(), name.to_string());
            format!(
                "Agent {id} ({}) sends the stored credential {name} ({}); credential \
                 {AUTO_CREDENTIAL} chose it, {why}.",
                runtime.id(),
                credential_words(name)
            )
        } else {
            format!(
                "Agent {id} ({}) sends the stored credential {asked} ({}).",
                runtime.id(),
                credential_words(&asked)
            )
        };
        resolved.notes.push(note);
        if let Some(name) = resolved.agent_credentials.get(&id) {
            chosen.insert(id, name.clone());
        }
    }
    Ok(chosen)
}

/// Apply the credentials a run's manifest recorded at admission (`choose`)
/// to `resolved` again, as a restart re-resolves the run's loadout: the
/// stored secrets may have changed since, and the run's Session sends what
/// its retained definition names.
pub fn apply_recorded(resolved: &mut ResolvedLoadout, recorded: &BTreeMap<String, String>) {
    for (id, name) in recorded {
        if let Some(credential) = resolved.agent_credentials.get_mut(id) {
            *credential = name.clone();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axocoatl_config::loadout::{builtin_loadouts, resolve_loadout, ParamValues};

    fn resolved(id: &str, params: &[(&str, &str)]) -> ResolvedLoadout {
        let loadout = builtin_loadouts()
            .into_iter()
            .map(|loadout| loadout.unwrap())
            .find(|loadout| loadout.file.id == id)
            .unwrap();
        let mut values = ParamValues::new();
        values.insert("reviewer_model".into(), "ollama:gpt-oss:120b".into());
        for (name, value) in params {
            values.insert((*name).into(), (*value).into());
        }
        resolve_loadout(&loadout, &values, "fix it", "/repo").unwrap()
    }

    /// fix-codex's `auto` takes the API key when it is stored, else the
    /// ChatGPT sign-in, else refuses saying how to connect; each choice is
    /// a note.
    #[test]
    fn auto_takes_the_first_stored_codex_credential() {
        for (stored, expected) in [
            (&["codex-openai", "codex-chatgpt"][..], Some("codex-openai")),
            (&["codex-openai"][..], Some("codex-openai")),
            (&["codex-chatgpt"][..], Some("codex-chatgpt")),
            (&[][..], None),
        ] {
            let mut run = resolved("fix-codex", &[]);
            let result = choose(&mut run, |name| stored.contains(&name));
            match expected {
                Some(name) => {
                    let chosen = result.unwrap();
                    assert_eq!(chosen["writer"], name, "{stored:?}");
                    assert_eq!(run.agent_credentials["writer"], name, "{stored:?}");
                    assert_eq!(
                        run.notes,
                        [format!(
                            "Agent writer (codex) sends the stored credential {name} ({}); \
                             credential auto chose it, the first of codex-openai and \
                             codex-chatgpt that is stored.",
                            credential_words(name)
                        )]
                    );
                }
                None => {
                    let error = result.unwrap_err();
                    assert!(matches!(error, RunError::Usage(_)), "{error}");
                    assert_eq!(
                        error.to_string(),
                        "Agent writer (codex) has credential auto, and neither codex-openai nor \
                         codex-chatgpt is stored: run `axocoatl connect codex` to sign in with \
                         your ChatGPT plan (stored as codex-chatgpt), or `axocoatl connect codex \
                         --api-key` to also store the OpenAI API key OpenAI issues with that \
                         sign-in (as codex-openai), then run again"
                    );
                    assert_eq!(run.agent_credentials["writer"], AUTO_CREDENTIAL);
                }
            }
        }
    }

    /// A credential the run names is kept as named, whatever is stored (the
    /// route's own check refuses one that is not), and noted.
    #[test]
    fn a_named_credential_is_kept_and_noted() {
        for name in ["codex-openai", "codex-chatgpt"] {
            let mut run = resolved("fix-codex", &[("credential", name)]);
            let chosen = choose(&mut run, |_| false).unwrap();
            assert_eq!(chosen["writer"], name);
            assert_eq!(
                run.notes,
                [format!(
                    "Agent writer (codex) sends the stored credential {name} ({}).",
                    credential_words(name)
                )]
            );
        }
        let mut run = resolved("fix-claude-code", &[]);
        let chosen = choose(&mut run, |_| false).unwrap();
        assert_eq!(chosen["writer"], "claude-code-oauth");
        assert_eq!(
            run.notes,
            [
                "Agent writer (claude-code) sends the stored credential claude-code-oauth (Claude \
              Code's OAuth token, sent to api.anthropic.com)."
            ]
        );
        // A native writer has no credential and no note.
        let mut run = resolved("fix", &[("writer_model", "ollama:qwen3:32b")]);
        assert!(choose(&mut run, |_| true).unwrap().is_empty());
        assert!(run.notes.is_empty());
    }

    /// `auto` on a Claude Code writer is its only credential, or a refusal
    /// that says how to connect.
    #[test]
    fn auto_on_claude_code_is_its_only_credential() {
        let mut run = resolved("fix-claude-code", &[]);
        run.agent_credentials
            .insert("writer".into(), AUTO_CREDENTIAL.into());
        let error = choose(&mut run.clone(), |_| false).unwrap_err().to_string();
        assert!(
            error.starts_with(
                "Agent writer (claude-code) has credential auto, and claude-code-oauth is not \
                 stored: run `axocoatl connect claude-code`"
            ),
            "{error}"
        );
        let chosen = choose(&mut run, |name| name == "claude-code-oauth").unwrap();
        assert_eq!(chosen["writer"], "claude-code-oauth");
        assert!(run.notes[0].ends_with(
            "credential auto chose it, the only credential a claude-code writer sends."
        ));
    }

    /// A restart applies the recorded choice, not a new one.
    #[test]
    fn a_restart_applies_the_recorded_choice() {
        let mut run = resolved("fix-codex", &[]);
        let mut recorded = BTreeMap::new();
        recorded.insert("writer".to_string(), "codex-chatgpt".to_string());
        recorded.insert("missing".to_string(), "codex-openai".to_string());
        apply_recorded(&mut run, &recorded);
        assert_eq!(run.agent_credentials["writer"], "codex-chatgpt");
        assert!(!run.agent_credentials.contains_key("missing"));
    }
}
