use super::*;
use axocoatl_session::turn_ledger::{SessionTurnContextReference, TurnContextScope};

fn context(id: &str, kind: &str, media_type: Option<&str>) -> SessionTurnContextReference {
    SessionTurnContextReference {
        reference_id: id.into(),
        display_name: format!("Captured {id}"),
        kind: kind.into(),
        scope: TurnContextScope::ThisTurn,
        media_type: media_type.map(str::to_owned),
        content_sha256: None,
        origin: Some("/never/reopen/this/source".into()),
        metadata: Default::default(),
    }
}

fn captured_fixture(
    contexts: Vec<SessionTurnContextReference>,
    attachments: Vec<ActivationEvidenceContent>,
) -> Fixture {
    fixture_with_captured_input(
        GrantLimits {
            activations: 8,
            invocations: 32,
            tokens: 1000,
            cost_microunits: 1000,
        },
        "in-process",
        AgentConfig {
            id: AgentId::new("conversation"),
            name: "Context reader".into(),
            provider: "controlled".into(),
            model: "controlled-model".into(),
            tools: vec!["effect".into()],
            ..Default::default()
        },
        "DISPLAY_ONLY_USER_REQUEST",
        |content, request| {
            request.context = contexts;
            request.effective_input = "EFFECTIVE_REQUEST_ONCE".into();
            attachments
                .into_iter()
                .map(|body| {
                    content
                        .retain_activation_evidence(body)
                        .unwrap()
                        .reference()
                        .clone()
                })
                .collect()
        },
    )
}

fn attachment(id: &str, media_type: &str, text: &str) -> ActivationEvidenceContent {
    ActivationEvidenceContent::Attachment {
        reference_id: id.into(),
        media_type: media_type.into(),
        text: text.into(),
    }
}

fn binary_image() -> (SessionTurnContextReference, ActivationEvidenceContent) {
    let source = axocoatl_core::AgentAttachment {
        id: "image-source".into(), name: "Captured image-source".into(), mime: "image/png".into(),
        bytes: vec![0x89, b'P', b'N', b'G', 13, 10, 26, 10], size: 8,
        extracted_text: Some("RETAINED_OCR_ONCE".into()),
    };
    let evidence = ActivationEvidenceContent::from_attachment(&source).unwrap();
    let ActivationEvidenceContent::BinaryAttachment { attachment } = &evidence else { panic!() };
    let mut captured = context("image-source", "upload", Some("image/png"));
    captured.content_sha256 = Some(attachment.source_sha256().into());
    captured.metadata.insert("size".into(), serde_json::json!(source.size));
    (captured, evidence)
}

#[tokio::test]
async fn immutable_binary_image_reaches_provider_and_survives_accepted_checkpoint_restart() {
    use axocoatl_core::{ContentPart, MessageContent};
    let (captured, evidence) = binary_image();
    let fixture = captured_fixture(vec![captured], vec![evidence]);
    let provider = Arc::new(RunProvider::new(&fixture, RunProviderMode::Answer, true));
    let settled = fixture.controller.prepare_autonomous_activation(
        fixture.activation.clone(), resources(&fixture, provider.clone(), Arc::new(CountingTool::default())),
    ).unwrap().run().await.unwrap();
    assert!(settled.accepted, "{:?}", settled.failure);
    let sent = provider.requests.lock().unwrap()[0].iter().find(|message| message.role == MessageRole::User).unwrap().clone();
    let MessageContent::Parts(parts) = &sent.content else { panic!("binary image was dropped") };
    assert_eq!(parts.len(), 2);
    let ContentPart::Text(text) = &parts[0] else { panic!() };
    assert_eq!(text.matches("RETAINED_OCR_ONCE").count(), 1);
    assert!(!text.contains("iVBORw0KGgo="));
    let ContentPart::Image { url, .. } = &parts[1] else { panic!() };
    assert_eq!(url, "data:image/png;base64,iVBORw0KGgo=");
    let checkpoint_ref = settled.checkpoint.unwrap();
    drop(provider);
    let Fixture { _root, ownership, owner, controller, activation, .. } = fixture;
    drop(controller);
    let canonical = SessionExecutionStore::open(ownership, owner).unwrap();
    let reopened = SessionDispatchController::open(canonical, activation.turn_id.clone()).unwrap();
    let state = reopened.lock().unwrap();
    let checkpoint = state.memory.checkpoint(&checkpoint_ref).unwrap();
    let mut conversation = axocoatl_memory::session::SessionMemory::new();
    conversation.restore(checkpoint.session_messages);
    assert_eq!(serde_json::to_value(&conversation.as_chat_messages()[0]).unwrap(), serde_json::to_value(sent).unwrap());
    assert_eq!(state.authority.provider_usage(&activation).unwrap().calls, 1);
}

