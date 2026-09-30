#![cfg(target_os = "linux")]

use axocoatl_exec::protocol::{
    read_frame, sha256, Control, ExecRequest, PrimaryExit, ProcessOutcome, ServerMessage,
    MAX_RESPONSE_BYTES, PROTOCOL_VERSION,
};
use std::io::{BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

struct Helper {
    process: Child,
    input: Option<ChildStdin>,
    messages: Receiver<Result<ServerMessage, String>>,
    reader: Option<JoinHandle<()>>,
    request: ExecRequest,
}

impl Helper {
    fn start(request: ExecRequest, fixture: Option<(&str, &Path)>) -> Self {
        Self::start_with_stdin(request, None, fixture)
    }

    fn start_with_stdin(
        request: ExecRequest,
        stdin: Option<&[u8]>,
        fixture: Option<(&str, &Path)>,
    ) -> Self {
        let helper = Self::launch(request, stdin, fixture);
        let ready = helper.next();
        ready.validate_for(&helper.request).unwrap();
        assert!(matches!(ready, ServerMessage::Ready { .. }));
        helper
    }

    fn launch(request: ExecRequest, stdin: Option<&[u8]>, fixture: Option<(&str, &Path)>) -> Self {
        let mut command = Command::new(env!("CARGO_BIN_EXE_axocoatl-exec-supervisor"));
        command
            .arg("--serve")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());
        if let Some((kind, directory)) = fixture {
            command
                .env("AXOCOATL_EXEC_TEST_KIND", kind)
                .env("AXOCOATL_EXEC_TEST_DIRECTORY", directory);
        }
        let mut process = command.spawn().unwrap();
        let input = process.stdin.take().unwrap();
        let output = process.stdout.take().unwrap();
        let (sender, messages) = mpsc::channel();
        let reader = thread::spawn(move || {
            let mut output = BufReader::new(output);
            loop {
                match read_frame(&mut output, MAX_RESPONSE_BYTES) {
                    Ok(Some(bytes)) => {
                        let message = serde_json::from_slice(&bytes)
                            .map_err(|error| format!("invalid helper message: {error}"));
                        if sender.send(message).is_err() {
                            break;
                        }
                    }
                    Ok(None) => {
                        let _ = sender.send(Err("helper stdout closed".into()));
                        break;
                    }
                    Err(error) => {
                        let _ = sender.send(Err(error));
                        break;
                    }
                }
            }
        });
        let mut helper = Self {
            process,
            input: Some(input),
            messages,
            reader: Some(reader),
            request,
        };
        let request = serde_json::to_vec(&helper.request).unwrap();
        helper.write(&request);
        if let Some(bytes) = stdin {
            let input = helper.input.as_mut().unwrap();
            input.write_all(bytes).unwrap();
            input.flush().unwrap();
        }
        helper
    }

    fn write(&mut self, bytes: &[u8]) {
        let input = self.input.as_mut().unwrap();
        input.write_all(bytes).unwrap();
        input.write_all(b"\n").unwrap();
        input.flush().unwrap();
    }
    fn control(&mut self, control: Control) {
        self.write(&serde_json::to_vec(&control).unwrap());
    }
    fn next(&self) -> ServerMessage {
        self.messages
            .recv_timeout(Duration::from_secs(15))
            .expect("helper response deadline")
            .expect("helper protocol response")
    }
    fn finished(&mut self) -> ServerMessage {
        let terminal = self.next();
        assert!(matches!(terminal, ServerMessage::Finished { .. }));
        terminal.validate_for(&self.request).unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            if let Some(exit) = self.process.try_wait().unwrap() {
                assert!(exit.success());
                break;
            }
            assert!(
                Instant::now() < deadline,
                "helper did not exit after terminal acknowledgment"
            );
            thread::sleep(Duration::from_millis(5));
        }
        terminal
    }
}

impl Drop for Helper {
    fn drop(&mut self) {
        // A failed assertion must not leave the test's protocol reader blocked.
        self.input.take();
        let _ = self.process.kill();
        let _ = self.process.wait();
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
    }
}

