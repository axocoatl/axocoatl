//! Host `delegate` port. A lead Agent hands one self-contained task to a fresh,
//! read-only helper and waits for its answer. Admission is the ordinary native
//! child path: the helper is a dynamic graph node with its own grant reserved
//! from the lead's, and this port only waits for its canonical outcome.
use super::coordinator::ChildAttempt;
use super::*;
use axocoatl_actor::{AdmittedChildExecution, AgentRunOutcome, ChildExecutionRequest};
use axocoatl_core::MeasuredTokenUsage;
use axocoatl_session::control_authority::AuthorityError;
use axocoatl_session::control_command::{
    CommandFailure, CommandSourceRecord, ControlCommandState, ControlParameters, ControlTransition,
};
use axocoatl_session::invocation_audit::ProtectedArguments;
use axocoatl_tools::{BuiltinTool, ToolError};
use serde::{Deserialize, Serialize};

pub(super) const NAME: &str = axocoatl_session::control_authority::DELEGATE_TOOL;
const ADAPTER: &str = "delegate-child-v1";
const MAX_TASK_BYTES: usize = 16 * 1024;
const MAX_ANSWER_BYTES: usize = 8192;
/// What one helper answer can add to the lead's next prompt bound: the answer
/// and its JSON fields, escaped once in the tool result and again in the
/// request body. An estimate that covers ordinary text, not a bound.
pub(super) const ANSWER_PROMPT_TOKENS: u64 = 2 * (MAX_ANSWER_BYTES as u64 + 1024);
/// Provider calls a lead must still be able to make after a helper's limits
/// are reserved: one reads the answer, and one more lets it answer after a
/// declined tool round.
const FOLLOW_UP_CALLS: u32 = 2;
/// Tools that change the workspace or run commands. A helper takes delegated
/// work only when it is read-only: none of these, or a write scope that allows
/// no path (`writes: []`), which withholds the file-writing tools and runs its
/// `bash` where it cannot change the repository. `browser_check` runs test
/// code the model writes against the apps on the exposed ports, which can do
/// whatever those apps allow, including writing files; `browser`, which runs
/// no model-written code, is not here.
const WRITE_TOOLS: [&str; 6] = [
    "write_file",
    "edit_file",
    "bash",
    "bash_background",
    "spawn_terminal",
    "browser_check",
];
/// The write tools an empty write scope withholds (`write_file`, `edit_file`)
/// or confines (`bash`).
const READ_ONLY_CONFINED: [&str; 3] = ["write_file", "edit_file", "bash"];

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct DelegateCall {
    helper: String,
    task: String,
}

/// Retained as the lead invocation's replay policy, so a lost return is read
/// back from the helper's canonical outcome instead of running it again.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct DelegateReplayPolicy {
    schema_version: u32,
    adapter: String,
    invocation_id: InvocationId,
    activation: ActivationRef,
    arguments: ProtectedArguments,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    node_id: Option<TurnNodeId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    command_id: Option<CommandId>,
}

struct DelegateTool {
    controller: SessionDispatchController,
    activation: ActivationRef,
    description: String,
    helpers: Vec<String>,
}

/// Whether a helper's profile allows no repository path to change.
fn read_only(profile: &ExecutionProfile) -> bool {
    profile.write_scope.as_ref().is_some_and(Vec::is_empty)
}

/// The tools with which a helper could still change the workspace or run
/// commands that can: every write tool, less those an empty write scope
/// withholds or confines. A helper with any cannot take delegated work.
fn write_tools(profile: &ExecutionProfile) -> Vec<&str> {
    changing_tools(&profile.tools, profile.write_scope.as_deref())
}

/// Of `tools`, those with which an Agent whose write scope is `writes` could
/// change the workspace or run commands that can. Only an Agent with none is
/// read-only: a `delegate` helper or a required reviewer.
pub(crate) fn changing_tools<'a>(tools: &'a [String], writes: Option<&[String]>) -> Vec<&'a str> {
    let read_only = writes.is_some_and(<[String]>::is_empty);
    tools
        .iter()
        .map(String::as_str)
        .filter(|tool| {
            WRITE_TOOLS.contains(tool) && !(read_only && READ_ONLY_CONFINED.contains(tool))
        })
        .collect()
}

