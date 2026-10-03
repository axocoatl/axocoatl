//! Wrap the existing attempt tasks with native activation evidence and limits.
use super::*;
use crate::bootstrap::session_repository::NativeWaysRuntimeFence;
use crate::session_dispatch::{PreparedActivation, SessionDispatchController};

pub(in crate::bootstrap) struct NativeWaysRuntime {
    pub fence: NativeWaysRuntimeFence,
    pub controller: SessionDispatchController,
}
pub(in crate::bootstrap) enum PreparedWay {
    Legacy(ractor::ActorRef<axocoatl_actor::AgentMessage>),
    Native(Box<NativeWayExecution>),
}
pub(in crate::bootstrap) struct NativeWayExecution {
    prepared: PreparedActivation,
    controller: SessionDispatchController,
    activation: ActivationRef,
}
impl NativeWayExecution {
    pub(in crate::bootstrap) fn new(
        prepared: PreparedActivation,
        controller: SessionDispatchController,
        activation: ActivationRef,
    ) -> Self {
        Self {
            prepared,
            controller,
            activation,
        }
    }

    pub(in crate::bootstrap) async fn run(
        self,
        trace: Arc<StdMutex<Vec<crate::trajectory::Action>>>,
    ) -> Result<axocoatl_actor::MeasuredAgentRunOutcome, SessionRunFailure> {
        let outcome = self.prepared.run().await;
        let usage = self
            .controller
            .activation_provider_usage(&self.activation)
            .map(|usage| usage.tokens)
            .unwrap_or(axocoatl_core::MeasuredTokenUsage {
                usage: Default::default(),
                complete: false,
            });
        let failure = |error: DaemonError| SessionRunFailure {
            error,
            token_usage: usage.usage.clone(),
            token_usage_known: usage.complete,
        };
        let result = match outcome {
            Ok(settled) if settled.accepted => Ok(axocoatl_actor::MeasuredAgentRunOutcome {
                outcome: axocoatl_actor::AgentRunOutcome::Completed(axocoatl_core::AgentOutput {
                    content: settled.output.content().output.text.clone(),
                    tool_calls: vec![],
                    token_usage: usage.usage.clone(),
                }),
                token_usage: usage.clone(),
            }),
            Ok(settled) => {
                Err(failure(admission_error(settled.failure.unwrap_or_else(
                    || "Way returned no accepted result".into(),
                ))))
            }
            Err(error) => Err(failure(admission_error(error))),
        };
        let route = self
            .controller
            .native_way_route(&self.activation)
            .map_err(|error| failure(admission_error(error)))?;
        *trace
            .lock()
            .map_err(|_| failure(admission_error("Way route recorder is unavailable")))? = route;
        self.controller
            .settle_native_way_task(
                &self.activation,
                result
                    .as_ref()
                    .err()
                    .map(|error| error.error.to_string())
                    .as_deref(),
            )
            .map_err(|error| failure(admission_error(error)))?;
        // Each Way owns a different isolated repository. A completed model
        // task cannot release the ordinary Session's single-repository lease.
        // The existing attempt owner retains every candidate until explicit
        // Keep/no-Keep joins the tasks and completes exact container cleanup.
        result
    }
}

