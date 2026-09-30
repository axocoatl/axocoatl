//! Stream notifications only wake this scan. Authority and causal identity come
//! from the source Session's canonical turn and protected repository evidence.
use super::*;
use axocoatl_session::session_history::HistoryVisibility;

impl AxocoatlDaemon {
    pub fn standing_work_sessions(&self) -> Result<Vec<String>, DaemonError> {
        let inbox = self.work_inbox()?;
        let mut sessions = std::collections::BTreeSet::new();
        for binding in inbox.bindings().map_err(work_error)? {
            sessions.insert(binding.binding.session_id.clone());
        }
        Ok(sessions.into_iter().collect())
    }

    pub fn reconcile_internal_session_work(&self) -> Result<(), DaemonError> {
        self.require_runtime_admission()?;
        let bindings = {
            let inbox = self.work_inbox()?;
            inbox
                .bindings()
                .map_err(work_error)?
                .iter()
                .filter(|binding| {
                    binding.armed
                        && matches!(binding.source, TeamWorkSource::SessionCompletion { .. })
                        && inbox
                            .current_binding(&binding.binding.binding_id)
                            .ok()
                            .flatten()
                            == Some(*binding)
                })
                .cloned()
                .collect::<Vec<_>>()
        };
        for binding in bindings {
            self.admit_canonical_candidates(&binding)?;
        }
        Ok(())
    }

    fn admit_canonical_candidates(
        &self,
        binding: &ArmedTeamWorkBinding,
    ) -> Result<(), DaemonError> {
        let TeamWorkSource::SessionCompletion {
            session_id: source_session,
        } = &binding.source
        else {
            return Ok(());
        };
        let Some(history) = self
            .session_dispatch_lifecycles
            .history_snapshot(source_session)?
        else {
            return Err(work_error(
                "The armed source Session has no retained canonical history",
            ));
        };
        let entries = history.entries(HistoryVisibility::IncludingSuperseded);
        let start = match &binding.source_after_turn {
            Some(frontier) => {
                entries
                    .iter()
                    .position(|entry| entry.turn_id() == frontier)
                    .ok_or_else(|| {
                        work_error("The armed source frontier is missing; review the work binding")
                    })?
                    + 1
            }
            None => 0,
        };
        let token = self
            .session_dispatch_lifecycles
            .session_team_token(source_session)?;
        for entry in entries.into_iter().skip(start) {
            let SessionHistoryEntry::ExecutionV2(turn) = entry else {
                continue;
            };
            if turn.state != LogicalTurnState::Completed {
                continue;
            }
            let event_id = format!("completed:{}:{}", source_session, turn.turn_id.as_str());
            if self
                .work_inbox()?
                .receipts()
                .map_err(work_error)?
                .iter()
                .any(|receipt| {
                    receipt.request.binding.binding_id == binding.binding.binding_id
                        && receipt.request.event.source_id == binding.binding.source_id
                        && receipt.request.event.event_id == event_id
                })
            {
                continue;
            }
            let observation = self.session_dispatch_lifecycles.with_session_team_stores(&token, |canonical,content,_| {
                if canonical.identity().map_err(work_error)?.owner().workspace_id != binding.binding.workspace_id { return Err(work_error("An internal source cannot cross Workspace authority")); }
                let snapshot = canonical.snapshot(&turn.turn_id).map_err(work_error)?;
                let Some(candidate) = content.completed_repository_candidate(&snapshot).map_err(work_error)? else { return Ok(None); };
                let mut initial_tree = None;
                for activation in snapshot.contract().activations() {
                    let captures = content.repository_snapshots(&snapshot,&activation.activation).map_err(work_error)?;
                    if let Some(initial) = captures.into_iter().find(|capture|capture.content.phase == axocoatl_session::execution_content::RepositorySnapshotPhase::Before && capture.content.tree_sha256.is_some()) { initial_tree = initial.content.tree_sha256; break; }
                }
                Ok(Some((snapshot.journal_id().to_owned(),candidate,initial_tree)))
            })?;
            let Some((journal, candidate, initial_tree)) = observation else {
                continue;
            };
            let version = candidate
                .content
                .tree_sha256
                .clone()
                .ok_or_else(|| work_error("Source candidate identity is unavailable"))?;
            let mut inbox = self.work_inbox()?;
            // Check the current armed version under the admission lock too.
            if inbox
                .current_binding(&binding.binding.binding_id)
                .map_err(work_error)?
                != Some(binding)
            {
                continue;
            }
            let parent = inbox
                .receipts()
                .map_err(work_error)?
                .iter()
                .find(|receipt| {
                    receipt.request.binding.session_id == *source_session
                        && receipt.turn_id == turn.turn_id.as_str()
                });
            let correlation_id = parent
                .map(|parent| parent.request.event.correlation_id.clone())
                .unwrap_or_else(|| format!("source:{}:{}", source_session, turn.turn_id.as_str()));
            let unchanged_self = parent.is_some()
                && source_session == &binding.binding.session_id
                && initial_tree.as_ref() == Some(&version);
            let already_reviewed = inbox.receipts().map_err(work_error)?.iter().any(|receipt| {
                receipt.request.binding.binding_id == binding.binding.binding_id
                    && receipt.request.event.correlation_id == correlation_id
                    && receipt.request.event.subject.kind == "tree_sha256"
                    && receipt.request.event.subject.version == version
            });
            let canonical_source = serde_json::json!({"journal":journal,"source_session":source_session,"source_turn":turn.turn_id,"candidate":candidate.reference,"tree_sha256":version});
            let event = TeamWorkEvent {
                source_id: binding.binding.source_id.clone(),
                event_id,
                event_kind: binding.binding.event_kind.clone(),
                content_sha256: format!(
                    "{:x}",
                    Sha256::digest(serde_json::to_vec(&canonical_source).map_err(work_error)?)
                ),
                correlation_id,
                caused_by_turn_id: Some(turn.turn_id.as_str().into()),
                subject: TeamWorkSubject {
                    kind: "tree_sha256".into(),
                    reference_id: format!("{}:{}", source_session, turn.turn_id.as_str()),
                    version,
                },
                evidence_refs: vec![candidate.reference.as_str().into()],
            };
            let request = TeamWorkRequest {
                binding: binding.binding.clone(),
                event,
            };
            if unchanged_self || already_reviewed {
                inbox.admit_bound_no_work(request,now_ms()?,"The verified causal event repeats an unchanged candidate; inspect the original turn instead of starting another execution".into()).map_err(work_error)?;
            } else {
                inbox.admit_bound(request, now_ms()?).map_err(work_error)?;
            }
        }
        Ok(())
    }

    pub fn record_standing_work_blocked(
        &self,
        session_id: &str,
        receipt_id: &str,
        reason: String,
    ) -> Result<(), DaemonError> {
        self.work_receipt(session_id, receipt_id)?;
        let mut reason = reason.replace(['\n', '\r', '\t'], " ");
        let mut end = reason.len().min(4096);
        while !reason.is_char_boundary(end) {
            end -= 1;
        }
        reason.truncate(end);
        self.work_inbox()?
            .record_blocked(receipt_id, Some(reason))
            .map_err(work_error)
    }
}
