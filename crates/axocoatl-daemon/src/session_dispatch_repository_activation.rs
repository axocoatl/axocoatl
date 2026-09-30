//! Exact Session checkout ownership beneath the existing native actor tools.
//! The host resolves one retained resource. Each acknowledged invocation then
//! receives its own executor, preserving its protected arguments and actual
//! built-in implementation. A path or recorded description cannot mint access.
use super::*;
use crate::bootstrap::session_repository::SessionRepositoryOwner;
use axocoatl_exec::protocol::{
    ExecRequest, ProcessOutcome, ServerMessage, StdinDescriptor, MAX_FILE_CAPTURE_BYTES,
    PROTOCOL_VERSION,
};
use axocoatl_isolation::supervisor_transport::{
    RunningSupervisedCommand, SupervisedExecution, SupervisorCancellation,
};
use axocoatl_isolation::{BgTask, ExecResult, IsolationError, Sandbox};
use axocoatl_tools::{BuiltinTool, ToolError, ToolExecutor};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

// These are the existing foreground Session tools whose complete process
// lifetime can be owned by the supervisor. Background and PTY tools still need
// their own transport/lifetime join; ordinary Session executors are unchanged.
const SUPPORTED_TOOLS: &[&str] = &[
    "read_file",
    "write_file",
    "edit_file",
    "list_dir",
    "grep",
    "glob",
    "bash",
];
const FOREGROUND_STREAM_BYTES: usize = 1024 * 1024;

/// Definition admission and activation preparation share the exact foreground
/// capability boundary. Reject unsupported names before provider observation.
pub(crate) fn validate_repository_tools(tools: &[String]) -> Result<()> {
    if let Some(tool) = tools
        .iter()
        .find(|tool| !SUPPORTED_TOOLS.contains(&tool.as_str()))
    {
        return Err(error(format!(
            "native Session repository tool '{tool}' has no owned foreground implementation; supported tools: {}. Background and PTY ownership is not integrated",
            SUPPORTED_TOOLS.join(", ")
        )));
    }
    Ok(())
}

/// Opaque host resource, deliberately neither deserializable nor constructible
/// from repository metadata. Cloning retains this exact owner, never a new gate.
#[derive(Clone)]
pub struct RepositoryActivationResource {
    owner: SessionRepositoryOwner,
    reference: EvidenceRef,
    description: ActivationEvidenceContent,
}

impl SessionDispatchController {
    #[allow(dead_code)] // Host port remains dormant until live v2 ingress is joined.
    pub(crate) fn repository_activation_resource(
        &self,
        reference: &EvidenceRef,
    ) -> Result<RepositoryActivationResource> {
        self.lock()?.repository_activation_resource(reference)
    }
}

impl DispatchState {
    /// Resolve under an already-held canonical lock for host control preview;
    /// no nested lock and no path/metadata-only authority substitution.
    pub(super) fn repository_activation_resource(
        &self,
        reference: &EvidenceRef,
    ) -> Result<RepositoryActivationResource> {
        self.execution_admission()?;
        let owner = self
            .repository_owners
            .get(reference)
            .ok_or_else(|| error("repository input has no registered live owner"))?
            .clone();
        repository::validate_retained_repository(self, &owner, reference)?;
        owner.validate_dispatch_resource().map_err(error)?;
        Ok(RepositoryActivationResource {
            owner,
            reference: reference.clone(),
            description: self
                .content
                .resolve_activation_evidence(reference)
                .map_err(error)?
                .clone(),
        })
    }
}

/// The write scopes an activation was admitted under, each a list of path
/// patterns. A path may change only if every scope allows it, and any empty
/// scope makes the activation read-only. No scope leaves every path open.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct AdmittedWriteScope(Vec<Vec<String>>);

impl AdmittedWriteScope {
    pub(super) fn is_unrestricted(&self) -> bool {
        self.0.is_empty()
    }
    pub(super) fn is_read_only(&self) -> bool {
        self.0.iter().any(Vec::is_empty)
    }
    pub(super) fn allows(&self, path: &str) -> bool {
        self.0
            .iter()
            .all(|scope| axocoatl_session::path_scope::scope_allows(Some(scope), path))
    }
    /// How the scope reads in messages to the Agent and the person.
    pub(super) fn describe(&self) -> String {
        if self.is_read_only() {
            "none; this Agent is read-only".to_owned()
        } else {
            self.0
                .iter()
                .map(|scope| scope.join(", "))
                .collect::<Vec<_>>()
                .join("; and only within ")
        }
    }
}

