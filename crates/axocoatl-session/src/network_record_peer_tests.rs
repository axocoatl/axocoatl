//! The `peer` of an `open` event: wire form, bounds and older records.

use super::*;

fn peer() -> PeerIdentity {
    PeerIdentity {
        pid: Some(4242),
        uid: Some(1000),
        gid: Some(1000),
        exe: Some("/usr/bin/curl".into()),
        exe_sha256: Some("ab".repeat(32)),
        ancestors: vec!["/usr/bin/bash".into(), "/axocoatl-exec-supervisor".into()],
        error: None,
    }
}

fn open_with(peer: Option<PeerIdentity>) -> NetworkEvent {
    NetworkEvent::Open {
        conn: "g1:7".into(),
        peer,
        decision: Decision::Allow,
        reason: None,
        status: None,
        rule: Some("config#0".into()),
        host: "registry.npmjs.org".into(),
        port: 443,
        conn_kind: ConnKind::Connect,
        method: None,
        path: None,
        addrs: vec!["104.16.0.1".into()],
        token: Some("0123456789abcdef".into()),
        binding: None,
        scope: Some(EgressScope::Session),
        policy_revision: Some(1),
    }
}

#[test]
fn the_peer_follows_conn_on_the_wire_and_is_omitted_when_unknown() {
    let event = open_with(Some(peer()));
    event.validate().unwrap();
    let wire = serde_json::to_string(&event).unwrap();
    assert!(
        wire.starts_with(r#"{"kind":"open","conn":"g1:7","peer":{"pid":4242,"uid":1000,"gid":1000,"exe":"/usr/bin/curl","#),
        "{wire}"
    );
    assert_eq!(serde_json::from_str::<NetworkEvent>(&wire).unwrap(), event);
    let without = serde_json::to_string(&open_with(None)).unwrap();
    assert!(!without.contains("peer"), "{without}");
    // Records written before the field existed still read.
    let older = r#"{"kind":"open","conn":"g1:1","decision":"deny","reason":"not_allowed","status":403,"host":"x.test","port":443,"conn_kind":"connect","addrs":[]}"#;
    match serde_json::from_str::<NetworkEvent>(older).unwrap() {
        NetworkEvent::Open { peer, .. } => assert_eq!(peer, None),
        other => panic!("{other:?}"),
    }
    let failed = PeerIdentity {
        uid: Some(1000),
        error: Some("no_access".into()),
        ..PeerIdentity::default()
    };
    assert_eq!(
        serde_json::to_string(&failed).unwrap(),
        r#"{"uid":1000,"error":"no_access"}"#
    );
    open_with(Some(failed)).validate().unwrap();
}

#[test]
fn a_peer_out_of_bounds_is_refused() {
    for broken in [
        PeerIdentity {
            exe: Some(format!("/{}", "a".repeat(MAX_RECORDED_PEER_PATH_CHARS))),
            ..peer()
        },
        PeerIdentity {
            exe: Some("/usr/bin/cu\nrl".into()),
            ..peer()
        },
        PeerIdentity {
            exe: Some(String::new()),
            ..peer()
        },
        PeerIdentity {
            exe_sha256: Some("xyz".into()),
            ..peer()
        },
        PeerIdentity {
            ancestors: vec!["/bin/sh".into(); MAX_RECORDED_PEER_ANCESTORS + 1],
            ..peer()
        },
        PeerIdentity {
            ancestors: vec![format!("/{}", "a".repeat(MAX_RECORDED_PEER_ANCESTOR_CHARS))],
            ..peer()
        },
        PeerIdentity {
            error: Some("No Access".into()),
            ..peer()
        },
    ] {
        assert!(
            open_with(Some(broken.clone())).validate().is_err(),
            "{broken:?}"
        );
    }
    assert!(serde_json::from_str::<PeerIdentity>(r#"{"pid":1,"shell":"sh"}"#).is_err());
}
