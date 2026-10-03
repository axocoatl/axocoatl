//! Agents' proposals (`request_network_access`): recorded, kept for a
//! person, approved only through the ordinary per-Session allow.
use super::tests::{open, FakeResolver};
use super::*;
use crate::session_dispatch::HostInvocationTool;
use crate::session_dispatch_network_tool::{
    parse_arguments, RequestNetworkAccessTool, NOT_EGRESS_REFUSAL, READ_ONLY_REFUSAL,
};
use crate::session_network_proposals::MAX_PENDING_PROPOSALS;
use axocoatl_config::EgressHostYaml;
use axocoatl_session::control_authority::ExecutionProfile;

/// An in-memory record whose ordinary appends can be made to fail, and
/// whose lines a test can drop. Its next control append can be held until
/// the test lets it go, and control appends can be made to fail.
#[derive(Debug, Default)]
struct Record {
    lines: Mutex<Vec<NetworkLine>>,
    fail: Mutex<Option<RecordFailure>>,
    fail_control: Mutex<Option<RecordFailure>>,
    hold_next_control: std::sync::atomic::AtomicBool,
    held: tokio::sync::Notify,
    go: tokio::sync::Notify,
}

impl Record {
    fn events(&self) -> Vec<NetworkEvent> {
        self.lines
            .lock()
            .unwrap()
            .iter()
            .map(|line| line.event.clone())
            .collect()
    }

    fn retain(&self, keep: impl Fn(&NetworkEvent) -> bool) {
        self.lines.lock().unwrap().retain(|line| keep(&line.event));
    }

    fn push(&self, event: NetworkEvent) -> u64 {
        let mut lines = self.lines.lock().unwrap();
        let seq = lines.len() as u64 + 1;
        lines.push(NetworkLine {
            v: 1,
            seq,
            ts_ms: 1,
            event,
        });
        seq
    }
}

#[async_trait::async_trait]
impl EgressRecordSink for Record {
    async fn append(&self, event: NetworkEvent) -> Result<u64, RecordFailure> {
        if let Some(failure) = self.fail.lock().unwrap().clone() {
            return Err(failure);
        }
        Ok(self.push(event))
    }

    async fn append_control(&self, event: NetworkEvent) -> Result<u64, RecordFailure> {
        if let Some(failure) = self.fail_control.lock().unwrap().clone() {
            return Err(failure);
        }
        if self
            .hold_next_control
            .swap(false, std::sync::atomic::Ordering::SeqCst)
        {
            self.held.notify_one();
            self.go.notified().await;
        }
        Ok(self.push(event))
    }

    async fn history(&self) -> Result<Vec<NetworkLine>, RecordFailure> {
        Ok(self.lines.lock().unwrap().clone())
    }
}

fn config() -> EgressPolicyConfig {
    EgressPolicyConfig {
        session_allow: vec![EgressAllowYaml::Host(EgressHostYaml {
            host: "a.test".into(),
            ports: None,
        })],
        session_private: Vec::new(),
        browser: None,
        ..Default::default()
    }
}

async fn opened(record: Arc<Record>) -> (Arc<SessionEgress>, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let egress = SessionEgress::open(
        "ses-1",
        config(),
        record,
        FakeResolver::with(&[
            ("a.test", &["93.184.216.1"]),
            ("api.test", &["93.184.216.2"]),
        ]),
        Some(SecureDir::open(dir.path()).unwrap()),
    )
    .await
    .unwrap();
    (egress, dir)
}

fn request(host: &str, ports: &[u16]) -> ProposalRequest {
    ProposalRequest {
        host: host.into(),
        ports: ports.to_vec(),
        reason: "the build fetches its schema from here".into(),
        agent: "writer".into(),
        invocation_id: "inv-1".into(),
        activation_id: "act-1".into(),
    }
}

async fn agent_hash(egress: &SessionEgress) -> (EgressGrant, String) {
    let grant = egress
        .grant(GrantSpec {
            invocation_id: Some("inv-2".into()),
            agent: Some("writer".into()),
            ..GrantSpec::new(GrantKind::Agent)
        })
        .await
        .unwrap();
    let url = std::fs::read_to_string(grant.env_file.as_ref().unwrap())
        .unwrap()
        .lines()
        .find_map(|line| line.strip_prefix("HTTPS_PROXY=").map(str::to_string))
        .unwrap();
    let token = url
        .trim_start_matches("http://axo:")
        .trim_end_matches("@127.0.0.1:3128")
        .to_string();
    (grant, credential_hash(&token))
}

