//! Resolved loadout → `SessionTeamEdit` (slots, dependencies, required
//! checks with options, required review, grants). Owner: core.
//!
//! Every Agent of a loadout becomes one Team slot whose definition the edit
//! carries inline (`template_id: None`): the loadout, not the configured
//! Agent templates, defines its tools, model and write scope. The edit goes
//! through the ordinary preview → apply path, so every grant, write scope and
//! readiness rule of a person's own Team and budget edit applies unchanged.

use axocoatl_config::loadout::{
    AgentRuntime, LoadoutAgent, LoadoutLimits, LoadoutRole, ModelSpec, ResolvedLoadout,
};
use axocoatl_session::check_options::RequiredCheckOptions;
use axocoatl_session::control_authority::GrantLimits;
use axocoatl_session::run_outcome::ModelIdentity;

use super::RunError;
use crate::{
    InlineAgentDefinition, InlineReviewer, ReviewSetting, SessionTeamConnection, SessionTeamEdit,
    SessionTeamSlotEdit,
};

/// The template id an inline loadout reviewer is shown under.
pub const LOADOUT_REVIEWER_TEMPLATE: &str = "loadout-reviewer";
/// The output bound per request of a loadout Agent or reviewer that names
/// none: a native Agent always runs with an explicit sampling maximum.
pub use axocoatl_config::loadout::DEFAULT_MAX_OUTPUT_TOKENS;

/// One slot to create: a loadout Agent, possibly instantiated per area.
#[derive(Debug, Clone, PartialEq)]
pub struct SlotPlan {
    /// Team slot id, such as `writer` or `worker-auth`.
    pub slot_id: String,
    pub agent: LoadoutAgent,
    pub model: ModelSpec,
    /// Replaces the Agent's instructions (audit area workers).
    pub instructions: Option<String>,
    /// Slot ids this slot depends on.
    pub depends_on: Vec<String>,
    pub required: bool,
}

/// The slots of the loadout's own Agents, in declaration order.
pub fn default_slots(resolved: &ResolvedLoadout) -> Result<Vec<SlotPlan>, RunError> {
    resolved
        .loadout
        .file
        .agents
        .iter()
        .map(|agent| {
            let model = resolved
                .agent_models
                .get(&agent.id)
                .cloned()
                .ok_or_else(|| {
                    RunError::Usage(format!("Agent {} has no resolved model", agent.id))
                })?;
            Ok(SlotPlan {
                slot_id: agent.id.clone(),
                agent: agent.clone(),
                model,
                instructions: None,
                depends_on: agent.depends_on.clone(),
                required: true,
            })
        })
        .collect()
}

/// Each native model caller of the run (an Agent or the reviewer) with its
/// resolved model ([`axocoatl_config::loadout::call_budgets`]).
pub fn resolved_call_budgets(
    resolved: &ResolvedLoadout,
) -> Vec<(axocoatl_config::loadout::CallBudget, Option<ModelSpec>)> {
    axocoatl_config::loadout::call_budgets(&resolved.loadout.file)
        .into_iter()
        .map(|budget| {
            let model = match &budget.agent {
                Some(agent) => resolved.agent_models.get(agent).cloned(),
                None => resolved.reviewer_model.clone(),
            };
            (budget, model)
        })
        .collect()
}

/// Refuse, as a usage error, a run whose native Agent or reviewer cannot
/// make one model call within its tokens budget: a call needs the model's
/// context plus the caller's output bound. `context_of` is a model's
/// observed context; `None` (not observed, or not observable before the
/// run) leaves the smallest context a native call runs with, which
/// validation already checked.
pub fn refuse_budgets_below_one_call(
    resolved: &ResolvedLoadout,
    context_of: impl Fn(&ModelSpec) -> Option<u64>,
) -> Result<(), RunError> {
    for (budget, model) in resolved_call_budgets(resolved) {
        let context = model.as_ref().and_then(&context_of);
        let name = model.as_ref().map(ToString::to_string);
        if let Some(error) = budget.refusal(context, name.as_deref()) {
            return Err(RunError::Usage(error.to_string()));
        }
    }
    Ok(())
}

/// Grant limits from loadout limits: `cost_microunits = cost_usd × 10⁶`.
pub fn grant_limits(limits: &LoadoutLimits) -> GrantLimits {
    GrantLimits {
        activations: limits.activations,
        invocations: limits.invocations,
        tokens: limits.tokens,
        cost_microunits: limits.cost_microunits(),
    }
}

