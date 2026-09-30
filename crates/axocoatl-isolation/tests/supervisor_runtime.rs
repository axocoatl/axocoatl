//! Opt-in checks of the actual bundled program through a Ready SessionSandbox.
//! Run with an explicitly prepared AXO_SUPERVISOR_TEST_IMAGE and its numeric
//! AXO_SUPERVISOR_TEST_EXPECTED_UID. Run the same suite for the ordinary image
//! and the non-root fixture; the test never replaces the image's USER.
//! CONTAINER_CONNECTION may select an already-configured Podman connection.
#![cfg(unix)]

use std::future::Future;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axocoatl_core::SecureDir;
use axocoatl_exec::protocol::{
    CapturedOutput, ExecRequest, PrimaryExit, ProcessOutcome, ServerMessage, PROTOCOL_VERSION,
};
use axocoatl_isolation::supervisor_transport::{PreparedSupervisedCommand, SupervisedExecution};
use axocoatl_isolation::{SandboxNetwork, SandboxPolicy, SessionSandbox};
use sha2::{Digest, Sha256};

const COMMAND_TIMEOUT: Duration = Duration::from_secs(10);
const OBSERVATION_TIMEOUT: Duration = Duration::from_secs(15);

struct RuntimeFixture {
    _directory: tempfile::TempDir,
    workspace: PathBuf,
    installation: SecureDir,
    session_id: String,
    image: String,
    expected_uid: u32,
    policy: SandboxPolicy,
    runtimes: Mutex<Vec<Arc<SessionSandbox>>>,
}

impl RuntimeFixture {
    fn new() -> Self {
        let image = std::env::var("AXO_SUPERVISOR_TEST_IMAGE")
            .expect("set AXO_SUPERVISOR_TEST_IMAGE to an explicitly prepared local image");
        assert!(!image.is_empty(), "test image must be explicit");
        let expected_uid = std::env::var("AXO_SUPERVISOR_TEST_EXPECTED_UID")
            .expect("set AXO_SUPERVISOR_TEST_EXPECTED_UID to the image's configured numeric USER")
            .parse::<u32>()
            .expect("expected image USER must be a numeric uid");
        let directory = tempfile::Builder::new()
            .prefix("axo-supervisor-runtime-")
            .tempdir()
            .unwrap();
        let root = SecureDir::open(directory.path().canonicalize().unwrap()).unwrap();
        let workspace = root.child("workspace").unwrap().path().to_owned();
        // Only this disposable repository is writable by the fixture's custom
        // container uid. The separately retained installation stays private.
        std::fs::set_permissions(&workspace, std::fs::Permissions::from_mode(0o777)).unwrap();
        let installation = root.child("supervisor-programs").unwrap();
        let identity = uuid::Uuid::new_v4().simple().to_string();
        let policy = SandboxPolicy {
            allow_untrusted_image: true,
            network: SandboxNetwork::None,
            runtime_authority: Some(format!("{:x}", Sha256::digest(identity.as_bytes()))),
            supervisor_installation: Some(installation.clone()),
            ..SandboxPolicy::default()
        };
        Self {
            _directory: directory,
            workspace,
            installation,
            session_id: format!("supervisor-test-{identity}"),
            image,
            expected_uid,
            policy,
            runtimes: Mutex::new(Vec::new()),
        }
    }

    async fn start(&self) -> Arc<SessionSandbox> {
        let sandbox = Arc::new(
            SessionSandbox::start(
                &self.session_id,
                &self.workspace,
                Some(&self.image),
                &[],
                &[],
                &self.policy,
            )
            .await
            .expect("automatic embedded supervisor installation and Ready preparation"),
        );
        // Retain exact handles before any assertion can panic. Cleanup also
        // covers earlier incarnations when a case explicitly restarts itself.
        self.runtimes.lock().unwrap().push(sandbox.clone());
        let uid = sandbox.exec(&["id", "-u"], COMMAND_TIMEOUT).await.unwrap();
        assert_eq!(uid.exit_code, 0, "{}", uid.stderr);
        assert_eq!(
            uid.stdout.trim().parse::<u32>().unwrap(),
            self.expected_uid,
            "sandbox creation must preserve the selected image's USER"
        );
        let expected_image =
            SessionSandbox::resolve_effective_image(Some(&self.image), true).unwrap();
        assert_eq!(sandbox.effective_image(), Some(expected_image.as_str()));
        sandbox
    }

