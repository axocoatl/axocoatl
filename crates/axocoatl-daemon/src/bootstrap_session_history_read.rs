//! Explicitly versioned presentation consumers. These values cannot restore an
//! actor or authorize a mutation. Journal order is local to each Session.
use super::*;
use axocoatl_session::session_history::{
    SessionHistoryEntry, SessionHistorySearchHit, SessionHistoryTranscriptEntry,
};

impl AxocoatlDaemon {
    async fn readable_session_history(
        &self,
        session_id: &str,
    ) -> Result<SessionHistory, DaemonError> {
        let session = self
            .get_session(session_id)
            .await
            .ok_or_else(|| DaemonError::Session(format!("session '{session_id}' not found")))?;
        // The version selector already excludes upgraded Sessions from the old
        // one-time checkpoint import. It cannot silently create an empty prefix.
        self.ensure_session_turns_migrated(&session).await?;
        self.versioned_session_history_snapshot(session_id).await
    }

    /// Whether a History read that names no version answers in the versioned
    /// form: exactly when this Session's History holds native execution, which
    /// the legacy readers refuse. Only a retained or upgraded Session can hold
    /// it, so a legacy Session keeps its exact legacy path, and a read that
    /// fails here leaves that path to give its original answer or error.
    pub async fn session_history_is_versioned(&self, session_id: &str) -> bool {
        let native = matches!(
            self._data_dir_lease.ownership,
            axocoatl_session::execution_ownership::DataRootFormatOwnership::Upgraded(_)
        ) || self
            .session_dispatch_lifecycles
            .retains_session(session_id)
            .unwrap_or(false);
        native
            && self
                .versioned_session_history_snapshot(session_id)
                .await
                .is_ok_and(|history| history.requires_versioned_consumer())
    }

    pub async fn list_versioned_session_turns(
        &self,
        session_id: &str,
    ) -> Result<Vec<SessionHistoryEntry>, DaemonError> {
        Ok(self
            .readable_session_history(session_id)
            .await?
            .entries(HistoryVisibility::Visible)
            .into_iter()
            .cloned()
            .collect())
    }

    pub async fn get_versioned_session_turn(
        &self,
        session_id: &str,
        turn_id: &str,
    ) -> Result<Option<SessionHistoryEntry>, DaemonError> {
        // Raw strings preserve supported v1 identities. The selected Session's
        // canonical history is the complete exact lookup domain.
        Ok(self
            .readable_session_history(session_id)
            .await?
            .get(turn_id)
            .cloned())
    }

    pub async fn search_versioned_session_turns(
        &self,
        session_id: Option<&str>,
        query: &str,
    ) -> Result<Vec<SessionHistorySearchHit>, DaemonError> {
        if let Some(id) = session_id {
            return Ok(self.readable_session_history(id).await?.search(query));
        }
        // Group by stable Session identity, then each journal's own order.
        // Independent journal positions/timestamps establish no global order.
        let mut sessions = self.list_sessions().await;
        sessions.sort_by(|a, b| a.id.cmp(&b.id));
        let mut hits = Vec::new();
        for session in sessions {
            hits.extend(
                self.readable_session_history(&session.id)
                    .await?
                    .search(query),
            );
        }
        Ok(hits)
    }

    pub async fn versioned_session_transcript(
        &self,
        session_id: &str,
    ) -> Result<Vec<SessionHistoryTranscriptEntry>, DaemonError> {
        Ok(self
            .readable_session_history(session_id)
            .await?
            .transcript())
    }

    pub async fn export_versioned_session_json(
        &self,
        session_id: &str,
    ) -> Result<String, DaemonError> {
        self.readable_session_history(session_id)
            .await?
            .export_json(HistoryVisibility::Visible)
            .map_err(|error| DaemonError::Session(error.to_string()))
    }