fn proposals(record: &Record) -> Vec<NetworkEvent> {
    record
        .events()
        .into_iter()
        .filter(|event| matches!(event, NetworkEvent::Proposal { .. }))
        .collect()
}

#[tokio::test]
async fn a_proposal_is_recorded_and_waits_for_a_person() {
    let record = Arc::new(Record::default());
    let (egress, _dir) = opened(record.clone()).await;
    let proposed = egress.propose(request("API.test.", &[443])).await.unwrap();
    assert!(proposed.created);
    assert!(is_proposal_id_like(&proposed.view.id));
    assert_eq!(proposed.view.host, "api.test");
    assert_eq!(*proposed.outcome.borrow(), ProposalState::Pending);
    let recorded = proposals(&record);
    assert_eq!(recorded.len(), 1);
    let NetworkEvent::Proposal {
        id,
        state,
        host,
        ports,
        reason,
        agent,
        invocation_id,
        activation_id,
        actor,
        ..
    } = &recorded[0]
    else {
        unreachable!()
    };
    assert_eq!(id, &proposed.view.id);
    assert_eq!(*state, ProposalState::Pending);
    assert_eq!((host.as_str(), ports.as_slice()), ("api.test", &[443][..]));
    assert_eq!(
        reason.as_deref(),
        Some("the build fetches its schema from here")
    );
    assert_eq!(agent.as_deref(), Some("writer"));
    assert_eq!(invocation_id.as_deref(), Some("inv-1"));
    assert_eq!(activation_id.as_deref(), Some("act-1"));
    assert_eq!(*actor, None);
    // A proposal changes nothing by itself.
    assert!(egress
        .policy(EgressScope::Session)
        .unwrap()
        .match_name("api.test", 443)
        .is_none());
    assert_eq!(egress.proposals(), vec![proposed.view]);
}

fn is_proposal_id_like(id: &str) -> bool {
    axocoatl_session::network_record::is_proposal_id(id)
}

#[tokio::test]
async fn approval_allows_the_host_for_the_session_and_wakes_the_waiting_call() {
    let record = Arc::new(Record::default());
    let (egress, _dir) = opened(record.clone()).await;
    let (_grant, hash) = agent_hash(&egress).await;
    assert!(matches!(
        egress.decide(open(1, "api.test", 443, Some(&hash))).await,
        Decision::Deny { .. }
    ));
    let mut proposed = egress.propose(request("api.test", &[443])).await.unwrap();
    let id = proposed.view.id.clone();
    let waiting = tokio::spawn(async move {
        *proposed
            .outcome
            .wait_for(|state| *state != ProposalState::Pending)
            .await
            .unwrap()
    });
    let (revision, digest) = egress
        .approve_proposal(&id, "human", "c-approve")
        .await
        .unwrap();
    assert_eq!(revision, 2);
    assert_eq!(
        digest,
        egress.policy(EgressScope::Session).unwrap().digest()
    );
    assert_eq!(
        tokio::time::timeout(std::time::Duration::from_secs(5), waiting)
            .await
            .unwrap()
            .unwrap(),
        ProposalState::Approved
    );
    // A new connection is allowed at once, under the Session's own allow.
    let decision = egress.decide(open(2, "api.test", 443, Some(&hash))).await;
    assert!(matches!(decision, Decision::Allow { .. }), "{decision:?}");
    let events = record.events();
    let policy = events
        .iter()
        .find_map(|event| match event {
            NetworkEvent::Policy {
                source: PolicySource::SessionAllow,
                change: Some(change),
                actor,
                revision,
                ..
            } => Some((change.clone(), actor.clone(), *revision)),
            _ => None,
        })
        .unwrap();
    assert_eq!(policy.0.proposal_id.as_deref(), Some(id.as_str()));
    assert_eq!(policy.0.command_id.as_deref(), Some("c-approve"));
    assert_eq!(policy.0.host, "api.test");
    assert_eq!(policy.1.as_deref(), Some("human"));
    assert_eq!(policy.2, 2);
    assert!(events.iter().any(|event| matches!(
        event,
        NetworkEvent::Proposal { id: decided, state: ProposalState::Approved, actor: Some(actor), revision: Some(2), command_id: Some(command), .. }
            if *decided == id && actor == "human" && command == "c-approve"
    )));
    let view = egress.proposal(&id).unwrap();
    assert_eq!(
        (view.state, view.revision, view.actor.as_deref()),
        (ProposalState::Approved, Some(2), Some("human"))
    );
    // A resend of the approval is a conflict, as with the allow route.
    let resent = egress
        .approve_proposal(&id, "human", "c-approve")
        .await
        .unwrap_err();
    assert!(
        matches!(resent, EgressPolicyError::Conflict(_)),
        "{resent:?}"
    );
    // The host is a Session allow like any other: a person can revoke it.
    egress
        .revoke(EgressScope::Session, "api.test", "human", "c-revoke")
        .await
        .unwrap();
    assert!(matches!(
        egress.decide(open(3, "api.test", 443, Some(&hash))).await,
        Decision::Deny { .. }
    ));
}