/// The tools a helper is offered, as the lead is told: an empty write scope
/// withholds the file-writing tools.
fn offered_tools(profile: &ExecutionProfile) -> Vec<&str> {
    profile
        .tools
        .iter()
        .map(String::as_str)
        .filter(|tool| !(read_only(profile) && ["write_file", "edit_file"].contains(tool)))
        .collect()
}

/// At most `max` bytes of `text`, cut on a character boundary.
fn cut_at_char_boundary(text: &str, max: usize) -> &str {
    if text.len() <= max {
        return text;
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

/// The text the live tool returns for a failed call, for a reconciled one.
fn failure_text(reason: String) -> String {
    ToolError::ExecutionFailed {
        tool: NAME.into(),
        reason,
    }
    .to_string()
}

fn completed_answer(
    helper: &str,
    node: &TurnNodeId,
    text: &str,
    usage: &MeasuredTokenUsage,
    reconciled: bool,
) -> serde_json::Value {
    let answer = cut_at_char_boundary(text, MAX_ANSWER_BYTES);
    let truncated = answer.len() < text.len();
    let mut value = serde_json::json!({
        "helper": helper,
        "node_id": node,
        "status": "completed",
        "result": answer,
        "truncated": truncated,
        "output_bytes": text.len(),
        "usage": {
            "input_tokens": usage.usage.input_tokens,
            "output_tokens": usage.usage.output_tokens,
            "complete": usage.complete,
        },
    });
    if truncated {
        value["note"] = format!(
            "The answer was cut at {MAX_ANSWER_BYTES} bytes; the full answer is kept in the \
             Session history. To get the rest, delegate a narrower task."
        )
        .into();
    }
    if reconciled {
        value["reconciled"] = true.into();
    }
    value
}

/// The model-facing text for a call whose helper was never admitted.
fn not_admitted(helper: &str) -> String {
    format!(
        "The call to helper '{helper}' was not admitted, so no helper ran. Call delegate again \
         if you still need it."
    )
}

/// Plain text for a refused helper admission. `limits` are the helper's,
/// when known.
fn refusal(helper: &str, limits: Option<&GrantLimits>, failure: &CommandFailure) -> String {
    // Reconstruction rejects a request that never reached acceptance.
    if failure.code == "request_not_accepted" {
        return not_admitted(helper);
    }
    let reason = failure
        .message
        .strip_prefix("Session dispatch: ")
        .unwrap_or(&failure.message);
    if reason == AuthorityError::Capacity.to_string() {
        let limits = limits
            .map(|limits| format!(" ({} steps, {} tokens)", limits.invocations, limits.tokens))
            .unwrap_or_default();
        return format!(
            "The helper '{helper}' was not started: its limits{limits} do not fit in what is left \
             of your budget. Finish the work yourself or write your final answer."
        );
    }
    if reason == "control exceeds the approved graph size" {
        return format!(
            "The helper '{helper}' was not started: this turn already has as many Agents as were \
             approved. Finish the work yourself or write your final answer."
        );
    }
    format!(
        "The helper '{helper}' was not started: {reason}. No helper ran; call delegate again if \
         you still need it, or continue without it."
    )
}

impl DispatchState {
    /// Every approved helper template of a holder grant, with its captured
    /// execution profile. Ad hoc selection is not offered to a lead.
    fn delegate_helpers(
        &self,
        policy: &AuthorityGrant,
    ) -> Result<Vec<(NativeCoordinatorWorker, ExecutionProfile)>> {
        let Some(approved) =
            crate::bootstrap::session_team::approved_coordinator_policy(&self.content, policy)
                .map_err(error)?
        else {
            return Ok(vec![]);
        };
        let mut helpers = vec![];
        for worker in approved.workers {
            if worker.template_id.starts_with("adhoc-") {
                continue;
            }
            let ActivationEvidenceContent::Definition { profile, .. } = &self
                .content
                .resolve_activation_evidence(&worker.definition.snapshot)
                .map_err(error)?
            else {
                return Err(error("an approved helper definition is unavailable"));
            };
            let profile = profile.clone();
            helpers.push((worker, profile));
        }
        Ok(helpers)
    }

    /// The exact child request for one call, or the reason the model gets
    /// when the call cannot become one.
    fn delegate_request(
        &self,
        lead: &ActivationRef,
        policy: &AuthorityGrant,
        call: &DelegateCall,
    ) -> Result<std::result::Result<(NativeCoordinatorWorker, ChildExecutionRequest), String>> {
        let helpers = self.delegate_helpers(policy)?;
        let Some((worker, profile)) = helpers
            .iter()
            .find(|(worker, _)| worker.template_id == call.helper)
        else {
            let names = helpers
                .iter()
                .filter(|(_, profile)| write_tools(profile).is_empty())
                .map(|(worker, _)| worker.template_id.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            return Ok(Err(format!(
                "There is no helper named '{}'. Choose one of: {names}.",
                call.helper
            )));
        };
        let writes = write_tools(profile);
        if !writes.is_empty() {
            return Ok(Err(format!(
                "The helper '{}' can change files or run commands ({}), and only read-only \
                 helpers can take delegated work. A person can make it read-only by setting \
                 `writes: []` on it (May change: Nothing in Team and budget). Choose a \
                 read-only helper or do this part yourself.",
                call.helper,
                writes.join(", ")
            )));
        }
        if call.task.trim().is_empty() {
            return Ok(Err(
                "The task is empty. Say exactly what the helper should do and what it should \
                 report back."
                    .into(),
            ));
        }
        if call.task.len() > MAX_TASK_BYTES {
            return Ok(Err(format!(
                "The task is {} bytes; keep it under {MAX_TASK_BYTES} bytes.",
                call.task.len()
            )));
        }
        let snapshot = self.current(lead)?;
        let conversation = &snapshot
            .contract()
            .activations()
            .iter()
            .find(|item| item.activation == *lead)
            .ok_or_else(|| error("the delegating activation is unavailable"))?
            .conversation_id;
        Ok(Ok((
            worker.clone(),
            ChildExecutionRequest {
                actor_id: conversation.as_str().into(),
                logical_worker_id: worker.template_id.clone(),
                subtask_index: 0,
                task_name: worker.template_id.clone(),
                task_input: call.task.clone(),
                tools: profile.tools.clone(),
                provider_id: profile.provider.clone(),
                model: profile.model.clone(),
                attachments: vec![],
            },
        )))
    }

    /// Why reserving a helper's `limits` would leave the lead unable to read
    /// its answer, or `None` when enough is left: the invocations of
    /// `FOLLOW_UP_CALLS` provider calls, the tokens and cost of one, and the
    /// invocations the host holds back to observe a lead that runs commands.
    /// A helper that does not fit at all is left to its admission command.
    /// Checked under the same lock as the reservation, so helpers admitted
    /// together from one model round each see the others' reserved limits.
    pub(super) fn delegate_follow_up_shortfall(
        &self,
        lead: &ActivationRef,
        policy: &AuthorityGrant,
        helper: &str,
        limits: &GrantLimits,
    ) -> Result<Option<String>> {
        let used = self.authority.usage(&policy.id).map_err(error)?;
        let total = &policy.limits;
        let (Some(invocations), Some(tokens), Some(cost)) = (
            total
                .invocations
                .checked_sub(used.invocations)
                .and_then(|left| left.checked_sub(limits.invocations)),
            total
                .tokens
                .checked_sub(used.tokens)
                .and_then(|left| left.checked_sub(limits.tokens)),
            total
                .cost_microunits
                .checked_sub(used.cost_microunits)
                .and_then(|left| left.checked_sub(limits.cost_microunits)),
        ) else {
            return Ok(None);
        };
        // The call that reads the answer reserves more than any earlier one:
        // its prompt adds this round's reasoning, the turn the lead returned
        // and the helper's answer. Use the estimate the lead's latest call
        // left, and never less than its largest reservation so far.
        let largest = self
            .authority
            .largest_provider_reservation(lead)
            .map_err(error)?
            .unwrap_or(DispatchReservation {
                tokens: 0,
                cost_microunits: 0,
            });
        let call = match self
            .follow_ups
            .get(&lead.activation_id)
            .filter(|(activation, _)| activation == lead)
        {
            Some((_, estimate)) => DispatchReservation {
                tokens: largest.tokens.max(estimate.tokens),
                cost_microunits: largest.cost_microunits.max(estimate.cost_microunits),
            },
            None => largest,
        };
        let reserve = self.host_observation_reserve(lead).unwrap_or(0);
        let needed = FOLLOW_UP_CALLS.saturating_add(reserve);
        if invocations >= needed && tokens >= call.tokens && cost >= call.cost_microunits {
            return Ok(None);
        }
        let held = if reserve > 0 {
            format!(
                ", including {reserve} held for the host to observe your changes and run \
                 required checks"
            )
        } else {
            String::new()
        };
        let cost = if cost < call.cost_microunits {
            " Its cost limit would also leave too little for your next model call."
        } else {
            ""
        };
        Ok(Some(format!(
            "The helper '{helper}' was not started: after reserving its limits ({} steps, \
             {} tokens) you would have {invocations} steps and {tokens} tokens left, not \
             enough to read its answer; that needs at least {needed} steps{held} and {} \
             tokens.{cost} Do this part yourself, or write your final answer now.",
            limits.invocations, limits.tokens, call.tokens
        )))
    }

    /// The helper node and admitting command a call names, when it names one.
    fn delegate_target(
        &self,
        lead: &ActivationRef,
        call: &DelegateCall,
    ) -> Result<Option<(TurnNodeId, CommandId)>> {
        let bound = self
            .bound
            .get(&lead.activation_id)
            .filter(|bound| bound.activation == *lead)
            .ok_or_else(|| error("the delegating Agent is no longer running"))?;
        let policy = self
            .authority
            .grant_policy(bound.grant.grant_id.as_str())
            .map_err(error)?;
        let Ok((worker, request)) = self.delegate_request(lead, &policy, call)? else {
            return Ok(None);
        };
        let digest = super::coordinator::native_child_digest(lead, &request, &worker, &None)?;
        let attempt = self.native_child_attempt(&digest)?;
        Ok(Some((attempt.node_id, attempt.command_id)))
    }

    pub(super) fn delegate_replay_policy(
        &mut self,
        activation: &ActivationRef,
        invocation: &InvocationId,
        request: &ToolInvocationRequest,
        arguments: &DurableToolArguments,
    ) -> Result<InvocationReplayPolicy> {
        let target =
            match serde_json::from_value::<DelegateCall>(request.tool_call.arguments.clone()) {
                Ok(call) => self.delegate_target(activation, &call)?,
                Err(_) => None,
            };
        let (node_id, command_id) = target.unzip();
        let policy = DelegateReplayPolicy {
            schema_version: 1,
            adapter: ADAPTER.into(),
            invocation_id: invocation.clone(),
            activation: activation.clone(),
            arguments: arguments.protected_arguments().clone(),
            node_id,
            command_id,
        };
        let policy_ref = self
            .content
            .retain_activation_evidence(ActivationEvidenceContent::Guidance {
                text: serde_json::to_string(&policy).map_err(error)?,
            })
            .map_err(error)?
            .reference()
            .clone();
        Ok(InvocationReplayPolicy::ReconcileBeforeReplay { policy_ref })
    }

    /// The accepted answer and measured usage of a helper node, when its
    /// latest activation was accepted with complete output evidence.
    fn accepted_helper_answer(
        &self,
        snapshot: &DurableTurnSnapshot,
        node: &TurnNodeId,
    ) -> Result<Option<(String, MeasuredTokenUsage)>> {
        let Some(item) = snapshot
            .contract()
            .activations()
            .iter()
            .rev()
            .find(|item| item.activation.node_id == *node)
        else {
            return Ok(None);
        };
        if item.state != ActivationState::Accepted {
            return Ok(None);
        }
        let reservation = self
            .content
            .activation_output_reservation(snapshot, &item.activation)
            .map_err(error)?
            .ok_or_else(|| error("accepted helper output reservation is missing"))?;
        let output = self
            .content
            .activation_output_settlement(&reservation)
            .map_err(error)?
            .ok_or_else(|| error("accepted helper output is missing"))?;
        if item.output.as_ref() != Some(output.reference()) || output.complete_output().is_none() {
            return Err(error("accepted helper has no complete output evidence"));
        }
        let usage = self
            .authority
            .provider_usage(&item.activation)
            .map(|usage| usage.tokens)
            .unwrap_or_default();
        Ok(Some((output.content().output.text.clone(), usage)))
    }

    /// The lead's `delegate` result read back from the command journal and the
    /// helper's canonical outcome, or `None` while it is still unknown.
    fn delegate_lookup(
        &self,
        snapshot: &DurableTurnSnapshot,
        intent: &InvocationIntent,
        policy: &DelegateReplayPolicy,
        helper: &str,
    ) -> Result<Option<std::result::Result<serde_json::Value, String>>> {
        let not_admitted = Err(failure_text(not_admitted(helper)));
        let (Some(node), Some(command)) = (&policy.node_id, &policy.command_id) else {
            return Ok(Some(not_admitted));
        };
        let Some(receipt) = self.commands.receipt(command).map_err(error)? else {
            return Ok(Some(not_admitted));
        };
        let view = receipt.view();
        let admits_node = matches!(&view.request.parameters,
            ControlParameters::AddAgent { input, .. } if input.activation.node_id == *node);
        // A retried lead reattaches to the command its earlier generation issued.
        let from_lead = matches!(&view.source,
            CommandSourceRecord::Agent { activation, .. }
                if activation.session_id == intent.activation.session_id
                    && activation.turn_id == intent.activation.turn_id
                    && activation.node_id == intent.activation.node_id);
        if !admits_node || !from_lead {
            return Ok(None);
        }
        Ok(match (&view.state, &view.last_transition) {
            (ControlCommandState::Rejected, Some(ControlTransition::Rejected { failure })) => {
                Some(Err(failure_text(refusal(helper, None, failure))))
            }
            // Reconstruction fails an accepted admission that has no canonical
            // node: no helper ran.
            (ControlCommandState::Failed, _) => Some(not_admitted),
            (ControlCommandState::Applied | ControlCommandState::Settled, _) => {
                Some(match self.accepted_helper_answer(snapshot, node)? {
                    Some((text, usage)) => Ok(completed_answer(helper, node, &text, &usage, true)),
                    None => Err(failure_text(format!(
                        "The helper '{helper}' did not complete (node {}), so there is no \
                         answer. Continue without it, or delegate a narrower task.",
                        node.as_str()
                    ))),
                })
            }
            _ => None,
        })
    }

    /// Whether a `delegate` call of this turn still has no recorded return.
    fn has_unresolved_delegate_return(&self) -> Result<bool> {
        Ok(self
            .audit
            .unresolved()
            .map_err(error)?
            .iter()
            .any(|audited| {
                audited.intent.tool_name == NAME
                    && audited.intent.activation.turn_id == self.turn_id
            }))
    }

    /// Run after control command reconciliation on reconstruction. A lost
    /// `delegate` return reads its admitting command's terminal state, which
    /// only command reconciliation settles after a crash; reconcile the
    /// invocations again so one reopen resolves it. Never called while an
    /// Agent runs: a live call legitimately has no return yet.
    pub(super) fn reconcile_delegate_returns(&mut self) -> Result<()> {
        if self.has_unresolved_delegate_return()? {
            self.reconcile()?;
        }
        Ok(())
    }

    /// Record the lead's lost `delegate` return from the command journal and
    /// the helper's canonical outcome. Nothing runs again. A command still
    /// requested or accepted stays unknown until command reconciliation ends
    /// it.
    pub(super) fn reconcile_delegate_outcome(
        &mut self,
        snapshot: &DurableTurnSnapshot,
        intent: &InvocationIntent,
        arguments: &DurableToolArguments,
    ) -> Result<bool> {
        if intent.tool_name != NAME || intent.activation.turn_id != self.turn_id {
            return Ok(false);
        }
        let InvocationReplayPolicy::ReconcileBeforeReplay { policy_ref } = &intent.replay_policy
        else {
            return Ok(false);
        };
        let ActivationEvidenceContent::Guidance { text } = &self
            .content
            .resolve_activation_evidence(policy_ref)
            .map_err(error)?
        else {
            return Ok(false);
        };
        let Ok(policy) = serde_json::from_str::<DelegateReplayPolicy>(text) else {
            return Ok(false);
        };
        if policy.schema_version != 1
            || policy.adapter != ADAPTER
            || policy.invocation_id != intent.invocation_id
            || policy.activation != intent.activation
            || policy.arguments != intent.arguments
            || arguments.protected_arguments() != &intent.arguments
        {
            return Ok(false);
        }
        let bytes = self.content.read_tool_arguments(arguments).map_err(error)?;
        let helper = serde_json::from_slice::<DelegateCall>(&bytes)
            .map(|call| call.helper)
            .unwrap_or_default();
        let Some(returned) = self.delegate_lookup(snapshot, intent, &policy, &helper)? else {
            return Ok(false);
        };
        let disposition = if returned.is_ok() {
            InvocationOutcome::Succeeded
        } else {
            InvocationOutcome::Failed
        };
        self.content
            .record_tool_result(
                arguments,
                disposition,
                &serde_json::to_vec(&returned).map_err(error)?,
                now_ms()?,
            )
            .map_err(error)?;
        Ok(true)
    }
}

impl SessionDispatchController {
    /// The `delegate` port for a delegation holder that may add Agents from
    /// approved templates. Isolated Ways keep their fixed candidate roster.
    pub(super) fn scoped_delegate_tool(
        &self,
        activation: &ActivationRef,
    ) -> Result<Option<Arc<dyn BuiltinTool>>> {
        let state = self.lock()?;
        let bound = state
            .bound
            .get(&activation.activation_id)
            .filter(|bound| bound.activation == *activation)
            .ok_or_else(|| error("delegate source is not bound"))?;
        let policy = state
            .authority
            .grant_policy(bound.grant.grant_id.as_str())
            .map_err(error)?;
        let delegates = policy.holder == activation.node_id
            && policy.delegation.as_deref().is_some_and(|delegation| {
                !delegation.templates.is_empty()
                    && delegation
                        .operations
                        .iter()
                        .any(|permission| permission.operation == DelegatedOperation::AddAgent)
            });
        if !delegates || state.is_isolated_ways()? {
            return Ok(None);
        }
        let mut helpers = vec![];
        let mut lines = vec![];
        for (worker, profile) in state.delegate_helpers(&policy)? {
            if !write_tools(&profile).is_empty() {
                continue;
            }
            let offered = offered_tools(&profile);
            let tools = if offered.is_empty() {
                "no tools".to_owned()
            } else {
                format!("tools {}", offered.join(", "))
            };
            let tools = if read_only(&profile) && offered.contains(&"bash") {
                format!("{tools} (its bash cannot change the repository)")
            } else {
                tools
            };
            lines.push(format!(
                "- {}: {tools}; up to {} steps and {} tokens.",
                worker.template_id, worker.limits.invocations, worker.limits.tokens
            ));
            helpers.push(worker.template_id);
        }
        let description = format!(
            "Give one task to a read-only helper Agent and wait for its answer. Use helpers \
             to do better work: before you change code, ask one to find the relevant code \
             and tests; before you finish, ask one to review your change against the task, \
             the documented contracts and edge cases, and the tests, then fix what it \
             reports. A helper starts fresh: it sees the Session's request and your task, \
             not this conversation, so put in the task every detail it needs (files, what \
             you changed, what to report). Its limits are held from your budget while it \
             runs and what it does not use comes back when it finishes; a step is one model \
             call or one tool call. The same task to the same helper again returns the \
             earlier answer; a call whose helper was not started is tried again. Several \
             delegate calls in one response run at the same time. Answers over \
             {MAX_ANSWER_BYTES} bytes are cut.\nHelpers:\n{}",
            lines.join("\n")
        );
        Ok(Some(Arc::new(DelegateTool {
            controller: self.clone(),
            activation: activation.clone(),
            description,
            helpers,
        })))
    }

    /// Admit the helper for one call, or reattach to the one an identical
    /// earlier call admitted. Returns the helper node and a handle that waits
    /// for its canonical outcome.
    fn delegate(
        &self,
        lead: &ActivationRef,
        call: &DelegateCall,
    ) -> std::result::Result<(TurnNodeId, Box<dyn AdmittedChildExecution>), String> {
        let (worker, request, attempt, control) = {
            let state = self.lock().map_err(|failure| failure.to_string())?;
            state
                .execution_admission()
                .map_err(|failure| failure.to_string())?;
            let bound = state
                .bound
                .get(&lead.activation_id)
                .filter(|bound| bound.activation == *lead)
                .cloned()
                .ok_or_else(|| "The delegating Agent is no longer running.".to_string())?;
            let policy = state
                .authority
                .grant_policy(bound.grant.grant_id.as_str())
                .map_err(|failure| failure.to_string())?;
            let (worker, request) = state
                .delegate_request(lead, &policy, call)
                .map_err(|failure| failure.to_string())??;
            let digest = super::coordinator::native_child_digest(lead, &request, &worker, &None)
                .map_err(|failure| failure.to_string())?;
            let attempt = state
                .native_child_attempt(&digest)
                .map_err(|failure| failure.to_string())?;
            // An identical call reattaches to the helper it admitted. With no
            // command, earlier attempts, if any, admitted no helper: this is a
            // fresh admission, whose follow-up reserve is checked with it.
            if !matches!(
                attempt.state,
                None | Some(ControlCommandState::Applied | ControlCommandState::Settled)
            ) {
                return Err(format!(
                    "The earlier call to helper '{}' with this task has not finished being \
                     recorded, so it cannot be repeated yet. Continue without it, or delegate a \
                     different task.",
                    call.helper
                ));
            }
            (worker, request, attempt, bound.control.clone())
        };
        let ChildAttempt {
            attempt,
            node_id,
            command_id,
            ..
        } = attempt;
        match self.admit_delegated_child(lead, &request, attempt, control) {
            Ok(Ok(wait)) => Ok((node_id, wait)),
            Ok(Err(refused)) => Err(refused),
            Err(failure) => {
                let state = self.lock().map_err(|failure| failure.to_string())?;
                match state.commands.receipt(&command_id) {
                    Ok(Some(receipt)) => match &receipt.view().last_transition {
                        Some(ControlTransition::Rejected { failure }) => {
                            Err(refusal(&call.helper, Some(&worker.limits), failure))
                        }
                        _ => Err(failure.to_string()),
                    },
                    _ => Err(failure.to_string()),
                }
            }
        }
    }
}

#[async_trait]
impl BuiltinTool for DelegateTool {
    fn description(&self) -> &str {
        &self.description
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "required": ["helper", "task"],
            "properties": {
                "helper": {
                    "type": "string",
                    "enum": self.helpers,
                    "description": "The helper to run."
                },
                "task": {
                    "type": "string",
                    "description": "Everything the helper needs, at most 16 KiB: the goal, the files or facts to look at, and what to report back. The helper cannot see this conversation."
                }
            },
            "additionalProperties": false
        })
    }
    fn advertised_parameters_schema(&self) -> Option<serde_json::Value> {
        if self.helpers.is_empty() {
            return None;
        }
        let state = self.controller.lock().ok()?;
        let bound = state.bound.get(&self.activation.activation_id)?;
        state
            .authority
            .attest_control_source(&bound.lease, now_ms().ok()?)
            .ok()?;
        Some(self.parameters_schema())
    }
    /// Helpers are read-only and each admission is serialized by the
    /// controller, so several calls of one model round run their helpers at
    /// the same time.
    fn concurrency_policy(&self) -> axocoatl_llm::ConcurrencyPolicy {
        axocoatl_llm::ConcurrencyPolicy::Safe
    }
    async fn execute(
        &self,
        arguments: serde_json::Value,
    ) -> std::result::Result<serde_json::Value, ToolError> {
        let failed = |reason: String| ToolError::ExecutionFailed {
            tool: NAME.into(),
            reason,
        };
        let call: DelegateCall =
            serde_json::from_value(arguments).map_err(|reason| ToolError::InvalidArgs {
                tool: NAME.into(),
                reason: format!(
                    "{reason}. Pass exactly {{\"helper\": \"<helper>\", \"task\": \"<task>\"}}."
                ),
            })?;
        let (node, wait) = self
            .controller
            .delegate(&self.activation, &call)
            .map_err(failed)?;
        match wait.run().await {
            Ok(measured) => match measured.outcome {
                AgentRunOutcome::Completed(output) => Ok(completed_answer(
                    &call.helper,
                    &node,
                    &output.content,
                    &measured.token_usage,
                    false,
                )),
                AgentRunOutcome::Cancelled { .. } => Err(failed(format!(
                    "The helper '{}' was stopped before it finished (node {}), so there is no \
                     answer. Continue without it, or delegate a narrower task.",
                    call.helper,
                    node.as_str()
                ))),
            },
            Err(failure) => {
                tracing::warn!(
                    helper = %call.helper,
                    node = %node.as_str(),
                    reason = %failure.message,
                    "Delegated helper activation failed"
                );
                Err(failed(format!(
                    "The helper '{}' did not finish (node {}): {}. Continue without its answer, or \
                     delegate a narrower task.",
                    call.helper,
                    node.as_str(),
                    failure.message
                )))
            }
        }
    }
}

#[cfg(test)]
impl SessionDispatchController {
    /// The lead's delegate intent, its audit record, its returned result and
    /// its retained replay policy.
    pub(crate) fn delegate_recovery_evidence_for_test(
        &self,
    ) -> (
        axocoatl_session::invocation_audit::AuditedInvocation,
        Option<serde_json::Value>,
        serde_json::Value,
    ) {
        let state = self.lock().unwrap();
        let intent = state
            .audit
            .turn_invocations(&state.turn_id)
            .unwrap()
            .into_iter()
            .map(|audited| audited.intent)
            .find(|intent| intent.tool_name == NAME)
            .unwrap();
        let snapshot = state
            .canonical
            .snapshot(&intent.activation.turn_id)
            .unwrap();
        let arguments = state
            .content
            .tool_arguments(&snapshot, &intent.activation, &intent.invocation_id)
            .unwrap()
            .unwrap();
        let result = state
            .content
            .tool_result(&arguments)
            .unwrap()
            .map(|result| {
                serde_json::from_slice(&state.content.read_tool_result(&result).unwrap()).unwrap()
            });
        let InvocationReplayPolicy::ReconcileBeforeReplay { policy_ref } = &intent.replay_policy
        else {
            panic!("delegate intent must reconcile before replay")
        };
        let ActivationEvidenceContent::Guidance { text } = &state
            .content
            .resolve_activation_evidence(policy_ref)
            .unwrap()
        else {
            panic!("delegate replay policy is guidance")
        };
        let policy = serde_json::from_str(text).unwrap();
        (
            state
                .audit
                .invocation(&intent.invocation_id)
                .unwrap()
                .unwrap()
                .clone(),
            result,
            policy,
        )
    }

    pub(crate) fn delegate_helper_answer_for_test(&self, node: &TurnNodeId) -> Option<String> {
        let state = self.lock().unwrap();
        let snapshot = state.canonical.snapshot(&state.turn_id).unwrap();
        state
            .accepted_helper_answer(&snapshot, node)
            .unwrap()
            .map(|(text, _)| text)
    }

    pub(crate) fn grant_usage_for_test(
        &self,
        grant_id: &str,
    ) -> axocoatl_session::control_authority::GrantUsage {
        self.lock().unwrap().authority.usage(grant_id).unwrap()
    }
}
