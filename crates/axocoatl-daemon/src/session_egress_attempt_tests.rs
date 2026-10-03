//! A Ways attempt under `network: egress` uses its Session's decision point:
//! its credentials are recorded in the Session's record with the attempt.

use super::tests::{FakeRecord, FakeResolver};
use super::*;

async fn session_egress(record: Arc<FakeRecord>, env_dir: &std::path::Path) -> Arc<SessionEgress> {
    let config = axocoatl_config::parse_config(
        "sandbox:\n  network: egress\n  egress:\n    allow:\n      - host: allowed.test\n",
        std::path::Path::new("test.yaml"),
    )
    .unwrap();
    SessionEgress::open(
        "ses-1",
        EgressPolicyConfig::from_config(&config),
        record,
        FakeResolver::with(&[("allowed.test", &["93.184.216.34"])]),
        Some(SecureDir::open(env_dir).unwrap()),
    )
    .await
    .unwrap()
}

#[tokio::test]
async fn an_attempts_credential_is_recorded_with_the_attempt() {
    let dir = tempfile::tempdir().unwrap();
    let record = Arc::new(FakeRecord::default());
    let egress = session_egress(record.clone(), dir.path()).await;
    let attempt = GrantSpec {
        invocation_id: Some("inv-a".into()),
        attempt_id: Some("attempt-s1-set1-0".into()),
        ..GrantSpec::new(GrantKind::Agent)
    };
    let attempt_grant = egress.grant(attempt).await.unwrap();
    let session_grant = egress
        .grant(GrantSpec {
            invocation_id: Some("inv-s".into()),
            ..GrantSpec::new(GrantKind::Agent)
        })
        .await
        .unwrap();
    let bindings: Vec<(String, EgressBinding, EgressScope)> = record
        .events()
        .into_iter()
        .filter_map(|event| match event {
            NetworkEvent::Bind {
                token,
                binding,
                scope,
            } => Some((token, binding, scope)),
            _ => None,
        })
        .collect();
    assert_eq!(bindings.len(), 2, "{bindings:?}");
    let (token, binding, scope) = &bindings[0];
    assert_eq!(*token, attempt_grant.token_tag);
    assert_eq!(binding.attempt_id.as_deref(), Some("attempt-s1-set1-0"));
    assert_eq!(binding.invocation_id.as_deref(), Some("inv-a"));
    // The attempt uses the Session's own allowlist.
    assert_eq!(*scope, EgressScope::Session);
    let (token, binding, _) = &bindings[1];
    assert_eq!(*token, session_grant.token_tag);
    assert_eq!(binding.attempt_id, None);
    // Each env file is the attempt's own credential.
    assert_ne!(attempt_grant.env_file, session_grant.env_file);
}