fn request(argv: Vec<String>, timeout_ms: u64) -> ExecRequest {
    ExecRequest {
        protocol: PROTOCOL_VERSION,
        stdin: None,
        invocation_id: "owned-process-test".into(),
        argv,
        timeout_ms,
        stdout_bytes: 4096,
        stderr_bytes: 4096,
        write_restriction: None,
    }
}

fn shell(script: &str, timeout_ms: u64) -> ExecRequest {
    request(
        vec!["/bin/sh".into(), "-c".into(), script.into()],
        timeout_ms,
    )
}

fn fixture_request(timeout_ms: u64) -> ExecRequest {
    request(
        vec![
            std::env::current_exe()
                .unwrap()
                .to_string_lossy()
                .into_owned(),
            "--exact".into(),
            "fixture_process".into(),
            "--nocapture".into(),
        ],
        timeout_ms,
    )
}

fn wait_for(path: &Path) {
    let deadline = Instant::now() + Duration::from_secs(3);
    while !path.exists() {
        assert!(
            Instant::now() < deadline,
            "fixture did not publish {}",
            path.display()
        );
        thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn ready_and_predispatch_cancel_never_launch_the_command() {
    let directory = tempfile::tempdir().unwrap();
    let marker = directory.path().join("must-not-exist");
    let mut input = request(
        vec![
            "/bin/sh".into(),
            "-c".into(),
            "touch \"$1\"".into(),
            "fixture".into(),
            marker.to_string_lossy().into_owned(),
        ],
        3000,
    );
    for cancel in [true, false] {
        input.invocation_id = format!("not-dispatched-{cancel}");
        let mut helper = Helper::start(input.clone(), None);
        assert!(!marker.exists());
        if cancel {
            helper.control(Control::Cancel);
        } else {
            helper.input.take();
        }
        assert!(matches!(
            helper.finished(),
            ServerMessage::Finished {
                outcome: ProcessOutcome::Cancelled,
                launched: false,
                primary_exit: None,
                quiescent: true,
                ..
            }
        ));
        assert!(!marker.exists());
    }
}

#[test]
fn a_write_restriction_blocks_the_protected_tree_or_refuses_to_launch() {
    use axocoatl_exec::protocol::WriteRestriction;
    let protected = tempfile::tempdir().unwrap();
    let scratch = tempfile::tempdir().unwrap();
    std::fs::write(protected.path().join("owned.js"), "before").unwrap();
    let script = "echo changed > \"$1/owned.js\"; mkdir \"$1/new\"; rm -f \"$1/owned.js\"; \
                  echo ok > \"$2/scratch.txt\"; ln -s /etc/passwd \"$1/link\"; true";
    let mut input = request(
        vec![
            "/bin/sh".into(),
            "-c".into(),
            script.into(),
            "restricted".into(),
            protected.path().to_string_lossy().into_owned(),
            scratch.path().to_string_lossy().into_owned(),
        ],
        5000,
    );
    input.write_restriction = Some(WriteRestriction {
        writable: vec![
            scratch.path().to_string_lossy().into_owned(),
            // Never reopens the protected tree, even when it contains it.
            "/".into(),
            "$HOME".into(),
        ],
        protected: vec![protected.path().to_string_lossy().into_owned()],
    });
    let mut helper = Helper::start(input, None);
    helper.control(Control::Dispatch);
    let ServerMessage::Finished {
        outcome, launched, ..
    } = helper.finished()
    else {
        panic!("terminal")
    };
    // SAFETY: querying the Landlock ABI takes no attribute pointer.
    let landlock = unsafe { libc::syscall(444, std::ptr::null::<u8>(), 0usize, 1u32) } >= 1;
    match outcome {
        ProcessOutcome::LaunchFailed { message } => {
            assert!(
                !landlock,
                "Landlock is available, so the restriction must apply: {message}"
            );
            // Without Landlock the command must not run at all.
            assert!(!launched);
            assert!(
                message.contains("write restriction unavailable"),
                "{message}"
            );
            assert!(!scratch.path().join("scratch.txt").exists());
        }
        other => {
            assert!(launched, "{other:?}");
            assert_eq!(
                std::fs::read_to_string(protected.path().join("owned.js")).unwrap(),
                "before",
                "the protected file was neither changed nor removed"
            );
            assert!(!protected.path().join("new").exists());
            assert!(!protected.path().join("link").exists());
            assert_eq!(
                std::fs::read_to_string(scratch.path().join("scratch.txt")).unwrap(),
                "ok\n",
                "writes beneath an allowed root still work"
            );
        }
    }
}

#[test]
fn binary_output_has_exact_full_observation_and_bounded_prefixes() {
    let stdout = b"\0\xffabcdef";
    let stderr = b"\x80err";
    let mut input = shell(
        "printf '\\000\\377abcdef'; printf '\\200err' >&2; exit 7",
        3000,
    );
    input.stdout_bytes = 3;
    input.stderr_bytes = 0;
    let mut helper = Helper::start(input, None);
    helper.control(Control::Dispatch);
    let ServerMessage::Finished {
        outcome,
        primary_exit,
        launched,
        stdout: actual_out,
        stderr: actual_err,
        quiescent,
        ..
    } = helper.finished()
    else {
        panic!("terminal")
    };
    assert_eq!(outcome, ProcessOutcome::Exited { code: 7 });
    assert_eq!(primary_exit, Some(PrimaryExit::Exited { code: 7 }));
    assert!(launched && quiescent && actual_out.complete && actual_err.complete);
    assert_eq!(actual_out.retained_bytes(3).unwrap(), stdout[..3]);
    assert_eq!(actual_out.observed_bytes, stdout.len() as u64);
    assert_eq!(actual_out.observed_sha256, sha256(stdout));
    assert!(actual_err.retained_bytes(0).unwrap().is_empty());
    assert_eq!(actual_err.observed_bytes, stderr.len() as u64);
    assert_eq!(actual_err.observed_sha256, sha256(stderr));
}

#[test]
fn child_cannot_open_parent_control_descriptor_or_forge_a_finished_frame() {
    let directory = tempfile::tempdir().unwrap();
    let mut helper = Helper::start(
        fixture_request(3000),
        Some(("probe-control", directory.path())),
    );
    helper.control(Control::Dispatch);
    let ServerMessage::Finished {
        outcome,
        stdout,
        quiescent,
        ..
    } = helper.finished()
    else {
        panic!("terminal")
    };
    assert_eq!(outcome, ProcessOutcome::Exited { code: 0 });
    assert!(quiescent);
    assert_eq!(
        std::fs::read(directory.path().join("control-protected")).unwrap(),
        b"permission-denied"
    );
    let observed = stdout.retained_bytes(4096).unwrap();
    assert!(
        String::from_utf8_lossy(&observed).contains("forged-child-output"),
        "ordinary child bytes should remain captured data"
    );
}

#[test]
fn double_fork_and_setsid_are_waited_even_after_primary_exit_and_pipe_eof() {
    let directory = tempfile::tempdir().unwrap();
    let mut helper = Helper::start(
        fixture_request(3000),
        Some(("double-root", directory.path())),
    );
    let began = Instant::now();
    helper.control(Control::Dispatch);
    assert!(matches!(
        helper.finished(),
        ServerMessage::Finished {
            outcome: ProcessOutcome::Exited { code: 7 },
            primary_exit: Some(PrimaryExit::Exited { code: 7 }),
            quiescent: true,
            ..
        }
    ));
    assert!(
        directory.path().join("late-write").exists(),
        "acknowledged before the detached descendant completed"
    );
    assert!(began.elapsed() >= Duration::from_millis(150));
}

struct Sentinel(Child);
impl Drop for Sentinel {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn timeout_reaps_orphaned_children_without_killing_an_unrelated_process() {
    let directory = tempfile::tempdir().unwrap();
    let mut sentinel = Sentinel(Command::new("/bin/sleep").arg("10").spawn().unwrap());
    let mut helper = Helper::start(
        fixture_request(1000),
        Some(("orphan-long-root", directory.path())),
    );
    helper.control(Control::Dispatch);
    wait_for(&directory.path().join("leaf-pid"));
    let leaf = read_pid(&directory.path().join("leaf-pid"));
    assert!(matches!(
        helper.finished(),
        ServerMessage::Finished {
            outcome: ProcessOutcome::TimedOut,
            primary_exit: Some(PrimaryExit::Exited { code: 9 }),
            quiescent: true,
            ..
        }
    ));
    assert!(
        !Path::new(&format!("/proc/{leaf}")).exists(),
        "owned child survived acknowledged quiescence"
    );
    assert!(
        sentinel.0.try_wait().unwrap().is_none(),
        "unrelated background process was killed"
    );
    assert!(!directory.path().join("late-write").exists());
}

#[test]
fn explicit_cancel_and_control_eof_reap_owned_descendants() {
    for explicit in [true, false] {
        let directory = tempfile::tempdir().unwrap();
        let mut helper =
            Helper::start(fixture_request(5000), Some(("long-root", directory.path())));
        helper.control(Control::Dispatch);
        wait_for(&directory.path().join("leaf-pid"));
        let root = read_pid(&directory.path().join("root-pid"));
        let leaf = read_pid(&directory.path().join("leaf-pid"));
        if explicit {
            helper.control(Control::Cancel);
        } else {
            helper.input.take();
        }
        assert!(matches!(
            helper.finished(),
            ServerMessage::Finished {
                outcome: ProcessOutcome::Cancelled,
                launched: true,
                quiescent: true,
                ..
            }
        ));
        assert!(!Path::new(&format!("/proc/{root}")).exists());
        assert!(!Path::new(&format!("/proc/{leaf}")).exists());
        assert!(!directory.path().join("late-write").exists());
    }
}

#[test]
fn duplicate_dispatch_and_malformed_control_cancel_instead_of_replaying() {
    for frame in [
        b"{\"kind\":\"dispatch\"}".as_slice(),
        b"not-json".as_slice(),
    ] {
        let directory = tempfile::tempdir().unwrap();
        let mut helper =
            Helper::start(fixture_request(5000), Some(("long-root", directory.path())));
        helper.control(Control::Dispatch);
        wait_for(&directory.path().join("leaf-pid"));
        helper.write(frame);
        assert!(matches!(
            helper.finished(),
            ServerMessage::Finished {
                outcome: ProcessOutcome::Failed { .. },
                launched: true,
                quiescent: true,
                ..
            }
        ));
        assert!(!Path::new(&format!(
            "/proc/{}",
            read_pid(&directory.path().join("leaf-pid"))
        ))
        .exists());
    }
}

#[test]
fn launch_failure_and_signal_exit_have_distinct_truthful_frames() {
    let mut missing = Helper::start(
        request(vec!["/does/not/exist/axocoatl-test".into()], 3000),
        None,
    );
    missing.control(Control::Dispatch);
    assert!(matches!(
        missing.finished(),
        ServerMessage::Finished {
            outcome: ProcessOutcome::LaunchFailed { .. },
            primary_exit: None,
            launched: false,
            quiescent: true,
            ..
        }
    ));
    let mut signal = Helper::start(shell("kill -TERM $$", 3000), None);
    signal.control(Control::Dispatch);
    assert!(matches!(
        signal.finished(),
        ServerMessage::Finished {
            outcome: ProcessOutcome::Signalled {
                signal: libc::SIGTERM
            },
            primary_exit: Some(PrimaryExit::Signalled {
                signal: libc::SIGTERM
            }),
            launched: true,
            quiescent: true,
            ..
        }
    ));
}

#[test]
fn helper_death_after_dispatch_produces_no_quiescence_acknowledgment() {
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    struct Descendant(OwnedFd);
    impl Drop for Descendant {
        fn drop(&mut self) {
            // Stable handles acquired while our test descendants are alive;
            // even a failed assertion cannot signal a subsequently reused PID.
            unsafe {
                libc::syscall(
                    libc::SYS_pidfd_send_signal,
                    self.0.as_raw_fd(),
                    libc::SIGKILL,
                    std::ptr::null::<libc::siginfo_t>(),
                    0,
                );
            }
        }
    }
    let directory = tempfile::tempdir().unwrap();
    let mut helper = Helper::start(fixture_request(5000), Some(("long-root", directory.path())));
    helper.control(Control::Dispatch);
    wait_for(&directory.path().join("leaf-pid"));
    let mut descendants = Vec::new();
    for name in ["root-pid", "leaf-pid"] {
        let pid = read_pid(&directory.path().join(name));
        let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
        assert!(fd >= 0, "test cleanup could not retain descendant pidfd");
        descendants.push(Descendant(unsafe { OwnedFd::from_raw_fd(fd as i32) }));
    }
    helper.process.kill().unwrap();
    assert!(!helper.process.wait().unwrap().success());
    assert!(
        helper
            .messages
            .recv_timeout(Duration::from_secs(2))
            .unwrap()
            .is_err(),
        "helper exit must not invent a Finished/quiescent frame"
    );
    drop(descendants);
}

#[test]
fn output_flood_does_not_starve_deadline_or_grow_the_retained_prefix() {
    let mut input = shell("while :; do printf '0123456789abcdef'; done", 500);
    input.stdout_bytes = 8;
    let mut helper = Helper::start(input, None);
    helper.control(Control::Dispatch);
    let ServerMessage::Finished {
        outcome,
        stdout,
        quiescent,
        ..
    } = helper.finished()
    else {
        panic!("terminal")
    };
    assert_eq!(outcome, ProcessOutcome::TimedOut);
    assert!(quiescent);
    assert_eq!(stdout.retained_bytes(8).unwrap(), b"01234567");
    assert!(stdout.observed_bytes > 8);
}

#[test]
fn undispatched_request_expires_without_running() {
    let mut helper = Helper::start(shell("exit 0", 50), None);
    assert!(matches!(
        helper.finished(),
        ServerMessage::Finished {
            outcome: ProcessOutcome::TimedOut,
            launched: false,
            quiescent: true,
            ..
        }
    ));
}

fn read_pid(path: &Path) -> i32 {
    std::fs::read_to_string(path).unwrap().parse().unwrap()
}

// This test is also a subprocess fixture. It is a no-op during the normal test
// run; only the helper's own child receives these per-Command environment vars.
#[test]
fn fixture_process() {
    let Ok(kind) = std::env::var("AXOCOATL_EXEC_TEST_KIND") else {
        return;
    };
    let directory = PathBuf::from(std::env::var_os("AXOCOATL_EXEC_TEST_DIRECTORY").unwrap());
    let spawn = |kind: &str| {
        Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "fixture_process", "--nocapture"])
            .env("AXOCOATL_EXEC_TEST_KIND", kind)
            .env("AXOCOATL_EXEC_TEST_DIRECTORY", &directory)
            .spawn()
            .unwrap()
    };
    match kind.as_str() {
        "stdin-output-first" => {
            use std::io::Read;
            std::io::stdout()
                .write_all(&vec![b'o'; 1024 * 1024])
                .unwrap();
            std::io::stderr()
                .write_all(&vec![b'e'; 1024 * 1024])
                .unwrap();
            let mut bytes = Vec::new();
            std::io::stdin()
                .take((axocoatl_exec::protocol::MAX_STDIN_BYTES + 1) as u64)
                .read_to_end(&mut bytes)
                .unwrap();
            assert!(bytes.len() <= axocoatl_exec::protocol::MAX_STDIN_BYTES);
            std::fs::write(directory.join("stdin-bytes"), bytes).unwrap();
            std::process::exit(0);
        }
        "stdin-close-after-effect" => {
            std::fs::write(directory.join("effect"), b"observed repository effect").unwrap();
            // Process exit closes stdin without reading it. Its exit status
            // is determined before the helper observes EPIPE on that pipe.
            std::process::exit(0);
        }
        "probe-control" => {
            let parent = unsafe { libc::getppid() };
            let mut attempted = std::fs::OpenOptions::new()
                .write(true)
                .open(format!("/proc/{parent}/fd/1"));
            if let Ok(file) = attempted.as_mut() {
                file.write_all(b"{\"kind\":\"finished\",\"forged-child-output\":true}\n")
                    .unwrap();
                std::process::exit(91);
            }
            let denied = attempted.err().unwrap();
            assert_eq!(
                denied.kind(),
                std::io::ErrorKind::PermissionDenied,
                "the parent must remain alive with a protected control descriptor"
            );
            std::fs::write(directory.join("control-protected"), b"permission-denied").unwrap();
            std::io::stdout()
                .write_all(b"{\"kind\":\"finished\",\"forged-child-output\":true}\n")
                .unwrap();
            std::process::exit(0);
        }
        "double-root" => {
            let _ = spawn("double-intermediate");
            std::process::exit(7);
        }
        "double-intermediate" => {
            // A new session defeats process-group-only cleanup; it does not
            // escape the helper's descendant/subreaper ownership.
            assert!(unsafe { libc::setsid() } >= 0);
            let _ = spawn("short-leaf");
            std::process::exit(0);
        }
        "orphan-long-root" => {
            let _ = spawn("long-leaf");
            std::process::exit(9);
        }
        "long-root" => {
            std::fs::write(directory.join("root-pid"), std::process::id().to_string()).unwrap();
            let _ = spawn("long-leaf");
            thread::sleep(Duration::from_secs(3));
            std::process::exit(0);
        }
        "short-leaf" | "long-leaf" => {
            if kind == "long-leaf" {
                assert!(unsafe { libc::setsid() } >= 0);
            }
            std::fs::write(directory.join("leaf-pid"), std::process::id().to_string()).unwrap();
            // Surviving descendants must be detected even after all inherited
            // output descriptors have closed and the parent has exited.
            unsafe {
                libc::close(0);
                libc::close(1);
                libc::close(2);
            }
            thread::sleep(if kind == "short-leaf" {
                Duration::from_millis(200)
            } else {
                Duration::from_secs(3)
            });
            std::fs::write(directory.join("late-write"), b"descendant completed").unwrap();
            std::process::exit(0);
        }
        _ => panic!("unknown process fixture"),
    }
}

fn with_stdin(mut request: ExecRequest, bytes: &[u8]) -> ExecRequest {
    request.stdin = Some(axocoatl_exec::protocol::StdinDescriptor::for_bytes(bytes).unwrap());
    request
}

#[test]
fn exact_full_file_tool_stdin_is_delivered_only_after_dispatch() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("exact-file");
    let pattern = b"line\n'\"$()\\\0\xff";
    let bytes = pattern
        .iter()
        .copied()
        .cycle()
        .take(axocoatl_exec::protocol::MAX_STDIN_BYTES)
        .collect::<Vec<_>>();
    // This is the existing WriteFile/EditFile argv; input bytes remain stdin.
    let request = with_stdin(
        request(
            vec![
                "/bin/sh".into(),
                "-c".into(),
                "cat > \"$1\"".into(),
                "sh".into(),
                path.to_string_lossy().into_owned(),
            ],
            10_000,
        ),
        &bytes,
    );
    let mut helper = Helper::start_with_stdin(request, Some(&bytes), None);
    assert!(!path.exists(), "preparation must not launch a file write");
    helper.control(Control::Dispatch);
    assert!(matches!(
        helper.finished(),
        ServerMessage::Finished {
            outcome: ProcessOutcome::Exited { code: 0 },
            launched: true,
            quiescent: true,
            ..
        }
    ));
    assert_eq!(std::fs::read(path).unwrap(), bytes);
}