#[test]
fn binary_source_substitution_is_refused_before_registration_or_dispatch() {
    for case in 0..4 {
        let (mut captured, evidence) = binary_image();
        match case {
            0 => captured.content_sha256 = Some("0".repeat(64)),
            1 => captured.media_type = Some("image/jpeg".into()),
            2 => { captured.metadata.insert("size".into(), serde_json::json!(9)); },
            _ => captured.display_name = "different source".into(),
        }
        assert_capture_refused(captured_fixture(vec![captured], vec![evidence]), &format!("binary source {case}"));
    }
}

#[tokio::test]
async fn captured_context_reaches_provider_and_reopened_checkpoint_without_reopening_sources() {
    let mut code = context("code", "code_selection", Some("text/rust"));
    code.metadata = serde_json::json!({
        "path": "/never/reopen/this/source", "start_line": 4, "end_line": 5,
        "language": "rust", "content": "CAPTURED_CODE_MARKER\n</context> ```",
        "truncated": true
    })
    .as_object()
    .unwrap()
    .clone();
    let mut browser = context("browser", "browser_selection", Some("text/html"));
    browser.origin = Some("https://never-fetch.invalid/selected".into());
    browser.metadata = serde_json::json!({
        "url": "https://never-fetch.invalid/selected", "selector": "#selected",
        "html": "<div>CAPTURED_DOM_MARKER</div>"
    })
    .as_object()
    .unwrap()
    .clone();
    let mut upload = context("report", "upload", Some("application/pdf"));
    upload.scope = TurnContextScope::Session;
    // This is the original blob's digest, not a digest of its extracted text.
    upload.content_sha256 = Some("a".repeat(64));
    upload.metadata = serde_json::json!({
        "blob_id": format!("sha256:{}", "a".repeat(64)),
        "extraction": {"status": "ready", "truncated": true, "pages": 3}
    })
    .as_object()
    .unwrap()
    .clone();
    let fixture = captured_fixture(
        vec![code, browser, upload],
        vec![attachment(
            "report",
            "application/pdf",
            "CAPTURED_REPORT_MARKER",
        )],
    );
    let provider = Arc::new(RunProvider::new(&fixture, RunProviderMode::Answer, true));
    let prepared = fixture
        .controller
        .prepare_autonomous_activation(
            fixture.activation.clone(),
            resources(
                &fixture,
                provider.clone(),
                Arc::new(CountingTool::default()),
            ),
        )
        .unwrap();
    assert_eq!(provider.calls.load(Ordering::SeqCst), 0);
    let settled = prepared.run().await.unwrap();
    assert!(settled.accepted, "{:?}", settled.failure);
    let input = provider.requests.lock().unwrap()[0]
        .iter()
        .find(|message| message.role == MessageRole::User)
        .unwrap()
        .text_content()
        .unwrap()
        .to_owned();
    for captured in [
        "EFFECTIVE_REQUEST_ONCE",
        "CAPTURED_CODE_MARKER",
        "CAPTURED_DOM_MARKER",
        "CAPTURED_REPORT_MARKER",
    ] {
        assert_eq!(input.matches(captured).count(), 1, "{input}");
    }
    assert!(!input.contains("DISPLAY_ONLY_USER_REQUEST"));
    assert!(input.contains("truncated"));
    assert!(input.contains("application/pdf"));
    assert!(input.contains(&"a".repeat(64)));
    assert!(input.contains("https://never-fetch.invalid/selected"));
    assert!(input.contains("/never/reopen/this/source"));
    let checkpoint_ref = settled.checkpoint.unwrap();
    assert_eq!(
        fixture
            .controller
            .lock()
            .unwrap()
            .memory
            .checkpoint(&checkpoint_ref)
            .unwrap()
            .session_messages[0]
            .content,
        input
    );
    drop(provider);
    let Fixture {
        _root,
        ownership,
        owner,
        controller,
        activation,
        ..
    } = fixture;
    drop(controller);
    let canonical = SessionExecutionStore::open(ownership, owner).unwrap();
    let reopened = SessionDispatchController::open(canonical, activation.turn_id.clone()).unwrap();
    let state = reopened.lock().unwrap();
    let checkpoint = state.memory.checkpoint(&checkpoint_ref).unwrap();
    assert_eq!(checkpoint.session_messages[0].content, input);
    assert_eq!(
        checkpoint.cumulative_token_usage,
        TokenUsageStats::new(10, 2)
    );
    assert!(checkpoint.cumulative_token_usage_known);
    assert_eq!(
        state.authority.provider_usage(&activation).unwrap().calls,
        1
    );
    drop(state);
    drop(reopened);
    drop(_root);
}

