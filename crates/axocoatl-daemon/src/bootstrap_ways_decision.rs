//! Freeze review evidence before the existing Keep owner changes or removes anything.
use super::*;
use axocoatl_core::TokenUsageStats;
use axocoatl_session::execution_content::{
    ActivationEvidenceContent, ExecutionModelRef, ExecutionUsage,
};
use axocoatl_session::turn_contract::{EvidenceRef, InvocationEvidence, SessionId};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(in crate::bootstrap) struct StoredWaysCheck {
    pub command: String,
    pub duration_ms: u64,
    pub output: ReviewText,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(in crate::bootstrap) struct StoredWaysJudge {
    pub criteria: String,
    pub model: ExecutionModelRef,
    pub usage: WaysUsage,
}
fn evidence(value: impl Into<String>) -> Result<EvidenceRef, DaemonError> {
    EvidenceRef::new(value).map_err(archive_error)
}
fn now() -> Result<u64, DaemonError> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(archive_error)?
        .as_millis()
        .try_into()
        .map_err(archive_error)
}
fn unavailable<T>() -> Recorded<T> {
    Recorded::Unavailable {
        reason: UnavailableReason::NotRecorded,
    }
}
fn absent(detail: &str) -> ReviewText {
    ReviewText::Unavailable {
        reason: UnavailableReason::NotProduced,
        detail: detail.into(),
    }
}
fn items<T>(mut values: Vec<T>, limit: usize) -> RetainedItems<T> {
    let original_count = values.len() as u64;
    values.truncate(limit);
    RetainedItems {
        items: values,
        original_count,
    }
}

impl AxocoatlDaemon {
    pub(in crate::bootstrap) fn frozen_ways_lane_states_host(
        root: &SecureDir,
        set: &crate::git::AttemptSet,
        active: &HashMap<String, ActiveAttemptRun>,
    ) -> Result<Vec<crate::git::AttemptLaneStatus>, DaemonError> {
        if active.contains_key(&set.session_id) {
            return Err(archive_error(
                "Stop and settle every attempt before freezing its decision",
            ));
        }
        let states = Self::read_attempt_lane_states_host(root, set, false)?;
        if !Self::attempt_lanes_terminal(&states) {
            return Err(archive_error(
                "Stop and settle every attempt before freezing its decision",
            ));
        }
        Ok(states)
    }

