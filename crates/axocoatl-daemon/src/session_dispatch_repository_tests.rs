use super::*;
use axocoatl_exec::protocol::{OutputCapture, SUPERVISOR_VERSION};

fn identity() -> SupervisedRunIdentity {
    SupervisedRunIdentity {
        request: ExecRequest {
            protocol: PROTOCOL_VERSION,
            stdin: None,
            invocation_id: "actual-check".into(),
            argv: vec!["/bin/true".into()],
            timeout_ms: 1_000,
            stdout_bytes: 4,
            stderr_bytes: 3,
            write_restriction: None,
        },
        runtime_identity: "retained-container".into(),
        program_sha256: "a".repeat(64),
        transport_identity: "owned-transport-one".into(),
    }
}

fn captured(bytes: &[u8], capacity: usize, complete: bool) -> CapturedOutput {
    let mut capture = OutputCapture::new(capacity).unwrap();
    capture.observe(bytes).unwrap();
    capture.finish(complete)
}

fn message(
    identity: &SupervisedRunIdentity,
    outcome: ProcessOutcome,
    primary_exit: Option<PrimaryExit>,
    launched: bool,
) -> ServerMessage {
    ServerMessage::Finished {
        protocol: PROTOCOL_VERSION,
        invocation_id: identity.request.invocation_id.clone(),
        request_sha256: identity.request.digest().unwrap(),
        outcome,
        primary_exit,
        launched,
        stdout: captured(b"", identity.request.stdout_bytes, true),
        stderr: captured(b"", identity.request.stderr_bytes, true),
        // Tests can supply bytes, but cannot mint an opaque ProcessSettlement.
        quiescent: true,
    }
}

#[test]
fn json_quiescence_cannot_become_authoritative_repository_settlement() {
    let identity = identity();
    let terminal = message(
        &identity,
        ProcessOutcome::Exited { code: 0 },
        Some(PrimaryExit::Exited { code: 0 }),
        true,
    );
    let observed = observation_from_message(&terminal, &identity, None).unwrap();
    assert_eq!(observed.status, ConditionProcessStatus::Exited { code: 0 });
    let supervision = observed.supervision.unwrap();
    assert!(!supervision.quiescent);
    assert!(supervision.launched);
    assert_eq!(
        supervision.primary_exit,
        Some(ConditionProcessStatus::Exited { code: 0 })
    );
    assert_eq!(supervision.invocation_id, identity.request.invocation_id);
    assert_eq!(
        supervision.request_sha256,
        identity.request.digest().unwrap()
    );
    assert_eq!(supervision.runtime_identity, identity.runtime_identity);
    assert_eq!(supervision.program_sha256, identity.program_sha256);
    assert_eq!(supervision.transport_identity, identity.transport_identity);
}

#[test]
fn timeout_and_cancellation_preserve_independent_primary_exit() {
    let identity = identity();
    for (outcome, expected) in [
        (ProcessOutcome::TimedOut, ConditionProcessStatus::TimedOut),
        (
            ProcessOutcome::Cancelled,
            ConditionProcessStatus::Interrupted,
        ),
    ] {
        let terminal = message(
            &identity,
            outcome,
            Some(PrimaryExit::Exited { code: 7 }),
            true,
        );
        let observed = observation_from_message(&terminal, &identity, None).unwrap();
        assert_eq!(observed.status, expected);
        assert_eq!(
            observed.supervision.unwrap().primary_exit,
            Some(ConditionProcessStatus::Exited { code: 7 })
        );
    }
    let terminal = message(
        &identity,
        ProcessOutcome::Cancelled,
        Some(PrimaryExit::Signalled { signal: 15 }),
        true,
    );
    assert_eq!(
        observation_from_message(&terminal, &identity, None)
            .unwrap()
            .supervision
            .unwrap()
            .primary_exit,
        Some(ConditionProcessStatus::Signalled { signal: 15 })
    );
}

#[test]
fn no_launch_bytes_without_opaque_receipt_do_not_establish_nondispatch() {
    let identity = identity();
    for (outcome, expected) in [
        (ProcessOutcome::TimedOut, ConditionProcessStatus::TimedOut),
        (
            ProcessOutcome::Cancelled,
            ConditionProcessStatus::Interrupted,
        ),
    ] {
        let terminal = message(&identity, outcome, None, false);
        let observed = observation_from_message(&terminal, &identity, None).unwrap();
        assert_eq!(observed.status, expected);
        assert!(!observed.supervision.unwrap().quiescent);
    }
}