#[tokio::test]
async fn rejection_changes_no_policy_and_wakes_the_waiting_call() {
    let record = Arc::new(Record::default());
    let (egress, _dir) = opened(record.clone()).await;
    let proposed = egress.propose(request("api.test", &[443])).await.unwrap();
    let id = proposed.view.id.clone();
    let digest = egress
        .policy(EgressScope::Session)
        .unwrap()
        .digest()
        .to_string();
    egress
        .reject_proposal(&id, "human", "c-reject")
        .await
        .unwrap();
    assert_eq!(*proposed.outcome.borrow(), ProposalState::Rejected);
    assert_eq!(
        egress.policy(EgressScope::Session).unwrap().digest(),
        digest
    );
    assert!(!record.events().iter().any(|event| matches!(
        event,
        NetworkEvent::Policy {
            source: PolicySource::SessionAllow,
            ..
        }
    )));
    assert!(record.events().iter().any(|event| matches!(
        event,
        NetworkEvent::Proposal { state: ProposalState::Rejected, actor: Some(actor), command_id: Some(command), .. }
            if actor == "human" && command == "c-reject"
    )));
    for second in [
        egress.reject_proposal(&id, "human", "c-again").await,
        egress
            .approve_proposal(&id, "human", "c-approve")
            .await
            .map(|_| ()),
    ] {
        assert!(
            matches!(second, Err(EgressPolicyError::Conflict(_))),
            "{second:?}"
        );
    }
    // A rejected host can be proposed again; it is a new proposal.
    let again = egress.propose(request("api.test", &[443])).await.unwrap();
    assert!(again.created);
    assert_ne!(again.view.id, id);
}

#[tokio::test]
async fn identical_pending_proposals_coalesce() {
    let record = Arc::new(Record::default());
    let (egress, _dir) = opened(record.clone()).await;
    let first = egress.propose(request("api.test", &[443])).await.unwrap();
    let second = egress.propose(request("api.test", &[443])).await.unwrap();
    assert!(first.created && !second.created);
    assert_eq!(first.view.id, second.view.id);
    assert_eq!(proposals(&record).len(), 1);
    // Other ports are another request.
    let other = egress
        .propose(request("api.test", &[443, 8443]))
        .await
        .unwrap();
    assert!(other.created);
    assert_eq!(proposals(&record).len(), 2);
    assert_eq!(egress.proposals().len(), 2);
    // One approval wakes everyone waiting on that proposal.
    egress
        .approve_proposal(&first.view.id, "human", "c-1")
        .await
        .unwrap();
    assert_eq!(*second.outcome.borrow(), ProposalState::Approved);
    assert_eq!(*other.outcome.borrow(), ProposalState::Pending);
}

