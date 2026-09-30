//! Actual repository checks run only through an owned first-party supervisor.
//! Descriptive evidence identifies an already-retained resource; it cannot
//! reconstruct ownership or turn a restored intent into permission to replay.
//! The daemon must retain this controller and its repository owners throughout
//! unknown outcomes.

use super::*;
use crate::bootstrap::session_repository::{
    SessionRepositoryExecutionLease, SessionRepositoryOwner,
};
use axocoatl_exec::protocol::{
    CapturedOutput, ExecRequest, PrimaryExit, ProcessOutcome, ServerMessage, PROTOCOL_VERSION,
};
use axocoatl_isolation::supervisor_transport::{
    ProcessSettlement, RunningSupervisedCommand, SupervisedExecution, SupervisorCancellation,
};
use axocoatl_session::execution_content::{
    ConditionOutputCapture, ConditionOutputEvidence, ConditionProcessStatus,
    ConditionSupervisionEvidence, DurableConditionArguments,
};

struct RepositoryCheckObservation {
    status: ConditionProcessStatus,
    stdout: ConditionOutputEvidence,
    stderr: ConditionOutputEvidence,
    supervision: Option<ConditionSupervisionEvidence>,
}

struct SupervisedRunIdentity {
    request: ExecRequest,
    runtime_identity: String,
    program_sha256: String,
    transport_identity: String,
}

/// Dropping the external wait requests Stop. The owned task still retains the
/// controller and repository execution lease until it collects the supervisor
/// result, saves observed evidence, and handles the exact settlement receipt.
pub struct OwnedRepositoryCheck {
    cancellation: SupervisorCancellation,
    task: Option<tokio::task::JoinHandle<Result<SettledRepositoryCheck>>>,
}

/// The concrete child future drops before its final execution ticket. This is
/// also true if the runtime aborts the task while the supervisor is settling.
struct OwnedRepositoryCheckTask {
    future:
        std::pin::Pin<Box<dyn std::future::Future<Output = Result<SettledRepositoryCheck>> + Send>>,
    _execution: super::execution_lifetime::ExecutionTicket,
}

impl OwnedRepositoryCheckTask {
    async fn run(mut self) -> Result<SettledRepositoryCheck> {
        self.future.as_mut().await
    }
}

impl OwnedRepositoryCheck {
    pub fn cancel(&self) {
        self.cancellation.cancel();
    }

    pub async fn finish(mut self) -> Result<SettledRepositoryCheck> {
        // Keep the task in self while awaiting it, so cancellation of this
        // future still wakes the separately owned execution/cleanup task.
        let result = self
            .task
            .as_mut()
            .ok_or_else(|| error("repository check wait already consumed"))?
            .await
            .map_err(|failure| error(format!("owned repository check task was lost: {failure}")))?;
        self.task.take();
        result
    }
}

impl Drop for OwnedRepositoryCheck {
    fn drop(&mut self) {
        if self.task.is_some() {
            self.cancellation.cancel();
        }
    }
}

impl SessionDispatchController {
    /// Retain actual runtime ownership alongside descriptive immutable evidence.
    /// No revision is asserted: this capability is not a checked tree snapshot.
    pub(crate) fn retain_repository_resource(
        &self,
        owner: SessionRepositoryOwner,
    ) -> Result<EvidenceRef> {
        let mut state = self.lock()?;
        state.ready()?;
        require_registration(&state)?;
        validate_owner_identity(&state, &owner)?;
        let expected = repository_description(&owner)?;
        if let Some((reference, _)) = state
            .repository_owners
            .iter()
            .find(|(_, retained)| retained.same_owner(&owner))
        {
            validate_retained_repository(&state, &owner, reference)?;
            return Ok(reference.clone());
        }
        let retained = state
            .content
            .retain_activation_evidence(expected)
            .map(|receipt| receipt.reference().clone())
            .map_err(error);
        let reference = state.fail_closed(retained)?;
        state.repository_owners.insert(reference.clone(), owner);
        Ok(reference)
    }

