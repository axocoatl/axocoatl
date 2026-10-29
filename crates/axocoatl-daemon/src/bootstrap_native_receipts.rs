//! Signed receipts for ordinary dashboard Session turns. Construction uses
//! retained terminal evidence only. In particular, export never reads today's
//! working tree to invent a historical file hash or isolation policy.
use super::*;
use crate::session_control_plane::{EvidenceValue, SessionTurnControlPlane};
use axocoatl_session::execution_content::{ContentResolution, ExecutionTurnView};
use axocoatl_session::receipt::NativeTurnFacts;
use axocoatl_session::run_outcome::RunVerdict;
use axocoatl_session::turn_contract::LogicalTurnState;
use serde::Serialize;
use std::collections::{BTreeSet, HashMap};

const CACHE_DIR: &str = "session-receipts";

#[derive(Debug, Clone, Serialize)]
pub struct NativeReceiptChecks {
    pub state: String,
    pub count: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct NativeReceiptReview {
    pub state: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct NativeTurnReceiptStatus {
    pub session_id: String,
    pub turn_id: String,
    pub state: String,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub receipt: Option<RunReceiptView>,
    pub checks: NativeReceiptChecks,
    pub review: NativeReceiptReview,
    pub outcome: String,
}

struct NativeEvidence {
    turn: ExecutionTurnView,
    view: SessionTurnControlPlane,
    digest: String,
}

fn available<T>(value: &EvidenceValue<T>) -> Option<&T> {
    match value {
        EvidenceValue::Available { value } => Some(value),
        _ => None,
    }
}

fn error(message: impl Into<String>) -> DaemonError {
    DaemonError::Session(message.into())
}

/// Only an exact retained native profile containing exclusively host read
/// tools proves that an activation cannot write. Unknown/custom tools and
/// external programs require captures even if their displayed role is review.
fn read_only_definitions(view: &SessionTurnControlPlane) -> BTreeSet<String> {
    view.nodes
        .iter()
        .filter_map(|node| {
            let definition = available(&node.definition)?;
            let provider = available(&definition.provider)?;
            if crate::external_agent::runtime_for_provider(provider).is_some() {
                return None;
            }
            let tools = available(&definition.tools)?;
            if !tools
                .iter()
                .all(|tool| matches!(tool.as_str(), "read_file" | "list_dir" | "grep" | "glob"))
            {
                return None;
            }
            available(&definition.snapshot).cloned()
        })
        .collect()
}

fn fingerprint(
    turn: &ExecutionTurnView,
    view: &SessionTurnControlPlane,
) -> Result<String, DaemonError> {
    // Exclude mutable UI capabilities and Session-wide network observations.
    // Bind the exact retained request, activations, captures, definitions and
    // completion conditions, all of which belong to this terminal revision.
    // A later Session operation may reacquire a runtime and append an ownership
    // reattachment to an already closed turn without changing its revision.
    // That observation does not change the execution or files this receipt
    // records. Keep original activation inputs and captures; omit only these
    // later reattachment observations from the stable historical identity.
    let mut retained_turn =
        serde_json::to_value(turn).map_err(|failure| error(failure.to_string()))?;
    if let Some(fields) = retained_turn.as_object_mut() {
        fields.remove("repository_reattachments");
    }
    let definitions: Vec<_> = view
        .nodes
        .iter()
        .map(|node| {
            serde_json::json!({"node_id": node.node_id, "definition_id": node.definition_id,
            "definition": node.definition, "dependencies": node.dependencies})
        })
        .collect();
    let bytes = serde_json::to_vec(&serde_json::json!({
        "turn": retained_turn,
        "graph_revision": view.graph_revision,
        "definitions": definitions,
        "checks": view.required_checks,
        "readiness": view.required_check_readiness,
        "review": view.required_review,
        "review_rounds": view.review_rounds,
        "conditions": view.conditions,
    }))
    .map_err(|failure| error(failure.to_string()))?;
    Ok(sha256_hex(&bytes))
}

impl NativeEvidence {
    fn filename(&self) -> String {
        // Do not interpolate caller-controlled identities into a path.
        let identity =
            sha256_hex(format!("{}\0{}", self.view.session_id, self.view.turn_id).as_bytes());
        format!("{identity}-{}.dsse.json", self.digest)
    }

    fn status(
        &self,
        state: &str,
        message: String,
        receipt: Option<RunReceiptView>,
    ) -> NativeTurnReceiptStatus {
        NativeTurnReceiptStatus {
            session_id: self.view.session_id.clone(),
            turn_id: self.view.turn_id.clone(),
            state: state.into(),
            message,
            receipt,
            checks: NativeReceiptChecks {
                state: self
                    .view
                    .required_check_readiness
                    .as_ref()
                    .map(|r| r.state.clone())
                    .unwrap_or_else(|| {
                        if self.view.required_checks.is_empty() {
                            "not_required".into()
                        } else {
                            "not_run".into()
                        }
                    }),
                count: self.view.required_checks.len(),
            },
            review: NativeReceiptReview {
                state: self
                    .view
                    .required_review
                    .as_ref()
                    .map(|r| r.state.clone())
                    .unwrap_or_else(|| "not_required".into()),
            },
            outcome: self.view.state.clone(),
        }
    }

    fn statement(&self) -> Result<Statement, DaemonError> {
        if !self.turn.state.is_closed() || self.turn.superseded {
            return Err(error(
                "A receipt requires a closed, visible native Session turn.",
            ));
        }
        if self.turn.state == LogicalTurnState::Completed
            && ((!self.view.required_checks.is_empty()
                && self
                    .view
                    .required_check_readiness
                    .as_ref()
                    .is_none_or(|readiness| readiness.state != "passed"))
                || self
                    .view
                    .required_checks
                    .iter()
                    .any(|check| check.state != "passed")
                || self
                    .view
                    .required_review
                    .as_ref()
                    .is_some_and(|review| review.state != "approved" || !review.current))
        {
            return Err(error("The completed turn's required check or review evidence is unavailable or inconsistent; no passing receipt can be signed."));
        }
        let request = match &self.turn.request {
            ContentResolution::Available { content, .. } => content,
            _ => {
                return Err(error(
                    "The turn's retained request is unavailable; no receipt can be signed.",
                ))
            }
        };
        let mut attribution = RunAttribution::default();
        attribution.add_native_receipt_turn(&self.turn, &read_only_definitions(&self.view));
        if self
            .turn
            .activations
            .iter()
            .all(|a| a.repository_snapshots.is_empty())
        {
            attribution
                .unattributable
                .push("No repository captures are retained for this turn.".into());
        }
        // This observer reads the exact historical projection, not current
        // Agent settings. Unknown usage remains explicitly incomplete.
        let observed = crate::bootstrap::loadout_runs::observation_from_control_plane(
            &self.view,
            &[],
            None,
            &HashMap::new(),
        );
        // Without a required check's candidate, concurrent or repeated
        // writers have no retained total ordering of their After captures.
        // Never pick one merely because its node sorts last in history.
        let after_trees: BTreeSet<_> = self
            .turn
            .activations
            .iter()
            .flat_map(|activation| &activation.repository_snapshots)
            .filter(|capture| {
                matches!(
                    capture.content.phase,
                    axocoatl_session::execution_content::RepositorySnapshotPhase::After
                )
            })
            .filter_map(|capture| capture.content.tree_sha256.clone())
            .collect();
        if after_trees.len() > 1 {
            attribution.last_writer_after = None;
            if !observed
                .checks
                .iter()
                .any(|check| check.candidate_sha256.is_some())
            {
                attribution.unattributable.push("Several distinct After captures are retained without a final required-check candidate; the final file hashes cannot be established.".into());
            }
        }
        let files =
            crate::run_attribution::receipt_files_from_captures(&attribution, &observed.checks);
        let mut configurations = BTreeSet::new();
        let mut definitions = BTreeSet::new();
        let mut agents = Vec::new();
        for node in &self.view.nodes {
            let definition = available(&node.definition).ok_or_else(|| {
                error(format!(
                    "The retained definition of {} is unavailable.",
                    node.node_id
                ))
            })?;
            let provider = available(&definition.provider).cloned().ok_or_else(|| {
                error(format!(
                    "The provider of {} was not retained.",
                    node.node_id
                ))
            })?;
            let model = available(&definition.model)
                .cloned()
                .ok_or_else(|| error(format!("The model of {} was not retained.", node.node_id)))?;
            if let Some(revision) = available(&definition.configuration_revision) {
                configurations.insert(*revision);
            }
            if let Some(snapshot) = available(&definition.snapshot) {
                definitions.insert(snapshot.clone());
            }
            let runtime = crate::external_agent::runtime_for_provider(&provider)
                .map(|runtime| runtime.id().to_string())
                .unwrap_or_else(|| "native".into());
            agents.push(AgentFacts {
                id: node.node_id.clone(),
                role: available(&definition.role)
                    .cloned()
                    .unwrap_or_else(|| "unknown".into()),
                runtime,
                program_version: None,
                provider,
                model,
                credential: None,
            });
        }
        let checks = observed
            .checks
            .iter()
            .map(|check| CheckFacts {
                name: check.name.clone(),
                state: serde_name(&check.state),
                exit_code: check.exit_code,
                // The display projection does not retain the configured timeout.
                timeout_ms: None,
                argv: check.argv.clone(),
                candidate_sha256: check.candidate_sha256.clone(),
            })
            .collect();
        let review = self.view.required_review.as_ref().map(|review| {
            let identity = agents
                .iter()
                .find(|agent| agent.id == axocoatl_session::turn_review::REVIEW_NODE_ID);
            ReviewFacts {
                provider: identity.map(|a| a.provider.clone()).unwrap_or_default(),
                model: identity
                    .map(|a| a.model.clone())
                    .unwrap_or_else(|| review.reviewer.clone()),
                runtime: identity
                    .map(|a| a.runtime.clone())
                    .unwrap_or_else(|| "unknown".into()),
                program_version: None,
                verdict: review.state.clone(),
                passed: review.state == "approved" && review.current,
                rounds: self.view.review_rounds.len() as u32,
                max_rounds: review.max_rounds,
                findings: self
                    .view
                    .review_rounds
                    .iter()
                    .map(|round| round.to_round().findings.len() as u32)
                    .sum(),
            }
        });
        let (verdict, exit_code) = match self.turn.state {
            LogicalTurnState::Completed => (RunVerdict::Pass, 0),
            LogicalTurnState::Cancelled => (RunVerdict::Interrupted, 6),
            LogicalTurnState::Finished => (RunVerdict::NeedsAttention, 2),
            _ => {
                return Err(error(
                    "The turn is still open; no final receipt is available.",
                ))
            }
        };
        let base_commit = self
            .turn
            .activations
            .iter()
            .flat_map(|a| &a.repository_snapshots)
            .find(|capture| {
                matches!(
                    capture.content.phase,
                    axocoatl_session::execution_content::RepositorySnapshotPhase::Before
                )
            })
            .and_then(|capture| capture.content.head.clone());
        let mut limitations = vec![
            "Historical sandbox settings, network totals, pre-existing dirty paths, program versions and the exact closure timestamp are not retained by this receipt. Absent fields mean unknown, not zero activity or isolation.".into(),
            "Usage is the retained activation subtotal; it may omit calls from failed or interrupted activations. It is not a complete billing record.".into(),
        ];
        if !self.view.required_checks.is_empty() {
            limitations.push("Historical check timeouts are not present in this projection; command, exit status and candidate hashes are retained.".into());
        }
        let warnings = if files.facts.complete {
            Vec::new()
        } else {
            vec![receipt::RECEIPT_INCOMPLETE.into()]
        };
        Ok(Statement {
            statement_type: STATEMENT_TYPE.into(),
            predicate_type: PREDICATE_TYPE.into(),
            subject: files.subject,
            predicate: Predicate {
                run: RunFacts {
                    run_id: self.view.turn_id.clone(),
                    session_id: self.view.session_id.clone(),
                    started_at_ms: request.recorded_at_unix_ms,
                    finished_at_ms: None,
                    axocoatl_version: env!("CARGO_PKG_VERSION").into(),
                    task_sha256: sha256_hex(request.effective_input.as_bytes()),
                },
                repository: RepositoryFacts {
                    base_commit,
                    dirty_paths: Vec::new(),
                    origin_url: None,
                },
                loadout: None,
                native_turn: Some(NativeTurnFacts {
                    turn_id: self.view.turn_id.clone(),
                    turn_revision: self.turn.revision,
                    graph_revision: available(&self.view.graph_revision)
                        .copied()
                        .ok_or_else(|| error("The native graph revision is unavailable."))?,
                    state: self.view.state.clone(),
                    evidence_sha256: self.digest.clone(),
                    configuration_revisions: configurations.into_iter().collect(),
                    definition_snapshots: definitions.into_iter().collect(),
                    limitations,
                }),
                agents,
                sandbox: None,
                network: None,
                checks,
                review,
                adjudications: Vec::new(),
                verdict,
                exit_code,
                attention: if verdict == RunVerdict::Pass {
                    Vec::new()
                } else {
                    vec![format!("The Session turn ended as {}.", self.view.state)]
                },
                not_covered: NotCoveredFacts {
                    count: files.facts.unattributed.len() as u64,
                    areas: files.facts.unattributed.clone(),
                },
                warnings,
                usage: UsageFacts {
                    input_tokens: observed.usage.input_tokens,
                    output_tokens: observed.usage.output_tokens,
                    cost_microunits: observed.usage.cost_microunits,
                    cost_usd: observed.usage.cost_microunits as f64 / 1_000_000.0,
                    complete: false,
                    cost_known: false,
                    cost_computed: observed.usage.cost_computed,
                },
                files: files.facts,
                signer: Default::default(),
            },
        })
    }
}

fn validate_cached(evidence: &NativeEvidence, bytes: &[u8]) -> Result<(), DaemonError> {
    let opened = receipt::open_receipt(bytes).map_err(|failure| error(failure.to_string()))?;
    let native = opened
        .statement
        .predicate
        .native_turn
        .as_ref()
        .ok_or_else(|| error("The cached receipt is not a native Session turn receipt."))?;
    if native.turn_id != evidence.view.turn_id
        || native.turn_revision != evidence.turn.revision
        || native.evidence_sha256 != evidence.digest
        || opened.statement.predicate.run.session_id != evidence.view.session_id
        || matches!(
            receipt::check_signature(&opened, &[]),
            receipt::SignatureCheck::Invalid(_)
        )
    {
        return Err(error(
            "The cached receipt does not match this turn's retained evidence.",
        ));
    }
    // A matching fingerprint string alone is not proof that the payload was
    // constructed from that evidence. Compare every evidence-derived fact.
    let expected = evidence.statement()?;
    let mut cached = opened.statement;
    cached.predicate.signer = Default::default();
    // The version identifies the binary that first signed these bytes. An
    // upgrade does not invalidate retained evidence or rewrite signed bytes.
    cached.predicate.run.axocoatl_version = expected.predicate.run.axocoatl_version.clone();
    if cached != expected {
        return Err(error(
            "The cached receipt's facts do not match this turn's retained evidence.",
        ));
    }
    Ok(())
}

impl AxocoatlDaemon {
    async fn native_receipt_evidence(
        &self,
        session_id: &str,
        turn_id: &str,
    ) -> Result<NativeEvidence, DaemonError> {
        let history = self.versioned_session_history_snapshot(session_id).await?;
        let turn = match history.get(turn_id) {
            Some(SessionHistoryEntry::ExecutionV2(turn)) => (**turn).clone(),
            Some(SessionHistoryEntry::LegacyV1(_)) => {
                return Err(error("This legacy turn has no native receipt evidence."))
            }
            None => {
                return Err(DaemonError::NotFound(format!(
                    "Session turn {turn_id} was not found."
                )))
            }
        };
        let view = self
            .session_turn_control_plane(session_id, turn_id)
            .await?
            .ok_or_else(|| error("The turn control plane is unavailable."))?;
        if view.session_id != session_id
            || view.turn_id != turn_id
            || available(&view.turn_revision).copied() != Some(turn.revision)
        {
            return Err(error(
                "The turn changed while its receipt evidence was read. Refresh and try again.",
            ));
        }
        let digest = fingerprint(&turn, &view)?;
        Ok(NativeEvidence { turn, view, digest })
    }

    fn cached_native_receipt(
        &self,
        evidence: &NativeEvidence,
    ) -> Result<Option<Vec<u8>>, DaemonError> {
        let directory = match self.data_root.existing_child(CACHE_DIR) {
            Ok(directory) => directory,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(failure) => return Err(error(failure.to_string())),
        };
        let bytes = match directory.read_limited(evidence.filename(), receipt::MAX_ENVELOPE_BYTES) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(failure) => return Err(error(failure.to_string())),
        };
        validate_cached(evidence, &bytes)?;
        Ok(Some(bytes))
    }

    /// Read-only availability: this method never creates a key or signs a receipt.
    pub async fn native_turn_receipt_status(
        &self,
        session_id: &str,
        turn_id: &str,
    ) -> Result<NativeTurnReceiptStatus, DaemonError> {
        let evidence = self.native_receipt_evidence(session_id, turn_id).await?;
        if evidence.turn.superseded {
            return Ok(evidence.status(
                "not_applicable",
                "This turn was superseded by a conversation rewind.".into(),
                None,
            ));
        }
        if !evidence.turn.state.is_closed() {
            return Ok(evidence.status("pending", "The turn is still open. Its receipt becomes available after it completes, is stopped, or is finished.".into(), None));
        }
        if let Err(failure) = evidence.statement() {
            return Ok(evidence.status("unavailable", failure.to_string(), None));
        }
        match self.cached_native_receipt(&evidence) {
            Ok(Some(bytes)) => Ok(evidence.status("available", "Signed receipt of this exact finished turn.".into(), receipt_view(&bytes))),
            Ok(None) => Ok(evidence.status("ready", "Download signs the retained turn evidence and captured file hashes. Current workspace files are not read.".into(), None)),
            Err(failure) => Ok(evidence.status("unavailable", failure.to_string(), None)),
        }
    }

    /// Seal and download immutable historical evidence. Cached bytes are served
    /// only while the exact native terminal revision still agrees with them.
    pub async fn native_turn_receipt(
        &self,
        session_id: &str,
        turn_id: &str,
    ) -> Result<Vec<u8>, DaemonError> {
        let evidence = self.native_receipt_evidence(session_id, turn_id).await?;
        let mut statement = evidence.statement()?;
        let bytes = match self.cached_native_receipt(&evidence)? {
            Some(bytes) => bytes,
            None => {
                let data_dir = self.data_root.path().to_path_buf();
                let (key, _) =
                    tokio::task::spawn_blocking(move || receipt_key::load_or_create(&data_dir))
                        .await
                        .map_err(|failure| error(failure.to_string()))?
                        .map_err(error)?;
                statement.predicate.signer = key.public().signer();
                let (envelope, _) = receipt::sign_statement(key.key_pair(), &statement)
                    .map_err(|failure| error(failure.to_string()))?;
                let bytes = receipt::envelope_file_bytes(&envelope);
                self.data_root
                    .child(CACHE_DIR)
                    .and_then(|directory| directory.atomic_write(evidence.filename(), &bytes))
                    .map_err(|failure| {
                        error(format!("Storing the native turn receipt: {failure}"))
                    })?;
                bytes
            }
        };
        // Retention, rewind and recovery can change the read evidence between
        // awaits. Never return a signature for the previously read revision.
        let current = self.native_receipt_evidence(session_id, turn_id).await?;
        if current.digest != evidence.digest
            || !current.turn.state.is_closed()
            || current.turn.superseded
        {
            return Err(error(
                "The turn changed while its receipt was signed. Refresh and try again.",
            ));
        }
        Ok(bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axocoatl_session::execution_content::ExecutionRequestContent;
    use axocoatl_session::execution_store::ExecutionStoreOwner;
    use axocoatl_session::turn_contract::{EvidenceRef, LogicalTurnId, SessionId};
    use axocoatl_session::turn_ledger::SessionTurn;

    fn evidence() -> NativeEvidence {
        let legacy: SessionTurn = serde_json::from_value(serde_json::json!({
            "id":"turn-1", "session_id":"session-1", "user_input":"Fix the counter",
            "status":"completed", "created_at":100, "updated_at":200
        }))
        .unwrap();
        let mut view = SessionTurnControlPlane::from_legacy(&legacy);
        view.history_version = "execution_v2".into();
        view.turn_revision = EvidenceValue::Available { value: 3 };
        view.graph_revision = EvidenceValue::Available { value: 1 };
        let turn = ExecutionTurnView {
            repository_reattachments: vec![],
            superseded: false,
            kept_way: None,
            owner: ExecutionStoreOwner {
                workspace_id: "workspace-1".into(),
                session_id: SessionId::new("session-1").unwrap(),
            },
            turn_id: LogicalTurnId::new("turn-1").unwrap(),
            revision: 3,
            state: LogicalTurnState::Completed,
            stop_requested: None,
            epochs: vec![],
            legacy_predecessor: None,
            request: ContentResolution::Available {
                reference: EvidenceRef::new("request-1").unwrap(),
                content: ExecutionRequestContent {
                    turn_id: LogicalTurnId::new("turn-1").unwrap(),
                    recorded_at_unix_ms: 100,
                    display_input: "Fix the counter".into(),
                    effective_input: "Fix the counter".into(),
                    context: vec![],
                    target_definition: None,
                    model: None,
                },
            },
            activations: vec![],
        };
        let digest = fingerprint(&turn, &view).unwrap();
        NativeEvidence { turn, view, digest }
    }

    fn signed(mut statement: Statement) -> Vec<u8> {
        let directory = tempfile::tempdir().unwrap();
        let (key, _) = receipt_key::load_or_create(directory.path()).unwrap();
        statement.predicate.signer = key.public().signer();
        let (envelope, _) = receipt::sign_statement(key.key_pair(), &statement).unwrap();
        receipt::envelope_file_bytes(&envelope)
    }

    #[test]
    fn only_exact_retained_native_read_tools_exempt_missing_captures() {
        use crate::session_control_plane::{ControlPlaneDefinition, ControlPlaneNode};
        let mut evidence = evidence();
        let definition = ControlPlaneDefinition {
            name: EvidenceValue::NotRecorded,
            role: EvidenceValue::NotRecorded,
            provider: EvidenceValue::Available {
                value: "ollama".into(),
            },
            model: EvidenceValue::Available {
                value: "reviewer".into(),
            },
            instructions: EvidenceValue::NotRecorded,
            tools: EvidenceValue::Available {
                value: vec!["read_file".into()],
            },
            configuration_revision: EvidenceValue::Available { value: 1 },
            snapshot: EvidenceValue::Available {
                value: "reviewer-snapshot".into(),
            },
        };
        evidence.view.nodes.push(ControlPlaneNode {
            node_id: "required-review".into(),
            definition_id: "reviewer".into(),
            label: "Review".into(),
            definition: EvidenceValue::Available {
                value: definition.clone(),
            },
            dependencies: vec![],
            activations: vec![],
        });
        assert_eq!(
            read_only_definitions(&evidence.view),
            BTreeSet::from(["reviewer-snapshot".into()])
        );
        for tool in ["write_file", "edit_file", "bash", "unknown_tool"] {
            let mut changed = definition.clone();
            changed.tools = EvidenceValue::Available {
                value: vec![tool.into()],
            };
            evidence.view.nodes[0].definition = EvidenceValue::Available { value: changed };
            assert!(
                read_only_definitions(&evidence.view).is_empty(),
                "{tool} can have file effects"
            );
        }
        let mut external = definition.clone();
        external.provider = EvidenceValue::Available {
            value: crate::external_agent::codex::PROVIDER.into(),
        };
        evidence.view.nodes[0].definition = EvidenceValue::Available { value: external };
        assert!(read_only_definitions(&evidence.view).is_empty());
        let mut missing = definition;
        missing.tools = EvidenceValue::NotRecorded;
        evidence.view.nodes[0].definition = EvidenceValue::Available { value: missing };
        assert!(read_only_definitions(&evidence.view).is_empty());
    }

    #[test]
    fn native_cache_preserves_original_bytes_across_binary_upgrades() {
        let evidence = evidence();
        let mut statement = evidence.statement().unwrap();
        statement.predicate.run.axocoatl_version = "earlier-signing-version".into();
        let bytes = signed(statement);
        validate_cached(&evidence, &bytes).unwrap();
        let original = receipt::open_receipt(&bytes).unwrap();
        assert_eq!(
            original.statement.predicate.run.axocoatl_version,
            "earlier-signing-version"
        );
    }

    #[test]
    fn native_cache_survives_later_repository_reattachment() {
        use axocoatl_session::execution_content::{
            ActivationEvidenceContent, RepositoryReattachment, RepositoryReattachmentView,
        };
        let mut evidence = evidence();
        let original_digest = evidence.digest.clone();
        let original_filename = evidence.filename();
        let bytes = signed(evidence.statement().unwrap());
        evidence
            .turn
            .repository_reattachments
            .push(RepositoryReattachmentView {
                reference: EvidenceRef::new("reattachment-1").unwrap(),
                content: RepositoryReattachment {
                    turn_id: evidence.turn.turn_id.clone(),
                    original: EvidenceRef::new("repository-original").unwrap(),
                    acquired: EvidenceRef::new("repository-reacquired").unwrap(),
                },
                original_resource: ActivationEvidenceContent::Repository {
                    description: "original execution identity".into(),
                    revision: None,
                },
                acquired_resource: ActivationEvidenceContent::Repository {
                    description: "runtime identity after restart".into(),
                    revision: None,
                },
            });
        evidence.digest = fingerprint(&evidence.turn, &evidence.view).unwrap();
        assert_eq!(evidence.digest, original_digest);
        assert_eq!(evidence.filename(), original_filename);
        validate_cached(&evidence, &bytes).unwrap();
        // The exception is narrow: original retained task evidence still binds
        // the signed receipt, even with a later reattachment observation.
        if let ContentResolution::Available { content, .. } = &mut evidence.turn.request {
            content.effective_input = "A different task".into();
        }
        evidence.digest = fingerprint(&evidence.turn, &evidence.view).unwrap();
        assert_ne!(evidence.digest, original_digest);
        assert!(validate_cached(&evidence, &bytes).is_err());
    }

    #[test]
    fn native_cache_rejects_changed_facts_and_terminal_revision() {
        let mut evidence = evidence();
        let bytes = signed(evidence.statement().unwrap());
        validate_cached(&evidence, &bytes).unwrap();
        let mut forged = evidence.statement().unwrap();
        forged.predicate.run.task_sha256 = "f".repeat(64);
        assert!(validate_cached(&evidence, &signed(forged)).is_err());
        evidence.turn.revision += 1;
        evidence.digest = fingerprint(&evidence.turn, &evidence.view).unwrap();
        assert!(validate_cached(&evidence, &bytes).is_err());
    }

    #[test]
    fn native_receipt_rejects_open_superseded_and_missing_request_evidence() {
        let mut evidence = evidence();
        for state in [LogicalTurnState::Running, LogicalTurnState::NeedsAttention] {
            evidence.turn.state = state;
            assert!(evidence.statement().is_err());
        }
        evidence.turn.state = LogicalTurnState::Completed;
        evidence.turn.superseded = true;
        assert!(evidence.statement().is_err());
        evidence.turn.superseded = false;
        evidence.turn.request = ContentResolution::NotRecorded;
        assert!(evidence.statement().is_err());
    }

    #[test]
    fn configured_checks_without_readiness_are_not_passed_or_not_required() {
        let mut evidence = evidence();
        evidence
            .view
            .required_checks
            .push(axocoatl_session::turn_checks::TurnCheckView::pending(vec![
                "npm".into(),
                "test".into(),
            ]));
        assert_eq!(
            evidence.status("ready", String::new(), None).checks.state,
            "not_run"
        );
        assert!(evidence.statement().is_err());
        evidence.turn.state = LogicalTurnState::Cancelled;
        evidence.view.state = "cancelled".into();
        let statement = evidence.statement().unwrap();
        assert_eq!(statement.predicate.verdict, RunVerdict::Interrupted);
        assert_eq!(statement.predicate.checks[0].state, "not_run");
        assert!(statement.predicate.run.finished_at_ms.is_none());
        assert!(!statement.predicate.files.complete);
    }
}
