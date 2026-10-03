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
use axocoatl_isolation::egress::{GrantKind, GrantSpec, ProcessEnv};
use axocoatl_isolation::supervisor_transport::{
    RunningSupervisedCommand, SupervisedExecution, SupervisorCancellation,
};
use axocoatl_isolation::{BgTask, ExecResult, IsolationError, Sandbox};
use axocoatl_session::control_authority::REPOSITORY_CAPTURE_PORT;
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
/// `workspace_knowledge` is the host's own port, offered only when listed.
/// Host invocation tools (`web_search`, `web_fetch`, `browser`,
/// `browser_check`) are accepted here by name; whether one is available is its
/// registered tool's decision.
pub(crate) fn validate_repository_tools(tools: &[String]) -> Result<()> {
    if let Some(tool) = tools.iter().find(|tool| {
        !SUPPORTED_TOOLS.contains(&tool.as_str())
            && tool.as_str() != super::knowledge::NAME
            && !super::host_tools::is_host_invocation_tool(tool)
    }) {
        return Err(error(format!(
            "native Session repository tool '{tool}' has no owned foreground implementation; supported tools: {}, {}, {}. Background and PTY ownership is not integrated",
            SUPPORTED_TOOLS.join(", "),
            super::knowledge::NAME,
            super::host_tools::HOST_INVOCATION_TOOLS.join(", ")
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

/// The path patterns an activation was admitted to change. An empty list
/// makes the activation read-only; no list leaves every path open.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct AdmittedWriteScope(Option<Vec<String>>);

impl AdmittedWriteScope {
    pub(super) fn is_unrestricted(&self) -> bool {
        self.0.is_none()
    }
    pub(super) fn is_read_only(&self) -> bool {
        self.0.as_ref().is_some_and(Vec::is_empty)
    }
    pub(super) fn allows(&self, path: &str) -> bool {
        axocoatl_session::path_scope::scope_allows(self.0.as_deref(), path)
    }
    /// Whether a change to `path` may stand. An ignore file decides which
    /// files the captures judge, so one may change only inside a directory
    /// every scope names whole (such as `lib/`): then all it could hide is
    /// in scope too.
    pub(super) fn allows_change(&self, path: &str) -> bool {
        self.allows(path) && (!is_ignore_file(path) || self.owns_ignore_file(path))
    }
    fn owns_ignore_file(&self, path: &str) -> bool {
        let Some((directory, _)) = path.rsplit_once('/') else {
            return false;
        };
        self.0.iter().all(|scope| {
            scope.iter().any(|pattern| {
                pattern.strip_suffix('/').is_some_and(|owned| {
                    !owned.contains(['*', '?'])
                        && (directory == owned || directory.starts_with(&format!("{owned}/")))
                })
            })
        })
    }
    /// How the scope reads in messages to the Agent and the person.
    pub(super) fn describe(&self) -> String {
        match &self.0 {
            Some(scope) if !scope.is_empty() => scope.join(", "),
            Some(_) => "none; this Agent is read-only".to_owned(),
            None => "any file".to_owned(),
        }
    }
}

impl DispatchState {
    /// The write scope an activation was admitted with, read from its durable
    /// authority record and never from a live copy. A caller that cannot read
    /// the scope must refuse the write or process it was checking.
    pub(super) fn admitted_write_scope(
        &self,
        activation: &ActivationRef,
    ) -> Result<AdmittedWriteScope> {
        let profile = self
            .authority
            .activation_profile(activation)
            .map_err(error)?;
        Ok(AdmittedWriteScope(profile.write_scope))
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

    /// The host directory holding this checkout's files.
    pub(super) fn host_checkout(&self) -> axocoatl_core::SecureDir {
        self.owner.host_checkout().clone()
    }

    /// Whether this checkout is a Ways attempt lane's own clone.
    pub(super) fn is_attempt(&self) -> bool {
        self.owner.is_attempt()
    }

    pub(super) fn description(&self) -> Result<ActivationEvidenceContent> {
        // The semantic input remains its immutable original description. The
        // separately retained reattachment proof joins it to this live owner.
        Ok(self.description.clone())
    }

    pub(super) fn preview_tools(
        &self,
        profile: &ExecutionProfile,
        host_tools: Vec<(&'static str, Arc<dyn BuiltinTool>)>,
    ) -> Result<Arc<ToolExecutor>> {
        validate_repository_tools(&profile.tools)?;
        // Definitions come from the actual built-ins. This executor cannot run:
        // acknowledged admission replaces it with an exact invocation executor.
        let mut executor = ToolExecutor::new();
        axocoatl_tools::register_session_tools(
            &mut executor,
            Arc::new(RepositorySandbox {
                resource: self.clone(),
                invocation: None,
            }),
        );
        // Host tools the profile lists, as descriptions only; admission binds
        // the real tool to the admitted invocation.
        for (name, definition) in host_tools {
            executor.register_builtin(name, definition);
        }
        Ok(Arc::new(executor))
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
        if intent.tool_name == super::delegate::NAME
            || intent.tool_name == super::knowledge::NAME
            || super::host_tools::is_host_invocation_tool(&intent.tool_name)
        {
            return Ok(None);
        }
        let Some(resource) = &bound.repository else {
            return Ok(None);
        };
        let capture_port = intent.tool_name == REPOSITORY_CAPTURE_PORT;
        if !SUPPORTED_TOOLS.contains(&intent.tool_name.as_str()) && !capture_port {
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
        if capture_port {
            // The port runs only the host's fixed capture and offers no tool.
            return Ok(Some(Self {
                scope,
                executor: Arc::new(ToolExecutor::new()),
            }));
        }
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
        if !matches!(
            self.scope.intent.tool_name.as_str(),
            "bash" | REPOSITORY_CAPTURE_PORT
        ) || super::repository_snapshot::capture_command_mode(command).is_none()
        {
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

/// Whether `path` names a `.gitignore` file.
fn is_ignore_file(path: &str) -> bool {
    path.rsplit('/').next() == Some(".gitignore")
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
        .is_some_and(|relative| scope.allows_change(relative))
    {
        return None;
    }
    if relative
        .as_deref()
        .is_some_and(|relative| scope.allows(relative) && is_ignore_file(relative))
    {
        return Some(format!(
            "{path} decides which files are checked for changes, so this Agent may change it \
             only inside a directory it may change whole ({}). Leave it unchanged and describe \
             the needed change in your answer.",
            scope.describe()
        ));
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
        let (write_restriction, writer, agent) = {
            let state = self.controller.lock().map_err(isolation_error)?;
            self.validate(&state).map_err(isolation_error)?;
            let read_only = state
                .admitted_write_scope(&self.intent.activation)
                .map_err(isolation_error)?
                .is_read_only();
            let agent = state
                .current(&self.intent.activation)
                .ok()
                .and_then(|snapshot| {
                    snapshot
                        .contract()
                        .activations()
                        .iter()
                        .find(|item| item.activation == self.intent.activation)
                        .map(|item| item.input.definition.definition_id.as_str().to_string())
                });
            (
                process_write_restriction(
                    read_only,
                    &self.intent.tool_name,
                    self.require_complete_capture.load(Ordering::Acquire),
                    self.resource.owner.root(),
                ),
                !read_only,
                agent,
            )
        };
        let index = self
            .process_index
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |value| {
                value.checked_add(1)
            })
            .map_err(|_| isolation_error("repository process index exhausted"))?;
        let argv = if write_restriction.is_some() {
            with_scratch_home(argv)
        } else {
            argv.iter().map(|arg| (*arg).to_owned()).collect()
        };
        let request = ExecRequest {
            protocol: PROTOCOL_VERSION,
            invocation_id: format!("{}:{index}", self.intent.invocation_id.as_str()),
            argv,
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
        // Under network: egress only a writer's shell gets a credential, for
        // exactly this process. Read-only helpers, the host's captures and
        // digest observations, and the other repository tools (fixed
        // commands that never need the network) get none.
        let credentialed = writer
            && self.intent.tool_name == "bash"
            && !self.require_complete_capture.load(Ordering::Acquire);
        let grant = match lease.sandbox().egress_authority() {
            Some(authority) if credentialed => {
                let mut spec = GrantSpec::new(GrantKind::Agent);
                spec.invocation_id = Some(self.intent.invocation_id.as_str().to_string());
                spec.activation_id =
                    Some(self.intent.activation.activation_id.as_str().to_string());
                spec.node_id = Some(self.intent.activation.node_id.as_str().to_string());
                spec.agent = agent;
                spec.process = Some(request.invocation_id.clone());
                match authority.grant(spec).await {
                    Ok(grant) => Some(grant),
                    Err(failure) => {
                        // The process still runs; without a credential the
                        // proxy refuses every connection it attempts.
                        tracing::warn!(
                            invocation = %self.intent.invocation_id.as_str(),
                            error = %failure,
                            "no egress credential for this process"
                        );
                        None
                    }
                }
            }
            _ => None,
        };
        let command = lease
            .sandbox()
            .prepare_supervised_command_with_env(
                request.clone(),
                stdin.map(|bytes| bytes.as_bytes().to_vec()),
                ProcessEnv {
                    env_file: grant.as_ref().and_then(|grant| grant.env_file.as_deref()),
                },
            )
            .await?;
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
            // The credential ends when its process is settled.
            let _grant = grant;
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

/// The kernel restriction for one repository process. Only the Agent's own
/// shell of a read-only activation runs under it: nothing beneath the
/// repository can change, and neither can the Session's shared home
/// directory, whose configuration later processes read, and the shell can
/// neither connect nor bind a TCP socket, loopback included. The
/// host-authored file tools (`read_file`, `grep`, ...) keep their own fixed
/// commands, and the host's repository captures and digest observations,
/// though admitted as `bash`, are exempt so a read-only helper still yields
/// its evidence. A supervisor that cannot apply all of it refuses to launch
/// that one process.
fn process_write_restriction(
    read_only: bool,
    tool: &str,
    host_observation: bool,
    root: &Path,
) -> Option<axocoatl_exec::protocol::WriteRestriction> {
    (read_only && tool == "bash" && !host_observation).then(|| {
        axocoatl_exec::protocol::WriteRestriction {
            writable: vec!["/tmp".into(), "/var/tmp".into(), "/dev".into()],
            protected: vec![root.to_string_lossy().into_owned()],
            deny_network: true,
        }
    })
}

/// Whether a supervisor refused to launch a restricted process because its
/// kernel cannot apply the whole restriction. The read-only Agent then has no
/// shell on this runtime, and its file tools still work.
fn restriction_unavailable(request: &ExecRequest, outcome: &ProcessOutcome) -> bool {
    matches!(outcome, ProcessOutcome::LaunchFailed { message }
        if request.write_restriction.is_some()
            && message.starts_with("write restriction unavailable"))
}

/// Runs `"$@"` with a fresh home directory of its own under `/tmp`, removed
/// when it ends, in place of the Session's shared one.
const SCRATCH_HOME: &str = "home=$(mktemp -d /tmp/axocoatl-home.XXXXXX) || exit 125
HOME=$home
XDG_CONFIG_HOME=$home/.config
XDG_CACHE_HOME=$home/.cache
XDG_DATA_HOME=$home/.local/share
XDG_STATE_HOME=$home/.local/state
export HOME XDG_CONFIG_HOME XDG_CACHE_HOME XDG_DATA_HOME XDG_STATE_HOME
\"$@\"
status=$?
rm -rf -- \"$home\"
exit \"$status\"
";

/// A restricted shell's argv, run with its own scratch home directory: a
/// read-only helper can still write configuration or caches for itself, but
/// nothing it writes there reaches any other process.
fn with_scratch_home(argv: &[&str]) -> Vec<String> {
    ["sh", "-c", SCRATCH_HOME, "sh"]
        .iter()
        .chain(argv)
        .map(|arg| (*arg).to_owned())
        .collect()
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
    if restriction_unavailable(execution.request(), outcome) {
        return Err(isolation_error(
            "This Agent is read-only and this runtime cannot enforce it, so bash is \
             unavailable; use read_file, grep, glob or list_dir instead.",
        ));
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

    fn scope(scope: Option<&[&str]>) -> AdmittedWriteScope {
        AdmittedWriteScope(
            scope.map(|scope| scope.iter().map(|pattern| (*pattern).to_owned()).collect()),
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
                // Nor can the shell open or accept a TCP connection.
                assert!(restriction.deny_network);
                // The shared home directory is never writable to a helper.
                assert_eq!(
                    restriction.effective_writable(Some("/home/agent")),
                    ["/tmp", "/var/tmp", "/dev"]
                );
            }
        }
    }

    /// A supervisor whose kernel cannot apply the whole restriction (Landlock
    /// below ABI 3 for writes, below ABI 4 for TCP) refuses to launch the
    /// shell, and the read-only Agent is told to use its file tools. Any
    /// other launch failure, or one of an unrestricted process, stays one.
    #[test]
    fn an_unavailable_restriction_leaves_a_read_only_agent_without_a_shell() {
        use super::restriction_unavailable;
        use axocoatl_exec::protocol::{ExecRequest, ProcessOutcome, PROTOCOL_VERSION};
        let restricted = ExecRequest {
            protocol: PROTOCOL_VERSION,
            invocation_id: "read-only:1".into(),
            argv: vec!["sh".into(), "-c".into(), "curl example.com".into()],
            stdin: None,
            timeout_ms: 1000,
            stdout_bytes: 16,
            stderr_bytes: 16,
            write_restriction: process_write_restriction(
                true,
                "bash",
                false,
                Path::new("/workspace/repo"),
            ),
        };
        let unrestricted = ExecRequest {
            write_restriction: None,
            ..restricted.clone()
        };
        let refused = |message: &str| ProcessOutcome::LaunchFailed {
            message: message.into(),
        };
        for kernel in [
            "write restriction unavailable: Landlock ABI 2 cannot refuse truncating files; \
             version 3 (Linux 6.2) or later is required",
            "write restriction unavailable: Landlock ABI 3 cannot refuse TCP connections; \
             version 4 (Linux 6.7) or later is required",
            "write restriction unavailable: Landlock is not available: Function not implemented",
        ] {
            assert!(
                restriction_unavailable(&restricted, &refused(kernel)),
                "{kernel}"
            );
            assert!(!restriction_unavailable(&unrestricted, &refused(kernel)));
        }
        assert!(!restriction_unavailable(
            &restricted,
            &refused("No such file or directory (os error 2)")
        ));
        assert!(!restriction_unavailable(
            &restricted,
            &ProcessOutcome::Exited { code: 0 }
        ));
    }

    /// A restricted shell runs with a fresh home directory under /tmp, never
    /// the Session's shared one, and the directory is gone when it ends.
    #[cfg(unix)]
    #[test]
    fn a_restricted_shell_gets_its_own_scratch_home() {
        use super::with_scratch_home;
        let argv = with_scratch_home(&[
            "sh",
            "-c",
            "printf '%s' \"$HOME\"; printf x > \"$HOME/.gitconfig\"; exit 3",
        ]);
        let output = std::process::Command::new(&argv[0])
            .args(&argv[1..])
            .env("HOME", "/nonexistent-shared-home")
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(3));
        let home = String::from_utf8(output.stdout).unwrap();
        assert!(home.starts_with("/tmp/axocoatl-home."), "{home}");
        assert!(!std::path::Path::new(&home).exists(), "{home}");
    }

    #[test]
    fn write_scope_refuses_paths_outside_it() {
        let root = Path::new("/workspace/repo");
        assert_eq!(write_refusal(Ok(scope(None)), root, "anything.js"), None);
        assert_eq!(
            write_refusal(Ok(scope(Some(&["lib/"]))), root, "lib/a.js"),
            None
        );
        assert_eq!(
            write_refusal(Ok(scope(Some(&["lib/"]))), root, "/workspace/repo/lib/a.js"),
            None
        );
        let outside = write_refusal(Ok(scope(Some(&["lib/"]))), root, "src/a.js").unwrap();
        assert_eq!(
            outside,
            "src/a.js is outside the paths this Agent may change (lib/). Leave it unchanged and \
             describe the needed change in your answer."
        );
        let several = scope(Some(&["lib/", "*.md"]));
        assert_eq!(write_refusal(Ok(several.clone()), root, "docs/a.md"), None);
        assert!(write_refusal(Ok(several.clone()), root, "src/b.js")
            .unwrap()
            .contains("(lib/, *.md)"));
        assert!(!several.is_read_only());
        let read_only = scope(Some(&[]));
        assert!(read_only.is_read_only());
        assert!(!read_only.is_unrestricted());
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
    }

    /// An ignore file may change only inside a directory the scope names
    /// whole, since it decides what the captures see.
    #[test]
    fn ignore_files_change_only_inside_an_owned_directory() {
        let root = Path::new("/workspace/repo");
        let owned = scope(Some(&["lib/", "docs/*.md", ".gitignore", "src/**/"]));
        assert!(owned.allows_change("lib/.gitignore"));
        assert!(owned.allows_change("lib/deep/.gitignore"));
        assert!(owned.allows_change("lib/a.js"));
        for refused in [
            ".gitignore",
            "docs/.gitignore",
            "src/x/.gitignore",
            "library/.gitignore",
        ] {
            assert!(!owned.allows_change(refused), "{refused}");
        }
        let message = write_refusal(Ok(owned.clone()), root, ".gitignore").unwrap();
        assert!(
            message.contains("decides which files are checked"),
            "{message}"
        );
        assert_eq!(write_refusal(Ok(owned), root, "lib/.gitignore"), None);
        // A file pattern does not own its directory's ignore file.
        let file = scope(Some(&["lib/a.js"]));
        assert!(!file.allows_change("lib/.gitignore"));
        assert!(file.allows_change("lib/a.js"));
    }

    /// No pattern opens Git's own directory to a file tool, although a
    /// bare name or wildcard matches a path at any depth.
    #[test]
    fn file_tools_never_write_into_a_git_directory() {
        let root = Path::new("/workspace/repo");
        let names = scope(Some(&["config", "HEAD", "pre-commit", "exclude", "**"]));
        for inside in [
            ".git/config",
            ".git/HEAD",
            ".git/hooks/pre-commit",
            "/workspace/repo/.git/info/exclude",
            ".GIT/config",
            "lib/../.git/config",
        ] {
            let refused = write_refusal(Ok(names.clone()), root, inside).unwrap();
            assert!(refused.contains(inside), "{refused}");
        }
        assert_eq!(write_refusal(Ok(names.clone()), root, "lib/config"), None);
        // An Agent without a write scope is not restricted by this rule.
        assert_eq!(write_refusal(Ok(scope(None)), root, ".git/config"), None);
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