/// The wall clock of the loadout in milliseconds.
pub fn wall_clock_ms(resolved: &ResolvedLoadout) -> Result<u64, RunError> {
    axocoatl_config::loadout::parse_duration_secs(&resolved.loadout.file.budgets.wall_clock)
        .map(|secs| secs.saturating_mul(1000))
        .ok_or_else(|| RunError::Usage("budgets.wall_clock is not a duration".into()))
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|time| time.as_millis() as u64)
        .unwrap_or(0)
}

fn source(resolved: &ResolvedLoadout, what: &str) -> String {
    let file = &resolved.loadout.file;
    format!(
        "loadout {}@{} sha256:{} {what}",
        file.id, file.version, resolved.loadout.digest
    )
}

/// The identity of an Agent as the Outcome records it.
pub fn model_identity(model: &ModelSpec, runtime: AgentRuntime) -> ModelIdentity {
    ModelIdentity {
        provider: model.provider.clone(),
        model: model.model.clone(),
        runtime: match runtime {
            AgentRuntime::Native => "native",
            AgentRuntime::ClaudeCode => "claude-code",
            AgentRuntime::Codex => "codex",
        }
        .into(),
    }
}

/// The resolved models of the loadout's writers.
pub fn writer_identities(resolved: &ResolvedLoadout) -> Vec<ModelIdentity> {
    resolved
        .loadout
        .file
        .agents
        .iter()
        .filter(|agent| agent.role == LoadoutRole::Writer)
        .filter_map(|agent| {
            resolved
                .agent_models
                .get(&agent.id)
                .map(|model| model_identity(model, agent.runtime))
        })
        .collect()
}

/// Fill each `detected` check's argv: the Session's detected check command,
/// or the run's `--check`, which wins. A `detected` check with neither is a
/// usage error.
pub fn fill_detected_checks(
    resolved: &mut ResolvedLoadout,
    check_command: Option<&str>,
    session_check: Option<&str>,
) -> Result<(), RunError> {
    let detected: Vec<String> = resolved
        .loadout
        .file
        .checks
        .iter()
        .filter(|check| check.run.detected)
        .map(|check| check.name.clone())
        .collect();
    for check in resolved
        .checks
        .iter_mut()
        .filter(|check| detected.contains(&check.name))
    {
        let command = check_command
            .or(session_check)
            .map(str::trim)
            .filter(|command| !command.is_empty())
            .ok_or_else(|| {
                RunError::Usage(format!(
                    "the check {} runs the repository's own check command, and none was \
                     detected: pass --check \"<command>\"",
                    check.name
                ))
            })?;
        check.argv = Some(vec!["sh".into(), "-c".into(), command.to_string()]);
    }
    Ok(())
}

/// The required checks as argv and their options, aligned by index.
pub fn required_checks(
    resolved: &ResolvedLoadout,
) -> Result<(Vec<Vec<String>>, Vec<RequiredCheckOptions>), RunError> {
    let mut argvs = Vec::with_capacity(resolved.checks.len());
    let mut options = Vec::with_capacity(resolved.checks.len());
    for check in &resolved.checks {
        let timeout_ms = check.timeout_secs.saturating_mul(1000);
        match (&check.argv, &check.e2e) {
            (_, Some(e2e)) => {
                let (argv, option) =
                    super::e2e::expand_e2e_check(&check.name, e2e, check.timeout_secs, resolved)?;
                argvs.push(argv);
                options.push(option);
            }
            (Some(argv), None) => {
                argvs.push(argv.clone());
                options.push(RequiredCheckOptions {
                    name: Some(check.name.clone()),
                    timeout_ms: Some(timeout_ms),
                    report: None,
                    egress: false,
                });
            }
            (None, None) => {
                return Err(RunError::Usage(format!(
                    "the check {} has no command: pass --check \"<command>\"",
                    check.name
                )))
            }
        }
    }
    Ok((argvs, options))
}