    pub async fn export_versioned_session_markdown(
        &self,
        session_id: &str,
    ) -> Result<String, DaemonError> {
        let session = self
            .get_session(session_id)
            .await
            .ok_or_else(|| DaemonError::Session(format!("session '{session_id}' not found")))?;
        let history = self.readable_session_history(session_id).await?;
        let mut markdown = format!("# {}\n\n", session.name);
        for entry in history.entries(HistoryVisibility::Visible) {
            match entry {
                SessionHistoryEntry::LegacyV1(turn) => {
                    markdown.push_str(&legacy_turn_markdown(turn))
                }
                SessionHistoryEntry::ExecutionV2(turn) => {
                    execution_turn_markdown(&mut markdown, turn)
                }
            }
        }
        Ok(markdown)
    }
}

fn execution_turn_markdown(
    markdown: &mut String,
    turn: &axocoatl_session::execution_content::ExecutionTurnView,
) {
    use axocoatl_session::execution_content::{ActivationStreamPayload, ContentResolution};
    markdown.push_str(&format!(
        "## Turn {}\n\nState: {:?}\n\n",
        turn.turn_id.as_str(),
        turn.state
    ));
    match &turn.request {
        ContentResolution::Available { content, .. } => {
            markdown.push_str(&format!("### User\n\n{}\n\n", content.display_input));
            for context in &content.context {
                markdown.push_str(&format!(
                    "- Context: {} (`{}`)\n",
                    context.display_name, context.kind
                ));
            }
            markdown.push('\n');
        }
        ContentResolution::Missing { .. } => markdown.push_str("Request text is unavailable.\n\n"),
        ContentResolution::NotRecorded => markdown.push_str("Request text was not recorded.\n\n"),
    }
    axocoatl_session::session_history::append_turn_stop_markdown(markdown, turn);
    for activation in &turn.activations {
        let exact = &activation.activation.activation;
        markdown.push_str(&format!("### Agent {} · generation {}\n\nActivation: `{}` · state: {:?} · currently accepted: {}\n\n",
            exact.node_id.as_str(), exact.generation, exact.activation_id.as_str(), activation.activation.state, activation.currently_accepted));
        match &activation.output {
            ContentResolution::Available { content, .. } => {
                markdown.push_str(&format!("Output evidence:\n\n{}\n\n", content.text))
            }
            ContentResolution::Missing { .. } => {
                markdown.push_str("Output evidence is unavailable.\n\n")
            }
            ContentResolution::NotRecorded => {
                markdown.push_str("Final output was not recorded.\n\n")
            }
        }
        for partial in &activation.partial_outputs {
            markdown.push_str(&format!(
                "Partial output (not accepted):\n\n{}\n\n",
                partial.text
            ));
        }
        axocoatl_session::session_history::append_reserved_output_markdown(markdown, activation);
        axocoatl_session::session_history::append_guidance_markdown(markdown, activation);
        if !activation.stream.is_empty() {
            markdown.push_str("#### Observed live output\n\n");
            for event in &activation.stream {
                match &event.content.payload {
                    ActivationStreamPayload::Text { delta } => markdown.push_str(delta),
                    ActivationStreamPayload::ProviderRetry { reason } => markdown.push_str(&format!("\n\n[Provider response ended early; retried once: {reason}]\n\n")),
                    ActivationStreamPayload::ReasoningSummary { delta } => markdown.push_str(&format!("\nReasoning summary: {delta}\n")),
                    ActivationStreamPayload::ToolProposed { name, call_id, arguments_sha256, arguments_bytes, .. } => markdown.push_str(&format!("\nProposed tool: `{name}`, call `{call_id}`; input {arguments_bytes} bytes, SHA-256 `{arguments_sha256}`.\n")),
                    ActivationStreamPayload::ToolResult { name, call_id, result_sha256, result_bytes, is_error } => markdown.push_str(&format!("\nObserved tool result: `{name}`, call `{call_id}`; error: {is_error}; {result_bytes} bytes, SHA-256 `{result_sha256}`.\n")),
                }
            }
            markdown.push_str("\n\n");
        }
    }
}

