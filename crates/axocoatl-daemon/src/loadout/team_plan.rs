//! Resolved loadout → `SessionTeamEdit` (slots, dependencies, required
//! checks with options, required review, grants). Owner: core.
//!
//! Every Agent of a loadout becomes one Team slot whose definition the edit
//! carries inline (`template_id: None`): the loadout, not the configured
//! Agent templates, defines its tools, model and write scope. The edit goes
//! through the ordinary preview → apply path, so every grant, write scope and
//! readiness rule of a person's own Team and budget edit applies unchanged.

use axocoatl_config::loadout::{
    AgentRuntime, CallBudget, CallFloor, LoadoutAgent, LoadoutLimits, LoadoutRole, ModelSpec,
    ResolvedLoadout,
};
use axocoatl_session::check_options::RequiredCheckOptions;
use axocoatl_session::control_authority::GrantLimits;
use axocoatl_session::run_outcome::ModelIdentity;
use std::collections::HashMap;

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

/// Each model caller of the run (a native Agent or the reviewer,
/// [`axocoatl_config::loadout::call_budgets`], and an external writer,
/// [`axocoatl_config::loadout::external_call_budgets`]) with its resolved
/// model.
pub fn resolved_call_budgets(resolved: &ResolvedLoadout) -> Vec<(CallBudget, Option<ModelSpec>)> {
    let file = &resolved.loadout.file;
    axocoatl_config::loadout::call_budgets(file)
        .into_iter()
        .chain(axocoatl_config::loadout::external_call_budgets(file))
        .map(|budget| {
            let model = match &budget.agent {
                Some(agent) => resolved.agent_models.get(agent).cloned(),
                None => resolved.reviewer_model.clone(),
            };
            (budget, model)
        })
        .collect()
}

/// What admission observed of the run's models before anything is created.
#[derive(Debug, Clone, Default)]
pub struct AdmissionObservations {
    /// Each native Ollama model's loaded context; `None` when it could not
    /// be observed.
    pub ollama_contexts: HashMap<ModelSpec, Option<u64>>,
    /// OpenRouter's public model catalog (`GET /models`), when a native
    /// OpenRouter model needed it and it could be read.
    pub openrouter_catalog: Option<serde_json::Value>,
}

/// What one model call of `budget`'s caller on `model` needs at least, as
/// admission can know it before the run:
///
/// - a native Ollama model: its loaded context plus the output bound (a
///   call reserves the whole context);
/// - a native OpenRouter model: the smallest call Team & budget will
///   check, from OpenRouter's catalog: the 4,096-token prompt allowance
///   within the model's `context_length` plus the output bound and the
///   reasoning allowance of its effort (a call reserves its own request,
///   not the context window);
/// - an external program's model in the pinned table: the most one call of
///   the program can use ([`crate::external_agent::models`]).
///
/// `None` when it cannot be known: the model was not observed, is not in
/// the catalog, or is not pinned.
pub fn call_floor(
    budget: &CallBudget,
    model: &ModelSpec,
    observed: &AdmissionObservations,
) -> Option<CallFloor> {
    if budget.runtime != AgentRuntime::Native {
        let pinned = crate::external_agent::models::pinned_model(budget.runtime, &model.model)?;
        return Some(CallFloor {
            tokens: pinned.call_tokens(),
            words: pinned.call_words(),
            lower: None,
        });
    }
    match model.provider.as_str() {
        "ollama" => observed
            .ollama_contexts
            .get(model)
            .copied()
            .flatten()
            .map(|context| budget.context_floor(Some(context))),
        "openrouter" => {
            let output = usize::try_from(budget.max_output_tokens).ok()?;
            let floor = axocoatl_llm_openai::catalog_call_floor(
                observed.openrouter_catalog.as_ref()?,
                &model.model,
                output,
                budget.reasoning_effort,
            )?;
            let reasoning = match floor.reasoning {
                Some(_) if floor.reasoning_tokens == 0 => String::new(),
                Some(axocoatl_llm_openai::NativeOpenRouterReasoningRequest::Effort(effort)) => {
                    format!(
                        " and {} reasoning tokens (reasoning effort {effort})",
                        floor.reasoning_tokens
                    )
                }
                Some(_) => format!(" and {} reasoning tokens", floor.reasoning_tokens),
                None => String::new(),
            };
            let lower = match (&budget.agent, floor.reasoning_tokens) {
                (Some(agent), tokens) if tokens > 0 => format!(
                    "{} or set agents.{agent}.reasoning_effort lower",
                    budget.output_field
                ),
                _ => budget.output_field.clone(),
            };
            Some(CallFloor {
                tokens: floor.tokens(output) as u64,
                words: format!(
                    "the {}-token prompt allowance a native OpenRouter call reserves at least \
                     (each call reserves its own request, within the model's {}-token context \
                     in OpenRouter's catalog) plus {} output tokens ({}){reasoning}",
                    floor.prompt_tokens,
                    floor.context_tokens,
                    budget.max_output_tokens,
                    budget.output_field
                ),
                lower: Some(lower),
            })
        }
        _ => None,
    }
}