impl DispatchState {
    /// The write scope an activation was admitted with, read from its durable
    /// authority record and never from a live copy. Standing work adds its
    /// route's scope for every activation of its turn. A caller that cannot
    /// read the scope must refuse the write or process it was checking.
    pub(super) fn admitted_write_scope(
        &self,
        activation: &ActivationRef,
    ) -> Result<AdmittedWriteScope> {
        let profile = self
            .authority
            .activation_profile(activation)
            .map_err(error)?;
        let standing = self.standing_work()?.and_then(|work| work.write_scope);
        Ok(AdmittedWriteScope(
            profile.write_scope.into_iter().chain(standing).collect(),
        ))
    }
}

impl RepositoryActivationResource {
    pub fn reference(&self) -> &EvidenceRef {
        &self.reference
    }

    /// Check the actual retained Session, Workspace and runtime registry before
    /// preparing the actor. Tool dispatch repeats this check through its lease.
    pub(super) async fn validate_current(&self) -> Result<()> {
        self.owner.validate_dispatch_resource().map_err(error)?;
        self.owner.validate_current().await.map_err(error)?;
        self.owner.validate_dispatch_resource().map_err(error)
    }

    pub(super) fn description(&self) -> Result<ActivationEvidenceContent> {
        // The semantic input remains its immutable original description. The
        // separately retained reattachment proof joins it to this live owner.
        Ok(self.description.clone())
    }

    pub(super) fn preview_tools(&self, profile: &ExecutionProfile) -> Result<Arc<ToolExecutor>> {
        validate_repository_tools(&profile.tools)?;
        // Definitions come from the actual built-ins. This executor cannot run:
        // acknowledged admission replaces it with an exact invocation executor.
        Ok(session_tools(Arc::new(RepositorySandbox {
            resource: self.clone(),
            invocation: None,
        })))
    }
}

pub(super) fn validate_input_resource(
    state: &DispatchState,
    manifest: &ActivationInputManifest,
    resource: Option<&RepositoryActivationResource>,
) -> Result<()> {
    match (&manifest.repository, resource) {
        (RepositoryInput::Unavailable, None) => Ok(()),
        (RepositoryInput::Recorded { snapshot }, Some(resource))
            if snapshot == &resource.reference =>
        {
            repository::validate_retained_repository(state, &resource.owner, snapshot)?;
            resource.owner.validate_dispatch_resource().map_err(error)
        }
        _ => Err(error(
            "activation repository differs from its exact registered resource",
        )),
    }
}

fn session_tools(sandbox: Arc<dyn Sandbox>) -> Arc<ToolExecutor> {
    let mut executor = ToolExecutor::new();
    axocoatl_tools::register_session_tools(&mut executor, sandbox);
    Arc::new(executor)
}

pub(super) struct RepositoryInvocation {
    scope: Arc<InvocationScope>,
    executor: Arc<ToolExecutor>,
}

impl RepositoryInvocation {
    pub(super) fn for_admission(
        state: &DispatchState,
        controller: SessionDispatchController,
        intent: &InvocationIntent,
    ) -> Result<Option<Self>> {
        let bound = state
            .bound
            .get(&intent.activation.activation_id)
            .filter(|bound| bound.activation == intent.activation)
            .ok_or_else(|| error("invocation has no exact bound executor"))?;
        if intent.tool_name == super::delegate::NAME || intent.tool_name == super::knowledge::NAME {
            return Ok(None);
        }
        let Some(resource) = &bound.repository else {
            return Ok(None);
        };
        if !SUPPORTED_TOOLS.contains(&intent.tool_name.as_str()) {
            return Err(error(
                "invocation has no owned repository tool implementation",
            ));
        }
        repository::validate_retained_repository(state, &resource.owner, &resource.reference)?;
        resource.owner.validate_dispatch_resource().map_err(error)?;
        let scope = Arc::new(InvocationScope {
            controller,
            intent: intent.clone(),
            resource: resource.clone(),
            control: bound.control.clone(),
            called: AtomicBool::new(false),
            uncertain: AtomicBool::new(false),
            process_index: AtomicU64::new(0),
            require_complete_capture: AtomicBool::new(false),
        });
        let backend = session_tools(Arc::new(RepositorySandbox {
            resource: resource.clone(),
            invocation: Some(scope.clone()),
        }));
        let definition = backend
            .as_llm_tools()
            .into_iter()
            .find(|tool| tool.name == intent.tool_name)
            .ok_or_else(|| error("the repository built-in is unavailable"))?;
        let mut executor = ToolExecutor::new();
        executor.register_builtin(
            intent.tool_name.clone(),
            Arc::new(InvocationTool {
                scope: scope.clone(),
                backend,
                definition,
            }),
        );
        Ok(Some(Self {
            scope,
            executor: Arc::new(executor),
        }))
    }