#[test]
fn stdin_backpressure_does_not_deadlock_output_drain() {
    let directory = tempfile::tempdir().unwrap();
    let bytes = vec![b'z'; axocoatl_exec::protocol::MAX_STDIN_BYTES];
    let request = with_stdin(fixture_request(10_000), &bytes);
    let mut helper = Helper::start_with_stdin(
        request,
        Some(&bytes),
        Some(("stdin-output-first", directory.path())),
    );
    helper.control(Control::Dispatch);
    let ServerMessage::Finished {
        outcome,
        stdout,
        stderr,
        launched,
        quiescent,
        ..
    } = helper.finished()
    else {
        panic!("terminal");
    };
    assert_eq!(outcome, ProcessOutcome::Exited { code: 0 });
    assert!(launched && quiescent);
    assert!(stdout.observed_bytes >= 1024 * 1024 && stderr.observed_bytes >= 1024 * 1024);
    assert_eq!(
        std::fs::read(directory.path().join("stdin-bytes")).unwrap(),
        bytes
    );
}

#[test]
fn child_stdin_delivery_failure_keeps_launched_effect_and_primary_exit() {
    let directory = tempfile::tempdir().unwrap();
    let bytes = vec![b'z'; axocoatl_exec::protocol::MAX_STDIN_BYTES];
    let request = with_stdin(fixture_request(5_000), &bytes);
    let mut helper = Helper::start_with_stdin(
        request,
        Some(&bytes),
        Some(("stdin-close-after-effect", directory.path())),
    );
    helper.control(Control::Dispatch);
    let ServerMessage::Finished {
        outcome,
        primary_exit,
        launched,
        quiescent,
        ..
    } = helper.finished()
    else {
        panic!("terminal");
    };
    assert!(matches!(outcome, ProcessOutcome::Failed { .. }));
    assert_eq!(primary_exit, Some(PrimaryExit::Exited { code: 0 }));
    assert!(launched && quiescent);
    assert_eq!(
        std::fs::read(directory.path().join("effect")).unwrap(),
        b"observed repository effect"
    );
}

