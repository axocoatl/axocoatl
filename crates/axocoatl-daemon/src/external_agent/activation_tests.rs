//! External agents through the real Session controller (workstream
//! `agents`). A child of the repository activation tests, whose fixtures
//! (a retained Session, its repository owner, controller and grant) these
//! reuse.
//!
//! The Podman test is ignored by default:
//!
//! ```text
//! CONTAINER_CONNECTION=axocoatl-ci-pr74 \
//!   cargo test -p axocoatl-daemon --lib actual_external_claude_code -- --ignored
//! ```
//!
//! It builds `localhost/axocoatl-external-agent-test:<digest>` from the
//! recipe base (Node 24 on Debian, pinned by digest) with a fake `claude`
//! program, labeled `io.axocoatl.test=external-agent`, and removes its
//! containers, networks and volumes at the end.
use super::*;
use crate::external_agent::{self, ExternalActivationRequest};
use crate::session_dispatch::{AutonomousActivationFactory, ExternalSettings};
use axocoatl_config::loadout::AgentRuntime;

/// An activation of an external writer, as Apply and admission create it:
/// the definition exactly as `external_agent_config` shapes it.
fn run_external(f: &mut Fixture, runtime: AgentRuntime, writes: Option<&[&str]>) -> Run {
    let config = external_agent::external_agent_config(
        AgentConfig {
            id: AgentId::new("conversation"),
            name: "External writer".into(),
            system_prompt: Some("Own the change.".into()),
            writes: writes.map(|writes| writes.iter().map(|path| (*path).into()).collect()),
            ..Default::default()
        },
        runtime,
        "claude-sonnet-4-5",
    )
    .unwrap();
    run_with_config(f, config, "FAKE-TASK")
}

fn run_with_config(f: &mut Fixture, config: AgentConfig, request: &str) -> Run {
    run_with_limits(
        f,
        config,
        request,
        GrantLimits {
            activations: 2,
            invocations: 12,
            tokens: 100_000,
            cost_microunits: 1_000_000,
        },
    )
}

fn run_with_limits(f: &mut Fixture, config: AgentConfig, request: &str, limits: GrantLimits) -> Run {
    let canonical = f._canonical.take().unwrap();
    let session_id = canonical.owner().session_id.clone();
    let registry = SessionDispatchRegistry::default();
    let token = registry
        .retain_existing_session(&mut Some(held_stores(canonical)))
        .unwrap();
    let team = registry.session_team_token(session_id.as_str()).unwrap();
    let profile = ExecutionProfile {
        definition: "external-definition".into(),
        provider: config.provider.clone(),
        model: config.model.clone(),
        isolation: "in-process".into(),
        tools: config.tools.clone(),
        write_scope: config.writes.clone(),
    };
    let activation = ActivationRef {
        session_id,
        turn_id: LogicalTurnId::new("external-turn").unwrap(),
        execution_epoch_id: ExecutionEpochId::new("external-epoch").unwrap(),
        node_id: TurnNodeId::new("writer").unwrap(),
        generation: 1,
        activation_id: ActivationId::new("external-activation").unwrap(),
    };
    let definition_id = AgentDefinitionId::new(profile.definition.clone()).unwrap();
    let definition = registry
        .with_session_team_stores(&team, |_, content, _| {
            let definition = content
                .retain_activation_evidence(ActivationEvidenceContent::Definition {
                    definition_id: definition_id.clone(),
                    revision: 1,
                    profile: profile.clone(),
                    configuration: serde_json::to_string(&config).unwrap(),
                })
                .unwrap();
            Ok(DefinitionSnapshotRef {
                definition_id: definition_id.clone(),
                snapshot: definition.reference().clone(),
            })
        })
        .unwrap();
    let spec = crate::session_dispatch::SuccessorTurn {
        command_id: CommandId::new("external-begin").unwrap(),
        turn_id: activation.turn_id.clone(),
        epoch_id: activation.execution_epoch_id.clone(),
        graph: TurnGraphSnapshot {
            snapshot_id: GraphSnapshotId::new("external-graph").unwrap(),
            revision: 1,
            nodes: vec![GraphNode {
                node_id: activation.node_id.clone(),
                slot_id: SessionTeamSlotId::new("writer").unwrap(),
                definition: definition.clone(),
                conversation_id: NodeConversationId::new("conversation").unwrap(),
                starting_savepoint: ConversationSavepoint::Empty,
                required: true,
            }],
            dependencies: vec![],
            conditions: vec![],
        },
        request: ExecutionRequestContent {
            turn_id: activation.turn_id.clone(),
            recorded_at_unix_ms: 1,
            display_input: request.into(),
            effective_input: request.into(),
            context: vec![],
            target_definition: None,
            model: None,
        },
    };
    let (controller, reference) = registry
        .begin_first_turn_checked(&token, f.owner.clone(), spec, |_, _, _| Ok(()))
        .unwrap();
    let request = controller
        .snapshot()
        .unwrap()
        .request_ref()
        .unwrap()
        .clone();
    let resource = controller
        .repository_activation_resource(&reference)
        .unwrap();
    let approval = controller
        .retain_activation_evidence(ActivationEvidenceContent::Guidance {
            text: "Fixture host permits the external program run".into(),
        })
        .unwrap();
    let policy = AuthorityGrant {
        id: "external-grant".into(),
        revision: 1,
        issuer_evidence: approval,
        holder: activation.node_id.clone(),
        descendants: vec![],
        allow_stop_descendants: false,
        delegation: None,
        conditions: vec![],
        profiles: vec![profile.clone()],
        limits: limits.clone(),
        expires_at_ms: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64
            + 600_000,
    };
    let grant = controller
        .retain_activation_evidence(ActivationEvidenceContent::Grant {
            policy: policy.clone(),
        })
        .unwrap();
    let budget = controller
        .retain_activation_evidence(ActivationEvidenceContent::Budget { limits })
        .unwrap();
    controller.install_grant(policy).unwrap();
    controller
        .append_host_event(TurnContractEnvelope {
            schema_version: TURN_CONTRACT_SCHEMA_VERSION,
            command_id: CommandId::new("external-start").unwrap(),
            expected_revision: 1,
            session_id: activation.session_id.clone(),
            turn_id: activation.turn_id.clone(),
            event: TurnContractEvent::StartActivation {
                input: Box::new(ActivationInputManifest {
                    manifest_id: InputManifestId::new("external-input").unwrap(),
                    activation: activation.clone(),
                    definition,
                    conversation_id: NodeConversationId::new("conversation").unwrap(),
                    starting_savepoint: ConversationSavepoint::Empty,
                    parents: vec![],
                    guidance: vec![request],
                    attachments: vec![],
                    repository: RepositoryInput::Recorded {
                        snapshot: reference,
                    },
                    budget,
                    grant: Some(GrantSnapshotRef {
                        grant_id: GrantId::new("external-grant").unwrap(),
                        revision: 1,
                        evidence: grant,
                    }),
                    revision_context: None,
                }),
            },
        })
        .unwrap();
    Run {
        registry,
        controller,
        activation,
        resource,
        config,
        profile,
    }
}

