//! Scripted peers exercise the host transport boundary only. They are not
//! trusted supervisor binaries and do not prove Linux process quiescence.

use super::*;
use axocoatl_exec::protocol::{
    OutputCapture, PrimaryExit, ProcessOutcome, PROTOCOL_VERSION, SUPERVISOR_VERSION,
};
use std::pin::Pin;
use std::task::{Context, Poll};

const RUNTIME: &str = "exact-owned-container-id";
const PROGRAM_SHA256: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

fn request() -> ExecRequest {
    ExecRequest {
        protocol: PROTOCOL_VERSION,
        stdin: None,
        invocation_id: "repository-check-17".into(),
        argv: vec!["/bin/true".into()],
        timeout_ms: 30_000,
        stdout_bytes: 128,
        stderr_bytes: 128,
        write_restriction: None,
    }
}

fn ready(request: &ExecRequest) -> ServerMessage {
    ServerMessage::Ready {
        protocol: PROTOCOL_VERSION,
        invocation_id: request.invocation_id.clone(),
        request_sha256: request.digest().unwrap(),
        supervisor_version: SUPERVISOR_VERSION.into(),
    }
}

fn finished(request: &ExecRequest, launched: bool, quiescent: bool) -> ServerMessage {
    ServerMessage::Finished {
        protocol: PROTOCOL_VERSION,
        invocation_id: request.invocation_id.clone(),
        request_sha256: request.digest().unwrap(),
        outcome: if launched {
            ProcessOutcome::Exited { code: 0 }
        } else {
            ProcessOutcome::TimedOut
        },
        primary_exit: launched.then_some(PrimaryExit::Exited { code: 0 }),
        launched,
        stdout: OutputCapture::new(request.stdout_bytes)
            .unwrap()
            .finish(true),
        stderr: OutputCapture::new(request.stderr_bytes)
            .unwrap()
            .finish(true),
        quiescent,
    }
}

fn frame(message: &ServerMessage) -> Vec<u8> {
    let mut bytes = serde_json::to_vec(message).unwrap();
    bytes.push(b'\n');
    bytes
}

fn peer(script: &str, request: &ExecRequest, terminal: &ServerMessage) -> Command {
    let mut command = Command::new("/bin/sh");
    command.args(["-c", script]);
    command.env("READY", serde_json::to_string(&ready(request)).unwrap());
    command.env("FINISHED", serde_json::to_string(terminal).unwrap());
    command
}

async fn prepare(command: Command, request: &ExecRequest) -> PreparedSupervisedCommand {
    prepare_command(
        command,
        request.clone(),
        RUNTIME.into(),
        PROGRAM_SHA256.into(),
    )
    .await
    .unwrap()
}

async fn collect(
    prepared: PreparedSupervisedCommand,
) -> Result<SupervisedExecution, IsolationError> {
    tokio::time::timeout(
        Duration::from_secs(5),
        prepared.dispatch().unwrap().finish(),
    )
    .await
    .expect("scripted peer did not complete")
}