#[test]
fn timeout_cancels_a_child_that_never_reads_stdin() {
    let bytes = vec![b'z'; axocoatl_exec::protocol::MAX_STDIN_BYTES];
    let mut helper = Helper::start_with_stdin(
        with_stdin(shell("sleep 100", 100), &bytes),
        Some(&bytes),
        None,
    );
    helper.control(Control::Dispatch);
    assert!(matches!(
        helper.finished(),
        ServerMessage::Finished {
            outcome: ProcessOutcome::TimedOut,
            launched: true,
            quiescent: true,
            ..
        }
    ));
}

#[test]
fn cancelled_prepared_stdin_never_launches_the_command() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("must-not-exist");
    let bytes = b"payload with\n{\"kind\":\"dispatch\"}\ninside";
    let request = with_stdin(
        request(
            vec![
                "/bin/sh".into(),
                "-c".into(),
                "cat > \"$1\"".into(),
                "sh".into(),
                path.to_string_lossy().into_owned(),
            ],
            5_000,
        ),
        bytes,
    );
    let mut helper = Helper::start_with_stdin(request, Some(bytes), None);
    helper.control(Control::Cancel);
    assert!(matches!(
        helper.finished(),
        ServerMessage::Finished {
            outcome: ProcessOutcome::Cancelled,
            launched: false,
            quiescent: true,
            ..
        }
    ));
    assert!(!path.exists());
}

#[test]
fn corrupted_or_truncated_stdin_is_refused_before_ready() {
    for supplied in [b"different".as_slice(), b"shor".as_slice()] {
        let request = with_stdin(shell("exit 91", 5_000), b"expected!");
        let mut helper = Helper::launch(request, Some(supplied), None);
        helper.input.take();
        assert!(helper
            .messages
            .recv_timeout(Duration::from_secs(2))
            .unwrap()
            .is_err());
        assert!(!helper.process.wait().unwrap().success());
    }
}