/// The native factory a test expects not to be asked, or asked once.
struct NativeStub(AtomicUsize);

#[async_trait::async_trait]
impl AutonomousActivationFactory for NativeStub {
    async fn resources(
        &self,
        _: &ActivationInputManifest,
    ) -> std::result::Result<AutonomousActivationResources, String> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Err("native factory".into())
    }
}

fn input_of(r: &Run) -> ActivationInputManifest {
    r.controller.snapshot().unwrap().contract().activations()[0]
        .input
        .clone()
}

/// An external definition's activation gets the program provider, as the
/// autonomous writer with `bash` (so the host captures its checkout) and
/// the profile admission granted; preparing it passes the controller's own
/// profile, grant and host-tool checks. Any other definition goes to the
/// native factory.
#[tokio::test]
async fn external_definitions_get_the_program_provider_and_others_the_native_factory() {
    for runtime in [AgentRuntime::ClaudeCode, AgentRuntime::Codex] {
        let mut f = fixture().await;
        let r = run_external(&mut f, runtime, Some(&["src/"]));
        let native = Arc::new(NativeStub(AtomicUsize::new(0)));
        let factory = r.controller.external_activation_factory(
            native.clone(),
            Arc::new(Counter),
            ExternalSettings::default(),
        );
        let resources = factory.resources(&input_of(&r)).await.unwrap();
        assert_eq!(native.0.load(Ordering::SeqCst), 0);
        assert_eq!(
            resources.provider.provider_id(),
            external_agent::runtime_provider(runtime).unwrap()
        );
        assert_eq!(resources.provider.model_id(), "claude-sonnet-4-5");
        assert_eq!(resources.profile, r.profile);
        assert_eq!(resources.config.tools, ["bash"]);
        assert!(resources.provider.capabilities().streaming);
        let provider = resources.provider.clone();
        let request = axocoatl_llm::ChatRequest::simple("x");
        // Unbound, it reserves nothing.
        assert!(provider.execution_bounds(&request).is_none());
        let prepared = r
            .controller
            .prepare_repository_activation(r.activation.clone(), resources, r.resource.clone())
            .unwrap();
        // Bound, the run reserves everything the grant still allows; nothing
        // follows it.
        let bounds = provider.execution_bounds(&request).unwrap();
        assert_eq!(
            (bounds.token_limit, bounds.cost_microunits),
            (100_000, 1_000_000)
        );
        assert_eq!(
            provider
                .follow_up_execution_bounds(&request, 1_000)
                .unwrap()
                .token_limit,
            0
        );
        drop(prepared);
    }

    let mut f = fixture().await;
    let r = run(&mut f, &["bash"], true);
    let native = Arc::new(NativeStub(AtomicUsize::new(0)));
    let factory = r.controller.external_activation_factory(
        native.clone(),
        Arc::new(Counter),
        ExternalSettings::default(),
    );
    assert_eq!(
        factory.resources(&input_of(&r)).await.err().as_deref(),
        Some("native factory")
    );
    assert_eq!(native.0.load(Ordering::SeqCst), 1);
}

/// A definition that is external by provider but not shaped as the writer
/// with `bash` is refused, never sent to the native factory.
#[tokio::test]
async fn a_misshapen_external_definition_is_refused() {
    let mut f = fixture().await;
    let mut config = external_agent::external_agent_config(
        AgentConfig {
            id: AgentId::new("conversation"),
            ..Default::default()
        },
        AgentRuntime::ClaudeCode,
        "claude-sonnet-4-5",
    )
    .unwrap();
    config.tools.push("write_file".into());
    let r = run_with_config(&mut f, config, "x");
    let native = Arc::new(NativeStub(AtomicUsize::new(0)));
    let factory = r.controller.external_activation_factory(
        native.clone(),
        Arc::new(Counter),
        ExternalSettings::default(),
    );
    let refused = factory.resources(&input_of(&r)).await.err().unwrap();
    assert!(refused.contains("tools [bash]"), "{refused}");
    assert_eq!(native.0.load(Ordering::SeqCst), 0);
}

/// The program runs only inside its activation's admitted model call.
#[tokio::test]
async fn an_external_program_runs_only_inside_its_admitted_model_call() {
    let mut f = fixture().await;
    let r = run_external(&mut f, AgentRuntime::ClaudeCode, None);
    let factory = r.controller.external_activation_factory(
        Arc::new(NativeStub(AtomicUsize::new(0))),
        Arc::new(Counter),
        ExternalSettings::default(),
    );
    let resources = factory.resources(&input_of(&r)).await.unwrap();
    let _prepared = r
        .controller
        .prepare_repository_activation(r.activation.clone(), resources, r.resource.clone())
        .unwrap();
    let refused = r
        .controller
        .run_external_request(
            &r.activation,
            AgentRuntime::ClaudeCode,
            "claude-sonnet-4-5",
            "prompt".into(),
            None,
            &ExternalSettings::default(),
        )
        .await
        .err()
        .unwrap()
        .to_string();
    assert!(refused.contains("admitted model call"), "{refused}");
    // Another runtime or model than the definition's is refused first.
    let refused = r
        .controller
        .run_external_request(
            &r.activation,
            AgentRuntime::Codex,
            "claude-sonnet-4-5",
            "prompt".into(),
            None,
            &ExternalSettings::default(),
        )
        .await
        .err()
        .unwrap()
        .to_string();
    assert!(refused.contains("differs from the activation's admitted definition"));
    let _ = ExternalActivationRequest {
        session_id: String::new(),
        turn_id: String::new(),
        node_id: String::new(),
        runtime: AgentRuntime::ClaudeCode,
        model: String::new(),
        prompt: String::new(),
        writes: None,
        timeout_ms: 0,
    };
}

// ------------------------------------------------------------- Podman

