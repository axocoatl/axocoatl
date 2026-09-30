//! Host-supplied acknowledgement boundary for a single actor activation.
//!
//! This module does not mint authority or persist evidence. Its host implementation
//! must bind the boundary to an exact activation and grant, protect the executable
//! argument bytes, acknowledge durable intent, and claim current dispatch authority
//! before returning admission. Provider call identifiers are correlation, not authority.

use std::sync::Arc;

use async_trait::async_trait;
use axocoatl_llm::{ConcurrencyPolicy, ToolCall};
use axocoatl_tools::{BuiltinTool, ToolError, ToolExecutor, ToolResult};

use crate::run_control::AgentRunControl;

/// Exact approved call after all pre-hook argument transformations. The provider
/// group and original index distinguish repeated, missing, or reused native ids.
#[derive(Debug, Clone)]
pub struct ToolInvocationRequest {
    pub actor_id: String,
    pub provider_id: String,
    pub model_id: String,
    pub provider_response_group: u64,
    pub provider_call_index: usize,
    pub provider_call_count: usize,
    pub tool_call: ToolCall,
}

/// A raw backend return, before any post-hook or model-history conversion.
/// A returned error does not establish that no external effect occurred.
#[derive(Debug, Clone, PartialEq)]
pub enum ToolInvocationOutcome {
    Returned(Result<serde_json::Value, String>),
    /// A panic/task failure cannot supply an authoritative backend result.
    Unknown {
        reason: String,
    },
}

/// The host's answer to an admission request that it may decline.
pub enum InvocationAdmission {
    Admitted(Box<dyn AdmittedToolInvocation>),
    /// Nothing was recorded; the reason is returned to the model.
    Declined(String),
}

/// One acknowledged invocation. Dropping this value proves neither cancellation
/// nor absence of effects: the host must retain the unresolved durable intent.
#[async_trait]
pub trait AdmittedToolInvocation: Send {
    /// An exact host-bound executor for this acknowledged invocation. This is
    /// only consulted for executor tools, never behavior-owned tools. The
    /// default preserves the actor's original executor unchanged.
    fn tool_executor(&self) -> Option<Arc<ToolExecutor>> {
        None
    }

    /// Persist the raw outcome before it can become model input. Failure is fatal
    /// to the actor run, even if the backend already changed external state.
    async fn record_outcome(self: Box<Self>, outcome: &ToolInvocationOutcome)
        -> Result<(), String>;
}

/// A Coordinator's proposed child, not an authorization to create one. The host
/// must validate topology, profile, input, budget, and isolation before admission.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ChildExecutionRequest {
    pub actor_id: String,
    pub logical_worker_id: String,
    pub subtask_index: usize,
    pub task_name: String,
    pub task_input: String,
    pub tools: Vec<String>,
    pub provider_id: String,
    pub model: String,
    /// The exact attachments selected for this worker request. The host must
    /// resolve them against retained input before admitting a child.
    pub attachments: Vec<axocoatl_core::AgentAttachment>,
}

/// One admitted child owned by the existing Session controller. Running this
/// handle waits for its canonical outcome; it must not spawn a second actor.
#[async_trait]
pub trait AdmittedChildExecution: Send + Sync {
    async fn run(
        self: Box<Self>,
    ) -> Result<crate::MeasuredAgentRunOutcome, crate::AgentExecutionFailure>;
}

/// Actual actor input acknowledgement. Dropping a handoff proves neither
/// delivery nor non-delivery; the host must retain its unresolved command.
pub trait SteeringAcknowledgement: Send {
    fn acknowledge(self: Box<Self>) -> Result<(), String>;
}

pub struct SteeringDelivery {
    pub text: String,
    pub attachments: Vec<axocoatl_core::AgentAttachment>,
    pub acknowledgement: Box<dyn SteeringAcknowledgement>,
}

#[async_trait]
pub trait ToolExecutionBoundary: Send + Sync {
    /// A native host returns its exact controller-owned child. Compatibility
    /// hosts return None and continue through the existing provision_child port.
    async fn schedule_child(
        &self,
        _request: &ChildExecutionRequest,
        _control: AgentRunControl,
    ) -> Result<Option<Box<dyn AdmittedChildExecution>>, String> {
        Ok(None)
    }
    fn approval_actor_scope(&self) -> Result<String, String> {
        Err("this execution host has no exact approval identity".into())
    }

    /// Only a trusted approval hook requests this wait. Default hosts refuse;
    /// the callback cannot create permission from tool output or model prose.
    async fn request_human_approval(
        &self,
        _request: &ToolInvocationRequest,
        _display_request: serde_json::Value,
        _timeout: std::time::Duration,
    ) -> Result<axocoatl_tools::HookApprovalResolution, String> {
        Err("this execution host has no durable human approval wait".into())
    }

