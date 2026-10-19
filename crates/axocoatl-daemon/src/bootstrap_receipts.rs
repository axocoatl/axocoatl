//! Receipts of loadout runs on the live daemon (docs/design/1.4-receipts.md):
//! the statement of a finished run, signed with the data root's key and
//! written as `receipt.dsse.json` just before its Outcome; the receipt and
//! the public key served to the API.
//!
//! A receipt never says more than the host observed. What it cannot
//! establish is absent or said so: the Session's history unreadable, a turn
//! it does not hold, a path it cannot read, each named in
//! `predicate.files.unattributed`. Signing that fails never changes the
//! verdict or the exit code: the run gets the warning `receipt_unavailable`.
use super::*;
use crate::loadout::api::{ReceiptKeyView, RunReceiptView};
use crate::receipt_key;
use crate::run_attribution::RunAttribution;
use axocoatl_config::loadout::{
    parse_loadout, resolve_loadout, AgentRuntime, LoadoutSource, AUTO_CREDENTIAL,
};
use axocoatl_session::receipt::{
    self, sha256_hex, AdjudicationFacts, AgentFacts, CheckFacts, LoadoutFacts, NotCoveredFacts,
    OpenedReceipt, PolicyFacts, Predicate, RepositoryFacts, ReviewFacts, RouteFacts, RunFacts,
    SandboxFacts, Statement, UsageFacts, PREDICATE_TYPE, STATEMENT_TYPE,
};
use axocoatl_session::run_outcome::RunOutcome;
use axocoatl_session::run_record::{RunEvent, RunManifest};
use axocoatl_session::session_history::SessionHistoryEntry;

/// Network-record events read per page.
const POLICY_PAGE: usize = 1000;

/// A value as its serde name (`passed`, `needs_attention`, `writer`).
fn serde_name<T: serde::Serialize>(value: &T) -> String {
    match serde_json::to_value(value) {
        Ok(serde_json::Value::String(name)) => name,
        _ => String::new(),
    }
}

/// The routes among a recorded policy's `rules`, each with whether it
/// checked model-call bodies (`refuse`), sent them unread under
/// `provider_fetch: allow` (`allow`), or reads no bodies (absent).
fn receipt_routes(rules: &[String]) -> Vec<RouteFacts> {
    rules
        .iter()
        .filter_map(|text| crate::egress_broker::rules::RecordedRoute::parse(text))
        .map(|route| RouteFacts {
            id: route.id,
            host: route.host,
            provider_fetch: match route.provider_fetch {
                crate::egress_broker::rules::ProviderFetch::Refused => Some("refuse".into()),
                crate::egress_broker::rules::ProviderFetch::Allowed => Some("allow".into()),
                crate::egress_broker::rules::ProviderFetch::NotChecked => None,
            },
        })
        .collect()
}

/// The `origin` remote as `https://host/path` without credentials, read
/// with host git the way Keep reads remotes (nothing the repository
/// configures runs). `None` without one, or for a local path.
async fn origin_url(repo: &std::path::Path) -> Option<String> {
    let output = crate::git_host::run_bounded(
        crate::git_host::repository_git(
            &crate::git_host::HostTools::default(),
            repo,
            &["config", "--get", "remote.origin.url"],
        ),
        crate::git_host::KEEP_LOCAL_TIMEOUT,
        "git config",
    )
    .await
    .ok()?;
    if !output.status.success() {
        return None;
    }
    let url = String::from_utf8_lossy(&output.stdout).trim().to_string();
    crate::git_host::normalize_to_https(&url)
}

/// The `program_version` of a native Agent on `model` that Ollama served:
/// `ollama` and the versions the run's Outcome records for the model, or
/// `None` when no call of the model ran.
fn ollama_program_version(outcome: &RunOutcome, model: &str) -> Option<String> {
    let versions = outcome.ollama_versions.get(model)?;
    (!versions.is_empty()).then(|| format!("ollama {}", versions.join(", ")))
}