/// A stand-in for `claude` in the test image: a Node program that reads the
/// prompt on stdin, makes one Messages API call with the token its
/// environment holds, through the Session's proxy (`NODE_USE_ENV_PROXY`)
/// and trusting the Session's authority (`NODE_EXTRA_CA_CERTS`), changes the
/// checkout as the prompt says, and prints stream-json as Claude Code does.
const FAKE_CLAUDE: &str = r##"#!/usr/bin/env node
const fs = require("fs");
const args = process.argv.slice(2);
const prompt = fs.readFileSync(0, "utf8");
const out = (value) => process.stdout.write(JSON.stringify(value) + "\n");
const port = (prompt.match(/FAKE-PORT=(\d+)/) || [])[1];
const status = fs.readFileSync("/proc/self/status", "utf8").split("\n")
  .filter((line) => /^(CapEff|NoNewPrivs|Seccomp_filters):/.test(line));
const facts = {
  uid: process.getuid(), gid: process.getgid(), home: process.env.HOME, cwd: process.cwd(),
  token: process.env.CLAUDE_CODE_OAUTH_TOKEN, telemetry: process.env.DISABLE_TELEMETRY,
  nonessential: process.env.CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC,
  model: args[args.indexOf("--model") + 1], skip: args.includes("--dangerously-skip-permissions"),
  budget: args[args.indexOf("--max-budget-usd") + 1], status, task: prompt.includes("# Task"),
};
fs.writeFileSync("/tmp/fake-claude-env.txt", Object.entries(process.env).map(([k, v]) => k + "=" + v).join("\n"));
fs.mkdirSync("src", { recursive: true });
fs.writeFileSync("src/fixed.txt", "fixed by the fake claude\n");
if (prompt.includes("WRITE-OUTSIDE")) fs.writeFileSync("outside.txt", "outside the scope\n");
(async () => {
  out({ type: "system", subtype: "init", model: facts.model, tools: ["Bash"] });
  let answer;
  const calls = Number((prompt.match(/FAKE-CALLS=(\d+)/) || [])[1] || 1);
  for (let call = 0; call < calls; call++) {
    if (call > 0) await new Promise((resolve) => setTimeout(resolve, 150));
    try {
      const response = await fetch(`https://api.anthropic.com:${port}/v1/messages?beta=true`, {
        method: "POST",
        headers: { authorization: "Bearer " + process.env.CLAUDE_CODE_OAUTH_TOKEN, "content-type": "application/json", "anthropic-beta": "oauth-2025-04-20" },
        body: JSON.stringify({ model: facts.model, max_tokens: 16, messages: [{ role: "user", content: "hi" }] }),
      });
      answer = `FAKE-ANSWER ${response.status} ${(await response.text()).trim()}`;
    } catch (error) {
      answer = `FAKE-FETCH-FAILED ${error.cause ? error.cause.code || error.cause.message : error.message}`;
    }
  }
  out({ type: "assistant", message: { content: [{ type: "text", text: "Changing src/fixed.txt." }, { type: "tool_use", id: "toolu_fake", name: "Bash", input: { command: "write src/fixed.txt" } }] } });
  out({ type: "user", message: { content: [{ type: "tool_result", tool_use_id: "toolu_fake", content: JSON.stringify(facts), is_error: false }] } });
  out({ type: "assistant", message: { content: [{ type: "text", text: answer }] } });
  out({ type: "result", subtype: "success", is_error: false, result: answer, num_turns: 2, total_cost_usd: 0.000123, usage: { input_tokens: 40, cache_creation_input_tokens: 0, cache_read_input_tokens: 2, output_tokens: 8 } });
})();
"##;

const TEST_LABEL: &str = "io.axocoatl.test=external-agent";

/// The test image: the recipe base with the fake `claude`.
fn external_test_image() -> String {
    use sha2::{Digest, Sha256};
    let containerfile = format!(
        "{}\nCOPY claude /usr/local/bin/claude\nRUN chmod 0755 /usr/local/bin/claude\nLABEL {TEST_LABEL}\n",
        axocoatl_isolation::recipes::RECIPE_BASE
    );
    let digest = format!(
        "{:x}",
        Sha256::digest(format!("{containerfile}{FAKE_CLAUDE}").as_bytes())
    );
    let image = format!("localhost/axocoatl-external-agent-test:{}", &digest[..12]);
    let exists = std::process::Command::new("podman")
        .args(["image", "exists", &image])
        .status()
        .unwrap();
    if exists.success() {
        return image;
    }
    let context = tempfile::tempdir().unwrap();
    std::fs::write(context.path().join("Containerfile"), containerfile).unwrap();
    std::fs::write(context.path().join("claude"), FAKE_CLAUDE).unwrap();
    let built = std::process::Command::new("podman")
        .args(["build", "--pull=missing", "-t", &image])
        .arg(context.path())
        .output()
        .unwrap();
    assert!(
        built.status.success(),
        "{}",
        String::from_utf8_lossy(&built.stderr)
    );
    image
}

/// The egress Session container for `f` on `image`, hardened with workload
/// users, mounting the Session's trust files (it has routes).
async fn external_sandbox(
    f: &mut Fixture,
    image: &str,
    upstream: &EgressUpstream,
    authority: Arc<crate::session_egress::SessionEgress>,
) -> Arc<axocoatl_isolation::SessionSandbox> {
    use axocoatl_isolation::{SandboxNetwork, SandboxPolicy, SessionSandbox};
    use sha2::{Digest, Sha256};
    let policy = SandboxPolicy {
        allow_untrusted_image: true,
        network: SandboxNetwork::Egress,
        runtime_authority: Some(format!(
            "{:x}",
            Sha256::digest(f.owner.metadata().session_id.as_bytes())
        )),
        supervisor_installation: Some(
            f.owner
                .inner
                .data_root
                .child("execution-supervisors")
                .unwrap(),
        ),
        egress: Some(axocoatl_isolation::egress::EgressAttachment {
            trust_files: authority.trust_files().unwrap(),
            authority,
            sidecar_network: Some(upstream.network.clone()),
            max_connections: 32,
            labels: vec![upstream.label.clone(), TEST_LABEL.into()],
        }),
        workload: Some(axocoatl_isolation::WorkloadUsers {
            writer: (1000, 1000),
            helper: (1001, 1001),
        }),
        ..SandboxPolicy::default()
    };
    let sandbox = Arc::new(
        SessionSandbox::start(
            &f.owner.metadata().session_id,
            f.owner.root(),
            Some(image),
            &[],
            &[],
            &policy,
        )
        .await
        .unwrap(),
    );
    let registered: Arc<dyn Sandbox> = sandbox.clone();
    {
        let inner = Arc::get_mut(&mut f.owner.inner).unwrap();
        inner.metadata.execution_identity = sandbox.execution_identity().unwrap().to_owned();
        inner.sandbox = registered.clone();
        inner
            .sandboxes
            .lock()
            .await
            .insert(inner.metadata.session_id.clone(), registered);
    }
    sandbox
}