#[tokio::test]
async fn invalid_and_excess_proposals_are_refused_and_not_recorded() {
    let record = Arc::new(Record::default());
    let (egress, _dir) = opened(record.clone()).await;
    for host in ["*.api.test", "10.0.0.1", "[::1]", "bad_host", ""] {
        let error = egress.propose(request(host, &[443])).await.unwrap_err();
        assert!(
            matches!(error, EgressPolicyError::Invalid(_)),
            "{host}: {error:?}"
        );
    }
    // A host the Session already allows was refused for another reason.
    let error = egress.propose(request("a.test", &[443])).await.unwrap_err();
    assert!(error.to_string().contains("already allowed"), "{error}");
    assert!(proposals(&record).is_empty());
    for index in 0..MAX_PENDING_PROPOSALS {
        egress
            .propose(request(&format!("h{index}.test"), &[443]))
            .await
            .unwrap();
    }
    let error = egress
        .propose(request("one-more.test", &[443]))
        .await
        .unwrap_err();
    assert!(matches!(error, EgressPolicyError::Conflict(_)), "{error:?}");
    assert_eq!(proposals(&record).len(), MAX_PENDING_PROPOSALS);
    // A full record refuses a proposal: nothing is kept that is not recorded.
    let full = Arc::new(Record::default());
    let (crowded, _dir) = opened(full.clone()).await;
    *full.fail.lock().unwrap() = Some(RecordFailure::Full);
    assert!(matches!(
        crowded.propose(request("api.test", &[443])).await,
        Err(EgressPolicyError::Unavailable(_))
    ));
    assert!(crowded.proposals().is_empty());
}

#[tokio::test]
async fn a_failed_approval_leaves_the_proposal_pending() {
    let record = Arc::new(Record::default());
    let (egress, _dir) = opened(record.clone()).await;
    let proposed = egress.propose(request("api.test", &[443])).await.unwrap();
    let id = proposed.view.id.clone();
    egress
        .allow(EgressScope::Session, "other.test", None, "human", "c-used")
        .await
        .unwrap();
    // The command id was used already: refused, and the proposal waits on.
    let used = egress
        .approve_proposal(&id, "human", "c-used")
        .await
        .unwrap_err();
    assert!(matches!(used, EgressPolicyError::Conflict(_)), "{used:?}");
    let invalid = egress.approve_proposal(&id, "human", "").await.unwrap_err();
    assert!(
        matches!(invalid, EgressPolicyError::Invalid(_)),
        "{invalid:?}"
    );
    assert_eq!(egress.proposal(&id).unwrap().state, ProposalState::Pending);
    assert_eq!(*proposed.outcome.borrow(), ProposalState::Pending);
    let unknown = egress
        .approve_proposal("prop_0000000000000000", "human", "c-x")
        .await
        .unwrap_err();
    assert!(
        matches!(unknown, EgressPolicyError::NotFound(_)),
        "{unknown:?}"
    );
    egress
        .approve_proposal(&id, "human", "c-new")
        .await
        .unwrap();
    assert_eq!(egress.proposal(&id).unwrap().state, ProposalState::Approved);
}

#[tokio::test]
async fn proposals_are_rebuilt_from_the_record() {
    let record = Arc::new(Record::default());
    let (egress, _dir) = opened(record.clone()).await;
    let approved = egress.propose(request("api.test", &[443])).await.unwrap();
    let rejected = egress.propose(request("b.test", &[443])).await.unwrap();
    let pending = egress.propose(request("c.test", &[8443])).await.unwrap();
    egress
        .approve_proposal(&approved.view.id, "human", "c-1")
        .await
        .unwrap();
    egress
        .reject_proposal(&rejected.view.id, "human", "c-2")
        .await
        .unwrap();
    drop(egress);

    let (reopened, _dir) = opened(record.clone()).await;
    let views = reopened.proposals();
    assert_eq!(views.len(), 3);
    assert_eq!(views[0].id, pending.view.id, "pending ones first");
    assert_eq!(views[0].state, ProposalState::Pending);
    assert_eq!(
        reopened.proposal(&approved.view.id).unwrap().state,
        ProposalState::Approved
    );
    assert_eq!(reopened.proposal(&rejected.view.id).unwrap().revision, None);
    assert_eq!(
        reopened.proposal(&rejected.view.id).unwrap().state,
        ProposalState::Rejected
    );
    // The allow the approval made is replayed with the Session's rules.
    assert!(reopened
        .policy(EgressScope::Session)
        .unwrap()
        .match_name("api.test", 443)
        .is_some());
    // A proposal left pending by the last daemon can still be decided, and
    // an Agent asking again joins it.
    let joined = reopened.propose(request("c.test", &[8443])).await.unwrap();
    assert!(!joined.created);
    assert_eq!(joined.view.id, pending.view.id);
    reopened
        .approve_proposal(&pending.view.id, "human", "c-3")
        .await
        .unwrap();
    assert_eq!(*joined.outcome.borrow(), ProposalState::Approved);
    // The command ids of earlier approvals stay used.
    let again = reopened.propose(request("d.test", &[443])).await.unwrap();
    assert!(matches!(
        reopened
            .approve_proposal(&again.view.id, "human", "c-1")
            .await,
        Err(EgressPolicyError::Conflict(_))
    ));
}