/// What the run status and the UI show of a receipt.
pub(crate) fn receipt_view(bytes: &[u8]) -> Option<RunReceiptView> {
    let opened: OpenedReceipt = receipt::open_receipt(bytes).ok()?;
    let files = &opened.statement.predicate.files;
    Some(RunReceiptView {
        key_id: opened.statement.predicate.signer.key_id.clone(),
        files: opened.statement.subject.len(),
        removed: files.removed.len(),
        source: files.source.clone(),
        complete: files.complete,
        statement_sha256: opened.payload_digest(),
    })
}

impl AxocoatlDaemon {
    /// The turns of a run: those its Outcome names, then those its record
    /// started (a run the daemon ended at startup names none in its
    /// Outcome).
    fn receipt_turn_ids(&self, run_id: &str, outcome: &RunOutcome) -> Vec<String> {
        let mut turns: Vec<String> = outcome
            .turns
            .iter()
            .map(|turn| turn.turn_id.clone())
            .collect();
        if let Ok(events) = self.loadout_run_recorded_events(run_id) {
            for event in events {
                if let RunEvent::TurnStarted { turn_id, .. } = event {
                    if !turns.contains(&turn_id) {
                        turns.push(turn_id);
                    }
                }
            }
        }
        turns
    }

    /// The paths the run's turns changed, from the Session's history. What
    /// cannot be read is named as unattributable, never left out.
    async fn receipt_attribution(&self, session_id: &str, turns: &[String]) -> RunAttribution {
        let mut attribution = RunAttribution::default();
        if turns.is_empty() {
            return attribution;
        }
        let history = match self.versioned_session_history_snapshot(session_id).await {
            Ok(history) => history,
            Err(error) => {
                attribution.unattributable.push(format!(
                    "the Session's history cannot be read, so no turn of the run is attributed: {error}"
                ));
                return attribution;
            }
        };
        let session = self.get_session(session_id).await;
        for turn_id in turns {
            match history.get(turn_id) {
                Some(SessionHistoryEntry::ExecutionV2(view)) => {
                    attribution.add_execution_turn(view)
                }
                Some(SessionHistoryEntry::LegacyV1(legacy)) => match &session {
                    Some(session) => attribution.add_legacy_turn(
                        &legacy.id,
                        rehydrated_turn_touched_paths(session, legacy),
                    ),
                    None => attribution
                        .unattributable
                        .push(format!("turn {turn_id}: its Session cannot be read")),
                },
                None => attribution
                    .unattributable
                    .push(format!("turn {turn_id} is not in the Session's history")),
            }
        }
        attribution
    }

    /// The last network policy each scope's record holds for the Session.
    async fn receipt_policies(&self, session_id: &str) -> Vec<PolicyFacts> {
        use axocoatl_session::network_record::NetworkEvent;
        let mut latest: std::collections::BTreeMap<String, PolicyFacts> = Default::default();
        let mut after = None;
        loop {
            let Ok(page) = self
                .session_network_records
                .read_after(session_id, after, POLICY_PAGE)
                .await
            else {
                break;
            };
            if page.events.is_empty() {
                break;
            }
            after = page.events.last().map(|line| line.seq);
            for line in page.events {
                if let NetworkEvent::Policy {
                    scope,
                    revision,
                    digest,
                    rules,
                    ..
                } = line.event
                {
                    let routes = receipt_routes(&rules);
                    latest.insert(
                        scope.as_str().to_string(),
                        PolicyFacts {
                            scope: scope.as_str().to_string(),
                            revision,
                            digest,
                            rules,
                            routes,
                        },
                    );
                }
            }
        }
        latest.into_values().collect()
    }