    /// Explicit host reattachment binds an already reacquired physical owner to
    /// this turn's immutable semantic repository input. Only the ephemeral
    /// execution identity may differ; environment replacement is not recovery.
    pub(crate) fn reattach_repository_resource(
        &self,
        owner: SessionRepositoryOwner,
    ) -> Result<EvidenceRef> {
        let mut state = self.lock()?;
        state.ready()?;
        require_registration(&state)?;
        validate_owner_identity(&state, &owner)?;
        owner.validate_dispatch_resource().map_err(error)?;
        if !state.repository_owners.is_empty() || !owner.execution_is_idle().map_err(error)? {
            return Err(error(
                "recovery requires an unbound controller and an idle acquired repository",
            ));
        }
        let snapshot = state.canonical.snapshot(&state.turn_id).map_err(error)?;
        let mut original = None;
        for activation in snapshot.contract().activations() {
            if let RepositoryInput::Recorded {
                snapshot: reference,
            } = &activation.input.repository
            {
                if original
                    .as_ref()
                    .is_some_and(|previous| previous != reference)
                {
                    return Err(error(
                        "recovery cannot substitute one owner for multiple repository inputs",
                    ));
                }
                original = Some(reference.clone());
            }
        }
        let Some(original) = original else {
            drop(state);
            return self.retain_repository_resource(owner);
        };
        let original_description = state
            .content
            .resolve_activation_evidence(&original)
            .map_err(error)?
            .clone();
        let acquired_description = repository_description(&owner)?;
        validate_reattachment_descriptions(&original_description, &acquired_description)?;
        if original_description != acquired_description {
            let result = (|| {
                let acquired = state
                    .content
                    .retain_activation_evidence(acquired_description)
                    .map_err(error)?
                    .reference()
                    .clone();
                state
                    .content
                    .retain_repository_reattachment(
                        &snapshot,
                        axocoatl_session::execution_content::RepositoryReattachment {
                            turn_id: snapshot.turn_id().clone(),
                            original: original.clone(),
                            acquired,
                        },
                    )
                    .map_err(error)
            })();
            let proof = state.fail_closed(result)?;
            state
                .repository_reattachments
                .insert(original.clone(), proof);
        }
        // No old lease is restored. Every invocation revalidates this exact new
        // owner and obtains a fresh supervisor-bound execution lease from it.
        state.repository_owners.insert(original.clone(), owner);
        Ok(original)
    }

    /// A trusted host port, not an RPC that accepts a path or a readiness flag.
    /// Its owner must already be retained by this exact canonical controller.
    pub(crate) async fn start_repository_check(
        &self,
        owner: SessionRepositoryOwner,
        run: ConditionRunRef,
        repository: EvidenceRef,
        grant: GrantSnapshotRef,
    ) -> Result<OwnedRepositoryCheck> {
        let execution = {
            let state = self.lock()?;
            state.execution_admission()?;
            validate_retained_repository(&state, &owner, &repository)?;
            state.acquire_execution_ticket(self)?
        };
        let mut lease = tokio::select! {
            biased;
            _ = self.wait_for_turn_stop() => return Err(error("whole-turn Stop cancelled repository check preparation")),
            lease = owner.execution_lease() => lease.map_err(error)?,
        };
        let permit =
            self.prepare_repository_check(run, repository.clone(), grant, owner.backend())?;
        let request = supervisor_request(permit.arguments())?;
        // Prepare can start the supervisor itself but never the repository
        // command. Dropped/error preparation therefore has a positive no-job
        // boundary while the durable permit records nondispatch on Drop.
        let command = tokio::select! {
            biased;
            _ = self.wait_for_turn_stop() => return Err(error("whole-turn Stop cancelled supervisor preparation")),
            command = lease.sandbox().prepare_supervised_command(request.clone()) => command.map_err(error)?,
        };
        if command.request() != &request {
            return Err(error(
                "prepared supervisor changed the durably admitted repository command",
            ));
        }
        let identity = SupervisedRunIdentity {
            request,
            runtime_identity: command.runtime_identity().to_owned(),
            program_sha256: command.program_sha256().to_owned(),
            transport_identity: command.transport_identity().to_owned(),
        };
        lease.bind_supervised_command(&command).map_err(error)?;
        {
            let state = self.lock()?;
            state.ready()?;
            validate_retained_repository(&state, &owner, &repository)?;
        }
        let cancellation = command.cancellation();
        let (in_flight, running) = permit.dispatch_supervised(&mut lease, command)?;
        // No await between the synchronous admission/handoff and spawning its
        // owner. Cancellation of the external caller cannot abandon this join.
        let controller = self.clone();
        let owned = OwnedRepositoryCheckTask {
            future: Box::pin(finish_owned_check(
                controller, in_flight, lease, running, identity,
            )),
            _execution: execution,
        };
        let task = tokio::spawn(owned.run());
        Ok(OwnedRepositoryCheck {
            cancellation,
            task: Some(task),
        })
    }
}