/// The approval is recorded by its `policy` line; a proposal whose
/// `approved` line was lost is still approved after a reopen.
#[tokio::test]
async fn an_approval_without_its_summary_line_is_still_approved_after_a_reopen() {
    let record = Arc::new(Record::default());
    let (egress, _dir) = opened(record.clone()).await;
    let proposed = egress.propose(request("api.test", &[443])).await.unwrap();
    egress
        .approve_proposal(&proposed.view.id, "human", "c-1")
        .await
        .unwrap();
    record.retain(|event| {
        !matches!(
            event,
            NetworkEvent::Proposal {
                state: ProposalState::Approved,
                ..
            }
        )
    });
    drop(egress);
    let (reopened, _dir) = opened(record).await;
    let view = reopened.proposal(&proposed.view.id).unwrap();
    assert_eq!(
        (view.state, view.revision, view.actor.as_deref()),
        (ProposalState::Approved, Some(2), Some("human"))
    );
}

/// A person's request that goes away while its decision is being recorded
/// (the client disconnects, or the daemon cancels the request) does not
/// interrupt the decision: it is recorded, applied and announced, and the
/// proposal is not left "being decided".
#[tokio::test]
async fn a_decision_whose_request_is_dropped_still_finishes() {
    use super::tests::wait_for;
    let record = Arc::new(Record::default());
    let (egress, _dir) = opened(record.clone()).await;
    let approved = egress.propose(request("api.test", &[443])).await.unwrap();
    let rejected = egress.propose(request("cdn.test", &[443])).await.unwrap();

    record
        .hold_next_control
        .store(true, std::sync::atomic::Ordering::SeqCst);
    tokio::select! {
        result = egress.approve_proposal(&approved.view.id, "human", "c-approve") => {
            panic!("the approval finished while its policy line was held: {result:?}")
        }
        () = record.held.notified() => {}
    }
    record.go.notify_one();
    wait_for(|| egress.proposal(&approved.view.id).unwrap().state == ProposalState::Approved).await;
    assert_eq!(*approved.outcome.borrow(), ProposalState::Approved);
    assert!(egress
        .policy(EgressScope::Session)
        .unwrap()
        .match_name("api.test", 443)
        .is_some());
    assert!(record.events().iter().any(|event| matches!(
        event,
        NetworkEvent::Policy { change: Some(change), .. }
            if change.proposal_id.as_deref() == Some(approved.view.id.as_str())
    )));

    record
        .hold_next_control
        .store(true, std::sync::atomic::Ordering::SeqCst);
    tokio::select! {
        result = egress.reject_proposal(&rejected.view.id, "human", "c-reject") => {
            panic!("the rejection finished while its line was held: {result:?}")
        }
        () = record.held.notified() => {}
    }
    record.go.notify_one();
    wait_for(|| egress.proposal(&rejected.view.id).unwrap().state == ProposalState::Rejected).await;
    assert_eq!(*rejected.outcome.borrow(), ProposalState::Rejected);
}

/// A rejection that cannot be recorded leaves the proposal pending and
/// decidable, not "being decided".
#[tokio::test]
async fn a_decision_that_cannot_be_recorded_leaves_the_proposal_decidable() {
    let record = Arc::new(Record::default());
    let (egress, _dir) = opened(record.clone()).await;
    let proposed = egress.propose(request("api.test", &[443])).await.unwrap();
    *record.fail_control.lock().unwrap() = Some(RecordFailure::Full);
    for _ in 0..2 {
        let error = egress
            .reject_proposal(&proposed.view.id, "human", "c-1")
            .await
            .unwrap_err();
        assert!(
            matches!(error, EgressPolicyError::Unavailable(_)),
            "{error:?}"
        );
        let error = egress
            .approve_proposal(&proposed.view.id, "human", "c-2")
            .await
            .unwrap_err();
        assert!(
            matches!(error, EgressPolicyError::Unavailable(_)),
            "{error:?}"
        );
        assert_eq!(
            egress.proposal(&proposed.view.id).unwrap().state,
            ProposalState::Pending
        );
    }
    *record.fail_control.lock().unwrap() = None;
    egress
        .reject_proposal(&proposed.view.id, "human", "c-3")
        .await
        .unwrap();
    assert_eq!(*proposed.outcome.borrow(), ProposalState::Rejected);
}