    /// The statement of `outcome`, signed by no one yet (`signer` is
    /// filled by [`Self::seal_loadout_run`]).
    async fn receipt_statement(
        &self,
        manifest: &RunManifest,
        outcome: &RunOutcome,
    ) -> Result<Statement, String> {
        let loadout = parse_loadout(&manifest.loadout_text, LoadoutSource::Builtin)
            .map_err(|error| format!("the run's loadout cannot be read: {error}"))?;
        let resolved = resolve_loadout(&loadout, &manifest.params, &manifest.task, &manifest.repo)
            .map_err(|error| format!("the run's loadout cannot be resolved: {error}"))?;
        let agents = loadout
            .file
            .agents
            .iter()
            .map(|agent| {
                let identity = resolved
                    .agent_models
                    .get(&agent.id)
                    .map(|model| crate::loadout::team_plan::model_identity(model, agent.runtime));
                AgentFacts {
                    id: agent.id.clone(),
                    role: serde_name(&agent.role),
                    runtime: agent.runtime.id().to_string(),
                    program_version: match agent.runtime {
                        // The Ollama that served the Agent's model, as the
                        // run's Outcome records it (`ollama 0.40.6`). An
                        // existing field, so a 1.4.0 verifier reads it.
                        AgentRuntime::Native => identity
                            .as_ref()
                            .filter(|identity| identity.provider == "ollama")
                            .and_then(|identity| ollama_program_version(outcome, &identity.model)),
                        AgentRuntime::ClaudeCode => Some(
                            crate::external_agent::claude_code::CLAUDE_CODE_VERSION.to_string(),
                        ),
                        AgentRuntime::Codex => {
                            Some(crate::external_agent::codex::CODEX_VERSION.to_string())
                        }
                    },
                    provider: identity
                        .as_ref()
                        .map(|identity| identity.provider.clone())
                        .unwrap_or_default(),
                    model: identity.map(|identity| identity.model).unwrap_or_default(),
                    // The credential admission chose for an external
                    // writer, as the manifest keeps it; a manifest
                    // written before 1.4 keeps none, and the
                    // loadout's own choice is used unless it was
                    // `auto`, which only admission resolved.
                    credential: match agent.runtime {
                        AgentRuntime::Native => None,
                        _ => manifest.credentials.get(&agent.id).cloned().or_else(|| {
                            resolved
                                .agent_credentials
                                .get(&agent.id)
                                .filter(|name| name.as_str() != AUTO_CREDENTIAL)
                                .cloned()
                        }),
                    },
                }
            })
            .collect();
        let turns = self.receipt_turn_ids(&manifest.run_id, outcome);
        let attribution = self.receipt_attribution(&manifest.session_id, &turns).await;
        let checks = outcome.checks.clone();
        let repo = std::path::PathBuf::from(&manifest.repo);
        let files = tokio::task::spawn_blocking(move || {
            crate::run_attribution::receipt_files(&attribution, &checks, &repo)
        })
        .await
        .map_err(|error| format!("reading the run's files: {error}"))?;
        let network = self
            .get_session(&manifest.session_id)
            .await
            .and_then(|session| session.loadout.map(|binding| binding.network))
            .unwrap_or_else(|| loadout.file.sandbox.network.clone());
        let mut areas: Vec<String> = Vec::new();
        for entry in &outcome.not_covered {
            if !areas.contains(&entry.area) {
                areas.push(entry.area.clone());
            }
        }
        let predicate = Predicate {
            run: RunFacts {
                run_id: manifest.run_id.clone(),
                session_id: manifest.session_id.clone(),
                started_at_ms: outcome.started_at_ms,
                finished_at_ms: outcome.finished_at_ms,
                axocoatl_version: env!("CARGO_PKG_VERSION").into(),
                task_sha256: sha256_hex(manifest.task.as_bytes()),
            },
            repository: RepositoryFacts {
                base_commit: manifest.repo_head.clone(),
                dirty_paths: manifest.dirty_paths.clone(),
                origin_url: origin_url(std::path::Path::new(&manifest.repo)).await,
            },
            loadout: LoadoutFacts {
                id: manifest.loadout.id.clone(),
                version: manifest.loadout.version,
                kind: manifest.loadout.kind.clone(),
                digest: manifest.loadout.digest.clone(),
                builtin: manifest.loadout.builtin,
            },
            agents,
            sandbox: SandboxFacts {
                backend: self.config.sandbox.backend.clone(),
                network,
                workload: "hardened".into(),
                policies: self.receipt_policies(&manifest.session_id).await,
            },
            network: outcome.network.clone(),
            checks: outcome
                .checks
                .iter()
                .map(|check| CheckFacts {
                    name: check.name.clone(),
                    state: serde_name(&check.state),
                    exit_code: check.exit_code,
                    timeout_ms: check.timeout_ms,
                    argv: check.argv.clone(),
                    candidate_sha256: check.candidate_sha256.clone(),
                })
                .collect(),
            review: outcome.review.as_ref().map(|review| ReviewFacts {
                provider: review.reviewer.provider.clone(),
                model: review.reviewer.model.clone(),
                runtime: review.reviewer.runtime.clone(),
                program_version: (review.reviewer.runtime == "native"
                    && review.reviewer.provider == "ollama")
                    .then(|| ollama_program_version(outcome, &review.reviewer.model))
                    .flatten(),
                verdict: review.state.clone(),
                passed: review.passed,
                rounds: review.rounds.len() as u32,
                max_rounds: review.max_rounds,
                findings: review
                    .rounds
                    .iter()
                    .map(|round| round.findings.len() as u32)
                    .sum(),
            }),
            adjudications: outcome
                .adjudications
                .iter()
                .map(|item| AdjudicationFacts {
                    round: item.round,
                    id: item.finding_id.clone(),
                    decision: serde_name(&item.decision),
                })
                .collect(),
            verdict: outcome.verdict,
            exit_code: outcome.exit_code,
            attention: outcome.attention.clone(),
            not_covered: NotCoveredFacts {
                count: outcome.not_covered.len() as u64,
                areas,
            },
            warnings: outcome
                .warnings
                .iter()
                .map(|warning| warning.code.clone())
                .collect(),
            usage: UsageFacts {
                input_tokens: outcome.usage.input_tokens,
                output_tokens: outcome.usage.output_tokens,
                cost_microunits: outcome.usage.cost_microunits,
                cost_usd: outcome.usage.cost_microunits as f64 / 1_000_000.0,
                complete: outcome.usage.complete,
                cost_known: outcome.usage.cost_known,
                cost_computed: outcome.usage.cost_computed,
            },
            files: files.facts,
            signer: Default::default(),
        };
        Ok(Statement {
            statement_type: STATEMENT_TYPE.into(),
            subject: files.subject,
            predicate_type: PREDICATE_TYPE.into(),
            predicate,
        })
    }