    /// The fixed host capture uses the existing supervisor stream ceiling;
    /// ordinary BashTool retains its smaller user-facing presentation prefix.
    pub(super) async fn capture_snapshot(&self, command: &str) -> Result<serde_json::Value> {
        if self.scope.intent.tool_name != "bash" || command != super::repository_snapshot::CAPTURE {
            return Err(error(
                "repository capture is not the fixed approved invocation",
            ));
        }
        self.scope.begin(&serde_json::json!({"command":command}))?;
        self.scope
            .require_complete_capture
            .store(true, Ordering::Release);
        let root = self.scope.resource.owner.root();
        let root = root.to_string_lossy();
        let result = self
            .scope
            .execute(
                &[
                    "sh",
                    "-c",
                    "cd \"$1\" && exec sh -c \"$2\" sh",
                    "sh",
                    root.as_ref(),
                    command,
                ],
                None,
                Duration::from_secs(180),
            )
            .await
            .map_err(error)?;
        Ok(
            serde_json::json!({"stdout":result.stdout,"stderr":result.stderr,"exit_code":result.exit_code,"stdout_truncated":false}),
        )
    }

    /// The fixed, read-only digest observation (see
    /// `repository_snapshot::digest_command`). Anything else is refused.
    pub(super) async fn observe_file_digests(&self, command: &str) -> Result<serde_json::Value> {
        if self.scope.intent.tool_name != "bash"
            || !super::repository_snapshot::is_digest_command(command)
        {
            return Err(error(
                "file digest observation is not the fixed approved invocation",
            ));
        }
        self.scope.begin(&serde_json::json!({"command":command}))?;
        self.scope
            .require_complete_capture
            .store(true, Ordering::Release);
        let root = self.scope.resource.owner.root();
        let root = root.to_string_lossy();
        let result = self
            .scope
            .execute(
                &[
                    "sh",
                    "-c",
                    "cd \"$1\" && exec sh -c \"$2\" sh",
                    "sh",
                    root.as_ref(),
                    command,
                ],
                None,
                Duration::from_secs(60),
            )
            .await
            .map_err(error)?;
        Ok(serde_json::json!({"stdout":result.stdout,"exit_code":result.exit_code}))
    }

    pub(super) fn executor(&self) -> Arc<ToolExecutor> {
        self.executor.clone()
    }
    pub(super) fn is_uncertain(&self) -> bool {
        self.scope.uncertain.load(Ordering::Acquire)
    }
}

struct InvocationTool {
    scope: Arc<InvocationScope>,
    backend: Arc<ToolExecutor>,
    definition: axocoatl_llm::ToolDefinition,
}

#[async_trait]
impl BuiltinTool for InvocationTool {
    fn description(&self) -> &str {
        &self.definition.description
    }
    fn parameters_schema(&self) -> serde_json::Value {
        self.definition.parameters.clone()
    }
    fn concurrency_policy(&self) -> axocoatl_llm::ConcurrencyPolicy {
        self.definition.concurrency
    }
    async fn execute(
        &self,
        arguments: serde_json::Value,
    ) -> std::result::Result<serde_json::Value, ToolError> {
        self.scope
            .begin(&arguments)
            .map_err(|failure| ToolError::ExecutionFailed {
                tool: self.definition.name.clone(),
                reason: failure.to_string(),
            })?;
        self.enforce_write_scope(&arguments)?;
        self.backend.execute(&self.definition.name, arguments).await
    }
}

impl InvocationTool {
    /// File-writing tools refuse a path outside the activation's admitted write
    /// scope before any effect. A scope that cannot be read refuses the write.
    fn enforce_write_scope(
        &self,
        arguments: &serde_json::Value,
    ) -> std::result::Result<(), ToolError> {
        if !matches!(self.definition.name.as_str(), "write_file" | "edit_file") {
            return Ok(());
        }
        let scope = self
            .scope
            .controller
            .lock()
            .and_then(|state| state.admitted_write_scope(&self.scope.intent.activation));
        let path = arguments
            .get("path")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();
        match write_refusal(scope, self.scope.resource.owner.root(), path) {
            Some(reason) => Err(ToolError::ExecutionFailed {
                tool: self.definition.name.clone(),
                reason,
            }),
            None => Ok(()),
        }
    }
}