#[test]
fn binary_capture_preserves_prefix_and_digest_of_every_observed_byte() {
    let identity = identity();
    let bytes = b"\0\xffab\x80more";
    let mut terminal = message(
        &identity,
        ProcessOutcome::Exited { code: 1 },
        Some(PrimaryExit::Exited { code: 1 }),
        true,
    );
    if let ServerMessage::Finished { stdout, stderr, .. } = &mut terminal {
        *stdout = captured(bytes, identity.request.stdout_bytes, true);
        *stderr = captured(b"partial-error", identity.request.stderr_bytes, false);
    }
    let observed = observation_from_message(&terminal, &identity, None).unwrap();
    assert_eq!(observed.stdout.retained_bytes().unwrap(), bytes[..4]);
    assert_eq!(observed.stdout.observed_byte_len(), bytes.len() as u64);
    assert_eq!(
        observed.stdout.observed_sha256(),
        axocoatl_exec::protocol::sha256(bytes)
    );
    assert!(observed.stdout.complete());
    assert!(observed.stdout.is_truncated());
    assert_eq!(observed.stderr.retained_bytes().unwrap(), b"par");
    assert!(!observed.stderr.complete());
    assert_eq!(observed.stderr.observed_byte_len(), 13);
}

#[test]
fn malformed_or_foreign_observations_are_rejected_before_content_retention() {
    let identity = identity();
    for mutation in ["invocation", "request", "digest", "capture"] {
        let mut terminal = message(
            &identity,
            ProcessOutcome::Exited { code: 0 },
            Some(PrimaryExit::Exited { code: 0 }),
            true,
        );
        if let ServerMessage::Finished {
            invocation_id,
            request_sha256,
            stdout,
            ..
        } = &mut terminal
        {
            match mutation {
                "invocation" => invocation_id.push_str("-other"),
                "request" => *request_sha256 = "b".repeat(64),
                "digest" => stdout.observed_sha256 = "b".repeat(64),
                "capture" => *stdout = captured(b"too much", 8, true),
                _ => unreachable!(),
            }
        }
        assert!(
            observation_from_message(&terminal, &identity, None).is_err(),
            "{mutation}"
        );
    }
    let ready = ServerMessage::Ready {
        protocol: PROTOCOL_VERSION,
        invocation_id: identity.request.invocation_id.clone(),
        request_sha256: identity.request.digest().unwrap(),
        supervisor_version: SUPERVISOR_VERSION.into(),
    };
    assert!(observation_from_message(&ready, &identity, None).is_err());
}

#[test]
fn transport_loss_keeps_launch_unknown_and_empty_capture_incomplete() {
    let identity = identity();
    let observed = uncertain_observation(&identity.request, "lost connection".into()).unwrap();
    assert_eq!(
        observed.status,
        ConditionProcessStatus::Uncertain {
            message: "lost connection".into()
        }
    );
    assert!(
        observed.supervision.is_none(),
        "unknown launch is neither true nor false"
    );
    for capture in [&observed.stdout, &observed.stderr] {
        assert!(!capture.complete());
        assert!(capture.retained_bytes().unwrap().is_empty());
        assert_eq!(capture.observed_byte_len(), 0);
    }
}

#[test]
fn launch_failure_is_distinct_from_unknown_supervision_failure() {
    let identity = identity();
    let launch = message(
        &identity,
        ProcessOutcome::LaunchFailed {
            message: "executable absent".into(),
        },
        None,
        false,
    );
    let failure = message(
        &identity,
        ProcessOutcome::Failed {
            message: "lost process observation".into(),
        },
        None,
        false,
    );
    assert_eq!(
        observation_from_message(&launch, &identity, None)
            .unwrap()
            .status,
        ConditionProcessStatus::LaunchFailed {
            message: "executable absent".into()
        }
    );
    assert_eq!(
        observation_from_message(&failure, &identity, None)
            .unwrap()
            .status,
        ConditionProcessStatus::Uncertain {
            message: "lost process observation".into()
        }
    );
}