    /// Poll only between complete provider/tool groups, never through the actor
    /// mailbox. A final empty poll closes steering admission atomically with
    /// command acceptance at the host, before this actor returns its answer.
    /// The default declares no steering support and adds no queue or authority.
    fn take_guidance(&self, _final_boundary: bool) -> Result<Option<SteeringDelivery>, String> {
        Ok(None)
    }

    /// A reason the host declines this call before any intent is recorded,
    /// returned to the model as an ordinary tool error so it can finish. Nothing
    /// is admitted or dispatched. `earlier` counts calls from the same provider
    /// response that already passed this check and are not yet admitted. The
    /// default declines nothing.
    fn preadmission_refusal(
        &self,
        _request: &ToolInvocationRequest,
        _earlier: u32,
    ) -> Option<String> {
        None
    }

    /// Return only after durable intent AND the live dispatch claim are
    /// acknowledged. On failure, this actor will not execute the backend. The
    /// host remains responsible for recording any proven pre-dispatch rejection.
    async fn admit(
        &self,
        request: &ToolInvocationRequest,
    ) -> Result<Box<dyn AdmittedToolInvocation>, String>;

    /// Admit, or decline under the host's admission lock without failing the
    /// activation; a declined call reaches the model as a tool error and
    /// records nothing. An `Err` still fails the activation. The default
    /// admits through [`Self::admit`].
    async fn admit_or_decline(
        &self,
        request: &ToolInvocationRequest,
    ) -> Result<InvocationAdmission, String> {
        self.admit(request).await.map(InvocationAdmission::Admitted)
    }

    /// Provision a distinct exact child activation and retain its cancellation
    /// handle for targeted Stop. Cloning the parent's authority is insufficient.
    /// The supplied handle inherits parent cancellation, but its own cancellation
    /// does not cancel siblings. The default deliberately refuses child execution.
    async fn provision_child(
        &self,
        _request: &ChildExecutionRequest,
        _control: AgentRunControl,
    ) -> Result<Arc<dyn ToolExecutionBoundary>, String> {
        Err("the execution boundary cannot provision a child activation".to_string())
    }
}

pub(crate) enum InvocationBackend {
    Behavior(Arc<dyn BuiltinTool>),
    Executor(Arc<ToolExecutor>),
}

pub(crate) struct PendingToolInvocation {
    pub request: ToolInvocationRequest,
    pub backend: InvocationBackend,
    pub policy: ConcurrencyPolicy,
}

pub(crate) struct AcknowledgedToolResult {
    pub result: ToolResult,
    pub run_post_hooks: bool,
}

async fn invoke(
    pending: PendingToolInvocation,
    boundary: Arc<dyn ToolExecutionBoundary>,
    control: AgentRunControl,
) -> Result<AcknowledgedToolResult, String> {
    let request = pending.request;
    if control.is_cancelled() {
        return Ok(AcknowledgedToolResult {
            result: ToolResult {
                seq: request.provider_call_index,
                tool_call: request.tool_call.clone(),
                result: Err(ToolError::ExecutionFailed {
                    tool: request.tool_call.name,
                    reason: "cancelled before tool admission".to_string(),
                }),
            },
            run_post_hooks: false,
        });
    }
    let declined = |reason: String| AcknowledgedToolResult {
        result: ToolResult {
            seq: request.provider_call_index,
            tool_call: request.tool_call.clone(),
            result: Err(ToolError::ExecutionFailed {
                tool: request.tool_call.name.clone(),
                reason,
            }),
        },
        run_post_hooks: false,
    };
    if let Some(reason) = boundary.preadmission_refusal(&request, 0) {
        return Ok(declined(reason));
    }
    let admitted = match boundary.admit_or_decline(&request).await {
        Ok(InvocationAdmission::Admitted(admitted)) => admitted,
        Ok(InvocationAdmission::Declined(reason)) => return Ok(declined(reason)),
        Err(error) => {
            let error = format!("tool invocation admission failed: {error}");
            control.fail_execution_boundary(error.clone());
            return Err(error);
        }
    };
    // Admission includes the final authority claim. Do not recheck a loose
    // cancellation flag here: Stop is ordered against that claim by the host,
    // and already-claimed work is allowed to reach its settlement boundary.
    // The worker owns both admission and settlement independently of the
    // caller's wait. Aborting an actor or its scheduling JoinSet must not drop
    // admission while a separately spawned backend continues executing.
    let worker_control = control.clone();
    tokio::spawn(invoke_admitted(
        request,
        pending.backend,
        admitted,
        worker_control,
    ))
    .await
    .map_err(|error| {
        let error = format!("owned tool settlement task failed: {error}");
        control.fail_execution_boundary(error.clone());
        error
    })?
}