/// Why a file tool must not write `path` under `scope`, or `None` to allow it.
fn write_refusal(scope: Result<AdmittedWriteScope>, root: &Path, path: &str) -> Option<String> {
    let scope = match scope {
        Ok(scope) => scope,
        Err(failure) => {
            return Some(format!(
                "the paths this Agent may change cannot be read ({failure}), so no file is \
                 written. Leave {path} unchanged and describe the needed change in your answer."
            ))
        }
    };
    if scope.is_unrestricted() {
        return None;
    }
    if let Some(reason) = unfollowed_write_path(root, path) {
        return Some(reason);
    }
    let relative = scoped_relative_path(root, path);
    if relative
        .as_deref()
        .is_some_and(|relative| scope.allows(relative))
    {
        return None;
    }
    Some(format!(
        "{path} is outside the paths this Agent may change ({}). Leave it unchanged and \
         describe the needed change in your answer.",
        scope.describe()
    ))
}

/// Why a scoped write to `path` must be refused before its text is matched:
/// a `..` or a symbolic link on the way could land it outside the scope.
fn unfollowed_write_path(root: &Path, path: &str) -> Option<String> {
    if Path::new(path)
        .components()
        .any(|part| part == std::path::Component::ParentDir)
    {
        return Some(format!(
            "{path} uses '..'; name the file by its repository path"
        ));
    }
    let relative = scoped_relative_path(root, path)?;
    let mut current = root.to_path_buf();
    for part in relative.split('/') {
        current.push(part);
        match std::fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Some(format!(
                    "{path} passes through a symbolic link; writes do not follow symbolic links"
                ));
            }
            Ok(_) => {}
            Err(failure) if failure.kind() == std::io::ErrorKind::NotFound => return None,
            Err(failure) => return Some(failure.to_string()),
        }
    }
    None
}

/// Repository-relative form of a tool path, or `None` if it leaves the root.
fn scoped_relative_path(root: &Path, path: &str) -> Option<String> {
    let candidate = Path::new(path);
    let relative = if candidate.is_absolute() {
        candidate.strip_prefix(root).ok()?
    } else {
        candidate
    };
    let mut parts: Vec<&str> = Vec::new();
    for component in relative.components() {
        match component {
            std::path::Component::Normal(part) => parts.push(part.to_str()?),
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                parts.pop()?;
            }
            _ => return None,
        }
    }
    (!parts.is_empty()).then(|| parts.join("/"))
}

struct InvocationScope {
    controller: SessionDispatchController,
    intent: InvocationIntent,
    resource: RepositoryActivationResource,
    control: AgentRunControl,
    called: AtomicBool,
    uncertain: AtomicBool,
    process_index: AtomicU64,
    require_complete_capture: AtomicBool,
}

impl InvocationScope {
    fn begin(&self, arguments: &serde_json::Value) -> Result<()> {
        let bytes = serde_json::to_vec(arguments).map_err(error)?;
        let state = self.controller.lock()?;
        self.validate(&state)?;
        let snapshot = state.current(&self.intent.activation)?;
        let stored = state
            .content
            .tool_arguments(
                &snapshot,
                &self.intent.activation,
                &self.intent.invocation_id,
            )
            .map_err(error)?
            .ok_or_else(|| error("protected repository arguments are missing"))?;
        if stored.protected_arguments() != &self.intent.arguments
            || state.content.read_tool_arguments(&stored).map_err(error)? != bytes
        {
            return Err(error(
                "repository tool arguments differ from acknowledged admission",
            ));
        }
        if self.called.swap(true, Ordering::AcqRel) {
            return Err(error(
                "repository invocation executor was already consumed; no replay",
            ));
        }
        Ok(())
    }
    fn validate(&self, state: &DispatchState) -> Result<()> {
        state.execution_admission()?;
        let snapshot = state.current(&self.intent.activation)?;
        let bound = state
            .bound
            .get(&self.intent.activation.activation_id)
            .filter(|bound| {
                bound.activation == self.intent.activation && !bound.control.is_cancelled()
            })
            .ok_or_else(|| error("repository invocation is no longer current"))?;
        let manifest = &snapshot
            .contract()
            .activations()
            .iter()
            .find(|item| item.activation == self.intent.activation)
            .unwrap()
            .input;
        validate_input_resource(state, manifest, Some(&self.resource))?;
        state
            .authority
            .validate_claimed_dispatch(&bound.lease, &self.intent, &state.audit, now_ms()?)
            .map_err(error)
    }