fn validate_owner_identity(state: &DispatchState, owner: &SessionRepositoryOwner) -> Result<()> {
    require_registration(state)?;
    if state.canonical.identity().map_err(error)? != *owner.identity()
        || owner.metadata().workspace_id != owner.identity().owner().workspace_id
        || owner.metadata().session_id != owner.identity().owner().session_id.as_str()
        || owner.metadata().execution_identity != owner.execution_identity()
        || owner.metadata().runtime_root != owner.root()
    {
        return Err(error(
            "repository owner differs from the retained canonical Session and resource",
        ));
    }
    Ok(())
}

impl SessionDispatchController {
    pub(crate) fn install_repository_registration(
        &self,
        gate: &Arc<crate::bootstrap::session_dispatch::RepositoryRegistrationGate>,
    ) -> Result<()> {
        let mut state = self.lock()?;
        state.ready()?;
        if state.repository_registration.is_some()
            || !state.repository_owners.is_empty()
            || !gate.permits(&state.canonical.identity().map_err(error)?)
        {
            return Err(error(
                "controller already has a repository registration or belongs to another owner",
            ));
        }
        state.repository_registration = Some(Arc::downgrade(gate));
        Ok(())
    }
}

fn require_registration(state: &DispatchState) -> Result<()> {
    if !state
        .repository_registration
        .as_ref()
        .and_then(std::sync::Weak::upgrade)
        .is_some_and(|gate| {
            state
                .canonical
                .identity()
                .is_ok_and(|identity| gate.permits(&identity))
        })
    {
        return Err(error(
            "repository dispatch requires its retained daemon lifecycle registration",
        ));
    }
    Ok(())
}

fn repository_description(owner: &SessionRepositoryOwner) -> Result<ActivationEvidenceContent> {
    Ok(ActivationEvidenceContent::Repository {
        description: serde_json::to_string(owner.metadata()).map_err(error)?,
        revision: None,
    })
}

pub(super) fn validate_retained_repository(
    state: &DispatchState,
    owner: &SessionRepositoryOwner,
    repository: &EvidenceRef,
) -> Result<()> {
    validate_owner_identity(state, owner)?;
    let retained = state
        .repository_owners
        .get(repository)
        .ok_or_else(|| error("repository evidence has no retained live resource owner"))?;
    if !retained.same_owner(owner) {
        return Err(error(
            "repository evidence belongs to a different physical owner",
        ));
    }
    let original = state
        .content
        .resolve_activation_evidence(repository)
        .map_err(error)?;
    let current = repository_description(owner)?;
    if original != &current {
        let proof_ref = state
            .repository_reattachments
            .get(repository)
            .ok_or_else(|| {
                error(
                    "repository execution identity changed without an explicit reattachment proof",
                )
            })?;
        let proof = state
            .content
            .repository_reattachment(proof_ref)
            .map_err(error)?;
        if &proof.original != repository
            || state
                .content
                .resolve_activation_evidence(&proof.acquired)
                .map_err(error)?
                != &current
        {
            return Err(error(
                "repository reattachment does not name this exact acquired owner",
            ));
        }
        validate_reattachment_descriptions(original, &current)?;
    }
    Ok(())
}

fn validate_reattachment_descriptions(
    original: &ActivationEvidenceContent,
    acquired: &ActivationEvidenceContent,
) -> Result<()> {
    let decode = |value: &ActivationEvidenceContent| -> Result<crate::bootstrap::session_repository::SessionRepositoryMetadata> {
        let ActivationEvidenceContent::Repository { description, revision: None } = value else {
            return Err(error("repository reattachment requires retained runtime descriptors"));
        };
        serde_json::from_str(description).map_err(error)
    };
    let mut previous = decode(original)?;
    let current = decode(acquired)?;
    if previous.execution_identity.is_empty() || current.execution_identity.is_empty() {
        return Err(error(
            "repository reattachment has no actual execution identity",
        ));
    }
    previous
        .execution_identity
        .clone_from(&current.execution_identity);
    if previous != current {
        return Err(error("repository reattachment changed its durable Session, Workspace, environment, backend, runtime, path, or inode"));
    }
    Ok(())
}