/// A record source over the test's in-memory record, for the route meter.
struct FakeRecordSource(Arc<crate::session_egress::tests::FakeRecord>);

impl crate::session_dispatch::RouteRequestSource for FakeRecordSource {
    fn events_after(
        &self,
        after: Option<u64>,
        limit: usize,
    ) -> std::result::Result<Vec<(u64, axocoatl_session::network_record::NetworkEvent)>, String>
    {
        Ok(self
            .0
            .events()
            .into_iter()
            .enumerate()
            .map(|(index, event)| (index as u64 + 1, event))
            .filter(|(seq, _)| after.is_none_or(|after| *seq > after))
            .take(limit)
            .collect())
    }
}

/// F end to end through real containers: an external writer's activation
/// is admitted, captured and judged like a native writer's, and its one
/// model call runs the program in the hardened egress Session as the
/// non-root writer user under the supervisor's filter. The program's model
/// call goes through the Session's route: the container holds only the
/// placeholder, the upstream receives the secret from the daemon's secret
/// store, the call is in the network record, and the activation's answer is
/// the program's final text, its usage settled from the program's report. A
/// change outside the writer's scope fails the activation.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires Podman (CONTAINER_CONNECTION) and the egress-capable embedded helper"]
async fn actual_external_claude_code_runs_through_the_route_as_the_hardened_writer() {
    use crate::egress_broker::UpstreamConnector;
    use crate::session_egress::route_tests::{loopback_is_public, Upstream};
    use crate::session_egress::tests::{FakeRecord, FakeResolver};
    use crate::session_egress::{EgressPolicyConfig, RouteSettings, SessionEgress};
    use axocoatl_session::network_record::{BindingKind, Decision as Recorded, NetworkEvent};
    use std::os::unix::fs::PermissionsExt;
    let image = external_test_image();
    let upstream_network = EgressUpstream::start();
    let upstream = Upstream::start("api.anthropic.com").await;
    let port = upstream.addr.port();
    // The daemon's secret store, owner-only and outside the Workspace.
    let store = tempfile::tempdir().unwrap();
    std::fs::set_permissions(store.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let secret = format!("sk-ant-oat01-test-{}", uuid::Uuid::new_v4().simple());
    crate::secret_store::set_secret(
        store.path(),
        "claude-code-oauth",
        format!("{secret}\n").as_bytes(),
    )
    .unwrap();
    let ca_file = store.path().join("upstream-ca.pem");
    std::fs::write(&ca_file, upstream.ca.pem()).unwrap();
    std::fs::set_permissions(&ca_file, std::fs::Permissions::from_mode(0o600)).unwrap();
    // The runtime's own route, on the test upstream's port and authority.
    let mut route = external_agent::routes_for(AgentRuntime::ClaudeCode)
        .unwrap()
        .remove(0);
    route.ports = Some(vec![port]);
    route.upstream_ca = Some(ca_file.display().to_string());
    let mut credentials = std::collections::BTreeMap::new();
    credentials.insert(
        "claude-code-oauth".to_string(),
        crate::secret_store::credential_source(store.path(), "claude-code-oauth")
            .unwrap()
            .unwrap(),
    );
    for (writes, prompt_marker, accepted) in [
        (None, "", true),
        (Some(&["src/"][..]), "WRITE-OUTSIDE", false),
    ] {
        let earlier_calls = upstream.seen().len();
        let mut f = fixture().await;
        let record = Arc::new(FakeRecord::default());
        let env_dir = f.owner.inner.data_root.child("egress-env").unwrap();
        let workspace = f._workspace.path().to_path_buf();
        let egress = SessionEgress::open_session(
            f.owner.metadata().session_id.clone(),
            EgressPolicyConfig {
                routes: vec![route.clone()],
                credentials: credentials.clone(),
                ..EgressPolicyConfig::default()
            },
            record.clone(),
            FakeResolver::with(&[("api.anthropic.com", &["127.0.0.1"])]),
            Some(env_dir.clone()),
            loopback_is_public,
            RouteSettings {
                upstream: Arc::new(UpstreamConnector::with_local_check(Arc::new(|_| false))),
                workspaces: {
                    let workspace = workspace.clone();
                    Arc::new(move || vec![workspace.clone()])
                },
                ..RouteSettings::default()
            },
        )
        .await
        .unwrap();
        let sandbox = external_sandbox(&mut f, &image, &upstream_network, egress.clone()).await;
        git_init(f._workspace.path());
        std::fs::write(f._workspace.path().join("readme.txt"), "hello\n").unwrap();
        let config = external_agent::external_agent_config(
            AgentConfig {
                id: AgentId::new("conversation"),
                name: "External writer".into(),
                writes: writes.map(|writes| writes.iter().map(|path| (*path).into()).collect()),
                ..Default::default()
            },
            AgentRuntime::ClaudeCode,
            "claude-sonnet-4-5",
        )
        .unwrap();
        let r = run_with_config(
            &mut f,
            config,
            &format!("FAKE-TASK FAKE-PORT={port} {prompt_marker}"),
        );
        let factory = r.controller.external_activation_factory(
            Arc::new(NativeStub(AtomicUsize::new(0))),
            Arc::new(Counter),
            ExternalSettings {
                source: Some(Arc::new(FakeRecordSource(record.clone()))),
                meter_interval: Duration::from_millis(100),
                adjust_argv: None,
            },
        );
        let resources = factory.resources(&input_of(&r)).await.unwrap();
        let result = tokio::time::timeout(Duration::from_secs(240), async {
            r.controller
                .prepare_repository_activation(r.activation.clone(), resources, r.resource.clone())
                .unwrap()
                .run()
                .await
        })
        .await;
        tokio::time::sleep(Duration::from_millis(500)).await;
        // The secret is nowhere in the container: not in the program's
        // environment, any process's, /etc, /tmp or the Workspace.
        let environment = sandbox
            .exec(
                &["sh", "-c", "cat /tmp/fake-claude-env.txt; for f in /proc/[0-9]*/environ; do tr '\\0' '\\n' < $f; done 2>/dev/null; true"],
                Duration::from_secs(30),
            )
            .await
            .unwrap()
            .stdout;
        assert!(
            environment.contains("CLAUDE_CODE_OAUTH_TOKEN=axocoatl-route:api.anthropic.com"),
            "{environment}"
        );
        assert!(environment.contains("NODE_EXTRA_CA_CERTS=/etc/axocoatl/ca/session-ca.pem"));
        assert!(!environment.contains(&secret));
        let found = sandbox
            .exec(
                &[
                    "sh",
                    "-c",
                    "grep -rIl -F -e \"$1\" /etc /tmp /home \"$2\" 2>/dev/null; true",
                    "sh",
                    &secret,
                    &workspace.display().to_string(),
                ],
                Duration::from_secs(60),
            )
            .await
            .unwrap()
            .stdout;
        assert_eq!(found.trim(), "");
        // Podman's own seccomp filters, for the supervisor's one more.
        let podman_filters: u32 = sandbox
            .exec(
                &[
                    "sh",
                    "-c",
                    "grep '^Seccomp_filters:' /proc/self/status | cut -f2",
                ],
                Duration::from_secs(30),
            )
            .await
            .unwrap()
            .stdout
            .trim()
            .parse()
            .unwrap();
        let session = f.owner.metadata().session_id.clone();
        for container in [format!("axo-ses-{session}"), format!("axo-egr-{session}")] {
            let inspect = std::process::Command::new("podman")
                .args(["inspect", &container])
                .output()
                .unwrap();
            assert!(!String::from_utf8_lossy(&inspect.stdout).contains(&secret));
        }
        let idle = f.owner.execution_is_idle();
        sandbox.stop_checked().await.unwrap();
        let settled = result.unwrap().unwrap();
        assert!(idle.unwrap());
        let text = settled.output.content().output.text.clone();
        let events = record.events();
        for event in &events {
            event.validate().unwrap();
            assert!(!serde_json::to_string(event).unwrap().contains(&secret));
        }
        // The upstream got the request with the stored secret, from the
        // daemon, in place of the placeholder.
        let seen = upstream.seen();
        let calls: Vec<_> = seen[earlier_calls..]
            .iter()
            .filter(|request| request.path == "/v1/messages")
            .collect();
        assert_eq!(calls.len(), 1, "{seen:?} {events:#?} {text}");
        assert_eq!(calls[0].method, "POST");
        assert_eq!(calls[0].authorization, [format!("Bearer {secret}")]);
        // The call is in the record: a route request with the credential's
        // name on a connection the activation's own credential opened, by
        // the program, as the writer user.
        let bind = events
            .iter()
            .find_map(|event| match event {
                NetworkEvent::Bind { token, binding, .. }
                    if binding.kind == BindingKind::Agent
                        && binding.activation_id.as_deref()
                            == Some(r.activation.activation_id.as_str()) =>
                {
                    Some((token.clone(), binding.clone()))
                }
                _ => None,
            })
            .unwrap_or_else(|| panic!("{events:#?}"));
        assert_eq!(
            bind.1.process.as_deref(),
            Some("external:external-activation")
        );
        assert_eq!(bind.1.agent.as_deref(), Some("external-definition"));
        let (conn, peer) = events
            .iter()
            .find_map(|event| match event {
                NetworkEvent::Open {
                    conn,
                    decision: Recorded::Allow,
                    rule: Some(rule),
                    host,
                    token: Some(token),
                    peer,
                    ..
                } if rule == "route#0" && host == "api.anthropic.com" && *token == bind.0 => {
                    Some((conn.clone(), peer.clone()))
                }
                _ => None,
            })
            .unwrap_or_else(|| panic!("{events:#?}"));
        let peer = peer.unwrap();
        assert_eq!(peer.uid, Some(1000), "{peer:?}");
        assert!(
            peer.exe
                .as_deref()
                .is_some_and(|exe| exe.ends_with("/node")),
            "{peer:?}"
        );
        assert!(peer
            .ancestors
            .iter()
            .any(|parent| parent == "/axocoatl-exec-supervisor"));
        assert!(events.iter().any(|event| matches!(event,
            NetworkEvent::Request { conn: request_conn, method, path, decision: Recorded::Allow, credential: Some(name), .. }
                if *request_conn == conn && method == "POST" && path == "/v1/messages" && name == "claude-code-oauth")),
            "{events:#?}");
        // The program ran as the non-root writer, without capabilities,
        // under the supervisor's seccomp filter and no-new-privileges, with
        // its environment and its own sandbox off.
        let stream: Vec<String> = r
            .controller
            .activation_stream_for_test(&r.activation)
            .into_iter()
            .collect();
        let log = stream.join("");
        for wanted in [
            "[external agent] @anthropic-ai/claude-code 2.1.292, model claude-sonnet-4-5: exit 0, 1 model request(s)",
            "[tool call] Bash: {\"command\":\"write src/fixed.txt\"}",
            r#""uid":1000,"gid":1000"#,
            r#""home":"/home/axocoatl""#,
            r#""token":"axocoatl-route:api.anthropic.com""#,
            r#""telemetry":"1""#,
            r#""nonessential":"1""#,
            r#""skip":true"#,
            r#""budget":"1.000000""#,
            r#"CapEff:\t0000000000000000"#,
            r#"NoNewPrivs:\t1"#,
            "[usage] 42 input and 8 output tokens, $0.000123",
        ] {
            assert!(log.contains(wanted), "{wanted}: {log}");
        }
        // The supervisor's seccomp filter on top of Podman's own: `--harden`.
        assert!(
            log.contains(&format!(r#"Seccomp_filters:\t{}"#, podman_filters + 1)),
            "{podman_filters}: {log}"
        );
        let usage = r
            .controller
            .activation_provider_usage(&r.activation)
            .unwrap();
        assert!(usage.tokens.complete);
        assert_eq!(
            (
                usage.tokens.usage.input_tokens,
                usage.tokens.usage.output_tokens
            ),
            (42, 8)
        );
        assert_eq!((usage.cost_microunits, usage.cost_known), (123, true));
        if accepted {
            assert!(settled.accepted, "{:?} {text}", settled.failure);
            assert_eq!(text, "FAKE-ANSWER 200 from upstream");
            assert_eq!(
                std::fs::read_to_string(workspace.join("src/fixed.txt")).unwrap(),
                "fixed by the fake claude\n"
            );
        } else {
            assert!(!settled.accepted);
            let failure = settled.failure.unwrap_or_default();
            assert!(failure.contains("outside.txt"), "{failure}");
        }
        assert_eq!(egress.live_bindings(), 0);
    }
}


/// The route requests of an external program count against its grant's
/// invocations: once it has made more than the grant still allowed when it
/// started, the host stops it, and the activation fails saying so.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires Podman (CONTAINER_CONNECTION) and the egress-capable embedded helper"]
async fn actual_external_program_is_stopped_when_its_requests_pass_the_grant() {
    use crate::egress_broker::UpstreamConnector;
    use crate::session_egress::route_tests::{loopback_is_public, Upstream};
    use crate::session_egress::tests::{FakeRecord, FakeResolver};
    use crate::session_egress::{EgressPolicyConfig, RouteSettings, SessionEgress};
    use std::os::unix::fs::PermissionsExt;
    let image = external_test_image();
    let upstream_network = EgressUpstream::start();
    let upstream = Upstream::start("api.anthropic.com").await;
    let port = upstream.addr.port();
    let store = tempfile::tempdir().unwrap();
    std::fs::set_permissions(store.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    crate::secret_store::set_secret(store.path(), "claude-code-oauth", b"sk-ant-oat01-limit")
        .unwrap();
    let ca_file = store.path().join("upstream-ca.pem");
    std::fs::write(&ca_file, upstream.ca.pem()).unwrap();
    std::fs::set_permissions(&ca_file, std::fs::Permissions::from_mode(0o600)).unwrap();
    let mut route = external_agent::routes_for(AgentRuntime::ClaudeCode)
        .unwrap()
        .remove(0);
    route.ports = Some(vec![port]);
    route.upstream_ca = Some(ca_file.display().to_string());
    let mut f = fixture().await;
    let record = Arc::new(FakeRecord::default());
    let egress = SessionEgress::open_session(
        f.owner.metadata().session_id.clone(),
        EgressPolicyConfig {
            routes: vec![route],
            credentials: std::collections::BTreeMap::from([(
                "claude-code-oauth".to_string(),
                crate::secret_store::credential_source(store.path(), "claude-code-oauth")
                    .unwrap()
                    .unwrap(),
            )]),
            ..EgressPolicyConfig::default()
        },
        record.clone(),
        FakeResolver::with(&[("api.anthropic.com", &["127.0.0.1"])]),
        Some(f.owner.inner.data_root.child("egress-env").unwrap()),
        loopback_is_public,
        RouteSettings {
            upstream: Arc::new(UpstreamConnector::with_local_check(Arc::new(|_| false))),
            ..RouteSettings::default()
        },
    )
    .await
    .unwrap();
    let sandbox = external_sandbox(&mut f, &image, &upstream_network, egress.clone()).await;
    git_init(f._workspace.path());
    let config = external_agent::external_agent_config(
        AgentConfig {
            id: AgentId::new("conversation"),
            ..Default::default()
        },
        AgentRuntime::ClaudeCode,
        "claude-sonnet-4-5",
    )
    .unwrap();
    let r = run_with_limits(
        &mut f,
        config,
        &format!("FAKE-TASK FAKE-PORT={port} FAKE-CALLS=60"),
        GrantLimits {
            activations: 2,
            invocations: 10,
            tokens: 100_000,
            cost_microunits: 1_000_000,
        },
    );
    let factory = r.controller.external_activation_factory(
        Arc::new(NativeStub(AtomicUsize::new(0))),
        Arc::new(Counter),
        ExternalSettings {
            source: Some(Arc::new(FakeRecordSource(record.clone()))),
            meter_interval: Duration::from_millis(50),
            adjust_argv: None,
        },
    );
    let resources = factory.resources(&input_of(&r)).await.unwrap();
    let settled = tokio::time::timeout(Duration::from_secs(240), async {
        r.controller
            .prepare_repository_activation(r.activation.clone(), resources, r.resource.clone())
            .unwrap()
            .run()
            .await
    })
    .await
    .unwrap()
    .unwrap();
    let idle = f.owner.execution_is_idle();
    sandbox.stop_checked().await.unwrap();
    assert!(idle.unwrap());
    assert!(!settled.accepted);
    let failure = settled.failure.unwrap_or_default();
    assert!(
        failure.contains("Axocoatl stopped the program: it made"),
        "{failure}"
    );
    let calls = upstream
        .seen()
        .iter()
        .filter(|request| request.path == "/v1/messages")
        .count();
    assert!((1..20).contains(&calls), "{calls}");
    // No usage report: the whole reservation stays charged.
    let usage = r.controller.activation_provider_usage(&r.activation).unwrap();
    assert!(!usage.tokens.complete);
    assert_eq!(egress.live_bindings(), 0);
}

// ----------------------------------------------- the pinned programs

/// One request a model API upstream received.
#[derive(Debug, Clone)]
struct ApiSeen {
    method: String,
    path: String,
    authorization: Vec<String>,
}

/// A local HTTPS model API for `host` answering the Messages and Responses
/// streaming endpoints with one fixed text answer each, as the captures in
/// `fixtures/` show the pinned programs read them.
struct ModelApi {
    addr: std::net::SocketAddr,
    ca: crate::egress_broker::SessionCa,
    seen: Arc<Mutex<Vec<ApiSeen>>>,
}

const ANTHROPIC_SSE: &str = concat!(
    "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_real\",\"type\":\"message\",\"role\":\"assistant\",\"model\":\"claude-sonnet-4-5\",\"content\":[],\"stop_reason\":null,\"stop_sequence\":null,\"usage\":{\"input_tokens\":120,\"output_tokens\":1,\"cache_creation_input_tokens\":0,\"cache_read_input_tokens\":0}}}\n\n",
    "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
    "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"PINNED-CLAUDE-OK\"}}\n\n",
    "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
    "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\",\"stop_sequence\":null},\"usage\":{\"output_tokens\":7}}\n\n",
    "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
);

const OPENAI_SSE: &str = concat!(
    "event: response.created\ndata: {\"type\":\"response.created\",\"sequence_number\":0,\"response\":{\"id\":\"resp_real\",\"object\":\"response\",\"created_at\":1,\"model\":\"gpt-5.5\",\"status\":\"in_progress\",\"output\":[]}}\n\n",
    "event: response.output_item.added\ndata: {\"type\":\"response.output_item.added\",\"sequence_number\":1,\"output_index\":0,\"item\":{\"type\":\"message\",\"id\":\"msg_real\",\"role\":\"assistant\",\"status\":\"in_progress\",\"content\":[]}}\n\n",
    "event: response.content_part.added\ndata: {\"type\":\"response.content_part.added\",\"sequence_number\":2,\"item_id\":\"msg_real\",\"output_index\":0,\"content_index\":0,\"part\":{\"type\":\"output_text\",\"text\":\"\",\"annotations\":[]}}\n\n",
    "event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"sequence_number\":3,\"item_id\":\"msg_real\",\"output_index\":0,\"content_index\":0,\"delta\":\"PINNED-CODEX-OK\"}\n\n",
    "event: response.output_text.done\ndata: {\"type\":\"response.output_text.done\",\"sequence_number\":4,\"item_id\":\"msg_real\",\"output_index\":0,\"content_index\":0,\"text\":\"PINNED-CODEX-OK\"}\n\n",
    "event: response.content_part.done\ndata: {\"type\":\"response.content_part.done\",\"sequence_number\":5,\"item_id\":\"msg_real\",\"output_index\":0,\"content_index\":0,\"part\":{\"type\":\"output_text\",\"text\":\"PINNED-CODEX-OK\",\"annotations\":[]}}\n\n",
    "event: response.output_item.done\ndata: {\"type\":\"response.output_item.done\",\"sequence_number\":6,\"output_index\":0,\"item\":{\"type\":\"message\",\"id\":\"msg_real\",\"role\":\"assistant\",\"status\":\"completed\",\"content\":[{\"type\":\"output_text\",\"text\":\"PINNED-CODEX-OK\",\"annotations\":[]}]}}\n\n",
    "event: response.completed\ndata: {\"type\":\"response.completed\",\"sequence_number\":7,\"response\":{\"id\":\"resp_real\",\"object\":\"response\",\"created_at\":1,\"model\":\"gpt-5.5\",\"status\":\"completed\",\"output\":[{\"type\":\"message\",\"id\":\"msg_real\",\"role\":\"assistant\",\"status\":\"completed\",\"content\":[{\"type\":\"output_text\",\"text\":\"PINNED-CODEX-OK\",\"annotations\":[]}]}],\"usage\":{\"input_tokens\":300,\"input_tokens_details\":{\"cached_tokens\":0},\"output_tokens\":9,\"output_tokens_details\":{\"reasoning_tokens\":0},\"total_tokens\":309}}}\n\n",
);

impl ModelApi {
    async fn start(host: &str) -> Self {
        use http_body_util::{BodyExt, Full};
        use hyper::body::Incoming;
        let ca = crate::egress_broker::SessionCa::new("model-api-test").unwrap();
        let (cert, key) = ca.leaf(host).unwrap();
        let mut config = rustls::ServerConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(vec![cert], key)
        .unwrap();
        config.alpn_protocols = vec![b"http/1.1".to_vec()];
        let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(config));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let seen_by_server = seen.clone();
        tokio::spawn(async move {
            while let Ok((tcp, _)) = listener.accept().await {
                let acceptor = acceptor.clone();
                let seen = seen_by_server.clone();
                tokio::spawn(async move {
                    let Ok(tls) = acceptor.accept(tcp).await else {
                        return;
                    };
                    let service = hyper::service::service_fn(move |request: hyper::Request<Incoming>| {
                        let seen = seen.clone();
                        async move {
                            let authorization = request
                                .headers()
                                .get_all(hyper::header::AUTHORIZATION)
                                .iter()
                                .map(|value| String::from_utf8_lossy(value.as_bytes()).into())
                                .collect();
                            let method = request.method().to_string();
                            let path = request.uri().path().to_string();
                            seen.lock().unwrap().push(ApiSeen {
                                method: method.clone(),
                                path: path.clone(),
                                authorization,
                            });
                            let _ = request.into_body().collect().await;
                            let body = match (method.as_str(), path.as_str()) {
                                ("POST", "/v1/messages") => Some(ANTHROPIC_SSE),
                                ("POST", "/v1/responses") => Some(OPENAI_SSE),
                                _ => None,
                            };
                            let mut response = hyper::Response::new(Full::new(bytes::Bytes::from_static(
                                body.unwrap_or("{\"error\":\"not found\"}").as_bytes(),
                            )));
                            if body.is_some() {
                                response.headers_mut().insert(
                                    hyper::header::CONTENT_TYPE,
                                    hyper::header::HeaderValue::from_static("text/event-stream"),
                                );
                            } else {
                                *response.status_mut() = hyper::StatusCode::NOT_FOUND;
                            }
                            Ok::<_, std::convert::Infallible>(response)
                        }
                    });
                    let _ = hyper::server::conn::http1::Builder::new()
                        .serve_connection(hyper_util::rt::TokioIo::new(tls), service)
                        .await;
                });
            }
        });
        Self { addr, ca, seen }
    }
}

/// The pinned Claude Code 2.1.292 and Codex 0.160.1, from the images
/// `axocoatl recipe build claude-code` and `axocoatl recipe build codex`
/// made, run as external writers in a hardened egress Session exactly as
/// production runs them, but for the model API's port (a local upstream;
/// Claude Code is pointed at it with `ANTHROPIC_BASE_URL`, Codex through its
/// provider's base URL): through the proxy and the Session's route, trusting
/// the Session's authority, with the credential from the secret store, their
/// output parsed into the activation's answer and settled usage. Needs both
/// recipe images built on the Podman connection.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires Podman (CONTAINER_CONNECTION), the egress-capable embedded helper and the claude-code and codex recipe images"]
async fn actual_pinned_claude_code_and_codex_run_through_the_route() {
    use crate::egress_broker::UpstreamConnector;
    use crate::session_egress::route_tests::loopback_is_public;
    use crate::session_egress::tests::{FakeRecord, FakeResolver};
    use crate::session_egress::{EgressPolicyConfig, RouteSettings, SessionEgress};
    use axocoatl_session::network_record::{Decision as Recorded, NetworkEvent};
    use std::os::unix::fs::PermissionsExt;
    let upstream_network = EgressUpstream::start();
    for (runtime, recipe, host, path, model, answer, usage) in [
        (
            AgentRuntime::ClaudeCode,
            "claude-code",
            "api.anthropic.com",
            "/v1/messages",
            "claude-sonnet-4-5",
            "PINNED-CLAUDE-OK",
            (120, 7),
        ),
        (
            AgentRuntime::Codex,
            "codex",
            "api.openai.com",
            "/v1/responses",
            "gpt-5.5",
            "PINNED-CODEX-OK",
            (300, 9),
        ),
    ] {
        let image =
            axocoatl_isolation::recipes::image_name(&[recipe.to_string()]).unwrap();
        assert!(
            std::process::Command::new("podman")
                .args(["image", "exists", &image])
                .status()
                .unwrap()
                .success(),
            "build {image} first: axocoatl recipe build {recipe}"
        );
        let api = ModelApi::start(host).await;
        let port = api.addr.port();
        let store = tempfile::tempdir().unwrap();
        std::fs::set_permissions(store.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let secret = format!("sk-test-{recipe}-{}", uuid::Uuid::new_v4().simple());
        let mut route = external_agent::routes_for(runtime).unwrap().remove(0);
        let credential = route.credential.clone().unwrap();
        crate::secret_store::set_secret(store.path(), &credential, secret.as_bytes()).unwrap();
        let ca_file = store.path().join("upstream-ca.pem");
        std::fs::write(&ca_file, api.ca.pem()).unwrap();
        std::fs::set_permissions(&ca_file, std::fs::Permissions::from_mode(0o600)).unwrap();
        route.ports = Some(vec![port]);
        route.upstream_ca = Some(ca_file.display().to_string());
        let mut f = fixture().await;
        let record = Arc::new(FakeRecord::default());
        let egress = SessionEgress::open_session(
            f.owner.metadata().session_id.clone(),
            EgressPolicyConfig {
                routes: vec![route],
                credentials: std::collections::BTreeMap::from([(
                    credential.clone(),
                    crate::secret_store::credential_source(store.path(), &credential)
                        .unwrap()
                        .unwrap(),
                )]),
                ..EgressPolicyConfig::default()
            },
            record.clone(),
            FakeResolver::with(&[(host, &["127.0.0.1"])]),
            Some(f.owner.inner.data_root.child("egress-env").unwrap()),
            loopback_is_public,
            RouteSettings {
                upstream: Arc::new(UpstreamConnector::with_local_check(Arc::new(|_| false))),
                ..RouteSettings::default()
            },
        )
        .await
        .unwrap();
        let sandbox = external_sandbox(&mut f, &image, &upstream_network, egress.clone()).await;
        git_init(f._workspace.path());
        let config = external_agent::external_agent_config(
            AgentConfig {
                id: AgentId::new("conversation"),
                ..Default::default()
            },
            runtime,
            model,
        )
        .unwrap();
        let r = run_with_config(&mut f, config, "Reply with one word.");
        let adjust: Arc<dyn Fn(Vec<String>) -> Vec<String> + Send + Sync> = match runtime {
            AgentRuntime::ClaudeCode => Arc::new(move |mut argv: Vec<String>| {
                let at = argv.iter().position(|arg| arg == "env").unwrap() + 1;
                argv.insert(at, format!("ANTHROPIC_BASE_URL=https://api.anthropic.com:{port}"));
                argv
            }),
            _ => Arc::new(move |argv: Vec<String>| {
                argv.into_iter()
                    .map(|arg| {
                        arg.replace(
                            "https://api.openai.com/v1",
                            &format!("https://api.openai.com:{port}/v1"),
                        )
                    })
                    .collect()
            }),
        };
        let factory = r.controller.external_activation_factory(
            Arc::new(NativeStub(AtomicUsize::new(0))),
            Arc::new(Counter),
            ExternalSettings {
                source: Some(Arc::new(FakeRecordSource(record.clone()))),
                meter_interval: Duration::from_millis(200),
                adjust_argv: Some(adjust),
            },
        );
        let resources = factory.resources(&input_of(&r)).await.unwrap();
        let settled = tokio::time::timeout(Duration::from_secs(300), async {
            r.controller
                .prepare_repository_activation(r.activation.clone(), resources, r.resource.clone())
                .unwrap()
                .run()
                .await
        })
        .await;
        tokio::time::sleep(Duration::from_millis(500)).await;
        let idle = f.owner.execution_is_idle();
        sandbox.stop_checked().await.unwrap();
        let settled = settled.unwrap().unwrap();
        assert!(idle.unwrap());
        let log = r.controller.activation_stream_for_test(&r.activation).join("");
        let events = record.events();
        assert!(
            settled.accepted,
            "{recipe}: {:?}\n{log}\n{events:#?}",
            settled.failure
        );
        assert_eq!(settled.output.content().output.text, answer, "{log}");
        let seen = api.seen.lock().unwrap().clone();
        let calls: Vec<_> = seen.iter().filter(|request| request.path == path).collect();
        assert!(!calls.is_empty(), "{recipe}: {seen:?}");
        for call in &calls {
            assert_eq!(call.method, "POST");
            assert_eq!(call.authorization, [format!("Bearer {secret}")], "{recipe}");
        }
        // Nothing else was asked of the model API.
        assert_eq!(calls.len(), seen.len(), "{recipe}: {seen:?}");
        for event in &events {
            assert!(!serde_json::to_string(event).unwrap().contains(&secret));
        }
        assert!(events.iter().any(|event| matches!(event,
            NetworkEvent::Request { method, path: request_path, decision: Recorded::Allow, credential: Some(name), .. }
                if method == "POST" && request_path == path && *name == credential)),
            "{recipe}: {events:#?}");
        // Every connection the program opened was the route's, or refused
        // and recorded.
        let refused: Vec<String> = events
            .iter()
            .filter_map(|event| match event {
                NetworkEvent::Open {
                    host: opened,
                    port,
                    decision: Recorded::Deny,
                    reason,
                    ..
                } => Some(format!("{opened}:{port} {}", reason.as_deref().unwrap_or(""))),
                _ => None,
            })
            .collect();
        eprintln!("{recipe}: refused connections {refused:?}");
        for event in &events {
            if let NetworkEvent::Open { host: opened, decision: Recorded::Allow, .. } = event {
                assert_eq!(opened, host, "{recipe}: {events:#?}");
            }
        }
        let measured = r.controller.activation_provider_usage(&r.activation).unwrap();
        assert!(measured.tokens.complete, "{recipe}: {log}");
        assert!(
            measured.tokens.usage.input_tokens >= usage.0
                && measured.tokens.usage.output_tokens >= usage.1,
            "{recipe}: {measured:?}"
        );
        assert_eq!(egress.live_bindings(), 0);
    }
}