    async fn execute(
        self: &Arc<Self>,
        argv: &[&str],
        stdin: Option<&str>,
        timeout: Duration,
    ) -> std::result::Result<ExecResult, IsolationError> {
        if !self.called.load(Ordering::Acquire) {
            return Err(isolation_error(
                "repository process has no admitted tool execution",
            ));
        }
        {
            let state = self.controller.lock().map_err(isolation_error)?;
            self.validate(&state).map_err(isolation_error)?;
        }
        // This await owns no command. Full actual Session, runtime, inode and
        // registration checks are repeated by the retained physical owner.
        let mut lease = tokio::select! {
            lease = self.resource.owner.queued_execution_lease() => lease.map_err(isolation_error)?,
            _ = self.control.cancelled() => return Err(isolation_error("repository tool cancelled before process admission")),
        };
        let write_restriction = {
            let state = self.controller.lock().map_err(isolation_error)?;
            self.validate(&state).map_err(isolation_error)?;
            let read_only = state
                .admitted_write_scope(&self.intent.activation)
                .map_err(isolation_error)?
                .is_read_only();
            process_write_restriction(
                read_only,
                &self.intent.tool_name,
                self.require_complete_capture.load(Ordering::Acquire),
                self.resource.owner.root(),
            )
        };
        let index = self
            .process_index
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |value| {
                value.checked_add(1)
            })
            .map_err(|_| isolation_error("repository process index exhausted"))?;
        let request = ExecRequest {
            protocol: PROTOCOL_VERSION,
            invocation_id: format!("{}:{index}", self.intent.invocation_id.as_str()),
            argv: argv.iter().map(|arg| (*arg).to_owned()).collect(),
            timeout_ms: timeout
                .as_millis()
                .try_into()
                .map_err(|_| isolation_error("repository timeout is out of range"))?,
            stdout_bytes: if self.intent.tool_name == "edit_file" {
                MAX_FILE_CAPTURE_BYTES
            } else {
                FOREGROUND_STREAM_BYTES
            },
            stderr_bytes: FOREGROUND_STREAM_BYTES,
            stdin: stdin
                .map(|bytes| StdinDescriptor::for_bytes(bytes.as_bytes()))
                .transpose()
                .map_err(isolation_error)?,
            write_restriction,
        };
        request.validate().map_err(isolation_error)?;
        let command = match stdin {
            Some(bytes) => {
                lease
                    .sandbox()
                    .prepare_supervised_command_with_stdin(
                        request.clone(),
                        bytes.as_bytes().to_vec(),
                    )
                    .await
            }
            None => {
                lease
                    .sandbox()
                    .prepare_supervised_command(request.clone())
                    .await
            }
        }?;
        if command.request() != &request {
            return Err(isolation_error(
                "supervisor changed the admitted repository process",
            ));
        }
        lease
            .bind_supervised_command(&command)
            .map_err(isolation_error)?;
        let cancellation = command.cancellation();
        let (running, ticket) = {
            let state = self.controller.lock().map_err(isolation_error)?;
            self.validate(&state).map_err(isolation_error)?;
            let ticket = state
                .acquire_execution_ticket(&self.controller)
                .map_err(isolation_error)?;
            lease.mark_dispatched().map_err(isolation_error)?;
            let running = command.dispatch().inspect_err(|_| {
                self.uncertain.store(true, Ordering::Release);
            })?;
            (running, ticket)
        };
        // No await between dispatch and ownership transfer. Cancelling a caller
        // requests Stop; the owned task still joins/reaps and settles this lease.
        let scope = self.clone();
        let task = tokio::spawn(async move {
            let _ticket = ticket;
            scope.finish(lease, running).await
        });
        let result = OwnedProcessWait {
            cancellation,
            task: Some(task),
        }
        .finish()
        .await;
        if result.is_err()
            && self
                .resource
                .owner
                .unresolved_execution()
                .map_or(true, |execution| execution.is_some())
        {
            self.uncertain.store(true, Ordering::Release);
        }
        result
    }

    async fn finish(
        self: Arc<Self>,
        lease: crate::bootstrap::session_repository::SessionRepositoryExecutionLease,
        running: RunningSupervisedCommand,
    ) -> std::result::Result<ExecResult, IsolationError> {
        let cancellation = running.cancellation();
        let finish = running.finish();
        tokio::pin!(finish);
        let execution = tokio::select! {
            result = &mut finish => result,
            _ = self.control.cancelled() => {
                cancellation.cancel();
                finish.await
            }
        };
        let observed = execution
            .as_ref()
            .map_err(isolation_error)
            .and_then(|execution| {
                observe_result(
                    execution,
                    self.intent.tool_name == "edit_file"
                        || self.require_complete_capture.load(Ordering::Acquire),
                )
            });
        let settlement = execution
            .as_ref()
            .ok()
            .and_then(SupervisedExecution::settlement);
        let released = match settlement {
            Some(proof) => lease.settle_supervised(proof).map_err(isolation_error),
            None => {
                self.uncertain.store(true, Ordering::Release);
                drop(lease); // Retains physical ownership and its exact unknown binding.
                Err(isolation_error(
                    "repository supervision did not prove process settlement",
                ))
            }
        };
        if let Err(failure) = &released {
            self.uncertain.store(true, Ordering::Release);
            if let Ok(mut state) = self.controller.lock() {
                let _ = state.fail_closed::<()>(Err(error(failure)));
            }
        }
        released?;
        observed
    }
}