/// The decision point the tool asks, and whether its sidecar runs.
struct Source(Arc<SessionEgress>);

#[async_trait::async_trait]
impl crate::session_dispatch_browser::SessionEgressSource for Source {
    async fn session_egress(&self, _: &str) -> Result<Arc<SessionEgress>, String> {
        Ok(self.0.clone())
    }
    async fn session_sidecar_ready(&self, _: &str) -> bool {
        true
    }
}

fn profile(writes: Option<Vec<String>>) -> ExecutionProfile {
    ExecutionProfile {
        definition: "writer".into(),
        provider: "ollama".into(),
        model: "m".into(),
        isolation: "in-process".into(),
        tools: vec!["bash".into(), "request_network_access".into()],
        write_scope: writes,
    }
}

fn context(read_only: bool) -> crate::session_dispatch::HostInvocationContext {
    use axocoatl_session::turn_contract::{
        ActivationId, ActivationRef, ExecutionEpochId, InvocationId, LogicalTurnId, SessionId,
        TurnNodeId,
    };
    crate::session_dispatch::HostInvocationContext {
        session_id: "ses-1".into(),
        invocation_id: InvocationId::new("inv-net-1").unwrap(),
        activation: ActivationRef {
            session_id: SessionId::new("ses-1").unwrap(),
            turn_id: LogicalTurnId::new("turn-1").unwrap(),
            execution_epoch_id: ExecutionEpochId::new("epoch-1").unwrap(),
            node_id: TurnNodeId::new("writer").unwrap(),
            generation: 1,
            activation_id: ActivationId::new("act-net-1").unwrap(),
        },
        agent: "writer".into(),
        read_only,
        checkout: None,
        attempt: false,
    }
}

#[test]
fn the_tool_is_withheld_outside_egress_and_refused_to_read_only_agents() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let (egress, _dir) = runtime.block_on(opened(Arc::new(Record::default())));
    let source = Arc::new(Source(egress));
    let outside = RequestNetworkAccessTool::new(false, source.clone());
    assert!(outside.withheld(), "an Agent that lists it runs without it");
    assert_eq!(
        outside.refusal(&profile(None)).as_deref(),
        Some(NOT_EGRESS_REFUSAL)
    );
    // Withheld means an Agent that lists it is still admitted.
    let registered: HashMap<&'static str, Arc<dyn HostInvocationTool>> = HashMap::from([(
        outside.name(),
        Arc::new(outside) as Arc<dyn HostInvocationTool>,
    )]);
    assert!(crate::session_dispatch::host_tool_refusal(&registered, &profile(None)).is_none());

    let inside = RequestNetworkAccessTool::new(true, source);
    assert!(!inside.withheld());
    assert!(inside.refusal(&profile(None)).is_none());
    assert!(inside
        .refusal(&profile(Some(vec!["src/**".into()])))
        .is_none());
    assert_eq!(
        inside.refusal(&profile(Some(Vec::new()))).as_deref(),
        Some(READ_ONLY_REFUSAL)
    );
    assert!(crate::session_dispatch::is_host_invocation_tool(
        "request_network_access"
    ));
}