    fn installed_program(&self, prepared: &PreparedSupervisedCommand) -> PathBuf {
        let hash = prepared.program_sha256();
        let path = self
            .installation
            .path()
            .join(format!("supervisor-sha256-{hash}"));
        let metadata = std::fs::metadata(&path).unwrap();
        assert!(metadata.is_file() && metadata.len() <= 64 * 1024 * 1024);
        assert_eq!(metadata.mode() & 0o7777, 0o555);
        assert_eq!(metadata.nlink(), 1);
        assert_eq!(
            format!("{:x}", Sha256::digest(std::fs::read(&path).unwrap())),
            hash
        );
        path
    }

    async fn cleanup(&self) -> Result<(), String> {
        let runtimes = self.runtimes.lock().unwrap().clone();
        let mut errors = Vec::new();
        for sandbox in runtimes {
            if let Err(error) = sandbox.stop_checked().await {
                errors.push(error.to_string());
            }
        }
        // This unguessable name belongs only to this fixture. Cover a failed
        // startup as well; never sweep containers owned by another test/user.
        if let Err(error) = SessionSandbox::remove_named_with_dependencies(&self.session_id).await {
            errors.push(error.to_string());
        }
        match SessionSandbox::named_running(&self.session_id).await {
            Ok(false) => (),
            Ok(true) => errors.push("test container still running after checked removal".into()),
            Err(error) => errors.push(error.to_string()),
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors.join("; "))
        }
    }
}

async fn with_fixture<F, Fut>(case: F)
where
    F: FnOnce(Arc<RuntimeFixture>) -> Fut + Send + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    let fixture = Arc::new(RuntimeFixture::new());
    let body_fixture = fixture.clone();
    // Assertions in a case must not bypass checked container cleanup.
    let outcome = tokio::spawn(async move { case(body_fixture).await }).await;
    let cleanup = fixture.cleanup().await;
    assert!(
        cleanup.is_ok(),
        "owned test cleanup failed: {cleanup:?}; case: {outcome:?}"
    );
    if let Err(error) = outcome {
        if error.is_panic() {
            std::panic::resume_unwind(error.into_panic());
        }
        panic!("test case was cancelled: {error}");
    }
}

fn request(invocation_id: &str, script: &str, timeout_ms: u64) -> ExecRequest {
    ExecRequest {
        protocol: PROTOCOL_VERSION,
        stdin: None,
        invocation_id: invocation_id.into(),
        argv: vec!["sh".into(), "-c".into(), script.into()],
        timeout_ms,
        stdout_bytes: 4096,
        stderr_bytes: 4096,
        write_restriction: None,
    }
}

/// Expected values are read from the actual prepared transport. This is not a
/// ProcessSettlement and cannot manufacture authority to release a writer.
struct ExpectedReceipt {
    transport: String,
    runtime: String,
    program: String,
    invocation: String,
    request: String,
}

impl ExpectedReceipt {
    fn observe(prepared: &PreparedSupervisedCommand) -> Self {
        Self {
            transport: prepared.transport_identity().into(),
            runtime: prepared.runtime_identity().into(),
            program: prepared.program_sha256().into(),
            invocation: prepared.request().invocation_id.clone(),
            request: prepared.request().digest().unwrap(),
        }
    }

    fn check(&self, execution: &SupervisedExecution) {
        let receipt = execution
            .settlement()
            .expect("actual helper must prove process settlement");
        assert_eq!(receipt.transport_identity(), self.transport);
        assert_eq!(receipt.runtime_identity(), self.runtime);
        assert_eq!(receipt.program_sha256(), self.program);
        assert_eq!(receipt.invocation_id(), self.invocation);
        assert_eq!(receipt.request_sha256(), self.request);
        assert_eq!(execution.runtime_identity(), self.runtime);
        assert_eq!(execution.program_sha256(), self.program);
        assert_eq!(execution.request().digest().unwrap(), self.request);
        execution
            .result()
            .validate_for(execution.request())
            .unwrap();
    }
}