/// The kernel write restriction for one repository process. Only the Agent's
/// own shell of a read-only activation runs under it: nothing beneath the
/// repository can change. The host-authored file tools (`read_file`, `grep`,
/// ...) keep their own fixed commands, and the host's repository captures and
/// digest observations, though admitted as `bash`, are exempt so a read-only
/// helper still yields its evidence. A supervisor that cannot apply the
/// restriction refuses to launch that one process.
fn process_write_restriction(
    read_only: bool,
    tool: &str,
    host_observation: bool,
    root: &Path,
) -> Option<axocoatl_exec::protocol::WriteRestriction> {
    (read_only && tool == "bash" && !host_observation).then(|| {
        axocoatl_exec::protocol::WriteRestriction {
            writable: vec![
                "/tmp".into(),
                "/var/tmp".into(),
                "/dev".into(),
                axocoatl_exec::protocol::HOME_PLACEHOLDER.into(),
            ],
            protected: vec![root.to_string_lossy().into_owned()],
        }
    })
}

struct OwnedProcessWait {
    cancellation: SupervisorCancellation,
    task: Option<tokio::task::JoinHandle<std::result::Result<ExecResult, IsolationError>>>,
}
impl OwnedProcessWait {
    async fn finish(mut self) -> std::result::Result<ExecResult, IsolationError> {
        let result = self
            .task
            .as_mut()
            .ok_or_else(|| isolation_error("process wait already consumed"))?
            .await
            .map_err(|failure| {
                isolation_error(format!("owned repository process task failed: {failure}"))
            })?;
        self.task.take();
        result
    }
}
impl Drop for OwnedProcessWait {
    fn drop(&mut self) {
        if self.task.is_some() {
            self.cancellation.cancel();
        }
    }
}

fn observe_result(
    execution: &SupervisedExecution,
    require_complete_capture: bool,
) -> std::result::Result<ExecResult, IsolationError> {
    execution
        .result()
        .validate_for(execution.request())
        .map_err(isolation_error)?;
    let ServerMessage::Finished {
        outcome,
        primary_exit,
        launched,
        stdout,
        stderr,
        ..
    } = execution.result()
    else {
        return Err(isolation_error(
            "repository process has no terminal observation",
        ));
    };
    let stdout_bytes = stdout
        .retained_bytes(execution.request().stdout_bytes)
        .map_err(isolation_error)?;
    let stderr_bytes = stderr
        .retained_bytes(execution.request().stderr_bytes)
        .map_err(isolation_error)?;
    let stdout_truncated = stdout.observed_bytes != stdout_bytes.len() as u64;
    let stderr_truncated = stderr.observed_bytes != stderr_bytes.len() as u64;
    if !stdout.complete
        || !stderr.complete
        || (require_complete_capture && (stdout_truncated || stderr_truncated))
    {
        // A truncated success must never become EditFile's source bytes.
        return Err(isolation_error("repository command output is incomplete or exceeds its capture bound; partial bytes cannot become file input"));
    }
    if let ProcessOutcome::LaunchFailed { message } = outcome {
        if execution.request().write_restriction.is_some()
            && message.starts_with("write restriction unavailable")
        {
            return Err(isolation_error(
                "This Agent is read-only and this runtime cannot enforce it, so bash is \
                 unavailable; use read_file, grep, glob or list_dir instead.",
            ));
        }
    }
    let ProcessOutcome::Exited { code } = outcome else {
        return Err(isolation_error(format!(
            "repository command did not complete normally: {outcome:?}; observed primary_exit={primary_exit:?}, launched={launched}; stdout_bytes={}, stdout_sha256={}, stderr_bytes={}, stderr_sha256={}",
            stdout.observed_bytes, stdout.observed_sha256, stderr.observed_bytes, stderr.observed_sha256,
        )));
    };
    let stdout_text = if require_complete_capture {
        std::str::from_utf8(&stdout_bytes)
            .map_err(|_| {
                isolation_error(
            "repository edit source is not valid UTF-8; no replacement bytes may be written",
        )
            })?
            .to_owned()
    } else {
        axocoatl_isolation::session_sandbox::captured_output_text(
            &stdout_bytes,
            stdout_truncated,
            "stdout",
        )
    };
    Ok(ExecResult {
        stdout: stdout_text,
        stderr: axocoatl_isolation::session_sandbox::captured_output_text(
            &stderr_bytes,
            stderr_truncated,
            "stderr",
        ),
        exit_code: *code,
    })
}