    pub(in crate::bootstrap) fn ways_decision_id(
        session_id: &str,
        set_id: &str,
    ) -> Result<DecisionId, DaemonError> {
        Ok(DecisionId(evidence(format!(
            "decision-{}-{}",
            crate::attempts::session_key(session_id),
            crate::attempts::set_key(set_id)
        ))?))
    }
    pub(in crate::bootstrap) fn retained_ways_decision(
        &self,
        session_id: &str,
        set_id: &str,
    ) -> Result<Option<WaysDecisionRecord>, DaemonError> {
        let id = Self::ways_decision_id(session_id, set_id)?;
        self.with_ways_archive(session_id, |archive| {
            Ok(archive.get(&id).map_err(archive_error)?.cloned())
        })
    }
    /// The caller holds the existing attempt-operation lease and has stopped
    /// and joined all candidate owners. A retry returns the frozen record.
    pub(in crate::bootstrap) async fn freeze_ways_decision(
        &self,
        session: &Session,
        set: &crate::git::AttemptSet,
        sandbox: &Arc<dyn Sandbox>,
        selected: Option<usize>,
    ) -> Result<WaysDecisionRecord, DaemonError> {
        if let Some(saved) = self.retained_ways_decision(&session.id, &set.id)? {
            let same = match &saved.human_decision.choice {
                WaysHumanChoice::Keep { patch } => selected == Some(patch.candidate.index as usize),
                WaysHumanChoice::NoKeep => selected.is_none(),
            };
            return if same {
                Ok(saved)
            } else {
                Err(archive_error(
                    "This Ways decision already chose a different result",
                ))
            };
        }
        let limits = self.with_ways_archive(&session.id, |archive| Ok(archive.limits()))?;
        let root = Self::open_attempt_root_host(&session.working_dir, &session.id, &set.id)?;
        let admission = self.native_ways_admission(&session.id, set, &root)?;
        // The caller owns the operation lease and has joined the executors.
        // That cleanup lease is not evidence of a live candidate: after a
        // restart, its old Running record must remain Interrupted here too.
        let states =
            Self::frozen_ways_lane_states_host(&root, set, &*self.active_attempts.lock().await)?;
        let indexes = set.lanes.iter().map(|lane| lane.index).collect::<Vec<_>>();
        let usages = Self::read_lane_usage_host(&root, &indexes).await?;
        let outputs = Self::read_lane_outputs_host(&root, &indexes).await?;
        let checked: Vec<StoredCheckedTree> =
            Self::read_host_json_file(&root, std::path::Path::new("checked-trees.json"))?
                .unwrap_or_default();
        let verdicts: Vec<crate::git::LaneVerdict> =
            Self::read_host_json_file(&root, std::path::Path::new("verdicts.json"))?
                .unwrap_or_default();
        let set_id = WaysSetId(evidence(set.id.clone())?);
        let decision_id = Self::ways_decision_id(&session.id, &set.id)?;
        let token = self
            .session_dispatch_lifecycles
            .session_team_token(&session.id)?;
        let repository_ref=self.session_dispatch_lifecycles.with_session_team_stores(&token,|_,content,_|{
            Ok(content.retain_activation_evidence(ActivationEvidenceContent::Repository{
                description:serde_json::to_string(&serde_json::json!({"workspace_id":session.workspace_id,"session_id":session.id,"path":session.working_dir,"set_id":set.id,"tree_oid":set.base_tree})).map_err(archive_error)?,revision:Some(set.base_sha.clone())
            }).map_err(archive_error)?.reference().clone())
        })?;
        let mut candidates = Vec::new();
        let mut pins = Vec::new();
        let mut cleanup = Vec::new();
        for lane in &set.lanes {
            let id = WaysCandidateId {
                set_id: set_id.clone(),
                index: lane.index.try_into().map_err(archive_error)?,
            };
            let identity = admission
                .candidates
                .iter()
                .find(|candidate| candidate.index == lane.index)
                .ok_or_else(|| archive_error("Missing exact candidate admission"))?;
            let state = states
                .iter()
                .find(|state| state.index == lane.index)
                .ok_or_else(|| archive_error("Missing candidate settlement"))?;
            let terminal = match state.state {
                crate::git::AttemptLaneState::Completed => WaysTerminalState::Completed,
                crate::git::AttemptLaneState::Failed => WaysTerminalState::Failed,
                crate::git::AttemptLaneState::Cancelled => WaysTerminalState::Cancelled,
                crate::git::AttemptLaneState::Interrupted => WaysTerminalState::Interrupted,
                _ => return Err(archive_error("Candidate is still live")),
            };
            // Even an unchecked or failed attempt may contain useful work.
            // Freeze it now through the same protected tree capture as Checks.
            let captured;
            let candidate_tree =
                if let Some(tree) = checked.iter().find(|tree| tree.index == lane.index) {
                    tree
                } else {
                    captured = self
                        .capture_attempt_candidate(sandbox, set, lane.index, true)
                        .await?;
                    &captured.checked
                };
            let (pin, paths) = self
                .retained_checked_patch(sandbox, set, candidate_tree, limits.aggregate_bytes)
                .await?;
            let patch = ProtectedWaysPatch {
                candidate: id.clone(),
                base_commit_oid: set.base_sha.clone(),
                base_tree_oid: set.base_tree.clone(),
                candidate_commit_oid: candidate_tree.commit_oid.clone(),
                candidate_tree_oid: candidate_tree.tree_oid.clone(),
                patch_sha256: pin.sha256.clone(),
                patch_bytes: pin.byte_len,
                protected_artifact_ref: pin.reference.clone(),
            };
            let diff = match String::from_utf8(pin.bytes().map_err(archive_error)?) {
                Ok(text) => retained_text(&text, limits.field_bytes),
                Err(_) => ReviewText::Unavailable {
                    reason: UnavailableReason::UnsupportedRepresentation,
                    detail:
                        "The protected binary patch is retained for export; it is not UTF-8 text."
                            .into(),
                },
            };
            pins.push(pin);
            let trace = Self::read_host_json_file::<crate::trajectory::Trajectory>(
                &root,
                std::path::Path::new(&format!("trace-{}.json", lane.index)),
            )?;
            let route = match trace {
                Some(trace) => retained_text(
                    &serde_json::to_string_pretty(&trace).map_err(archive_error)?,
                    limits.field_bytes,
                ),
                None => absent("No route was produced"),
            };
            let tools = self.session_dispatch_lifecycles.with_session_team_stores(
                &token,
                |canonical, _, _| {
                    let snapshot = canonical
                        .snapshot(&admission.source_turn_id)
                        .map_err(archive_error)?;
                    let mut tools = Vec::new();
                    for invocation in snapshot
                        .contract()
                        .invocations()
                        .iter()
                        .filter(|invocation| invocation.activation == identity.activation)
                    {
                        let detail =
                            serde_json::to_string(&invocation.evidence).map_err(archive_error)?;
                        let reference = match &invocation.evidence {
                            InvocationEvidence::Outcome { evidence, .. }
                            | InvocationEvidence::CancelledBeforeDispatch { evidence } => {
                                evidence.clone()
                            }
                            _ => evidence(format!(
                                "invocation-{}",
                                invocation.invocation_id.as_str()
                            ))?,
                        };
                        tools.push(WaysToolReference {
                            invocation_id: invocation.invocation_id.clone(),
                            event_ref: reference,
                            detail: retained_text(&detail, limits.field_bytes),
                        });
                    }
                    Ok(items(tools, limits.items_per_field))
                },
            )?;
            let mut checks = Vec::new();
            if let Some(verdict) = verdicts.iter().find(|verdict| verdict.index == lane.index) {
                let facts: Option<StoredWaysCheck> = Self::read_host_json_file(
                    &root,
                    std::path::Path::new(&format!("check-evidence-{}.json", lane.index)),
                )?;
                let checked_patch =
                    if verdict.patch_sha256.as_deref() == Some(patch.patch_sha256.as_str()) {
                        Recorded::Available {
                            value: patch.clone(),
                        }
                    } else {
                        unavailable()
                    };
                let outcome =
                    if verdict.passed && matches!(checked_patch, Recorded::Available { .. }) {
                        WaysCheckOutcome::Passed
                    } else if verdict.exit_code != 0 {
                        WaysCheckOutcome::Failed
                    } else {
                        WaysCheckOutcome::VerificationRejected {
                            reason: retained_text(&verdict.output, limits.field_bytes),
                        }
                    };
                checks.push(WaysCheckEvidence {
                    check_id: evidence(format!(
                        "check-{}-{}",
                        crate::attempts::set_key(&set.id),
                        lane.index
                    ))?,
                    command: facts
                        .as_ref()
                        .map(|facts| Recorded::Available {
                            value: facts.command.clone(),
                        })
                        .unwrap_or_else(unavailable),
                    outcome,
                    exit_code: Recorded::Available {
                        value: verdict.exit_code,
                    },
                    output: facts
                        .as_ref()
                        .map(|facts| facts.output.clone())
                        .unwrap_or_else(|| retained_text(&verdict.output, limits.field_bytes)),
                    duration_ms: facts
                        .map(|facts| Recorded::Available {
                            value: facts.duration_ms,
                        })
                        .unwrap_or_else(unavailable),
                    checked_patch,
                });
            }
            let usage = usages.iter().find(|usage| usage.index == lane.index);
            let tokens = usage
                .map(|usage| TokenUsageStats {
                    input_tokens: usage.input_tokens as usize,
                    output_tokens: usage.output_tokens as usize,
                    reasoning_tokens: Some(usage.reasoning_tokens as usize),
                })
                .unwrap_or_default();
            let usage = WaysUsage {
                measurement_id: evidence(format!(
                    "usage-{}-{}",
                    crate::attempts::set_key(&set.id),
                    lane.index
                ))?,
                tokens: if usage.is_some_and(|usage| usage.token_usage_known) {
                    ExecutionUsage::Measured { usage: tokens }
                } else {
                    ExecutionUsage::Unknown {
                        known_subtotal: tokens,
                    }
                },
                cost_usd_known_subtotal: usage.map_or(0.0, |usage| usage.cost_usd),
                cost_complete: usage.is_some_and(|usage| usage.cost_known),
            };
            let reason = state
                .error
                .as_ref()
                .map(|error| Recorded::Available {
                    value: retained_text(error, limits.field_bytes),
                })
                .unwrap_or_else(|| {
                    if paths.is_empty() {
                        Recorded::Available {
                            value: ReviewText::complete("This attempt made no repository change"),
                        }
                    } else {
                        unavailable()
                    }
                });
            candidates.push(WaysCandidateEvidence {
                id: id.clone(),
                agent: identity.definition.definition_id.clone(),
                model: Recorded::Available {
                    value: identity.model.clone(),
                },
                isolation: Recorded::Available {
                    value: self.config.sandbox.backend.clone(),
                },
                terminal,
                failure_or_no_change_reason: reason,
                outcome: outputs
                    .iter()
                    .find(|output| output.index == lane.index)
                    .map(|output| retained_text(&output.content, limits.field_bytes))
                    .unwrap_or_else(|| absent("No outcome was produced")),
                route,
                tools,
                changed_paths: items(paths, limits.items_per_field),
                patch: Recorded::Available { value: patch },
                reviewable_diff: diff,
                checks,
                usage,
            });
            let container = crate::attempts::container_id(&session.id, &set.id, lane.index);
            for (kind, backend, resource_id) in [
                (
                    WaysResourceKind::Sandbox,
                    "podman",
                    format!("axo-ses-{container}"),
                ),
                (
                    WaysResourceKind::DependencyVolume,
                    "podman",
                    SessionSandbox::dependency_volume_name(&container),
                ),
                (
                    WaysResourceKind::Clone,
                    "filesystem",
                    crate::attempts::worktree_path(
                        &session.working_dir,
                        &session.id,
                        &set.id,
                        lane.index,
                    )
                    .to_string_lossy()
                    .into_owned(),
                ),
                (
                    WaysResourceKind::DisposableCandidateGitRefs,
                    "git",
                    crate::attempts::checked_candidate_ref(&set.id, lane.index),
                ),
            ] {
                cleanup.push(WaysCleanupTarget {
                    candidate: id.clone(),
                    kind,
                    backend: backend.into(),
                    resource_id,
                    ownership_ref: admission.request.clone(),
                    outcome: WaysCleanupOutcome::Pending,
                });
            }
        }
        let inventory_candidate = candidates
            .first()
            .ok_or_else(|| archive_error("Empty Ways decision"))?
            .id
            .clone();
        for resource_id in [
            crate::attempts::base_ref(&set.id),
            crate::attempts::keep_preimage_ref(&set.id),
            crate::attempts::keep_postimage_ref(&set.id),
            format!("refs/heads/{}", crate::attempts::clone_branch(&set.id)),
        ] {
            cleanup.push(WaysCleanupTarget {
                candidate: inventory_candidate.clone(),
                kind: WaysResourceKind::DisposableCandidateGitRefs,
                backend: "git".into(),
                resource_id,
                ownership_ref: admission.request.clone(),
                outcome: WaysCleanupOutcome::Pending,
            });
        }
        cleanup.push(WaysCleanupTarget {
            candidate: inventory_candidate,
            kind: WaysResourceKind::Clone,
            backend: "filesystem".into(),
            resource_id: root.path().to_string_lossy().into_owned(),
            ownership_ref: admission.request.clone(),
            outcome: WaysCleanupOutcome::Pending,
        });
        // Checks clear a previous ranking with JSON null. Keep and no-Keep
        // must accept that same tombstone as the live results projection.
        let judgment = Self::read_host_json_file::<Option<crate::git::Judgment>>(
            &root,
            std::path::Path::new("judgment.json"),
        )?
        .flatten();
        let judge = if let Some(judgment) = judgment {
            let facts:StoredWaysJudge=Self::read_host_json_file(&root,std::path::Path::new("judge-evidence.json"))?.ok_or_else(||archive_error("Judge lacks immutable model/criteria evidence; rerun Judge before closing this decision"))?;
            let patches = judgment
                .candidates
                .iter()
                .map(|ranked| {
                    candidates
                        .iter()
                        .find(|candidate| candidate.id.index as usize == ranked.index)
                        .and_then(|candidate| match &candidate.patch {
                            Recorded::Available { value } => Some(value.clone()),
                            _ => None,
                        })
                        .ok_or_else(|| archive_error("Judged candidate patch is unavailable"))
                })
                .collect::<Result<Vec<_>, _>>()?;
            Some(WaysJudgeEvidence {
                judgment_id: evidence(format!("judge-{}", crate::attempts::set_key(&set.id)))?,
                criteria: retained_text(&facts.criteria, limits.field_bytes),
                model: facts.model,
                candidate_patches: patches,
                result: retained_text(
                    &serde_json::to_string_pretty(&judgment).map_err(archive_error)?,
                    limits.field_bytes,
                ),
                recommended: Some(WaysCandidateId {
                    set_id: set_id.clone(),
                    index: judgment.winner as u32,
                }),
                usage: facts.usage,
            })
        } else {
            None
        };
        let choice = match selected {
            Some(index) => WaysHumanChoice::Keep {
                patch: candidates
                    .iter()
                    .find(|candidate| candidate.id.index as usize == index)
                    .and_then(|candidate| match &candidate.patch {
                        Recorded::Available { value } => Some(value.clone()),
                        _ => None,
                    })
                    .ok_or_else(|| archive_error("Selected patch is unavailable"))?,
            },
            None => WaysHumanChoice::NoKeep,
        };
        let record = WaysDecisionRecord {
            schema_version: WAYS_DECISION_SCHEMA_VERSION,
            retention_limits_version: limits.version,
            decision_id,
            session_id: SessionId::new(session.id.clone()).map_err(archive_error)?,
            source_turn_id: admission.source_turn_id,
            set_id,
            task: retained_text(&set.task, limits.field_bytes),
            starting_repository: WaysRepositoryIdentity {
                workspace_id: evidence(session.workspace_id.clone())?,
                repository_ref,
                commit_oid: set.base_sha.clone(),
                tree_oid: set.base_tree.clone(),
            },
            candidates,
            judge,
            shared_usage: Self::read_host_json_file(
                &root,
                std::path::Path::new("preparation-usage.json"),
            )?
            .ok_or_else(|| archive_error("Shared preparation accounting is missing"))?,
            human_decision: WaysHumanDecision {
                decision_intent_id: evidence(format!(
                    "choice-{}",
                    crate::attempts::set_key(&set.id)
                ))?,
                decided_at_unix_ms: now()?,
                choice,
            },
            application: WaysApplicationOutcome::NotStarted,
            selected_session_turn: None,
            cleanup: WaysCleanupEvidence {
                inventory_complete: true,
                targets: cleanup,
                completed_at_unix_ms: None,
            },
        };
        self.with_ways_archive(&session.id, |archive| {
            archive.freeze(record.clone(), pins).map_err(archive_error)
        })?;
        Ok(record)
    }
    pub(in crate::bootstrap) fn record_ways_keep_pending(
        &self,
        session_id: &str,
        set_id: &str,
        apply: &StoredKeepApply,
    ) -> Result<(), DaemonError> {
        let mut record = self
            .retained_ways_decision(session_id, set_id)?
            .ok_or_else(|| archive_error("Keep has no frozen decision"))?;
        let WaysHumanChoice::Keep { patch } = record.human_decision.choice.clone() else {
            return Err(archive_error("Cannot apply a No keep decision"));
        };
        if patch.patch_sha256 != apply.patch_sha256
            || patch.candidate_tree_oid != apply.candidate_tree
            || patch.candidate.index as usize != apply.index
        {
            return Err(archive_error(
                "Keep apply journal differs from frozen decision",
            ));
        }
        if matches!(record.application, WaysApplicationOutcome::Applied { .. }) {
            return Ok(());
        }
        record.application = WaysApplicationOutcome::Pending {
            identity: WaysApplicationIdentity {
                operation_id: evidence(format!("keep-{}", crate::attempts::set_key(set_id)))?,
                patch,
                preimage_tree_oid: apply.preimage_tree.clone(),
                postimage_tree_oid: apply.postimage_tree.clone(),
            },
        };
        self.with_ways_archive(session_id, |archive| {
            archive.record_progress(record).map_err(archive_error)
        })
    }
    pub(in crate::bootstrap) fn record_ways_application(
        &self,
        session_id: &str,
        set_id: &str,
        link: Option<WaysSelectedSessionTurn>,
    ) -> Result<(), DaemonError> {
        let mut record = self
            .retained_ways_decision(session_id, set_id)?
            .ok_or_else(|| archive_error("Missing frozen decision"))?;
        let receipt_ref = evidence(format!("ways-result-{}", crate::attempts::set_key(set_id)))?;
        record.application = match record.application {
            WaysApplicationOutcome::Pending { identity } => WaysApplicationOutcome::Applied {
                identity,
                receipt_ref,
                applied_at_unix_ms: now()?,
            },
            WaysApplicationOutcome::NotStarted
                if matches!(record.human_decision.choice, WaysHumanChoice::NoKeep) =>
            {
                WaysApplicationOutcome::NoKeepRecorded {
                    receipt_ref,
                    recorded_at_unix_ms: now()?,
                }
            }
            WaysApplicationOutcome::Applied { .. }
            | WaysApplicationOutcome::NoKeepRecorded { .. } => return Ok(()),
            _ => return Err(archive_error("Decision application was not prepared")),
        };
        record.selected_session_turn = link;
        self.with_ways_archive(session_id, |archive| {
            archive.record_progress(record).map_err(archive_error)
        })
    }
    pub(in crate::bootstrap) fn record_ways_reconciliation_failure(
        &self,
        session_id: &str,
        set_id: &str,
        error: &DaemonError,
    ) -> Result<(), DaemonError> {
        let mut record = self
            .retained_ways_decision(session_id, set_id)?
            .ok_or_else(|| archive_error("Missing frozen Keep decision"))?;
        let identity = match record.application {
            WaysApplicationOutcome::Pending { identity }
            | WaysApplicationOutcome::ReconciliationRequired { identity, .. }
            | WaysApplicationOutcome::Failed { identity, .. } => identity,
            // Already applied evidence remains authoritative even if later
            // transcript or resource cleanup needs another attempt.
            WaysApplicationOutcome::Applied { .. } => return Ok(()),
            _ => return Err(archive_error("Keep reconciliation was not prepared")),
        };
        let limits = self.with_ways_archive(session_id, |archive| Ok(archive.limits()))?;
        record.application = WaysApplicationOutcome::ReconciliationRequired {
            identity,
            detail: retained_text(&error.to_string(), limits.field_bytes),
        };
        self.with_ways_archive(session_id, |archive| {
            archive.record_progress(record).map_err(archive_error)
        })
    }
    pub(in crate::bootstrap) fn record_ways_cleanup(
        &self,
        session_id: &str,
        set_id: &str,
    ) -> Result<(), DaemonError> {
        let mut record = self
            .retained_ways_decision(session_id, set_id)?
            .ok_or_else(|| archive_error("Missing frozen decision"))?;
        if record.cleanup.completed_at_unix_ms.is_some() {
            return Ok(());
        }
        let completed_at_unix_ms = now()?;
        for target in &mut record.cleanup.targets {
            target.outcome = WaysCleanupOutcome::Completed {
                receipt_ref: evidence(format!("cleanup-{}", crate::attempts::set_key(set_id)))?,
                completed_at_unix_ms,
            };
        }
        record.cleanup.completed_at_unix_ms = Some(completed_at_unix_ms);
        self.with_ways_archive(session_id, |archive| {
            archive.record_progress(record).map_err(archive_error)
        })
    }
    pub(in crate::bootstrap) async fn resume_disposed_ways_cleanup(
        &self,
        session_id: &str,
        set_id: &str,
    ) -> Result<(), DaemonError> {
        let record = self
            .retained_ways_decision(session_id, set_id)?
            .ok_or_else(|| archive_error("Missing retained cleanup ownership"))?;
        if record.cleanup.completed_at_unix_ms.is_some() {
            return Ok(());
        }
        let kept_index = match &record.application {
            WaysApplicationOutcome::Applied { identity, .. }
                if record.selected_session_turn.is_some() =>
            {
                Some(identity.patch.candidate.index as usize)
            }
            WaysApplicationOutcome::NoKeepRecorded { .. } => None,
            _ => {
                return Err(archive_error(
                    "Finish application and Session recording before cleanup",
                ))
            }
        };
        if self
            .session_dispatch_lifecycles
            .live_native_turns()?
            .iter()
            .any(|(session, _)| session == session_id)
        {
            return Err(archive_error(
                "Finish live Session work before retrying repository cleanup",
            ));
        }
        if self
            .peek_current_attempt_set(session_id)
            .await?
            .is_some_and(|current| current.id != set_id)
        {
            return Err(archive_error(
                "Finish the current Ways set before retrying older cleanup",
            ));
        }
        let session = self
            .get_session(session_id)
            .await
            .ok_or_else(|| archive_error("Session unavailable"))?;
        let mut lanes = Vec::new();
        for (index, candidate) in record.candidates.iter().enumerate() {
            if candidate.id.index as usize != index {
                return Err(archive_error("Retained cleanup candidate order is invalid"));
            }
            let model = match &candidate.model {
                Recorded::Available { value } => Some(value),
                _ => None,
            };
            lanes.push(crate::git::Variant {
                index,
                branch: crate::attempts::branch_name(set_id, index),
                worktree: crate::attempts::worktree_path(
                    &session.working_dir,
                    session_id,
                    set_id,
                    index,
                )
                .to_string_lossy()
                .into_owned(),
                model: model.map(|model| model.model_id.clone()),
                provider: model.map(|model| model.provider_id.clone()),
                agent: Some(candidate.agent.as_str().to_owned()),
            });
        }
        let set = crate::git::AttemptSet {
            id: set_id.into(),
            session_id: session_id.into(),
            task: String::new(),
            instruction: String::new(),
            base_sha: record.starting_repository.commit_oid,
            base_tree: record.starting_repository.tree_oid,
            state: if kept_index.is_some() {
                crate::git::AttemptSetState::TranscriptRecorded
            } else {
                crate::git::AttemptSetState::Discarding
            },
            kept_index,
            created_at: record.human_decision.decided_at_unix_ms,
            lanes,
        };
        self.stop_attempt_runtime(session_id, &set).await?;
        self.remove_attempt_worktrees(session_id, &set).await?;
        self.record_ways_cleanup(session_id, set_id)
    }
}