/// The edit that applies `slots` with the loadout's checks, review and
/// budgets. `with_checks_and_review` is false for an audit's plan turn.
pub fn team_edit(
    resolved: &ResolvedLoadout,
    slots: &[SlotPlan],
    with_checks_and_review: bool,
    expected_configuration_revision: u64,
) -> Result<SessionTeamEdit, RunError> {
    if slots.is_empty() {
        return Err(RunError::Usage("a team needs at least one Agent".into()));
    }
    let file = &resolved.loadout.file;
    let expires_at_ms = now_ms().saturating_add(wall_clock_ms(resolved)?);
    let mut edits = Vec::with_capacity(slots.len());
    for plan in slots {
        let agent = &plan.agent;
        let limits = agent.budget.as_ref().unwrap_or(&file.budgets.agent);
        edits.push(SessionTeamSlotEdit {
            slot_id: plan.slot_id.clone(),
            template_id: None,
            source_slot_id: None,
            // A top-level slot is never a Worker: Workers run only as
            // helpers or as the required reviewer.
            role: axocoatl_core::AgentRole::Autonomous,
            delegation: None,
            name: plan.slot_id.clone(),
            provider: plan.model.provider.clone(),
            model: plan.model.model.clone(),
            instructions: plan
                .instructions
                .clone()
                .or_else(|| agent.instructions.clone()),
            max_output_tokens: Some(agent.max_output_tokens.unwrap_or(DEFAULT_MAX_OUTPUT_TOKENS)),
            // `writes` absent in the loadout is every path (writers only);
            // the edit always says so explicitly.
            writes: Some(agent.writes.clone()),
            required: plan.required,
            reset_history: true,
            limits: Some(grant_limits(limits)),
            expires_at_ms: Some(expires_at_ms),
            definition: Some(InlineAgentDefinition {
                source: source(resolved, &format!("agent {}", agent.id)),
                tools: agent.tools.clone(),
                runtime: agent.runtime,
                reasoning_effort: agent.reasoning_effort,
            }),
        });
    }
    let ids: Vec<&str> = slots.iter().map(|plan| plan.slot_id.as_str()).collect();
    let mut dependencies = Vec::new();
    for plan in slots {
        for parent in &plan.depends_on {
            if ids.contains(&parent.as_str()) {
                dependencies.push(SessionTeamConnection {
                    parent: parent.clone(),
                    child: plan.slot_id.clone(),
                });
            }
        }
    }
    let (required_checks, check_options, required_review) = if with_checks_and_review {
        let (checks, options) = required_checks(resolved)?;
        let review = match (&file.review, &resolved.reviewer_model) {
            (Some(review), Some(model)) => {
                let limits = file.budgets.reviewer.as_ref().ok_or_else(|| {
                    RunError::Usage("a loadout with a review sets budgets.reviewer".into())
                })?;
                Some(ReviewSetting {
                    template_id: LOADOUT_REVIEWER_TEMPLATE.into(),
                    max_rounds: review.rounds,
                    limits: grant_limits(limits),
                    max_output_tokens: Some(
                        review
                            .max_output_tokens
                            .unwrap_or(DEFAULT_MAX_OUTPUT_TOKENS),
                    ),
                    inline: Some(InlineReviewer {
                        name: "reviewer".into(),
                        provider: model.provider.clone(),
                        model: model.model.clone(),
                        instructions: review.instructions.clone(),
                        definition: InlineAgentDefinition {
                            source: source(resolved, "review"),
                            tools: review.tools.clone(),
                            runtime: AgentRuntime::Native,
                            reasoning_effort: None,
                        },
                    }),
                })
            }
            (Some(_), None) => {
                return Err(RunError::Usage(
                    "the reviewer's model is not resolved".into(),
                ))
            }
            (None, _) => None,
        };
        (checks, options, review)
    } else {
        (Vec::new(), Vec::new(), None)
    };
    Ok(SessionTeamEdit {
        command_id: format!("loadout-{}", uuid::Uuid::new_v4()),
        expected_configuration_revision,
        slots: edits,
        dependencies,
        layout: Vec::new(),
        required_checks,
        required_review,
        check_options,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use axocoatl_config::loadout::{builtin_loadouts, resolve_loadout, ParamValues};

    fn resolved_fix() -> ResolvedLoadout {
        let fix = builtin_loadouts()
            .into_iter()
            .map(Result::unwrap)
            .find(|loadout| loadout.file.id == "fix")
            .unwrap();
        let mut params = ParamValues::new();
        params.insert("writer_model".into(), "openrouter:qwen/qwen3-coder".into());
        params.insert(
            "reviewer_model".into(),
            "openrouter:openai/gpt-oss-120b".into(),
        );
        resolve_loadout(&fix, &params, "fix the parser", "/repo").unwrap()
    }

    /// The qa re-smoke's `qa-tight` run: 40000 tokens for an explorer whose
    /// model was loaded with a 32768-token context and an 8192-token output
    /// bound. Admission refuses it, naming the budget and the minimum.
    #[test]
    fn a_budget_below_one_call_of_the_observed_context_is_refused() {
        let qa = builtin_loadouts()
            .into_iter()
            .map(Result::unwrap)
            .find(|loadout| loadout.file.id == "qa")
            .unwrap();
        let mut params = ParamValues::new();
        params.insert(
            "explorer_model".into(),
            "ollama:qwen3-coder:axocoatl-launch".into(),
        );
        let mut resolved = resolve_loadout(&qa, &params, "find bugs", "/repo").unwrap();
        resolved.loadout.file.budgets.agent.tokens = 40_000;
        let budgets = resolved_call_budgets(&resolved);
        assert_eq!(budgets.len(), 1);
        assert_eq!(
            budgets[0].1,
            ModelSpec::parse("ollama:qwen3-coder:axocoatl-launch")
        );
        let observed = |model: &ModelSpec| (model.provider == "ollama").then_some(32_768);
        let Err(RunError::Usage(message)) = refuse_budgets_below_one_call(&resolved, observed)
        else {
            panic!("admitted")
        };
        assert_eq!(
            message,
            "loadout field budgets.agent.tokens: 40000 tokens is less than one model call of \
             Agent explorer (ollama:qwen3-coder:axocoatl-launch) needs: the model's 32768-token \
             context plus 8192 output tokens (agents.explorer.max_output_tokens), at least \
             40960 tokens; raise it to at least 40960 or lower \
             agents.explorer.max_output_tokens"
        );
        // Enough for one call, or a context that is not known before the
        // run: admitted.
        assert!(refuse_budgets_below_one_call(&resolved, |_| Some(30_000)).is_ok());
        assert!(refuse_budgets_below_one_call(&resolved, |_| None).is_ok());
        resolved.loadout.file.budgets.agent.tokens = 40_960;
        assert!(refuse_budgets_below_one_call(&resolved, observed).is_ok());
        // The reviewer is checked against its own model and budget.
        let mut fix = resolved_fix();
        fix.loadout.file.budgets.reviewer.as_mut().unwrap().tokens = 20_000;
        let Err(RunError::Usage(message)) =
            refuse_budgets_below_one_call(&fix, |model: &ModelSpec| {
                (model.model == "openai/gpt-oss-120b").then_some(16_384)
            })
        else {
            panic!("admitted")
        };
        assert!(
            message.starts_with(
                "loadout field budgets.reviewer.tokens: 20000 tokens is less than one model \
                 call of the reviewer (openrouter:openai/gpt-oss-120b)"
            ),
            "{message}"
        );
    }

    #[test]
    fn a_resolved_fix_maps_to_one_writer_with_checks_and_review() {
        let mut resolved = resolved_fix();
        assert!(matches!(
            required_checks(&resolved),
            Err(RunError::Usage(_))
        ));
        fill_detected_checks(&mut resolved, None, Some("cargo test")).unwrap();
        let slots = default_slots(&resolved).unwrap();
        assert_eq!(slots.len(), 1);
        let edit = team_edit(&resolved, &slots, true, 0).unwrap();
        assert!(edit.command_id.starts_with("loadout-"));
        assert_eq!(edit.expected_configuration_revision, 0);
        let [writer] = edit.slots.as_slice() else {
            panic!("one slot")
        };
        assert_eq!(writer.slot_id, "writer");
        assert_eq!(writer.template_id, None);
        assert_eq!(writer.role, axocoatl_core::AgentRole::Autonomous);
        assert_eq!(
            (writer.provider.as_str(), writer.model.as_str()),
            ("openrouter", "qwen/qwen3-coder")
        );
        assert_eq!(
            writer.writes,
            Some(None),
            "the writer may change every path"
        );
        assert!(writer.required && writer.reset_history);
        assert_eq!(
            writer.limits,
            Some(GrantLimits {
                activations: 4,
                invocations: 400,
                tokens: 4_000_000,
                cost_microunits: 10_000_000,
            })
        );
        let expires = writer.expires_at_ms.unwrap();
        let hour = now_ms() + 3_600_000;
        assert!(expires <= hour && expires + 60_000 > hour, "{expires}");
        let definition = writer.definition.as_ref().unwrap();
        assert_eq!(
            definition.tools,
            vec![
                "read_file",
                "list_dir",
                "grep",
                "glob",
                "write_file",
                "edit_file",
                "bash"
            ]
        );
        assert_eq!(definition.runtime, AgentRuntime::Native);
        assert!(definition.source.starts_with("loadout fix@1 sha256:"));
        assert!(writer
            .instructions
            .as_deref()
            .unwrap()
            .contains("ADJUDICATIONS"));
        assert_eq!(
            edit.required_checks,
            vec![vec!["sh".to_string(), "-c".into(), "cargo test".into()]]
        );
        assert_eq!(
            edit.check_options,
            vec![RequiredCheckOptions {
                name: Some("tests".into()),
                timeout_ms: Some(180_000),
                report: None,
                egress: false,
            }]
        );
        let review = edit.required_review.as_ref().unwrap();
        assert_eq!(review.template_id, LOADOUT_REVIEWER_TEMPLATE);
        assert_eq!(review.max_rounds, 3);
        assert_eq!(
            review.limits,
            GrantLimits {
                activations: 3,
                invocations: 120,
                tokens: 1_500_000,
                cost_microunits: 3_000_000,
            }
        );
        let inline = review.inline.as_ref().unwrap();
        assert_eq!(
            (inline.provider.as_str(), inline.model.as_str()),
            ("openrouter", "openai/gpt-oss-120b")
        );
        assert_eq!(
            inline.definition.tools,
            vec!["read_file", "list_dir", "grep", "glob"]
        );
        assert!(edit.dependencies.is_empty());
        // The edit round-trips through the API's JSON.
        let text = serde_json::to_string(&edit).unwrap();
        let back: SessionTeamEdit = serde_json::from_str(&text).unwrap();
        assert_eq!(back, edit);
        // --check wins over the detected command.
        let mut resolved = resolved_fix();
        fill_detected_checks(&mut resolved, Some("npm test"), Some("cargo test")).unwrap();
        assert_eq!(
            resolved.checks[0].argv,
            Some(vec!["sh".into(), "-c".into(), "npm test".into()])
        );
    }

    #[test]
    fn a_plan_turn_has_no_checks_or_review_and_dependencies_stay_within_the_slots() {
        let audit = builtin_loadouts()
            .into_iter()
            .map(Result::unwrap)
            .find(|loadout| loadout.file.id == "audit")
            .unwrap();
        let mut params = ParamValues::new();
        for name in ["planner_model", "worker_model", "integrator_model"] {
            params.insert(name.into(), "ollama:qwen3:32b".into());
        }
        let resolved = resolve_loadout(&audit, &params, "audit", "/repo").unwrap();
        let mut slots = default_slots(&resolved).unwrap();
        assert_eq!(slots.len(), 3);
        slots[2].depends_on = vec!["planner".into(), "elsewhere".into()];
        let edit = team_edit(&resolved, &slots, false, 7).unwrap();
        assert!(edit.required_checks.is_empty() && edit.check_options.is_empty());
        assert!(edit.required_review.is_none());
        assert_eq!(edit.expected_configuration_revision, 7);
        assert_eq!(
            edit.dependencies,
            vec![SessionTeamConnection {
                parent: "planner".into(),
                child: "integrator".into(),
            }]
        );
        assert!(edit
            .slots
            .iter()
            .all(|slot| slot.writes == Some(Some(Vec::new()))));
        assert_eq!(
            edit.slots[1].provider, "ollama",
            "a model splits at its first colon"
        );
        assert_eq!(edit.slots[1].model, "qwen3:32b");
    }

    #[test]
    fn writer_identities_carry_the_runtime() {
        let mut resolved = resolved_fix();
        resolved.loadout.file.agents[0].runtime = AgentRuntime::ClaudeCode;
        let writers = writer_identities(&resolved);
        assert_eq!(writers.len(), 1);
        assert_eq!(writers[0].runtime, "claude-code");
    }
}