fn isolation_error(message: impl std::fmt::Display) -> IsolationError {
    IsolationError::Io(std::io::Error::other(message.to_string()))
}

struct RepositorySandbox {
    resource: RepositoryActivationResource,
    invocation: Option<Arc<InvocationScope>>,
}
#[async_trait]
impl Sandbox for RepositorySandbox {
    fn root(&self) -> &Path {
        self.resource.owner.root()
    }
    fn execution_identity(&self) -> Option<&str> {
        Some(self.resource.owner.execution_identity())
    }
    async fn exec(
        &self,
        argv: &[&str],
        timeout: Duration,
    ) -> std::result::Result<ExecResult, IsolationError> {
        self.invocation
            .as_ref()
            .ok_or_else(|| {
                isolation_error("repository tools require acknowledged invocation admission")
            })?
            .execute(argv, None, timeout)
            .await
    }
    async fn exec_stdin(
        &self,
        argv: &[&str],
        stdin: &str,
        timeout: Duration,
    ) -> std::result::Result<ExecResult, IsolationError> {
        self.invocation
            .as_ref()
            .ok_or_else(|| {
                isolation_error("repository tools require acknowledged invocation admission")
            })?
            .execute(argv, Some(stdin), timeout)
            .await
    }
    fn spawn_background(&self, _: &str) -> String {
        // This method has no error channel. Its tool is excluded before binding;
        // panic is an unknown backend result, never a fabricated task id.
        panic!("background tool cannot be admitted by the repository foreground port")
    }
    fn spawn_pty(
        &self,
        _: &str,
        _: u16,
        _: u16,
    ) -> std::result::Result<Arc<axocoatl_isolation::pty::PtyTerminal>, String> {
        Err("repository PTY ownership is not integrated".into())
    }
    fn get_terminal(&self, _: &str) -> Option<Arc<axocoatl_isolation::pty::PtyTerminal>> {
        None
    }
    fn kill_terminal(&self, _: &str) -> bool {
        false
    }
    fn list_terminals(&self) -> Vec<(String, String, bool)> {
        self.resource.owner.sandbox().list_terminals()
    }
    fn list_tasks(&self) -> Vec<BgTask> {
        self.resource.owner.sandbox().list_tasks()
    }
    fn with_root(&self, _: &Path) -> Arc<dyn Sandbox> {
        // A caller cannot move this resource to another checkout by supplying a path.
        Arc::new(Self {
            resource: self.resource.clone(),
            invocation: None,
        })
    }
    async fn stop(&self) {}
    async fn stop_checked(&self) -> std::result::Result<(), IsolationError> {
        Err(isolation_error(
            "repository cleanup belongs to the Session lifecycle owner",
        ))
    }
}

#[cfg(test)]
mod write_scope_tests {
    use super::{
        error, process_write_restriction, scoped_relative_path, write_refusal, AdmittedWriteScope,
    };
    use std::path::Path;

    fn scope(scopes: &[&[&str]]) -> AdmittedWriteScope {
        AdmittedWriteScope(
            scopes
                .iter()
                .map(|scope| scope.iter().map(|pattern| (*pattern).to_owned()).collect())
                .collect(),
        )
    }

    #[test]
    fn only_agent_shell_processes_of_read_only_helpers_are_restricted() {
        let root = Path::new("/workspace/repo");
        for (read_only, tool, host_observation, restricted) in [
            (true, "bash", false, true),
            // The host's own captures and digest observations run as bash.
            (true, "bash", true, false),
            // Host-authored file tools keep their fixed commands.
            (true, "read_file", false, false),
            (true, "grep", false, false),
            (true, "glob", false, false),
            (true, "list_dir", false, false),
            // Scoped and unrestricted writers are judged by their captures.
            (false, "bash", false, false),
            (false, "write_file", false, false),
        ] {
            let restriction = process_write_restriction(read_only, tool, host_observation, root);
            assert_eq!(
                restriction.is_some(),
                restricted,
                "{read_only} {tool} {host_observation}"
            );
            if let Some(restriction) = restriction {
                restriction.validate().unwrap();
                assert_eq!(restriction.protected, vec!["/workspace/repo".to_owned()]);
                assert!(restriction.writable.iter().any(|path| path == "/tmp"));
            }
        }
    }