#[test]
fn uncertain_messages_are_nonempty_and_bounded_without_breaking_utf8() {
    assert!(!bounded_transport_message(String::new()).is_empty());
    let bounded = bounded_transport_message("a".repeat(511) + "€");
    assert_eq!(bounded.len(), 511);
    assert_eq!(bounded, "a".repeat(511));
    let observed = uncertain_observation(&identity().request, "€".repeat(1_000)).unwrap();
    let ConditionProcessStatus::Uncertain { message } = observed.status else {
        panic!("expected unknown")
    };
    assert!(message.len() <= 512);
    assert!(message.chars().all(|character| character == '€'));
}

/// This is an actual binary → Podman → controller observation test. It does not
/// claim to exercise the daemon's owner registry or every repository writer.
#[cfg(unix)]
#[tokio::test]
#[ignore = "requires a running Podman machine and a trusted image with repository tooling"]
async fn actual_embedded_supervisor_receipt_drives_repository_observation() {
    use axocoatl_core::SecureDir;
    use axocoatl_isolation::{SandboxPolicy, SessionSandbox, DEFAULT_IMAGE};
    let workspace = tempfile::tempdir().unwrap();
    let private = tempfile::tempdir().unwrap();
    let root = SecureDir::open(private.path().canonicalize().unwrap())
        .unwrap()
        .child("supervisor")
        .unwrap();
    let session = format!("repository-proof-{}", uuid::Uuid::new_v4());
    let image =
        std::env::var("AXOCOATL_TEST_SUPERVISOR_IMAGE").unwrap_or_else(|_| DEFAULT_IMAGE.into());
    let policy = SandboxPolicy {
        allow_untrusted_image: true,
        supervisor_installation: Some(root),
        runtime_authority: Some(format!("repository-proof-{}", uuid::Uuid::new_v4())),
        ..SandboxPolicy::default()
    };
    let sandbox =
        SessionSandbox::start(&session, workspace.path(), Some(&image), &[], &[], &policy)
            .await
            .unwrap();
    let exercised: Result<()> = async {
        for dispatch in [false, true] {
            let request = ExecRequest {
                protocol: PROTOCOL_VERSION,
            stdin: None,
                invocation_id: format!("observed-{}", uuid::Uuid::new_v4()),
                argv: vec!["/bin/sh".into(), "-c".into(), "printf '\\000\\377ok'; printf error >&2; exit 7".into()],
                timeout_ms: 10_000,
                stdout_bytes: 4,
                stderr_bytes: 3,
                write_restriction: None,
            };
            let prepared = sandbox.prepare_supervised_command(request.clone()).await.map_err(error)?;
            let identity = SupervisedRunIdentity {
                request,
                runtime_identity: prepared.runtime_identity().to_owned(),
                program_sha256: prepared.program_sha256().to_owned(),
                transport_identity: prepared.transport_identity().to_owned(),
            };
            if !dispatch { prepared.cancellation().cancel(); }
            let execution = prepared.dispatch().map_err(error)?.finish().await.map_err(error)?;
            let observed = observe_execution(&execution, &identity)?;
            if !observed.supervision.as_ref().is_some_and(|evidence| evidence.quiescent) {
                return Err(error("actual execution omitted its opaque settlement"));
            }
            if dispatch {
                if observed.status != (ConditionProcessStatus::Exited { code: 7 })
                    || observed.stdout.retained_bytes().map_err(error)? != b"\0\xffok"
                    || observed.stderr.retained_bytes().map_err(error)? != b"err"
                    || observed.stderr.observed_byte_len() != 5
                {
                    return Err(error("actual dispatched command evidence differed from its observed bytes and exit"));
                }
            } else if observed.status != ConditionProcessStatus::NotDispatched
                || observed.supervision.as_ref().is_none_or(|evidence| evidence.launched)
            {
                return Err(error("cancelled preparation did not retain exact nondispatch"));
            }
            let other_identity = SupervisedRunIdentity { transport_identity: "foreign-transport".into(), ..identity };
            if observe_execution(&execution, &other_identity).is_ok() {
                return Err(error("actual receipt accepted a foreign transport identity"));
            }
        }
        Ok(())
    }.await;
    let cleaned = sandbox.stop_checked().await;
    exercised.unwrap();
    cleaned.unwrap();
}