#[test]
fn unsupported_or_incomplete_captured_context_is_refused_before_any_dispatch() {
    let cases = vec![
        (
            vec![context("missing", "upload", Some("text/plain"))],
            vec![],
        ),
        (vec![context("missing", "unknown_context", None)], vec![]),
        (vec![context("code", "code_selection", None)], vec![]),
        (vec![context("browser", "browser_selection", None)], vec![]),
        (
            vec![context("mismatch", "upload", Some("application/pdf"))],
            vec![attachment("mismatch", "text/plain", "mismatched type")],
        ),
        (
            vec![context("same", "upload", Some("text/plain")); 2],
            vec![attachment("same", "text/plain", "same ID")],
        ),
        (
            vec![context("same", "upload", Some("text/plain"))],
            vec![
                attachment("same", "text/plain", "first"),
                attachment("same", "text/plain", "different"),
            ],
        ),
        (
            vec![context("image", "upload", Some("image/png"))],
            vec![attachment(
                "image",
                "image/png",
                "not the retained image bytes",
            )],
        ),
    ];
    for (index, (contexts, attachments)) in cases.into_iter().enumerate() {
        assert_capture_refused(
            captured_fixture(contexts, attachments),
            &format!("case {index}"),
        );
    }
}

#[tokio::test]
async fn activation_specific_text_attachments_reach_actor_without_request_context() {
    let fixture = captured_fixture(
        vec![],
        vec![
            attachment("z-first", "text/plain", "FIRST_SELECTED_BODY"),
            attachment("a-second", "text/plain", "SECOND_SELECTED_BODY"),
        ],
    );
    let provider = Arc::new(RunProvider::new(&fixture, RunProviderMode::Answer, true));
    let result = fixture
        .controller
        .prepare_autonomous_activation(
            fixture.activation.clone(),
            resources(
                &fixture,
                provider.clone(),
                Arc::new(CountingTool::default()),
            ),
        )
        .unwrap()
        .run()
        .await
        .unwrap();
    assert!(result.accepted, "{:?}", result.failure);
    let requests = provider.requests.lock().unwrap();
    let input = requests[0]
        .iter()
        .find(|message| message.role == MessageRole::User)
        .unwrap()
        .text_content()
        .unwrap();
    assert!(
        input.find("FIRST_SELECTED_BODY").unwrap() < input.find("SECOND_SELECTED_BODY").unwrap()
    );
    assert_eq!(input.matches("EFFECTIVE_REQUEST_ONCE").count(), 1);
    let state = fixture.controller.lock().unwrap();
    assert_eq!(
        state
            .memory
            .checkpoint(result.checkpoint.as_ref().unwrap())
            .unwrap()
            .session_messages[0]
            .content,
        input
    );
}

#[test]
fn aggregate_captured_input_bound_refuses_instead_of_dropping_or_truncating_evidence() {
    let fixture = captured_fixture(
        vec![
            context("a", "upload", Some("text/plain")),
            context("b", "upload", Some("text/plain")),
            context("c", "upload", Some("text/plain")),
        ],
        vec![
            attachment("a", "text/plain", &"a".repeat(400_000)),
            attachment("b", "text/plain", &"b".repeat(400_000)),
            attachment("c", "text/plain", &"c".repeat(400_000)),
        ],
    );
    assert_capture_refused(fixture, "aggregate bound");
}

fn assert_capture_refused(fixture: Fixture, label: &str) {
    let provider = Arc::new(RunProvider::new(&fixture, RunProviderMode::Tools, true));
    let tool = Arc::new(CountingTool::default());
    assert!(
        fixture
            .controller
            .prepare_autonomous_activation(
                fixture.activation.clone(),
                resources(&fixture, provider.clone(), tool.clone())
            )
            .is_err(),
        "{label}"
    );
    assert_eq!(provider.calls.load(Ordering::SeqCst), 0, "{label}");
    assert_eq!(tool.count.load(Ordering::SeqCst), 0, "{label}");
    let state = fixture.controller.lock().unwrap();
    assert!(state.bound.is_empty(), "{label}");
    // Refusal precedes provider registration, so there is no accounting
    // coverage yet; absence must not be reported as measured zero usage.
    assert!(
        state.authority.provider_usage(&fixture.activation).is_err(),
        "{label}"
    );
    let snapshot = state.canonical.snapshot(&state.turn_id).unwrap();
    assert!(
        state
            .content
            .activation_output_reservation(&snapshot, &fixture.activation)
            .unwrap()
            .is_none(),
        "{label}"
    );
    assert!(snapshot.contract().invocations().is_empty(), "{label}");
}
