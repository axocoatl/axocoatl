//! The exact Send -> applied Team -> immutable model profile admission seam.
use super::*;
use sha2::{Digest, Sha256};

fn choose_model(f: &mut NativeFixture, model: &str, widen_output: bool) {
    let token = f
        .registry
        .session_team_token(f.request.session_id.as_str())
        .unwrap();
    let selection = f
        .registry
        .with_session_team_stores(&token, |canonical, content, _| {
            let team = SessionTeamStore::open_owned(
                canonical
                    .component_namespace(ExecutionComponent::SessionTeam)
                    .unwrap(),
                canonical,
                content,
                None,
            )
            .unwrap();
            let slot = team.current().unwrap().unwrap().graph.slots[0].clone();
            drop(team);
            let ActivationEvidenceContent::Definition { configuration, .. } = content
                .resolve_activation_evidence(&slot.definition.snapshot)
                .unwrap()
            else {
                panic!("definition")
            };
            let mut config: AgentConfig = serde_json::from_str(configuration).unwrap();
            config.model = model.into();
            if widen_output {
                config.sampling.max_tokens = Some(4096);
            }
            let id = AgentDefinitionId::new(format!(
                "turn-model-{:x}",
                Sha256::digest(
                    serde_json::to_vec(&(
                        &f.request.turn_id,
                        &slot.node_id,
                        &slot.definition,
                        model
                    ))
                    .unwrap()
                )
            ))
            .unwrap();
            let profile = ExecutionProfile {
                definition: id.as_str().into(),
                provider: config.provider.clone(),
                model: config.model.clone(),
                isolation: "in-process".into(),
                tools: config.tools.clone(),
            };
            let definition = content
                .retain_activation_evidence(ActivationEvidenceContent::Definition {
                    definition_id: id.clone(),
                    revision: 1,
                    profile,
                    configuration: serde_json::to_string(&config).unwrap(),
                })
                .unwrap()
                .reference()
                .clone();
            Ok(crate::bootstrap::native_turn::NativeTurnModelSelection {
                node_id: slot.node_id,
                approved_definition: slot.definition,
                approved_grant: slot.grant.unwrap(),
                definition: DefinitionSnapshotRef {
                    definition_id: id,
                    snapshot: definition,
                },
                selected_model: model.into(),
            })
        })
        .unwrap();
    f.request.target_definition = Some(selection.approved_definition.definition_id.clone());
    f.request.request.target_definition = f.request.target_definition.clone();
    f.request
        .grants
        .retain(|grant| grant.holder == selection.node_id);
    f.request
        .node_evidence
        .retain(|node| node.node_id == selection.node_id);
    f.request.ingress.as_mut().unwrap()["model_override"] = serde_json::json!(model);
    f.request.model_selections = vec![selection];
}

#[tokio::test]
async fn per_turn_model_selection_preserves_limits_and_future_team_and_exact_replay() {
    let mut f = native_fixture().await;
    choose_model(&mut f, "another-local-model:latest", false);
    let original = f.request.grants[0].clone();
    let token = f
        .registry
        .session_team_token(f.request.session_id.as_str())
        .unwrap();
    let source = f.request.source().unwrap();
    let data = SecureDir::open(f.repository._data.path()).unwrap();
    let setup = prepare_admission(&f.registry, &token, &data, &f.request, &source).unwrap();
    assert_eq!(setup.content.graph.nodes.len(), 1);
    assert_eq!(
        setup.content.graph.nodes[0].definition,
        f.request.model_selections[0].definition
    );
    f.registry
        .with_session_team_stores(&token, |canonical, content, _| {
            let ActivationEvidenceContent::Grant { policy } = content
                .resolve_activation_evidence(&setup.content.nodes[0].grant.evidence)
                .unwrap()
            else {
                panic!("grant")
            };
            assert_eq!(policy.limits, original.limits);
            assert_eq!(policy.expires_at_ms, original.expires_at_ms);
            assert_eq!(policy.issuer_evidence, original.issuer_evidence);
            assert_eq!(policy.profiles[0].model, "another-local-model:latest");
            assert_ne!(policy.id, original.id);
            let team = SessionTeamStore::open_owned(
                canonical
                    .component_namespace(ExecutionComponent::SessionTeam)
                    .unwrap(),
                canonical,
                content,
                None,
            )
            .unwrap();
            assert_eq!(
                team.current().unwrap().unwrap().graph.slots[0].definition,
                f.request.model_selections[0].approved_definition
            );
            Ok(())
        })
        .unwrap();
    assert_eq!(
        prepare_admission(&f.registry, &token, &data, &f.request, &source)
            .unwrap()
            .content,
        setup.content
    );
    let (controller, _) = begin(&f, &f.request);
    assert_eq!(
        controller.snapshot().unwrap().contract().graph().unwrap(),
        &setup.content.graph
    );
    f.registry
        .request_human_turn_stop(f.request.session_id.as_str(), f.request.turn_id.as_str())
        .unwrap();
}

#[tokio::test]
async fn per_turn_model_selection_cannot_widen_output_or_change_original_approval() {
    let mut f = native_fixture().await;
    choose_model(&mut f, "another-local-model:latest", true);
    let token = f
        .registry
        .session_team_token(f.request.session_id.as_str())
        .unwrap();
    let data = SecureDir::open(f.repository._data.path()).unwrap();
    assert!(prepare_admission(
        &f.registry,
        &token,
        &data,
        &f.request,
        &f.request.source().unwrap()
    )
    .is_err());
    let mut f = native_fixture().await;
    choose_model(&mut f, "another-local-model:latest", false);
    let token = f
        .registry
        .session_team_token(f.request.session_id.as_str())
        .unwrap();
    let data = SecureDir::open(f.repository._data.path()).unwrap();
    f.request.grants[0].limits.tokens += 1;
    assert!(prepare_admission(
        &f.registry,
        &token,
        &data,
        &f.request,
        &f.request.source().unwrap()
    )
    .is_err());
    f.request.grants[0].limits.tokens -= 1;
    f.request.ingress.as_mut().unwrap()["model_override"] =
        serde_json::json!("different-selection");
    assert!(prepare_admission(
        &f.registry,
        &token,
        &data,
        &f.request,
        &f.request.source().unwrap()
    )
    .is_err());
}