fn supervisor_request(arguments: &DurableConditionArguments) -> Result<ExecRequest> {
    let definition = arguments.definition();
    let request = ExecRequest {
        protocol: PROTOCOL_VERSION,
        stdin: None,
        invocation_id: arguments.run().run_id.as_str().to_owned(),
        argv: definition.argv.clone(),
        timeout_ms: definition.timeout_ms,
        stdout_bytes: definition.stdout_bytes,
        stderr_bytes: definition.stderr_bytes,
        write_restriction: None,
    };
    request.validate().map_err(error)?;
    Ok(request)
}

async fn finish_owned_check(
    controller: SessionDispatchController,
    in_flight: conditions::InFlightRepositoryCheck,
    lease: SessionRepositoryExecutionLease,
    running: RunningSupervisedCommand,
    identity: SupervisedRunIdentity,
) -> Result<SettledRepositoryCheck> {
    let execution = running.finish().await;
    let observation = match &execution {
        Ok(execution) => observe_execution(execution, &identity),
        Err(failure) => uncertain_observation(
            &identity.request,
            format!("Repository supervisor transport failed: {failure}"),
        ),
    };
    let persisted = match observation {
        Ok(observation) => match now_ms() {
            Ok(recorded_at) => in_flight.settle_observation(
                observation.status,
                observation.stdout,
                observation.stderr,
                recorded_at,
                observation.supervision,
            ),
            Err(failure) => Err(failure),
        },
        Err(failure) => Err(failure),
    };
    // Persistence and process ownership are independent. Even failed storage
    // cannot invalidate an exact proof that these processes have stopped. A
    // persistence error poisons the controller before any future admission.
    if let Err(failure) = &persisted {
        if let Ok(mut state) = controller.lock() {
            let _: Result<()> = state.fail_closed(Err(error(failure)));
        }
        // A poisoned mutex also refuses admission; it cannot prevent release
        // of an independently validated process settlement below.
    }
    let released = match execution
        .as_ref()
        .ok()
        .and_then(SupervisedExecution::settlement)
    {
        Some(settlement) => lease.settle_supervised(settlement).map_err(error),
        None => {
            drop(lease);
            Ok(())
        }
    };
    let result = match (persisted, released) {
        (Ok(settled), Ok(())) => Ok(settled),
        (Err(failure), Ok(())) | (Ok(_), Err(failure)) => Err(failure),
        (Err(persist), Err(release)) => Err(error(format!(
            "{persist}; repository settlement: {release}"
        ))),
    };
    let mut state = controller.lock()?;
    state.changed.notify_waiters();
    state.fail_closed(result)
}

fn observe_execution(
    execution: &SupervisedExecution,
    identity: &SupervisedRunIdentity,
) -> Result<RepositoryCheckObservation> {
    if execution.request() != &identity.request
        || execution.runtime_identity() != identity.runtime_identity
        || execution.program_sha256() != identity.program_sha256
    {
        return Err(error(
            "supervised execution differs from its actual prepared resource",
        ));
    }
    observation_from_message(execution.result(), identity, execution.settlement())
}