async fn invoke_admitted(
    request: ToolInvocationRequest,
    backend: InvocationBackend,
    admitted: Box<dyn AdmittedToolInvocation>,
    control: AgentRunControl,
) -> Result<AcknowledgedToolResult, String> {
    let backend = match (backend, admitted.tool_executor()) {
        (InvocationBackend::Executor(original), replacement) => {
            InvocationBackend::Executor(replacement.unwrap_or(original))
        }
        (backend, _) => backend,
    };
    let call = request.tool_call.clone();
    let backend_result = tokio::spawn(async move {
        match backend {
            InvocationBackend::Behavior(tool) => tool.execute(call.arguments).await,
            InvocationBackend::Executor(executor) => {
                executor.execute(&call.name, call.arguments).await
            }
        }
    })
    .await;
    let (result, outcome) = match backend_result {
        Ok(result) => {
            let outcome = ToolInvocationOutcome::Returned(
                result
                    .as_ref()
                    .map(Clone::clone)
                    .map_err(ToString::to_string),
            );
            (result, outcome)
        }
        Err(error) => {
            let reason = format!("tool task failed without an authoritative outcome: {error}");
            (
                Err(ToolError::ExecutionFailed {
                    tool: request.tool_call.name.clone(),
                    reason: reason.clone(),
                }),
                ToolInvocationOutcome::Unknown { reason },
            )
        }
    };
    admitted.record_outcome(&outcome).await.map_err(|error| {
        let error = format!("tool invocation outcome persistence failed: {error}");
        control.fail_execution_boundary(error.clone());
        error
    })?;
    if matches!(outcome, ToolInvocationOutcome::Unknown { .. }) {
        let error = "tool invocation outcome is unknown; execution requires reconciliation";
        control.fail_execution_boundary(error.into());
        return Err(error.into());
    }
    Ok(AcknowledgedToolResult {
        result: ToolResult {
            seq: request.provider_call_index,
            tool_call: request.tool_call,
            result,
        },
        run_post_hooks: true,
    })
}

/// Match existing Safe/Ordered/Exclusive scheduling, while retaining each
/// original provider index. Every started task is joined before a boundary
/// failure returns; a successful sibling is not abandoned on first error.
pub(crate) async fn dispatch_acknowledged(
    calls: Vec<PendingToolInvocation>,
    boundary: Arc<dyn ToolExecutionBoundary>,
    control: AgentRunControl,
) -> Result<Vec<AcknowledgedToolResult>, String> {
    let exclusive = calls
        .iter()
        .any(|call| call.policy == ConcurrencyPolicy::Exclusive);
    let mut results = Vec::new();
    let mut failure = None;
    if exclusive {
        for call in calls {
            match invoke(call, boundary.clone(), control.clone()).await {
                Ok(result) => results.push(result),
                Err(error) => {
                    failure.get_or_insert(error);
                    break;
                }
            }
        }
    } else {
        let mut running = tokio::task::JoinSet::new();
        let mut ordered = Vec::new();
        for call in calls {
            if call.policy == ConcurrencyPolicy::Safe {
                running.spawn(invoke(call, boundary.clone(), control.clone()));
            } else {
                ordered.push(call);
            }
        }
        while let Some(result) = running.join_next().await {
            match result {
                Ok(Ok(result)) => results.push(result),
                Ok(Err(error)) => {
                    failure.get_or_insert(error);
                }
                Err(error) => {
                    let error = format!("invocation boundary task failed: {error}");
                    control.fail_execution_boundary(error.clone());
                    failure.get_or_insert(error);
                }
            }
        }
        if failure.is_none() {
            for call in ordered {
                match invoke(call, boundary.clone(), control.clone()).await {
                    Ok(result) => results.push(result),
                    Err(error) => {
                        failure.get_or_insert(error);
                        break;
                    }
                }
            }
        }
    }
    if let Some(error) = failure {
        Err(error)
    } else {
        results.sort_by_key(|result| result.result.seq);
        Ok(results)
    }
}

/// Wrap a boundary with one actual native call before entering the hook task.
pub(crate) struct InvocationHookApproval {
    pub boundary: Arc<dyn ToolExecutionBoundary>,
    pub request: ToolInvocationRequest,
}
#[async_trait]
impl axocoatl_tools::HookApprovalBoundary for InvocationHookApproval {
    fn actor_scope(&self) -> Result<String, String> {
        self.boundary.approval_actor_scope()
    }

    async fn request_human_approval(
        &self,
        context: &axocoatl_tools::HookContext,
        display_request: serde_json::Value,
        timeout: std::time::Duration,
    ) -> Result<axocoatl_tools::HookApprovalResolution, String> {
        if context.agent_id != self.actor_scope()?
            || context.tool_name != self.request.tool_call.name
        {
            return Err("approval hook changed its exact actor or tool".into());
        }
        let mut request = self.request.clone();
        request.tool_call.arguments = context.value.clone();
        self.boundary
            .request_human_approval(&request, display_request, timeout)
            .await
    }
}