    /// Sign the finished run's `outcome` and write its receipt, before the
    /// Outcome itself is written (so a run that has finished has its
    /// receipt settled). The data root's key is made the first time. The
    /// error says why there is no receipt; it never changes the verdict.
    pub async fn seal_loadout_run(&self, run_id: &str, outcome: &RunOutcome) -> Result<(), String> {
        let store = self
            .loadout_runs
            .store()
            .map_err(|error| error.to_string())?;
        let manifest = store.manifest(run_id).map_err(|error| error.to_string())?;
        if outcome.run_id != manifest.run_id {
            return Err(format!("the Outcome names run {}", outcome.run_id));
        }
        let mut statement = self.receipt_statement(&manifest, outcome).await?;
        let data_dir = self.data_root.path().to_path_buf();
        let (key, created) =
            tokio::task::spawn_blocking(move || receipt_key::load_or_create(&data_dir))
                .await
                .map_err(|error| format!("reading the signing key: {error}"))??;
        if created {
            tracing::info!(
                key_id = %key.public().short_id(),
                "made this data root's receipt signing key"
            );
        }
        statement.predicate.signer = key.public().signer();
        let (envelope, _) = receipt::sign_statement(key.key_pair(), &statement)
            .map_err(|error| format!("signing: {error}"))?;
        store
            .write_receipt(run_id, &receipt::envelope_file_bytes(&envelope))
            .map_err(|error| format!("writing the receipt: {error}"))
    }