fn observation_from_message(
    message: &ServerMessage,
    identity: &SupervisedRunIdentity,
    settlement: Option<&ProcessSettlement>,
) -> Result<RepositoryCheckObservation> {
    message.validate_for(&identity.request).map_err(error)?;
    let ServerMessage::Finished {
        outcome,
        primary_exit,
        launched,
        stdout,
        stderr,
        ..
    } = message
    else {
        return Err(error(
            "repository check has no terminal supervisor observation",
        ));
    };
    if let Some(proof) = settlement {
        if proof.invocation_id() != identity.request.invocation_id
            || proof.request_sha256() != identity.request.digest().map_err(error)?
            || proof.runtime_identity() != identity.runtime_identity
            || proof.program_sha256() != identity.program_sha256
            || proof.transport_identity() != identity.transport_identity
        {
            return Err(error(
                "repository settlement belongs to another prepared supervisor",
            ));
        }
    }
    // A JSON quiescent flag alone never becomes an authoritative observation.
    let quiescent = settlement.is_some();
    let status = match outcome {
        ProcessOutcome::Exited { code } => ConditionProcessStatus::Exited { code: *code },
        ProcessOutcome::Signalled { signal } => {
            ConditionProcessStatus::Signalled { signal: *signal }
        }
        ProcessOutcome::TimedOut | ProcessOutcome::Cancelled if !launched && quiescent => {
            ConditionProcessStatus::NotDispatched
        }
        ProcessOutcome::TimedOut => ConditionProcessStatus::TimedOut,
        ProcessOutcome::Cancelled => ConditionProcessStatus::Interrupted,
        ProcessOutcome::LaunchFailed { message } => ConditionProcessStatus::LaunchFailed {
            message: bounded_transport_message(message.clone()),
        },
        ProcessOutcome::Failed { message } => ConditionProcessStatus::Uncertain {
            message: bounded_transport_message(message.clone()),
        },
    };
    Ok(RepositoryCheckObservation {
        status,
        stdout: output_evidence(stdout, identity.request.stdout_bytes)?,
        stderr: output_evidence(stderr, identity.request.stderr_bytes)?,
        supervision: Some(ConditionSupervisionEvidence {
            invocation_id: identity.request.invocation_id.clone(),
            request_sha256: identity.request.digest().map_err(error)?,
            runtime_identity: identity.runtime_identity.clone(),
            program_sha256: identity.program_sha256.clone(),
            transport_identity: identity.transport_identity.clone(),
            launched: *launched,
            quiescent,
            primary_exit: primary_exit.map(|exit| match exit {
                PrimaryExit::Exited { code } => ConditionProcessStatus::Exited { code },
                PrimaryExit::Signalled { signal } => ConditionProcessStatus::Signalled { signal },
            }),
        }),
    })
}

fn output_evidence(output: &CapturedOutput, capacity: usize) -> Result<ConditionOutputEvidence> {
    ConditionOutputEvidence::from_observed_transport(
        &output.retained_bytes(capacity).map_err(error)?,
        output.observed_bytes,
        output.observed_sha256.clone(),
        output.complete,
    )
    .map_err(error)
}

fn uncertain_observation(
    request: &ExecRequest,
    failure: String,
) -> Result<RepositoryCheckObservation> {
    Ok(RepositoryCheckObservation {
        status: ConditionProcessStatus::Uncertain {
            message: bounded_transport_message(failure),
        },
        stdout: ConditionOutputCapture::new(request.stdout_bytes)
            .map_err(error)?
            .finish(false),
        stderr: ConditionOutputCapture::new(request.stderr_bytes)
            .map_err(error)?
            .finish(false),
        supervision: None,
    })
}

fn bounded_transport_message(mut message: String) -> String {
    if message.is_empty() {
        message.push_str("Supervisor result was uncertain");
    }
    let mut end = message.len().min(512);
    while !message.is_char_boundary(end) {
        end -= 1;
    }
    message.truncate(end);
    message
}

#[cfg(test)]
#[path = "session_dispatch_repository_tests.rs"]
mod tests;

#[cfg(test)]
mod reattachment_tests {
    use super::*;
    #[test]
    fn reattachment_accepts_only_a_changed_execution_incarnation() {
        let metadata = serde_json::json!({"workspace_id":"workspace", "session_id":"session", "environment_generation":2,
            "backend":"podman", "runtime_id":"session", "execution_identity":"old-owner", "runtime_root":"/repo", "host_workspace_inode":"1:2"});
        let description = |value: &serde_json::Value| ActivationEvidenceContent::Repository {
            description: value.to_string(),
            revision: None,
        };
        let original = description(&metadata);
        let mut next = metadata.clone();
        next["execution_identity"] = "new-owner".into();
        assert!(validate_reattachment_descriptions(&original, &description(&next)).is_ok());
        for (field, value) in [
            ("workspace_id", serde_json::json!("other")),
            ("session_id", serde_json::json!("other")),
            ("environment_generation", serde_json::json!(3)),
            ("backend", serde_json::json!("e2b")),
            ("runtime_id", serde_json::json!("other")),
            ("runtime_root", serde_json::json!("/other")),
            ("host_workspace_inode", serde_json::json!("1:3")),
        ] {
            let mut foreign = next.clone();
            foreign[field] = value;
            assert!(
                validate_reattachment_descriptions(&original, &description(&foreign)).is_err(),
                "{field}"
            );
        }
        next["execution_identity"] = "".into();
        assert!(validate_reattachment_descriptions(&original, &description(&next)).is_err());
    }
}