async fn wait_until_collected(prepared: &PreparedSupervisedCommand) {
    tokio::time::timeout(Duration::from_secs(5), async {
        while !prepared.task.as_ref().unwrap().is_finished() {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .expect("terminal frame was not collected before dispatch");
}

fn assert_failure(result: Result<SupervisedExecution, IsolationError>, needle: &str) {
    match result {
        Ok(_) => panic!("invalid transport evidence unexpectedly returned a result"),
        Err(error) => assert!(error.to_string().contains(needle), "{error}"),
    }
}

const SUCCESS_PEER: &str = r#"
IFS= read -r request || exit 91
printf '%s\n' "$READY"
IFS= read -r control || exit 92
[ "$control" = '{"kind":"dispatch"}' ] || exit 93
printf '%s\n' "$FINISHED"
"#;

#[tokio::test]
async fn preparation_does_not_dispatch_and_settlement_binds_the_exact_transport() {
    let request = request();
    let terminal = finished(&request, true, true);
    let directory = tempfile::tempdir().unwrap();
    let marker = directory.path().join("dispatched");
    let mut command = peer(
        r#"
IFS= read -r request || exit 91
printf '%s\n' "$READY"
IFS= read -r control || exit 92
[ "$control" = '{"kind":"dispatch"}' ] || exit 93
printf dispatched > "$MARKER"
printf '%s\n' "$FINISHED"
"#,
        &request,
        &terminal,
    );
    command.env("MARKER", &marker);
    let prepared = prepare(command, &request).await;
    assert!(!marker.exists(), "Ready must not launch repository work");
    let transport_id = prepared.transport_identity().to_owned();
    let execution = collect(prepared).await.unwrap();
    assert!(marker.exists());
    assert_eq!(execution.result(), &terminal);
    assert_eq!(execution.request(), &request);
    assert_eq!(execution.runtime_identity(), RUNTIME);
    assert_eq!(execution.program_sha256(), PROGRAM_SHA256);
    let receipt = execution.settlement().unwrap();
    assert_eq!(receipt.transport_identity(), transport_id);
    assert_eq!(receipt.invocation_id(), request.invocation_id);
    assert_eq!(receipt.request_sha256(), request.digest().unwrap());
    assert_eq!(receipt.runtime_identity(), RUNTIME);
    assert_eq!(receipt.program_sha256(), PROGRAM_SHA256);

    // Even a repeated logical request cannot settle a different live transport.
    let second = prepare(peer(SUCCESS_PEER, &request, &terminal), &request).await;
    assert_ne!(second.transport_identity(), transport_id);
    let second_id = second.transport_identity().to_owned();
    assert_eq!(
        collect(second)
            .await
            .unwrap()
            .settlement()
            .unwrap()
            .transport_identity(),
        second_id
    );
}

#[tokio::test]
async fn expired_ready_without_dispatch_remains_collectable_after_handoff_receiver_closes() {
    let request = request();
    let terminal = finished(&request, false, true);
    let prepared = prepare(
        peer(
            r#"
IFS= read -r request || exit 91
printf '%s\n%s\n' "$READY" "$FINISHED"
"#,
            &request,
            &terminal,
        ),
        &request,
    )
    .await;
    wait_until_collected(&prepared).await;
    let execution = collect(prepared).await.unwrap();
    assert_eq!(execution.result(), &terminal);
    assert!(execution.settlement().is_some());
}

#[tokio::test]
async fn cancelled_preparation_sends_cancel_and_preserves_no_launch_receipt() {
    let request = request();
    let mut terminal = finished(&request, false, true);
    if let ServerMessage::Finished { outcome, .. } = &mut terminal {
        *outcome = ProcessOutcome::Cancelled;
    }
    let prepared = prepare(
        peer(
            r#"
IFS= read -r request || exit 91
printf '%s\n' "$READY"
IFS= read -r control || exit 92
[ "$control" = '{"kind":"cancel"}' ] || exit 93
printf '%s\n' "$FINISHED"
"#,
            &request,
            &terminal,
        ),
        &request,
    )
    .await;
    prepared.cancellation().cancel();
    let execution = collect(prepared).await.unwrap();
    assert_eq!(execution.result(), &terminal);
    assert!(execution.settlement().is_some());
}

#[tokio::test]
async fn stop_racing_a_collected_finish_does_not_discard_its_receipt() {
    let request = request();
    let terminal = finished(&request, true, true);
    let prepared = prepare(peer(SUCCESS_PEER, &request, &terminal), &request).await;
    let running = prepared.dispatch().unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        while !running.task.as_ref().unwrap().is_finished() {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .unwrap();
    running.cancellation().cancel();
    let execution = running.finish().await.unwrap();
    assert_eq!(execution.result(), &terminal);
    assert!(execution.settlement().is_some());
}

#[tokio::test]
async fn stop_during_a_partial_finished_frame_collects_the_original_terminal_result() {
    let request = request();
    let terminal = finished(&request, true, true);
    let encoded = serde_json::to_string(&terminal).unwrap();
    let split = encoded.len() / 2;
    let directory = tempfile::tempdir().unwrap();
    let marker = directory.path().join("terminal-prefix-sent");
    let mut command = peer(
        r#"
IFS= read -r request || exit 91
printf '%s\n' "$READY"
IFS= read -r control || exit 92
[ "$control" = '{"kind":"dispatch"}' ] || exit 93
printf '%s' "$PREFIX"
printf sent > "$MARKER"
IFS= read -r control || exit 94
[ "$control" = '{"kind":"cancel"}' ] || exit 95
printf '%s\n' "$SUFFIX"
"#,
        &request,
        &terminal,
    );
    command
        .env("PREFIX", &encoded[..split])
        .env("SUFFIX", &encoded[split..])
        .env("MARKER", &marker);
    let running = prepare(command, &request).await.dispatch().unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        while !marker.exists() {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .unwrap();
    running.cancellation().cancel();
    let execution = tokio::time::timeout(Duration::from_secs(5), running.finish())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(execution.result(), &terminal);
    assert!(execution.settlement().is_some());
}

#[tokio::test]
async fn confirmed_primary_exit_without_quiescence_does_not_mint_settlement() {
    let request = request();
    let terminal = finished(&request, true, false);
    let execution = collect(prepare(peer(SUCCESS_PEER, &request, &terminal), &request).await)
        .await
        .unwrap();
    assert_eq!(execution.result(), &terminal);
    assert!(execution.settlement().is_none());
}

#[tokio::test]
async fn helper_loss_after_dispatch_cannot_mint_settlement() {
    let request = request();
    let terminal = finished(&request, true, true);
    let prepared = prepare(
        peer(
            r#"
IFS= read -r request || exit 91
printf '%s\n' "$READY"
IFS= read -r control || exit 92
exit 0
"#,
            &request,
            &terminal,
        ),
        &request,
    )
    .await;
    assert_failure(collect(prepared).await, "without a complete result");
}

#[tokio::test]
async fn valid_finished_followed_by_failed_transport_cannot_mint_settlement() {
    let request = request();
    let terminal = finished(&request, true, true);
    let script = format!("{SUCCESS_PEER}\nexit 17\n");
    let prepared = prepare(peer(&script, &request, &terminal), &request).await;
    assert_failure(collect(prepared).await, "ended unsuccessfully");
}

#[tokio::test]
async fn trailing_duplicate_or_partial_evidence_cannot_mint_settlement() {
    let request = request();
    let terminal = finished(&request, true, true);
    for (suffix, expected) in [
        ("printf '%s\\n' \"$FINISHED\"", "unexpected trailing"),
        ("printf '{'", "incomplete helper control frame"),
    ] {
        let script = format!("{SUCCESS_PEER}\n{suffix}\n");
        let prepared = prepare(peer(&script, &request, &terminal), &request).await;
        assert_failure(collect(prepared).await, expected);
    }
}

#[tokio::test]
async fn mismatched_identity_or_output_digest_cannot_mint_settlement() {
    let request = request();
    for field in ["invocation", "request", "output"] {
        let mut terminal = finished(&request, true, true);
        if let ServerMessage::Finished {
            invocation_id,
            request_sha256,
            stdout,
            ..
        } = &mut terminal
        {
            match field {
                "invocation" => invocation_id.push_str("-other"),
                "request" => *request_sha256 = "f".repeat(64),
                "output" => stdout.observed_sha256 = "f".repeat(64),
                _ => unreachable!(),
            }
        }
        let prepared = prepare(peer(SUCCESS_PEER, &request, &terminal), &request).await;
        assert!(
            collect(prepared).await.is_err(),
            "accepted mismatched {field}"
        );
    }
}

#[tokio::test]
async fn repeated_ready_after_dispatch_cannot_mint_settlement() {
    let request = request();
    let prepared = prepare(peer(SUCCESS_PEER, &request, &ready(&request)), &request).await;
    assert_failure(
        collect(prepared).await,
        "unexpected repeated helper readiness",
    );
}

#[tokio::test]
async fn launched_result_before_dispatch_is_rejected() {
    let request = request();
    let terminal = finished(&request, true, true);
    let prepared = prepare(
        peer(
            r#"
IFS= read -r request || exit 91
printf '%s\n%s\n' "$READY" "$FINISHED"
"#,
            &request,
            &terminal,
        ),
        &request,
    )
    .await;
    wait_until_collected(&prepared).await;
    assert_failure(collect(prepared).await, "launched before host dispatch");
}

#[tokio::test]
async fn finished_instead_of_ready_rejects_preparation() {
    let request = request();
    let terminal = finished(&request, false, true);
    let command = peer(
        r#"
IFS= read -r request || exit 91
printf '%s\n' "$FINISHED"
"#,
        &request,
        &terminal,
    );
    let result = prepare_command(command, request, RUNTIME.into(), PROGRAM_SHA256.into()).await;
    match result {
        Ok(_) => panic!("Finished cannot admit a new prepared command"),
        Err(error) => assert!(
            error.to_string().contains("before host admission"),
            "{error}"
        ),
    }
}

#[tokio::test]
async fn failed_preparation_returns_bounded_stderr_after_transport_cleanup() {
    let mut command = Command::new("/bin/sh");
    command.args([
        "-c",
        "IFS= read -r request || exit 91; printf '%s' \"$DIAGNOSTIC\" >&2; exit 17",
    ]);
    command.env(
        "DIAGNOSTIC",
        format!(
            "fixture startup failure: {}NOT-RETAINED",
            "x".repeat(STDERR_LIMIT * 2)
        ),
    );
    let result = tokio::time::timeout(
        Duration::from_secs(5),
        prepare_command(command, request(), RUNTIME.into(), PROGRAM_SHA256.into()),
    )
    .await
    .expect("failed preparation should collect and reap promptly");
    match result {
        Ok(_) => panic!("peer did not send Ready"),
        Err(error) => {
            let message = error.to_string();
            assert!(message.contains("fixture startup failure"), "{message}");
            assert!(!message.contains("NOT-RETAINED"));
            assert!(message.len() < STDERR_LIMIT + 512);
        }
    }
}

#[tokio::test]
async fn framing_rejects_malformed_partial_and_oversized_data() {
    for (bytes, expected) in [
        (b"not-json\n".to_vec(), "expected"),
        (b"{\"kind\":".to_vec(), "incomplete helper control frame"),
        (vec![b' '; MAX_RESPONSE_BYTES + 1], "frame exceeds bound"),
    ] {
        let mut reader = FrameReader::new(bytes.as_slice());
        match reader.read_frame().await {
            Ok(_) => panic!("malformed frame was accepted"),
            Err(error) => assert!(error.to_string().contains(expected), "{error}"),
        }
    }
}

#[tokio::test]
async fn framing_accepts_exact_bound_and_leaves_each_frame_separate() {
    let expected = ready(&request());
    let mut bytes = frame(&expected);
    bytes.pop();
    bytes.resize(MAX_RESPONSE_BYTES - 1, b' ');
    bytes.push(b'\n');
    bytes.extend(frame(&expected));
    let mut reader = FrameReader::new(bytes.as_slice());
    assert_eq!(reader.read_frame().await.unwrap(), Some(expected.clone()));
    assert_eq!(reader.read_frame().await.unwrap(), Some(expected));
    assert!(reader.read_frame().await.unwrap().is_none());
}

#[tokio::test]
async fn partial_frame_survives_cancelled_read_future() {
    let expected = finished(&request(), true, true);
    let bytes = frame(&expected);
    let (mut writer, reader) = tokio::io::duplex(bytes.len());
    let mut reader = FrameReader::new(reader);
    let split = bytes.len() / 3;
    for part in bytes[..split * 2].chunks(split) {
        writer.write_all(part).await.unwrap();
        // The first branch consumes all available bytes, then blocks waiting
        // for the newline. Interrupting it must retain those consumed bytes.
        tokio::select! {
            biased;
            result = reader.read_frame() => panic!("partial frame completed: {result:?}"),
            _ = tokio::task::yield_now() => (),
        }
    }
    assert_eq!(reader.pending, bytes[..split * 2]);
    writer.write_all(&bytes[split * 2..]).await.unwrap();
    assert_eq!(reader.read_frame().await.unwrap(), Some(expected));
}

struct BrokenControl;

impl tokio::io::AsyncWrite for BrokenControl {
    fn poll_write(
        self: Pin<&mut Self>,
        _: &mut Context<'_>,
        _: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        Poll::Ready(Err(std::io::Error::new(
            std::io::ErrorKind::BrokenPipe,
            "fixture closed input",
        )))
    }
    fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Poll::Ready(Ok(()))
    }
    fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

#[tokio::test]
async fn failed_dispatch_or_stop_write_still_collects_valid_terminal_frame() {
    let request = request();
    for (control, before_dispatch, launched) in [
        (Control::Dispatch, false, false),
        (Control::Cancel, false, true),
        (Control::Cancel, true, false),
    ] {
        let terminal = finished(&request, launched, true);
        let bytes = frame(&terminal);
        let mut output = FrameReader::new(bytes.as_slice());
        let mut input = Some(BrokenControl);
        let failure = transmit_control(&mut input, &control).await.unwrap_err();
        assert!(input.is_none(), "failed control must close its input");
        let collected =
            terminal_after_control(&mut output, &request, before_dispatch, Some(failure))
                .await
                .unwrap();
        assert_eq!(collected, terminal);
    }
}

#[tokio::test]
async fn failed_control_without_terminal_preserves_both_failures() {
    let request = request();
    let mut input = Some(BrokenControl);
    let failure = transmit_control(&mut input, &Control::Cancel)
        .await
        .unwrap_err();
    let mut output = FrameReader::new(&b""[..]);
    let error = terminal_after_control(&mut output, &request, true, Some(failure))
        .await
        .unwrap_err();
    assert!(
        error.to_string().contains("fixture closed input"),
        "{error}"
    );
    assert!(
        error.to_string().contains("without a complete result"),
        "{error}"
    );
}

#[tokio::test(start_paused = true)]
async fn pre_dispatch_cancel_collection_uses_cleanup_bound_not_request_deadline() {
    let mut request = request();
    request.timeout_ms = axocoatl_exec::protocol::MAX_TIMEOUT_MS;
    let (_writer, reader) = tokio::io::duplex(64);
    let mut output = FrameReader::new(reader);
    let started = tokio::time::Instant::now();
    let error = terminal_after_control(&mut output, &request, true, None)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("after control"), "{error}");
    assert_eq!(
        started.elapsed(),
        Duration::from_millis(CLEANUP_TIMEOUT_MS) + TRANSPORT_GRACE
    );
}

#[tokio::test]
async fn raw_stdin_arrives_before_ready_and_is_bound_to_the_exact_transport() {
    let directory = tempfile::tempdir().unwrap();
    let retained = directory.path().join("stdin");
    let dispatched = directory.path().join("dispatched");
    let bytes = b"line\n{\"kind\":\"dispatch\"}\n\0\xff'\"$()\\tail".to_vec();
    let mut request = request();
    request.stdin = Some(axocoatl_exec::protocol::StdinDescriptor::for_bytes(&bytes).unwrap());
    let terminal = finished(&request, true, true);
    let mut command = peer(
        r#"
IFS= read -r header || exit 91
dd bs=1 count="$BYTE_LENGTH" of="$BODY" 2>/dev/null || exit 92
printf '%s\n' "$READY"
IFS= read -r control || exit 93
[ "$control" = '{"kind":"dispatch"}' ] || exit 94
printf dispatched > "$DISPATCHED"
printf '%s\n' "$FINISHED"
"#,
        &request,
        &terminal,
    );
    command
        .env("BYTE_LENGTH", bytes.len().to_string())
        .env("BODY", &retained)
        .env("DISPATCHED", &dispatched);
    let prepared = prepare_command_with_stdin(
        command,
        request.clone(),
        Some(std::sync::Arc::from(bytes.clone())),
        RUNTIME.into(),
        PROGRAM_SHA256.into(),
    )
    .await
    .unwrap();
    assert_eq!(std::fs::read(retained).unwrap(), bytes);
    assert!(!dispatched.exists());
    let transport = prepared.transport_identity().to_owned();
    let observed = collect(prepared).await.unwrap();
    assert_eq!(observed.request(), &request);
    let settlement = observed.settlement().unwrap();
    assert_eq!(settlement.transport_identity(), transport);
    assert_eq!(settlement.request_sha256(), request.digest().unwrap());
    assert!(dispatched.exists());
    // This scripted peer proves host framing/binding only; Linux helper tests
    // independently verify actual child input and descendant settlement.
}

#[tokio::test]
async fn mismatched_or_missing_stdin_is_rejected_before_transport_spawn() {
    let directory = tempfile::tempdir().unwrap();
    let marker = directory.path().join("spawned");
    let mut request = request();
    request.stdin = Some(axocoatl_exec::protocol::StdinDescriptor::for_bytes(b"expected").unwrap());
    let terminal = finished(&request, true, true);
    for body in [
        None,
        Some(std::sync::Arc::<[u8]>::from(b"different".as_slice())),
    ] {
        let mut command = peer("printf spawned > \"$MARKER\"; exit 91", &request, &terminal);
        command.env("MARKER", &marker);
        assert!(prepare_command_with_stdin(
            command,
            request.clone(),
            body,
            RUNTIME.into(),
            PROGRAM_SHA256.into()
        )
        .await
        .is_err());
        assert!(!marker.exists());
    }
}

#[tokio::test]
async fn old_protocol_ready_is_refused_before_dispatch() {
    let request = request();
    let terminal = finished(&request, true, true);
    let directory = tempfile::tempdir().unwrap();
    let marker = directory.path().join("must-not-dispatch");
    let mut old_ready = ready(&request);
    if let ServerMessage::Ready { protocol, .. } = &mut old_ready {
        *protocol = 1;
    }
    let mut command = peer(
        r#"
IFS= read -r request || exit 91
printf '%s\n' "$READY"
IFS= read -r control || exit 0
printf dispatched > "$MARKER"
"#,
        &request,
        &terminal,
    );
    command
        .env("READY", serde_json::to_string(&old_ready).unwrap())
        .env("MARKER", &marker);
    assert!(
        prepare_command(command, request, RUNTIME.into(), PROGRAM_SHA256.into())
            .await
            .is_err()
    );
    assert!(!marker.exists());
}

/// J3: in a hardened container the workload users' supervised commands run
/// under `--harden`; root's, and every command of an image-mode container,
/// run without it.
#[test]
fn only_the_workload_users_commands_of_a_hardened_container_are_hardened() {
    use crate::ExecIdentity::{Helper, Root, Writer};
    for identity in [Writer, Helper] {
        assert_eq!(
            supervisor_serve_args(true, identity),
            ["/axocoatl-exec-supervisor", "--serve", "--harden"]
        );
        assert_eq!(
            supervisor_serve_args(false, identity),
            ["/axocoatl-exec-supervisor", "--serve"]
        );
    }
    for hardened in [true, false] {
        assert_eq!(
            supervisor_serve_args(hardened, Root),
            ["/axocoatl-exec-supervisor", "--serve"]
        );
    }
}
