//! Retained Ways review evidence under the Session's canonical storage owner.
//! Runtime comparison and Keep keep their existing transaction authority.
use super::*;
use axocoatl_session::execution_namespace::ExecutionComponent;
use axocoatl_session::ways_decision::*;
use axocoatl_session::ways_decision_store::{
    DeletedWaysDecision, PinnedWaysPatch, WaysDecisionStore, MAX_WAYS_PATCH_BYTES,
};
use serde::{Deserialize, Serialize};
#[path = "bootstrap_ways_decision.rs"]
mod decision;
pub(super) use decision::{StoredWaysCheck, StoredWaysJudge};

fn archive_error(error: impl std::fmt::Display) -> DaemonError {
    DaemonError::SessionConflict(error.to_string())
}
#[derive(Debug, Clone, Serialize)]
pub struct WaysHistoryView {
    pub schema_version: u32,
    pub supports_retention: bool,
    pub session_id: String,
    pub limits: Option<WaysRetentionLimits>,
    pub decisions: Vec<WaysDecisionRecord>,
    pub deleted: Vec<DeletedWaysDecision>,
}
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WaysHistoryConfiguration {
    pub limits: WaysRetentionLimits,
}
#[derive(Debug, Clone, Serialize)]
pub struct WaysDecisionExport {
    pub record: WaysDecisionRecord,
    pub patches: Vec<PinnedWaysPatch>,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct WaysPreparationReceipt {
    schema_version: u32,
    session_id: String,
    task: Option<String>,
    instruction: Option<String>,
    provider: String,
    model: String,
    usage: WaysUsage,
}

impl AxocoatlDaemon {
    pub fn retain_ways_plan_preparation(
        &self,
        session_id: &str,
        task: &str,
        instruction: &str,
        agent_id: &str,
        usage: &crate::git::ControlUsage,
    ) -> Result<Option<String>, WaysControlFailure> {
        let agent = self
            .config
            .agents
            .iter()
            .find(|agent| agent.id == agent_id)
            .ok_or_else(|| {
                WaysControlFailure::with_usage(
                    archive_error("Planning Agent is no longer configured"),
                    usage.clone(),
                )
            })?;
        self.retain_ways_preparation(
            session_id,
            Some(task),
            Some(instruction),
            &agent.provider,
            &agent.model,
            usage,
        )
        .map_err(|error| WaysControlFailure::with_usage(error, usage.clone()))
    }
    /// Preserve observed shared work once, outside all candidate measurements.
    pub fn retain_ways_preparation(
        &self,
        session_id: &str,
        task: Option<&str>,
        instruction: Option<&str>,
        provider: &str,
        model: &str,
        usage: &crate::git::ControlUsage,
    ) -> Result<Option<String>, DaemonError> {
        if !self
            .session_dispatch_lifecycles
            .retains_session(session_id)?
        {
            return Ok(None);
        }
        let tokens = axocoatl_core::TokenUsageStats {
            input_tokens: usage.input_tokens as usize,
            output_tokens: usage.output_tokens as usize,
            reasoning_tokens: Some(usage.reasoning_tokens as usize),
        };
        let local = provider == "ollama" && self.ollama_model_api_cost_known_zero();
        let price = self.config.pricing.get(model);
        let cost = price
            .map(|price| {
                crate::git::ModelPrice {
                    input_per_mtok: price.input_per_mtok,
                    output_per_mtok: price.output_per_mtok,
                }
                .cost(
                    usage.input_tokens,
                    usage.output_tokens.saturating_add(usage.reasoning_tokens),
                )
            })
            .unwrap_or(0.0);
        let identity = format!("ways-preparation-{}", uuid::Uuid::new_v4());
        let receipt = WaysPreparationReceipt {
            schema_version: 1,
            session_id: session_id.into(),
            task: task.map(str::to_owned),
            instruction: instruction.map(str::to_owned),
            provider: provider.into(),
            model: model.into(),
            usage: WaysUsage {
                measurement_id: axocoatl_session::turn_contract::EvidenceRef::new(&identity)
                    .map_err(archive_error)?,
                tokens: if usage.token_usage_known {
                    axocoatl_session::execution_content::ExecutionUsage::Measured { usage: tokens }
                } else {
                    axocoatl_session::execution_content::ExecutionUsage::Unknown {
                        known_subtotal: tokens,
                    }
                },
                cost_usd_known_subtotal: if local { 0.0 } else { cost },
                cost_complete: usage.token_usage_known && (local || price.is_some()),
            },
        };
        let token = self
            .session_dispatch_lifecycles
            .session_team_token(session_id)?;
        self.session_dispatch_lifecycles.with_session_team_stores(&token,|_,content,_|{
            let retained=content.retain_activation_evidence(axocoatl_session::execution_content::ActivationEvidenceContent::Attachment{reference_id:identity,media_type:"application/vnd.axocoatl.ways-preparation+json".into(),text:serde_json::to_string(&receipt).map_err(archive_error)?}).map_err(archive_error)?;
            Ok(Some(retained.reference().as_str().to_owned()))
        })
    }
    pub(super) fn resolve_ways_preparations(
        &self,
        session_id: &str,
        task: &str,
        instruction: &str,
        lanes: &[crate::git::Variant],
        references: &[String],
    ) -> Result<Vec<WaysUsage>, DaemonError> {
        if references.is_empty() {
            return Ok(vec![]);
        }
        let maximum =
            self.with_ways_archive(session_id, |archive| Ok(archive.limits().items_per_field))?;
        if references.len() > maximum {
            return Err(archive_error("Too many shared preparation references"));
        }
        let token = self
            .session_dispatch_lifecycles
            .session_team_token(session_id)?;
        self.session_dispatch_lifecycles.with_session_team_stores(&token,|_,content,_|{
            let mut result=Vec::new();let mut seen=std::collections::HashSet::new();
            for reference in references {
                if !seen.insert(reference){continue;}
                let reference=axocoatl_session::turn_contract::EvidenceRef::new(reference).map_err(archive_error)?;
                let axocoatl_session::execution_content::ActivationEvidenceContent::Attachment{reference_id,media_type,text}=&content.resolve_activation_evidence(&reference).map_err(archive_error)? else{return Err(archive_error("Shared preparation reference has the wrong type"))};
                if !reference_id.starts_with("ways-preparation-")||media_type!="application/vnd.axocoatl.ways-preparation+json"{return Err(archive_error("Reference is not retained shared preparation"));}
                let receipt:WaysPreparationReceipt=serde_json::from_str(text).map_err(archive_error)?;
                if receipt.schema_version!=1||receipt.session_id!=session_id||receipt.task.as_deref().is_some_and(|value|value!=task)||receipt.instruction.as_deref().is_some_and(|value|value.trim()!=instruction.trim()) {
                    return Err(archive_error("Shared preparation does not match this exact Ways request"));
                }
                if receipt.task.is_none()&&!lanes.iter().any(|lane|lane.provider.as_deref()==Some(receipt.provider.as_str())&&lane.model.as_deref()==Some(receipt.model.as_str())){return Err(archive_error("Model check is not for this selected roster"));}
                result.push(receipt.usage);
            }
            Ok(result)
        })
    }
    pub async fn export_ways_decision(
        &self,
        session_id: &str,
        decision_id: &str,
    ) -> Result<WaysDecisionExport, DaemonError> {
        let id = DecisionId(
            axocoatl_session::turn_contract::EvidenceRef::new(decision_id)
                .map_err(archive_error)?,
        );
        self.with_ways_archive(session_id, |archive| {
            let record = archive.get(&id).map_err(archive_error)?.ok_or_else(|| {
                archive_error("This decision is unavailable or was explicitly deleted")
            })?;
            let mut patches = Vec::new();
            let mut seen = std::collections::HashSet::new();
            for candidate in &record.candidates {
                if let Recorded::Available { value } = &candidate.patch {
                    if seen.insert(value.protected_artifact_ref.clone()) {
                        let bytes = archive
                            .patch(&value.protected_artifact_ref)
                            .map_err(archive_error)?
                            .ok_or_else(|| {
                                archive_error("A promised protected patch is missing")
                            })?;
                        patches.push(PinnedWaysPatch::capture(&bytes).map_err(archive_error)?);
                    }
                }
            }
            Ok(WaysDecisionExport { record, patches })
        })
    }
    pub async fn delete_ways_decision(
        &self,
        session_id: &str,
        decision_id: &str,
    ) -> Result<(), DaemonError> {
        let _operation = self
            .take_session_workspace_operation(
                session_id,
                super::workspace_operation::WorkspaceRequest {
                    doing: format!("a Ways decision of Session {session_id} being deleted"),
                    refused: "The Ways decision was not deleted".into(),
                },
            )
            .await?;
        let id = DecisionId(
            axocoatl_session::turn_contract::EvidenceRef::new(decision_id)
                .map_err(archive_error)?,
        );
        if self.with_ways_archive(session_id, |archive| {
            Ok(archive
                .deleted_decision(&id)
                .map_err(archive_error)?
                .is_some())
        })? {
            return Ok(());
        }
        let exported = self.export_ways_decision(session_id, decision_id).await?;
        if self
            .peek_current_attempt_set(session_id)
            .await?
            .is_some_and(|set| set.id == exported.record.set_id.0.as_str())
        {
            return Err(archive_error(
                "Finish this decision's runtime cleanup before deleting retained evidence",
            ));
        }
        let receipt =
            axocoatl_session::turn_contract::EvidenceRef::new(format!("purged-{}", decision_id))
                .map_err(archive_error)?;
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(archive_error)?
            .as_millis()
            .try_into()
            .map_err(archive_error)?;
        self.with_ways_archive(session_id, |archive| {
            archive
                .delete_verified(&exported.record.decision_id, timestamp, |record| {
                    // Patch bodies are owned only by this archive. Historical composer
                    // references hold independent captured bytes. The archive keeps
                    // every pin shared by another decision and a permanent tombstone.
                    if record == &exported.record {
                        Ok(receipt)
                    } else {
                        Err(
                            axocoatl_session::ways_decision_store::WaysArchiveError::Invalid(
                                "Decision changed while verifying deletion ownership",
                            ),
                        )
                    }
                })
                .map_err(archive_error)
        })
    }
    pub(super) async fn resolve_ways_decision_context(
        &self,
        session_id: &str,
        reference: &SessionTurnContextReference,
    ) -> Result<SessionTurnContextReference, DaemonError> {
        let source = reference
            .metadata
            .get("source_session_id")
            .and_then(serde_json::Value::as_str);
        let decision = reference
            .metadata
            .get("decision_id")
            .and_then(serde_json::Value::as_str);
        if source != Some(session_id) || decision != Some(reference.reference_id.as_str()) {
            return Err(archive_error(
                "Decision reference must identify this Session's exact retained decision",
            ));
        }
        let exported = self
            .export_ways_decision(session_id, &reference.reference_id)
            .await?;
        let limits = self.with_ways_archive(session_id, |archive| Ok(archive.limits()))?;
        let body = retained_text(
            &serde_json::to_string_pretty(&exported.record).map_err(archive_error)?,
            limits.field_bytes,
        );
        let text = serde_json::to_string(&body).map_err(archive_error)?;
        let mut captured = reference.clone();
        captured.media_type = Some("application/json".into());
        captured.content_sha256 = Some(Self::bytes_sha256(text.as_bytes()));
        captured.metadata=serde_json::json!({"source_session_id":session_id,"decision_id":reference.reference_id,"content":text}).as_object().cloned().unwrap_or_default();
        Ok(captured)
    }
    pub async fn ways_history(&self, session_id: &str) -> Result<WaysHistoryView, DaemonError> {
        self.get_session(session_id)
            .await
            .ok_or_else(|| archive_error("Session does not exist"))?;
        if !self
            .session_dispatch_lifecycles
            .retains_session(session_id)?
        {
            return Ok(WaysHistoryView {
                schema_version: 1,
                supports_retention: false,
                session_id: session_id.into(),
                limits: None,
                decisions: vec![],
                deleted: vec![],
            });
        }
        let token = self
            .session_dispatch_lifecycles
            .session_team_token(session_id)?;
        self.session_dispatch_lifecycles
            .with_session_team_stores(&token, |canonical, _, _| {
                let namespace = canonical
                    .component_namespace(ExecutionComponent::WaysDecisions)
                    .map_err(archive_error)?;
                let archive = WaysDecisionStore::open_configured(namespace, canonical)
                    .map_err(archive_error)?;
                Ok(match archive {
                    Some(archive) => WaysHistoryView {
                        schema_version: 1,
                        supports_retention: true,
                        session_id: session_id.into(),
                        limits: Some(archive.limits()),
                        decisions: archive.records().map_err(archive_error)?,
                        deleted: archive.deleted().map_err(archive_error)?,
                    },
                    None => WaysHistoryView {
                        schema_version: 1,
                        supports_retention: true,
                        session_id: session_id.into(),
                        limits: None,
                        decisions: vec![],
                        deleted: vec![],
                    },
                })
            })
    }
    pub async fn configure_ways_history(
        &self,
        session_id: &str,
        request: WaysHistoryConfiguration,
    ) -> Result<WaysHistoryView, DaemonError> {
        self.get_session(session_id)
            .await
            .ok_or_else(|| archive_error("Session does not exist"))?;
        let token = self
            .session_dispatch_lifecycles
            .session_team_token(session_id)?;
        self.session_dispatch_lifecycles
            .with_session_team_stores(&token, |canonical, _, _| {
                let namespace = canonical
                    .component_namespace(ExecutionComponent::WaysDecisions)
                    .map_err(archive_error)?;
                match WaysDecisionStore::open_configured(namespace, canonical)
                    .map_err(archive_error)?
                {
                    Some(mut archive) => archive
                        .configure_limits(request.limits)
                        .map_err(archive_error)?,
                    None => {
                        let namespace = canonical
                            .component_namespace(ExecutionComponent::WaysDecisions)
                            .map_err(archive_error)?;
                        WaysDecisionStore::open_owned(namespace, canonical, request.limits)
                            .map_err(archive_error)?;
                    }
                }
                Ok(())
            })?;
        self.ways_history(session_id).await
    }
    pub(super) fn with_ways_archive<T>(
        &self,
        session_id: &str,
        operation: impl FnOnce(&mut WaysDecisionStore) -> Result<T, DaemonError>,
    ) -> Result<T, DaemonError> {
        let token = self
            .session_dispatch_lifecycles
            .session_team_token(session_id)?;
        self.session_dispatch_lifecycles.with_session_team_stores(&token,|canonical,_,_|{
            let namespace=canonical.component_namespace(ExecutionComponent::WaysDecisions).map_err(archive_error)?;
            let mut archive=WaysDecisionStore::open_configured(namespace,canonical).map_err(archive_error)?
                .ok_or_else(||archive_error("Configure decision-history storage in Ways before starting a comparison"))?;
            operation(&mut archive)
        })
    }
    /// Called only while the existing attempt-operation lease has frozen the
    /// candidates. Git writes a bounded passive artifact, which is read through
    /// the same retained descriptor and pinned independently before cleanup.
    pub(super) async fn retained_checked_patch(
        &self,
        sandbox: &Arc<dyn Sandbox>,
        set: &crate::git::AttemptSet,
        checked: &StoredCheckedTree,
        limit: usize,
    ) -> Result<(PinnedWaysPatch, Vec<String>), DaemonError> {
        Self::validate_checked_candidate_reference_in_sandbox(sandbox, set, checked).await?;
        let workspace = SecureDir::open(sandbox.root()).map_err(archive_error)?;
        let git = workspace.existing_child(".git").map_err(archive_error)?;
        let guard = Self::retain_passive_git_write_topology(&git)?;
        let key = format!(
            "axo-retained-patch-{}-{}",
            crate::attempts::set_key(&set.id),
            checked.index
        );
        let output_path = sandbox.root().join(".git").join(&key);
        let output = format!("--output={}", output_path.display());
        let root = sandbox.root().to_string_lossy().to_string();
        let result = async {
            Self::remove_passive_git_leaves(
                &git,
                &[key.as_str()],
                "preparing retained decision patch",
            )?;
            Self::require_git_output(
                Self::session_git_in_sandbox(
                    sandbox,
                    &root,
                    &[
                        "-c",
                        "diff.algorithm=myers",
                        "diff",
                        "--binary",
                        "--full-index",
                        "--no-ext-diff",
                        "--no-textconv",
                        "--no-renames",
                        &output,
                        &set.base_sha,
                        &checked.tree_oid,
                    ],
                )
                .await?,
                "retaining exact checked patch",
            )?;
            // One patch is bounded by the per-patch limit, never by how much
            // the Session may retain in total.
            let limit = limit.min(MAX_WAYS_PATCH_BYTES);
            if git.file_len(&key).map_err(archive_error)? > limit as u64 {
                return Err(archive_error(WaysDecisionError::Capacity));
            }
            let bytes = git.read_limited(&key, limit).map_err(archive_error)?;
            let pinned = PinnedWaysPatch::capture(&bytes).map_err(archive_error)?;
            if pinned.sha256 != checked.patch_sha256 {
                return Err(archive_error(
                    "Checked patch changed before decision retention",
                ));
            }
            let paths = Self::require_raw_git_output(
                Self::session_git_in_sandbox(
                    sandbox,
                    &root,
                    &[
                        "diff",
                        "--name-only",
                        "-z",
                        "--no-renames",
                        &set.base_sha,
                        &checked.tree_oid,
                    ],
                )
                .await?,
                "reading retained candidate paths",
            )?;
            let paths = paths
                .split('\0')
                .filter(|path| !path.is_empty())
                .map(str::to_owned)
                .collect();
            Ok((pinned, paths))
        }
        .await;
        let cleanup = Self::remove_passive_git_leaves(
            &git,
            &[key.as_str()],
            "removing passive retained-patch staging",
        );
        Self::finalize_passive_git_write(
            &[&guard],
            || workspace.verify_ambient_identity().map_err(archive_error),
            cleanup,
            result,
        )
    }
}

pub(super) fn retained_text(text: &str, maximum: usize) -> ReviewText {
    if text.len() <= maximum {
        return ReviewText::complete(text);
    }
    let mut end = maximum;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    let retained = &text[..end];
    ReviewText::Truncated {
        text: retained.into(),
        retained_sha256: AxocoatlDaemon::bytes_sha256(retained.as_bytes()),
        original_sha256: AxocoatlDaemon::bytes_sha256(text.as_bytes()),
        original_bytes: text.len() as u64,
        offset_bytes: 0,
    }
}