fn finished<'a>(
    execution: &'a SupervisedExecution,
    expected: &ProcessOutcome,
    expected_launched: bool,
) -> (&'a CapturedOutput, &'a CapturedOutput) {
    let ServerMessage::Finished {
        outcome,
        launched,
        stdout,
        stderr,
        quiescent,
        ..
    } = execution.result()
    else {
        panic!("expected the actual helper's terminal message");
    };
    assert_eq!(outcome, expected);
    assert_eq!(*launched, expected_launched);
    assert!(*quiescent && stdout.complete && stderr.complete);
    (stdout, stderr)
}

async fn wait_for_file(path: &Path, expected: &[u8]) {
    tokio::time::timeout(OBSERVATION_TIMEOUT, async {
        loop {
            if std::fs::read(path).is_ok_and(|bytes| bytes == expected) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("repository observation did not arrive: {}", path.display()));
}

async fn sentinel(sandbox: &SessionSandbox, workspace: &Path) -> String {
    let id = sandbox.spawn_background(
        "printf 'ready\\n' > sentinel-ready; while :; do printf x >> sentinel-ticks; sleep 0.1; done",
    );
    wait_for_file(&workspace.join("sentinel-ready"), b"ready\n").await;
    id
}

async fn assert_sentinel_alive(sandbox: &SessionSandbox, workspace: &Path, id: &str) {
    let ticks = workspace.join("sentinel-ticks");
    let before = std::fs::metadata(&ticks)
        .map(|metadata| metadata.len())
        .unwrap_or(0);
    tokio::time::timeout(OBSERVATION_TIMEOUT, async {
        loop {
            if std::fs::metadata(&ticks).is_ok_and(|metadata| metadata.len() > before) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("unrelated background process must keep making progress");
    assert!(sandbox
        .list_tasks()
        .iter()
        .any(|task| task.id == id && task.status == "running"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires an explicit prepared Podman image, expected USER uid, and configured connection"]
async fn ready_does_not_dispatch_and_prelaunch_cancel_returns_exact_settlement() {
    with_fixture(|fixture| async move {
        let sandbox = fixture.start().await;
        let prepared = sandbox
            .prepare_supervised_command(request(
                "ready-no-dispatch",
                "printf 'ran\\n' > forbidden-before-dispatch",
                30_000,
            ))
            .await
            .unwrap();
        fixture.installed_program(&prepared);
        let expected = ExpectedReceipt::observe(&prepared);
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(!fixture.workspace.join("forbidden-before-dispatch").exists());
        prepared.cancellation().cancel();
        let execution = prepared.dispatch().unwrap().finish().await.unwrap();
        expected.check(&execution);
        let (stdout, stderr) = finished(&execution, &ProcessOutcome::Cancelled, false);
        assert_eq!(stdout.observed_bytes, 0);
        assert_eq!(stderr.observed_bytes, 0);
        assert!(!fixture.workspace.join("forbidden-before-dispatch").exists());
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires an explicit prepared Podman image, expected USER uid, and configured connection"]
async fn a_read_only_command_cannot_write_the_repository_but_keeps_scratch() {
    with_fixture(|fixture| async move {
        let sandbox = fixture.start().await;
        std::fs::write(fixture.workspace.join("owned.js"), "before").unwrap();
        let root = sandbox.root().to_string_lossy().into_owned();
        let mut input = request(
            "read-only-route",
            "printf changed > owned.js; mkdir created; ln -s /etc/passwd link; \
             printf ok > /tmp/scratch-ok; cat /tmp/scratch-ok; cat owned.js",
            30_000,
        );
        input.write_restriction = Some(axocoatl_exec::protocol::WriteRestriction {
            writable: vec!["/tmp".into(), "/dev".into(), "$HOME".into()],
            protected: vec![root],
        });
        let prepared = sandbox.prepare_supervised_command(input).await.unwrap();
        let execution = prepared.dispatch().unwrap().finish().await.unwrap();
        let (stdout, _) = finished(&execution, &ProcessOutcome::Exited { code: 0 }, true);
        assert_eq!(stdout.retained_bytes(4096).unwrap(), b"okbefore");
        assert_eq!(
            std::fs::read_to_string(fixture.workspace.join("owned.js")).unwrap(),
            "before"
        );
        assert!(!fixture.workspace.join("created").exists());
        assert!(!fixture.workspace.join("link").exists());
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires an explicit prepared Podman image, expected USER uid, and configured connection"]
async fn binary_output_and_closed_pipes_wait_for_the_actual_descendant() {
    with_fixture(|fixture| async move {
        let sandbox = fixture.start().await;
        let script = r#"
sh -c '(exec >/dev/null 2>&1; printf "waiting\n" > descendant-waiting; while ! test -f release-descendant; do sleep 0.05; done; printf "done\n" > descendant-done) &'
printf '\000\377out\n'
printf '\376err\000' >&2
exit 23
"#;
        let mut requested = request("binary-and-descendants", script, 30_000);
        requested.stdout_bytes = 4;
        requested.stderr_bytes = 16;
        let prepared = sandbox.prepare_supervised_command(requested).await.unwrap();
        let expected = ExpectedReceipt::observe(&prepared);
        let mut completion = tokio::spawn(prepared.dispatch().unwrap().finish());
        wait_for_file(&fixture.workspace.join("descendant-waiting"), b"waiting\n").await;
        assert!(tokio::time::timeout(Duration::from_millis(200), &mut completion).await.is_err(),
            "closed output pipes and the primary exit cannot prove descendants stopped");
        std::fs::write(fixture.workspace.join("release-descendant"), b"release").unwrap();
        let execution = completion.await.unwrap().unwrap();
        expected.check(&execution);
        let (stdout, stderr) = finished(&execution, &ProcessOutcome::Exited { code: 23 }, true);
        assert_eq!(stdout.retained_bytes(4).unwrap(), b"\0\xffou");
        assert_eq!(stdout.observed_bytes, 6);
        assert_eq!(stdout.observed_sha256, format!("{:x}", Sha256::digest(b"\0\xffout\n")));
        assert_eq!(stderr.retained_bytes(16).unwrap(), b"\xfeerr\0");
        assert_eq!(stderr.observed_bytes, 5);
        assert!(matches!(execution.result(), ServerMessage::Finished {
            primary_exit: Some(PrimaryExit::Exited { code: 23 }), ..
        }));
        wait_for_file(&fixture.workspace.join("descendant-done"), b"done\n").await;
    }).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires an explicit prepared Podman image, expected USER uid, and configured connection"]
async fn cancellation_settles_only_its_command_and_preserves_background_work() {
    with_fixture(|fixture| async move {
        let sandbox = fixture.start().await;
        let background = sentinel(&sandbox, &fixture.workspace).await;
        let prepared = sandbox
            .prepare_supervised_command(request(
                "cancel-one-command",
                "trap '' TERM; printf 'started\\n' > cancel-started; while :; do sleep 1; done",
                30_000,
            ))
            .await
            .unwrap();
        let expected = ExpectedReceipt::observe(&prepared);
        let cancellation = prepared.cancellation();
        let running = prepared.dispatch().unwrap();
        wait_for_file(&fixture.workspace.join("cancel-started"), b"started\n").await;
        cancellation.cancel();
        let execution = running.finish().await.unwrap();
        expected.check(&execution);
        finished(&execution, &ProcessOutcome::Cancelled, true);
        assert_sentinel_alive(&sandbox, &fixture.workspace, &background).await;
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires an explicit prepared Podman image, expected USER uid, and configured connection"]
async fn timeout_preserves_the_primary_exit_and_unrelated_background_work() {
    with_fixture(|fixture| async move {
        let sandbox = fixture.start().await;
        let background = sentinel(&sandbox, &fixture.workspace).await;
        let script = r#"
sh -c '(exec >/dev/null 2>&1; trap "" TERM; printf "waiting\n" > timeout-descendant; while :; do sleep 1; done) &'
exit 7
"#;
        let prepared = sandbox.prepare_supervised_command(request(
            "timeout-with-primary-exit", script, 5_000,
        )).await.unwrap();
        let expected = ExpectedReceipt::observe(&prepared);
        let running = prepared.dispatch().unwrap();
        wait_for_file(&fixture.workspace.join("timeout-descendant"), b"waiting\n").await;
        let execution = running.finish().await.unwrap();
        expected.check(&execution);
        finished(&execution, &ProcessOutcome::TimedOut, true);
        assert!(matches!(execution.result(), ServerMessage::Finished {
            primary_exit: Some(PrimaryExit::Exited { code: 7 }), ..
        }));
        assert_sentinel_alive(&sandbox, &fixture.workspace, &background).await;
    }).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires an explicit prepared Podman image, expected USER uid, and configured connection"]
async fn repeated_automatic_installation_keeps_bytes_but_never_reuses_transport_receipts() {
    with_fixture(|fixture| async move {
        let mut sandbox = fixture.start().await;
        let requested = request("explicit-repeat", "printf x >> explicit-executions", 30_000);
        let mut expected_runs = Vec::new();
        let mut installed_identity = None;
        for index in 0..3 {
            if index == 2 {
                sandbox.stop_checked().await.unwrap();
                sandbox = fixture.start().await;
            }
            let prepared = sandbox
                .prepare_supervised_command(requested.clone())
                .await
                .unwrap();
            let path = fixture.installed_program(&prepared);
            let metadata = std::fs::metadata(path).unwrap();
            let identity = (metadata.dev(), metadata.ino());
            if let Some(previous) = installed_identity {
                assert_eq!(identity, previous);
            }
            installed_identity = Some(identity);
            let expected = ExpectedReceipt::observe(&prepared);
            let execution = prepared.dispatch().unwrap().finish().await.unwrap();
            expected.check(&execution);
            finished(&execution, &ProcessOutcome::Exited { code: 0 }, true);
            expected_runs.push(expected);
        }
        assert_eq!(
            std::fs::read(fixture.workspace.join("explicit-executions")).unwrap(),
            b"xxx"
        );
        assert_eq!(expected_runs[0].runtime, expected_runs[1].runtime);
        assert_ne!(expected_runs[1].runtime, expected_runs[2].runtime);
        for left in 0..expected_runs.len() {
            for right in left + 1..expected_runs.len() {
                assert_ne!(
                    expected_runs[left].transport,
                    expected_runs[right].transport
                );
                assert_eq!(expected_runs[left].program, expected_runs[right].program);
                assert_eq!(expected_runs[left].request, expected_runs[right].request);
            }
        }
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires an explicit prepared Podman image, expected USER uid, and configured connection"]
async fn exact_stdin_uses_the_bundled_helper_and_preserves_unrelated_background_work() {
    with_fixture(|fixture| async move {
        let sandbox = fixture.start().await;
        let background = sentinel(&sandbox, &fixture.workspace).await;
        let pattern = b"unchanged bytes\n'\"$()\\\0\xff";
        let bytes = pattern
            .iter()
            .copied()
            .cycle()
            .take(axocoatl_exec::protocol::MAX_STDIN_BYTES)
            .collect::<Vec<_>>();
        let mut requested = request("exact-stdin-cancel", "unused", 30_000);
        requested.argv = vec![
            "sh".into(),
            "-c".into(),
            "cat > \"$1\"".into(),
            "sh".into(),
            "stdin-result".into(),
        ];
        requested.stdin =
            Some(axocoatl_exec::protocol::StdinDescriptor::for_bytes(&bytes).unwrap());
        let prepared = sandbox
            .prepare_supervised_command_with_stdin(requested.clone(), bytes.clone())
            .await
            .unwrap();
        fixture.installed_program(&prepared);
        let expected = ExpectedReceipt::observe(&prepared);
        assert!(!fixture.workspace.join("stdin-result").exists());
        prepared.cancellation().cancel();
        let execution = prepared.dispatch().unwrap().finish().await.unwrap();
        expected.check(&execution);
        finished(&execution, &ProcessOutcome::Cancelled, false);
        assert!(!fixture.workspace.join("stdin-result").exists());
        requested.invocation_id = "exact-stdin-write".into();
        let prepared = sandbox
            .prepare_supervised_command_with_stdin(requested.clone(), bytes.clone())
            .await
            .unwrap();
        let expected = ExpectedReceipt::observe(&prepared);
        let execution = prepared.dispatch().unwrap().finish().await.unwrap();
        expected.check(&execution);
        finished(&execution, &ProcessOutcome::Exited { code: 0 }, true);
        assert_eq!(execution.request(), &requested);
        assert_eq!(
            std::fs::read(fixture.workspace.join("stdin-result")).unwrap(),
            bytes
        );
        assert_sentinel_alive(&sandbox, &fixture.workspace, &background).await;
    })
    .await;
}