    #[test]
    fn scopes_combine_so_every_scope_must_allow_a_write() {
        let root = Path::new("/workspace/repo");
        assert_eq!(write_refusal(Ok(scope(&[])), root, "anything.js"), None);
        assert_eq!(
            write_refusal(Ok(scope(&[&["lib/"]])), root, "lib/a.js"),
            None
        );
        assert_eq!(
            write_refusal(Ok(scope(&[&["lib/"]])), root, "/workspace/repo/lib/a.js"),
            None
        );
        let outside = write_refusal(Ok(scope(&[&["lib/"]])), root, "src/a.js").unwrap();
        assert_eq!(
            outside,
            "src/a.js is outside the paths this Agent may change (lib/). Leave it unchanged and \
             describe the needed change in your answer."
        );
        // A profile scope and a standing route scope both apply.
        let both = scope(&[&["lib/"], &["lib/a.js"]]);
        assert_eq!(write_refusal(Ok(both.clone()), root, "lib/a.js"), None);
        assert!(write_refusal(Ok(both.clone()), root, "lib/b.js").is_some());
        assert!(!both.is_read_only());
        let read_only = scope(&[&["lib/"], &[]]);
        assert!(read_only.is_read_only());
        assert!(write_refusal(Ok(read_only.clone()), root, "lib/a.js")
            .unwrap()
            .contains("(none; this Agent is read-only)"));
        // A scope that cannot be read refuses every write.
        let unreadable =
            write_refusal(Err(error("authority unavailable")), root, "lib/a.js").unwrap();
        assert!(unreadable.contains("cannot be read"), "{unreadable}");
        assert!(
            unreadable.contains("Leave lib/a.js unchanged"),
            "{unreadable}"
        );
        for message in [outside, unreadable] {
            assert!(!message.contains("signal"), "{message}");
        }
    }

    #[test]
    fn tool_paths_resolve_to_repository_relative_form_or_are_refused() {
        let root = Path::new("/workspace/repo");
        assert_eq!(
            scoped_relative_path(root, "lib/paths.js").as_deref(),
            Some("lib/paths.js")
        );
        assert_eq!(
            scoped_relative_path(root, "./lib//paths.js").as_deref(),
            Some("lib/paths.js")
        );
        assert_eq!(
            scoped_relative_path(root, "/workspace/repo/lib/manifest.js").as_deref(),
            Some("lib/manifest.js")
        );
        assert_eq!(
            scoped_relative_path(root, "lib/x/../paths.js").as_deref(),
            Some("lib/paths.js")
        );
        // Escaping or foreign paths never match an owned pattern.
        assert_eq!(scoped_relative_path(root, "../outside.js"), None);
        assert_eq!(scoped_relative_path(root, "/etc/passwd"), None);
        assert_eq!(scoped_relative_path(root, "."), None);
    }

    #[cfg(unix)]
    #[test]
    fn scoped_writes_never_follow_parent_segments_or_links() {
        use super::unfollowed_write_path;
        let repo = tempfile::tempdir().unwrap();
        let root = repo.path();
        std::fs::create_dir_all(root.join("lib")).unwrap();
        std::fs::create_dir_all(root.join("config")).unwrap();
        std::fs::write(root.join("lib/paths.js"), "").unwrap();
        std::fs::write(root.join("config/prod.js"), "").unwrap();
        std::os::unix::fs::symlink("../config/prod.js", root.join("lib/current.js")).unwrap();
        std::os::unix::fs::symlink("../config", root.join("lib/ext")).unwrap();
        assert_eq!(unfollowed_write_path(root, "lib/paths.js"), None);
        assert_eq!(unfollowed_write_path(root, "lib/new.js"), None);
        assert_eq!(unfollowed_write_path(root, "lib/sub/new.js"), None);
        for escaping in ["lib/current.js", "lib/ext/prod.js", "lib/x/../paths.js"] {
            assert!(
                unfollowed_write_path(root, escaping).is_some(),
                "{escaping} must be refused"
            );
        }
    }
}
