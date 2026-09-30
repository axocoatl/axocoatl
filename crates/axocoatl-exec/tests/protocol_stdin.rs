use axocoatl_exec::protocol::{ExecRequest, StdinDescriptor, MAX_STDIN_BYTES, PROTOCOL_VERSION};

fn request() -> ExecRequest {
    ExecRequest {
        protocol: PROTOCOL_VERSION,
        invocation_id: "stdin-contract".into(),
        argv: vec!["/bin/cat".into()],
        stdin: None,
        timeout_ms: 1_000,
        stdout_bytes: 128,
        stderr_bytes: 128,
        write_restriction: None,
    }
}

#[test]
fn stdin_exact_bytes_are_bound_into_request_identity_and_existing_absence_is_explicit() {
    let mut request = request();
    let plain = request.digest().unwrap();
    assert!(serde_json::to_value(&request)
        .unwrap()
        .get("stdin")
        .is_none());
    request.validate_stdin(None).unwrap();
    assert!(request.validate_stdin(Some(b"ignored")).is_err());
    request.stdin = Some(StdinDescriptor::for_bytes(b"a\0b\n\xff").unwrap());
    request.validate_stdin(Some(b"a\0b\n\xff")).unwrap();
    assert_ne!(request.digest().unwrap(), plain);
    assert!(request.validate_stdin(Some(b"a\0b\n\xfe")).is_err());
    assert!(request.validate_stdin(None).is_err());
    let encoded = serde_json::to_vec(&request).unwrap();
    let decoded: ExecRequest = serde_json::from_slice(&encoded).unwrap();
    assert_eq!(decoded, request);
    request.stdin = Some(StdinDescriptor::for_bytes(b"").unwrap());
    request.validate_stdin(Some(b"")).unwrap();
    assert!(request.validate_stdin(None).is_err());
}

#[test]
fn stdin_size_hash_and_old_protocol_are_rejected_without_dispatch() {
    assert!(StdinDescriptor::for_bytes(&vec![0; MAX_STDIN_BYTES]).is_ok());
    assert!(StdinDescriptor::for_bytes(&vec![0; MAX_STDIN_BYTES + 1]).is_err());
    let mut request = request();
    request.protocol = 1;
    assert!(request.validate().is_err());
    request.protocol = PROTOCOL_VERSION;
    request.stdin = Some(StdinDescriptor {
        byte_len: MAX_STDIN_BYTES + 1,
        sha256: "0".repeat(64),
    });
    assert!(request.validate().is_err());
    request.stdin = Some(StdinDescriptor {
        byte_len: 1,
        sha256: "F".repeat(64),
    });
    assert!(request.validate().is_err());
    request.stdin = Some(StdinDescriptor {
        byte_len: 1,
        sha256: "0".repeat(63),
    });
    assert!(request.validate().is_err());
}