    /// [`Self::seal_loadout_run`], or, when it fails, the warning the
    /// Outcome carries instead of a receipt.
    pub async fn seal_or_warn(
        &self,
        run_id: &str,
        outcome: &RunOutcome,
    ) -> Option<axocoatl_session::run_outcome::RunWarning> {
        match self.seal_loadout_run(run_id, outcome).await {
            Ok(()) => None,
            Err(reason) => {
                tracing::warn!(run = %run_id, %reason, "a finished run could not be signed");
                Some(receipt_unavailable(&reason))
            }
        }
    }

    /// `GET /api/runs/{run_id}/receipt`: the envelope exactly as written.
    pub fn loadout_run_receipt(&self, run_id: &str) -> Result<Vec<u8>, DaemonError> {
        let store = self.loadout_runs.store()?;
        let outcome = store.outcome(run_id).map_err(|error| match error {
            axocoatl_session::run_record::RunRecordError::NotFound(run) => {
                DaemonError::NotFound(format!("no run {run}"))
            }
            other => DaemonError::Session(other.to_string()),
        })?;
        let Some(outcome) = outcome else {
            return Err(DaemonError::NotFound(format!(
                "run {run_id} has not finished; its receipt is signed when it finishes"
            )));
        };
        match store
            .receipt(run_id)
            .map_err(|error| DaemonError::Session(error.to_string()))?
        {
            Some(bytes) => Ok(bytes),
            None => {
                let why = outcome
                    .warnings
                    .iter()
                    .find(|warning| warning.code == receipt::RECEIPT_UNAVAILABLE)
                    .map(|warning| warning.message.clone())
                    .unwrap_or_else(|| {
                        "it finished before this Axocoatl signed receipts".to_string()
                    });
                Err(DaemonError::NotFound(format!(
                    "run {run_id} has no receipt: {why}"
                )))
            }
        }
    }

    /// The receipt of a finished run, as the run status shows it.
    pub(crate) fn loadout_run_receipt_view(&self, run_id: &str) -> Option<RunReceiptView> {
        let bytes = self.loadout_runs.store().ok()?.receipt(run_id).ok()??;
        receipt_view(&bytes)
    }

    /// `GET /api/receipts/key`: the data root's public key, made when there
    /// is none yet.
    pub async fn receipt_public_key(&self) -> Result<ReceiptKeyView, DaemonError> {
        let data_dir = self.data_root.path().to_path_buf();
        let (key, created) =
            tokio::task::spawn_blocking(move || receipt_key::load_or_create(&data_dir))
                .await
                .map_err(|error| DaemonError::Session(error.to_string()))?
                .map_err(DaemonError::Session)?;
        let public = key.public();
        if created {
            tracing::info!(key_id = %public.short_id(), "made this data root's receipt signing key");
        }
        Ok(ReceiptKeyView {
            algorithm: receipt::KEY_ALGORITHM.into(),
            public_key: public.base64(),
            key_id: public.key_id(),
            line: public.line(&receipt_key::host_comment()),
        })
    }
}

/// The warning of a finished run without a receipt.
pub fn receipt_unavailable(reason: &str) -> axocoatl_session::run_outcome::RunWarning {
    axocoatl_session::run_outcome::RunWarning {
        code: receipt::RECEIPT_UNAVAILABLE.into(),
        message: format!(
            "The run finished, but its receipt could not be signed: {reason}. The verdict and \
             exit code are unchanged."
        ),
    }
}