pub(in crate::bootstrap) fn legacy_turn_markdown(turn: &SessionTurn) -> String {
    let mut markdown = String::new();
    markdown.push_str(&format!("## User\n\n{}\n\n", turn.user_input));
    if !turn.context.is_empty() {
        markdown.push_str("Context:\n\n");
        for reference in &turn.context {
            markdown.push_str(&format!(
                "- {} (`{}`)\n",
                reference.display_name, reference.kind
            ));
        }
        markdown.push('\n');
    }
    if !turn.execution_events.is_empty() {
        markdown.push_str("Route:\n\n");
        for execution in &turn.execution_events {
            if !matches!(
                execution.event.kind.as_str(),
                "tool_started" | "tool_result"
            ) {
                continue;
            }
            let metadata = &execution.event.metadata;
            let tool = metadata
                .get("tool_name")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("tool");
            let phase = if execution.event.kind == "tool_started" {
                "started"
            } else if metadata
                .get("is_error")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false)
            {
                "failed"
            } else {
                "result"
            };
            let value_key = if execution.event.kind == "tool_started" {
                "arguments"
            } else {
                "result"
            };
            let value = metadata
                .get(value_key)
                .cloned()
                .unwrap_or(serde_json::Value::Null);
            let explicitly_truncated = metadata
                .get(&format!("{value_key}_truncated"))
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false);
            let rendered = serde_json::to_string(&value).unwrap_or_else(|_| "null".to_string());
            let preview = truncate_utf8(&rendered, 2 * 1024);
            markdown.push_str(&format!("- `{tool}` {phase}: `{preview}`"));
            if explicitly_truncated || rendered.len() > 2 * 1024 {
                markdown.push_str(" _(truncated)_");
            }
            markdown.push('\n');
        }
        markdown.push('\n');
    }
    if turn.agent_outputs.is_empty() {
        if let Some(output) = turn
            .final_output
            .as_deref()
            .or_else(|| (!turn.partial_output.is_empty()).then_some(turn.partial_output.as_str()))
        {
            markdown.push_str(&format!("## Assistant\n\n{output}\n\n"));
        }
    } else {
        for output in turn
            .agent_outputs
            .iter()
            .filter(|output| !output.superseded)
        {
            markdown.push_str(&format!(
                "## Assistant ({})\n\n{}\n\n",
                output.agent_id, output.output
            ));
        }
    }
    if turn.status != SessionTurnLifecycle::Completed {
        markdown.push_str(&format!("_Turn status: {:?}_\n\n", turn.status));
    }
    markdown
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use axocoatl_session::execution_content::{
        ActivationOutputContent, ActivationOutputLimits, ExecutionContentStore, ExecutionUsage,
        OutputKind,
    };
    use axocoatl_session::execution_namespace::ExecutionComponent;
    use axocoatl_session::execution_ownership::LegacyFormatOwnership;
    use axocoatl_session::execution_store::{ExecutionStoreOwner, SessionExecutionStore};
    use axocoatl_session::turn_contract::{LogicalTurnId, SessionId, TurnContractEnvelope};

    #[tokio::test]
    async fn versioned_markdown_consumer_preserves_owned_reserved_prefix_and_recorded_empty_output()
    {
        // Run the close/reopen boundary without unrelated tests spawning
        // processes in this address space. On Linux a concurrent fork retains
        // even CLOEXEC lock descriptors until exec, so dropping our last handle
        // can otherwise leave a transient foreign copy of the Session lock.
        // Production locking must remain nonblocking and must not be weakened
        // to accommodate this test-harness race.
        const CHILD: &str = "AXOCOATL_TEST_MARKDOWN_REOPEN_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let result = tokio::time::timeout(
                Duration::from_secs(60),
                tokio::process::Command::new(std::env::current_exe().unwrap())
                    .args([
                        "--exact",
                        "bootstrap::session_history::read::tests::versioned_markdown_consumer_preserves_owned_reserved_prefix_and_recorded_empty_output",
                        "--nocapture",
                    ])
                    .env(CHILD, "1")
                    .kill_on_drop(true)
                    .output(),
            )
            .await
            .expect("isolated Markdown reopen test timed out")
            .unwrap();
            assert!(
                result.status.success(),
                "{}\n{}",
                String::from_utf8_lossy(&result.stdout),
                String::from_utf8_lossy(&result.stderr),
            );
            return;
        }
        for (kind, text, limit) in [
            (OutputKind::Partial, "exported-prefix with omitted tail", 15),
            (OutputKind::Final, "", 1),
        ] {
            let root = tempfile::tempdir().unwrap();
            let _legacy = SessionTurnStore::open(root.path().join("session-history")).unwrap();
            let guard = Arc::new(
                LegacyFormatOwnership::acquire(root.path())
                    .unwrap()
                    .upgrade()
                    .unwrap(),
            );
            let owner = ExecutionStoreOwner {
                workspace_id: "workspace-a".into(),
                session_id: SessionId::new("session-a").unwrap(),
            };
            let mut canonical = SessionExecutionStore::open(guard.clone(), owner.clone()).unwrap();
            let mut content = ExecutionContentStore::open_owned(
                canonical
                    .component_namespace(ExecutionComponent::ExecutionContent)
                    .unwrap(),
            )
            .unwrap();
            let fixture: serde_json::Value = serde_json::from_str(include_str!("../../axocoatl-session/tests/fixtures/turn_contract/partial_finish_is_not_success.json")).unwrap();
            for step in &fixture["steps"].as_array().unwrap()[..2] {
                let envelope: TurnContractEnvelope =
                    serde_json::from_value(step["envelope"].clone()).unwrap();
                canonical.append(envelope).unwrap();
            }
            let turn_id = LogicalTurnId::new("turn-a").unwrap();
            let snapshot = canonical.snapshot(&turn_id).unwrap();
            let exact = snapshot.contract().activations()[0].activation.clone();
            let reservation = content
                .reserve_activation_output(
                    &snapshot,
                    &exact,
                    ActivationOutputLimits {
                        partial_records: 0,
                        partial_bytes: 0,
                        settlement_bytes: limit,
                    },
                )
                .unwrap();
            content
                .settle_activation_output(
                    &reservation,
                    ActivationOutputContent {
                        activation: exact,
                        recorded_at_unix_ms: 101,
                        text: text.into(),
                        kind,
                        usage: ExecutionUsage::Unknown {
                            known_subtotal: Default::default(),
                        },
                    },
                )
                .unwrap();
            drop(canonical);
            assert!(
                matches!(
                    SessionExecutionStore::open(guard.clone(), owner.clone()),
                    Err(axocoatl_session::execution_store::ExecutionStoreError::Io(error))
                        if error.kind() == std::io::ErrorKind::WouldBlock
                ),
                "owned content must keep its canonical Session writer locked"
            );
            drop(content);
            let canonical = SessionExecutionStore::open(guard, owner).unwrap();
            let content = ExecutionContentStore::open_owned(
                canonical
                    .component_namespace(ExecutionComponent::ExecutionContent)
                    .unwrap(),
            )
            .unwrap();
            let before = std::fs::read(canonical.path()).unwrap();
            let view = content
                .project(&canonical.snapshot(&turn_id).unwrap())
                .unwrap();
            assert!(!view.activations[0].currently_accepted);
            let mut markdown = String::new();
            execution_turn_markdown(&mut markdown, &view);
            if text.is_empty() {
                assert!(markdown.contains("Recorded empty output."));
            } else {
                assert_eq!(markdown.matches("exported-prefix").count(), 1);
                assert!(markdown.contains(&format!("retained 15 of {} bytes", text.len())));
                assert!(!markdown.contains("omitted tail"));
            }
            assert!(markdown.contains("not acceptance"));
            assert!(!markdown.contains("currently accepted: true"));
            assert_eq!(std::fs::read(canonical.path()).unwrap(), before);
        }
    }
}
