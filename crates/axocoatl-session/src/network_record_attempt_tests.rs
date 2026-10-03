use super::*;

#[test]
fn an_attempt_binding_round_trips_and_older_lines_still_read() {
    let line = NetworkLine {
        v: 1,
        seq: 4,
        ts_ms: 1_000,
        event: NetworkEvent::Bind {
            token: "0123456789abcdef".into(),
            binding: EgressBinding {
                invocation_id: Some("inv-1".into()),
                attempt_id: Some("attempt-s1-set1-0".into()),
                ..EgressBinding::new(BindingKind::Agent)
            },
            scope: EgressScope::Session,
        },
    };
    line.event.validate().unwrap();
    let json = serde_json::to_string(&line).unwrap();
    assert!(
        json.contains(r#""attempt_id":"attempt-s1-set1-0""#),
        "{json}"
    );
    assert_eq!(serde_json::from_str::<NetworkLine>(&json).unwrap(), line);

    // A binding without an attempt omits the field, so lines written before
    // it existed and lines from the Session's own container look the same.
    let session = serde_json::to_string(&EgressBinding::new(BindingKind::Setup)).unwrap();
    assert_eq!(session, r#"{"kind":"setup"}"#);
    let older: EgressBinding =
        serde_json::from_str(r#"{"kind":"agent","invocation_id":"inv-0"}"#).unwrap();
    assert_eq!(older.attempt_id, None);
    assert_eq!(older.invocation_id.as_deref(), Some("inv-0"));
}