/// Refuse, as a usage error, a run whose Agent or reviewer cannot make one
/// model call within its tokens budget ([`call_floor`]). A native caller
/// whose floor cannot be known keeps the smallest context a native call
/// runs with, which validation already checked; an external program whose
/// model is not pinned is not checked (admission warns about it,
/// [`crate::external_agent::models::unpinned_warning`]).
pub fn refuse_budgets_below_one_call(
    resolved: &ResolvedLoadout,
    observed: &AdmissionObservations,
) -> Result<(), RunError> {
    for (budget, model) in resolved_call_budgets(resolved) {
        let name = model.as_ref().map(ToString::to_string);
        let floor = model
            .as_ref()
            .and_then(|model| call_floor(&budget, model, observed));
        let refusal = match (floor, budget.runtime) {
            (Some(floor), _) => budget.refusal_with(&floor, name.as_deref()),
            (None, AgentRuntime::Native) => budget.refusal(None, name.as_deref()),
            (None, _) => None,
        };
        if let Some(error) = refusal {
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

/// The identity of an Agent as the Outcome records it: its configured
/// provider and model, and where it runs. An external writer's configured
/// provider is the one whose API its program calls (loadout validation
/// refuses any other, [`AgentRuntime::model_refusal`]), so the identity is
/// what ran, and the same-model check compares it with the reviewer's
/// across providers ([`axocoatl_core::same_model`]).
pub fn model_identity(model: &ModelSpec, runtime: AgentRuntime) -> ModelIdentity {
    ModelIdentity {
        provider: model.provider.clone(),
        model: model.model.clone(),
        runtime: runtime.id().into(),
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
        let ollama = |context: Option<u64>| AdmissionObservations {
            ollama_contexts: HashMap::from([(
                ModelSpec::parse("ollama:qwen3-coder:axocoatl-launch").unwrap(),
                context,
            )]),
            openrouter_catalog: None,
        };
        let observed = ollama(Some(32_768));
        let Err(RunError::Usage(message)) = refuse_budgets_below_one_call(&resolved, &observed)
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
        assert!(refuse_budgets_below_one_call(&resolved, &ollama(Some(30_000))).is_ok());
        assert!(refuse_budgets_below_one_call(&resolved, &ollama(None)).is_ok());
        assert!(
            refuse_budgets_below_one_call(&resolved, &AdmissionObservations::default()).is_ok()
        );
        resolved.loadout.file.budgets.agent.tokens = 40_960;
        assert!(refuse_budgets_below_one_call(&resolved, &observed).is_ok());
    }

    /// OpenRouter's catalog row of the fix re-smoke reviewer's model, as
    /// the public catalog shapes it: a 131,072-token context and reasoning
    /// on by default at medium effort.
    fn openrouter_catalog() -> serde_json::Value {
        serde_json::json!({"data": [
            {"id": "openai/gpt-oss-120b", "context_length": 131072,
             "reasoning": {"mandatory": false, "default_enabled": true,
                 "default_effort": "medium", "supported_efforts": ["high", "medium", "low"]}},
            {"id": "qwen/qwen3-coder", "context_length": 262144}
        ]})
    }

    /// A native OpenRouter caller's floor is the smallest call Team & budget
    /// checks, read from OpenRouter's catalog before anything is created: the
    /// prompt allowance within the catalog's context, the output bound and
    /// the reasoning allowance of the model's effort. Never the whole context
    /// window: a call reserves its own request.
    #[test]
    fn an_openrouter_floor_is_read_from_the_catalog() {
        let mut fix = resolved_fix();
        let observed = AdmissionObservations {
            ollama_contexts: HashMap::new(),
            openrouter_catalog: Some(openrouter_catalog()),
        };
        // The fix loadout's budgets hold one call of either model.
        assert!(refuse_budgets_below_one_call(&fix, &observed).is_ok());
        // The reviewer reasons at medium by default: 4096 + 8192 + 8192.
        fix.loadout.file.budgets.reviewer.as_mut().unwrap().tokens = 20_000;
        let Err(RunError::Usage(message)) = refuse_budgets_below_one_call(&fix, &observed) else {
            panic!("admitted")
        };
        assert_eq!(
            message,
            "loadout field budgets.reviewer.tokens: 20000 tokens is less than one model call of \
             the reviewer (openrouter:openai/gpt-oss-120b) needs: the 4096-token prompt \
             allowance a native OpenRouter call reserves at least (each call reserves its own \
             request, within the model's 131072-token context in OpenRouter's catalog) plus \
             8192 output tokens (review.max_output_tokens) and 8192 reasoning tokens \
             (reasoning effort medium), at least 20480 tokens; raise it to at least 20480 or \
             lower review.max_output_tokens"
        );
        // Without the catalog, the smallest native context stands, as before.
        assert!(refuse_budgets_below_one_call(&fix, &AdmissionObservations::default()).is_ok());
        // A writer's effort is part of its floor, and lowers it.
        let mut fix = resolved_fix();
        fix.loadout.file.budgets.agent.tokens = 12_000;
        let writer = fix.loadout.file.agents[0].id.clone();
        let Err(RunError::Usage(message)) = refuse_budgets_below_one_call(&fix, &observed) else {
            panic!("admitted")
        };
        assert!(
            message.contains(
                "plus 8192 output tokens (agents.writer.max_output_tokens), at \
                              least 12288 tokens"
            ),
            "{message}"
        );
        fix.loadout.file.budgets.agent.tokens = 13_000;
        assert!(refuse_budgets_below_one_call(&fix, &observed).is_ok());
        assert_eq!(writer, "writer");
        // A model the catalog does not list cannot say: its observation
        // refuses it by name when the Team is applied.
        let budgets = resolved_call_budgets(&fix);
        let unlisted = ModelSpec::parse("openrouter:vendor/unlisted").unwrap();
        assert_eq!(call_floor(&budgets[0].0, &unlisted, &observed), None);
    }

    /// An external writer's floor is the most one call of its pinned model
    /// can use; a model the pinned table does not list is not checked.
    #[test]
    fn an_external_writers_floor_is_its_pinned_models_call() {
        let fix = builtin_loadouts()
            .into_iter()
            .map(Result::unwrap)
            .find(|loadout| loadout.file.id == "fix")
            .unwrap();
        let mut codex = fix.clone();
        codex.file.agents[0].runtime = AgentRuntime::Codex;
        let mut params = ParamValues::new();
        params.insert("writer_model".into(), "openai:gpt-5.5".into());
        params.insert("reviewer_model".into(), "ollama:gpt-oss:120b".into());
        let mut resolved = resolve_loadout(&codex, &params, "fix it", "/repo").unwrap();
        let observed = AdmissionObservations::default();
        assert!(refuse_budgets_below_one_call(&resolved, &observed).is_ok());
        resolved.loadout.file.budgets.agent.tokens = 399_999;
        let Err(RunError::Usage(message)) = refuse_budgets_below_one_call(&resolved, &observed)
        else {
            panic!("admitted")
        };
        assert_eq!(
            message,
            "loadout field budgets.agent.tokens: 399999 tokens is less than one model call of \
             Agent writer (openai:gpt-5.5) needs: the 272000-token context Codex 0.160.1 keeps \
             for gpt-5.5 plus the model's 128000 output tokens, at least 400000 tokens; raise \
             it to at least 400000"
        );
        resolved.loadout.file.budgets.agent.tokens = 400_000;
        assert!(refuse_budgets_below_one_call(&resolved, &observed).is_ok());
        // Claude Code on a 1M-context model.
        let mut claude = fix.clone();
        claude.file.agents[0].runtime = AgentRuntime::ClaudeCode;
        params.insert("writer_model".into(), "anthropic:claude-sonnet-5-5".into());
        let mut resolved = resolve_loadout(&claude, &params, "fix it", "/repo").unwrap();
        resolved.loadout.file.budgets.agent.tokens = 900_000;
        let Err(RunError::Usage(message)) = refuse_budgets_below_one_call(&resolved, &observed)
        else {
            panic!("admitted")
        };
        assert!(
            message.contains(
                "needs: claude-sonnet-5-5's whole 1000000-token context window, input and \
                 output, which one call of Claude Code 2.1.292 can fill, at least 1000000 tokens"
            ),
            "{message}"
        );
        // Not pinned: not checked.
        params.insert("writer_model".into(), "anthropic:sonnet".into());
        let mut resolved = resolve_loadout(&claude, &params, "fix it", "/repo").unwrap();
        resolved.loadout.file.budgets.agent.tokens = 1;
        assert!(refuse_budgets_below_one_call(&resolved, &observed).is_ok());
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