impl AxocoatlDaemon {
    pub(in crate::bootstrap) async fn prepare_native_ways_execution(
        &self,
        session: &Session,
        set: &crate::git::AttemptSet,
        preparation: NativeWaysPreparation,
        sandboxes: &[Arc<dyn Sandbox>],
        roots: &[SecureDir],
        fence: NativeWaysRuntimeFence,
    ) -> Result<(NativeWaysRuntime, Vec<(crate::git::Variant, PreparedWay)>), DaemonError> {
        if sandboxes.is_empty()
            || sandboxes.len() != set.lanes.len()
            || roots.len() != set.lanes.len()
            || preparation.admission.candidates.len() != set.lanes.len()
            || preparation.inputs.len() != set.lanes.len()
        {
            return Err(admission_error(
                "Actual Ways resources do not cover the exact approved candidate roster",
            ));
        }
        let token = self
            .session_dispatch_lifecycles
            .prepare_first_turn(&session.id)?;
        let mut owners = Vec::new();
        for (index, (sandbox, root)) in sandboxes.iter().zip(roots).enumerate() {
            owners.push(
                self.native_attempt_repository_owner(
                    &token,
                    session,
                    set,
                    index,
                    sandbox.clone(),
                    root.clone(),
                    &fence,
                )
                .await?,
            );
        }
        let (controller, first) = self
            .session_dispatch_lifecycles
            .begin_retained_successor_checked(
                &token,
                owners[0].clone(),
                preparation.spec,
                |_, _, _| Ok(()),
            )?;
        controller
            .attach_stream_bus(self.stream_bus.clone())
            .map_err(admission_error)?;
        let mut references = vec![first];
        for owner in owners.into_iter().skip(1) {
            references.push(
                controller
                    .retain_repository_resource(owner)
                    .map_err(admission_error)?,
            );
        }
        for grant in preparation.grants {
            controller.install_grant(grant).map_err(admission_error)?;
        }
        let root = Self::open_attempt_root_host(&session.working_dir, &session.id, &set.id)?;
        self.persist_native_ways_admission(set, &root, &preparation.admission)?;
        let factory = self.native_session_activation_factory(&controller)?;
        // Like MCP tools and Skills in legacy Ways, web tools reach outside
        // the attempt, so candidates do not get them.
        for tool in crate::session_dispatch_web::withheld_web_tools() {
            controller
                .register_host_invocation_tool(tool)
                .map_err(admission_error)?;
        }
        let mut prepared = Vec::new();
        for (candidate, (captured, repository)) in preparation
            .admission
            .candidates
            .iter()
            .zip(preparation.inputs.iter().zip(&references))
        {
            let snapshot = controller.snapshot().map_err(admission_error)?;
            let node = snapshot
                .contract()
                .graph()
                .unwrap()
                .nodes
                .iter()
                .find(|node| node.node_id == candidate.activation.node_id)
                .ok_or_else(|| admission_error("Way canonical node is missing"))?;
            let input = ActivationInputManifest {
                manifest_id: InputManifestId::new(format!(
                    "ways-input-{}-{}",
                    crate::attempts::set_key(&set.id),
                    candidate.index
                ))
                .map_err(admission_error)?,
                activation: candidate.activation.clone(),
                definition: candidate.definition.clone(),
                conversation_id: node.conversation_id.clone(),
                starting_savepoint: node.starting_savepoint.clone(),
                parents: vec![],
                guidance: captured.guidance.clone(),
                attachments: vec![],
                repository: RepositoryInput::Recorded {
                    snapshot: repository.clone(),
                },
                budget: captured.budget.clone(),
                grant: Some(candidate.grant.clone()),
                revision_context: None,
            };
            controller
                .append_host_event(TurnContractEnvelope {
                    schema_version: TURN_CONTRACT_SCHEMA_VERSION,
                    command_id: CommandId::new(format!(
                        "ways-start-{}-{}",
                        crate::attempts::set_key(&set.id),
                        candidate.index
                    ))
                    .map_err(admission_error)?,
                    expected_revision: snapshot.contract().revision(),
                    session_id: snapshot.owner().session_id.clone(),
                    turn_id: snapshot.turn_id().clone(),
                    event: TurnContractEvent::StartActivation {
                        input: Box::new(input.clone()),
                    },
                })
                .map_err(admission_error)?;
            let resources = factory.resources(&input).await.map_err(admission_error)?;
            let repository = controller
                .repository_activation_resource(repository)
                .map_err(admission_error)?;
            let activation = controller
                .prepare_repository_activation(candidate.activation.clone(), resources, repository)
                .map_err(admission_error)?;
            prepared.push((
                set.lanes[candidate.index].clone(),
                PreparedWay::Native(Box::new(NativeWayExecution::new(
                    activation,
                    controller.clone(),
                    candidate.activation.clone(),
                ))),
            ));
        }
        Ok((NativeWaysRuntime { fence, controller }, prepared))
    }
}