#[tokio::test]
async fn the_bound_tool_waits_for_the_decision_and_says_what_it_was() {
    let record = Arc::new(Record::default());
    let (egress, _dir) = opened(record.clone()).await;
    let tool = RequestNetworkAccessTool::new(true, Arc::new(Source(egress.clone())));

    // Nobody decides in time: pending, and the proposal stays.
    let pending = tool
        .bind(context(false))
        .execute(serde_json::json!({"host": "api.test", "reason": "schema", "wait_secs": 0}))
        .await
        .unwrap();
    assert_eq!(pending["decision"], "pending");
    let id = pending["proposal_id"].as_str().unwrap().to_string();
    assert_eq!(egress.proposal(&id).unwrap().state, ProposalState::Pending);
    assert!(proposals(&record).iter().any(|event| matches!(
        event,
        NetworkEvent::Proposal { invocation_id: Some(invocation), activation_id: Some(activation), agent: Some(agent), .. }
            if invocation == "inv-net-1" && activation == "act-net-1" && agent == "writer"
    )));

    // Asking again waits on the same proposal; a person approves it.
    let waiting = {
        let bound = tool.bind(context(false));
        tokio::spawn(async move {
            bound
                .execute(
                    serde_json::json!({"host": "api.test", "reason": "schema", "wait_secs": 30}),
                )
                .await
        })
    };
    super::tests::wait_for(|| egress.proposal_book().waiting(&id) > 0).await;
    egress.approve_proposal(&id, "human", "c-1").await.unwrap();
    let approved = waiting.await.unwrap().unwrap();
    assert_eq!(approved["decision"], "approved");
    assert_eq!(approved["proposal_id"], id.as_str());
    assert_eq!(approved["joined"], true);
    assert_eq!(approved["revision"], 2);
    assert!(approved["message"]
        .as_str()
        .unwrap()
        .contains("Retry the connection now"));

    // A rejection is reported as such.
    let rejecting = {
        let bound = tool.bind(context(false));
        tokio::spawn(async move {
            bound
                .execute(serde_json::json!({"host": "b.test", "ports": [8443], "reason": "mirror", "wait_secs": 30}))
                .await
        })
    };
    let id = loop {
        if let Some(view) = egress
            .proposals()
            .into_iter()
            .find(|view| view.host == "b.test")
        {
            break view.id;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    };
    egress.reject_proposal(&id, "human", "c-2").await.unwrap();
    let rejected = rejecting.await.unwrap().unwrap();
    assert_eq!(rejected["decision"], "rejected");
    assert_eq!(rejected["ports"], serde_json::json!([8443]));

    // A read-only activation of a writer Agent is refused before anything
    // is recorded.
    let before = proposals(&record).len();
    let refused = tool
        .bind(context(true))
        .execute(serde_json::json!({"host": "c.test", "reason": "x"}))
        .await
        .unwrap_err()
        .to_string();
    assert!(refused.contains("read-only"), "{refused}");
    let outside = RequestNetworkAccessTool::new(false, Arc::new(Source(egress.clone())));
    let refused = outside
        .bind(context(false))
        .execute(serde_json::json!({"host": "c.test", "reason": "x"}))
        .await
        .unwrap_err()
        .to_string();
    assert!(refused.contains("network: egress"), "{refused}");
    assert_eq!(proposals(&record).len(), before);
}

#[test]
fn tool_arguments_are_checked_like_a_persons_allow() {
    let parsed =
        parse_arguments(&serde_json::json!({"host": "API.test", "reason": "  schema  "})).unwrap();
    assert_eq!(parsed.host, "api.test");
    assert_eq!(parsed.ports, vec![443]);
    assert_eq!(parsed.reason, "schema");
    assert_eq!(parsed.wait, std::time::Duration::from_secs(120));
    let parsed = parse_arguments(
        &serde_json::json!({"host": "api.test", "ports": [8443, 443], "reason": "r", "wait_secs": 600}),
    )
    .unwrap();
    assert_eq!(parsed.ports, vec![443, 8443]);
    assert_eq!(parsed.wait, std::time::Duration::from_secs(600));
    for bad in [
        serde_json::json!({"reason": "r"}),
        serde_json::json!({"host": "api.test"}),
        serde_json::json!({"host": "api.test", "reason": "   "}),
        serde_json::json!({"host": "api.test", "reason": "x".repeat(1025)}),
        serde_json::json!({"host": "*.api.test", "reason": "r"}),
        serde_json::json!({"host": "10.1.2.3", "reason": "r"}),
        serde_json::json!({"host": "api.test", "reason": "r", "ports": []}),
        serde_json::json!({"host": "api.test", "reason": "r", "ports": [0]}),
        serde_json::json!({"host": "api.test", "reason": "r", "ports": [70000]}),
        serde_json::json!({"host": "api.test", "reason": "r", "ports": [443, 443]}),
        serde_json::json!({"host": "api.test", "reason": "r", "wait_secs": 601}),
        serde_json::json!({"host": "api.test", "reason": "r", "approve": true}),
        serde_json::json!("api.test"),
    ] {
        assert!(parse_arguments(&bad).is_err(), "{bad}");
    }
}
