//! Owned host side of the supervisor protocol. Preparing the helper launches no
//! repository command. The host consumes its durable permit before dispatch().
use crate::{IsolationError, SessionSandbox};
use axocoatl_exec::protocol::{
    Control, ExecRequest, ServerMessage, CLEANUP_TIMEOUT_MS, MAX_RESPONSE_BYTES,
};
use std::process::Stdio;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStderr, Command};
use tokio::sync::{oneshot, watch};

const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
const TRANSPORT_GRACE: Duration = Duration::from_secs(10);
const STDERR_LIMIT: usize = 16 * 1024;
const PREPARATION_REAP_TIMEOUT: Duration = Duration::from_secs(2);

fn error(message: impl std::fmt::Display) -> IsolationError {
    IsolationError::OciContainerFailed(format!("command supervisor: {message}"))
}

/// Raw captured evidence plus privately minted proof for this exact helper run.
pub struct SupervisedExecution {
    request: ExecRequest,
    runtime_identity: String,
    program_sha256: String,
    result: ServerMessage,
    settlement: Option<ProcessSettlement>,
}

impl SupervisedExecution {
    pub fn request(&self) -> &ExecRequest {
        &self.request
    }
    pub fn runtime_identity(&self) -> &str {
        &self.runtime_identity
    }
    pub fn program_sha256(&self) -> &str {
        &self.program_sha256
    }
    pub fn result(&self) -> &ServerMessage {
        &self.result
    }
    pub fn settlement(&self) -> Option<&ProcessSettlement> {
        self.settlement.as_ref()
    }
}

/// Neither serializable nor publicly constructible. A bool or command exit
/// supplied by a caller cannot release owned repository execution.
pub struct ProcessSettlement {
    transport_id: String,
    invocation_id: String,
    request_sha256: String,
    runtime_identity: String,
    program_sha256: String,
}

impl ProcessSettlement {
    pub fn transport_identity(&self) -> &str {
        &self.transport_id
    }
    pub fn invocation_id(&self) -> &str {
        &self.invocation_id
    }
    pub fn request_sha256(&self) -> &str {
        &self.request_sha256
    }
    pub fn runtime_identity(&self) -> &str {
        &self.runtime_identity
    }
    pub fn program_sha256(&self) -> &str {
        &self.program_sha256
    }
}

/// Cancellation remains addressable after dispatch while the caller waits.
#[derive(Clone)]
pub struct SupervisorCancellation(watch::Sender<bool>);
impl SupervisorCancellation {
    pub fn cancel(&self) {
        self.0.send_replace(true);
    }
    pub fn is_cancelled(&self) -> bool {
        *self.0.borrow()
    }
}

/// Dropping this value requests cancellation. It does not return a settlement
/// receipt: an owner which must release repository authority after cleanup must
/// retain the command and collect `dispatch()?.finish().await`, even after Stop.
pub struct PreparedSupervisedCommand {
    transport_id: String,
    runtime_identity: String,
    program_sha256: String,
    request: ExecRequest,
    dispatch: Option<oneshot::Sender<()>>,
    cancellation: SupervisorCancellation,
    task: Option<tokio::task::JoinHandle<Result<SupervisedExecution, IsolationError>>>,
}

impl PreparedSupervisedCommand {
    pub fn transport_identity(&self) -> &str {
        &self.transport_id
    }
    pub fn runtime_identity(&self) -> &str {
        &self.runtime_identity
    }
    pub fn program_sha256(&self) -> &str {
        &self.program_sha256
    }
    pub fn request(&self) -> &ExecRequest {
        &self.request
    }
    pub fn cancellation(&self) -> SupervisorCancellation {
        self.cancellation.clone()
    }

    /// Synchronous owned handoff, suitable for the controller's last admission
    /// gate. Stop and dispatch use this same retained cancellation channel. A
    /// helper that has already expired/cancelled remains collectable through
    /// finish(); a closed handoff channel must not discard its terminal receipt.
    pub fn dispatch(mut self) -> Result<RunningSupervisedCommand, IsolationError> {
        let dispatch = self
            .dispatch
            .take()
            .ok_or_else(|| error("dispatch already consumed"))?;
        if !self.cancellation.is_cancelled() {
            // Receiver closure can mean that a valid no-launch acknowledgment
            // is already in the task result. Transfer ownership in either case.
            let _ = dispatch.send(());
        }
        Ok(RunningSupervisedCommand {
            cancellation: self.cancellation.clone(),
            task: self.task.take(),
        })
    }
}

impl Drop for PreparedSupervisedCommand {
    fn drop(&mut self) {
        if self.task.is_some() {
            self.cancellation.cancel();
        }
    }
}

/// The runtime owner must retain and await this value to collect settlement.
/// Drop requests cleanup, but detaches the transport task and discards its later
/// result; it cannot by itself authorize releasing repository ownership.
pub struct RunningSupervisedCommand {
    cancellation: SupervisorCancellation,
    task: Option<tokio::task::JoinHandle<Result<SupervisedExecution, IsolationError>>>,
}

impl RunningSupervisedCommand {
    pub fn cancellation(&self) -> SupervisorCancellation {
        self.cancellation.clone()
    }
    pub async fn finish(mut self) -> Result<SupervisedExecution, IsolationError> {
        // Keep the JoinHandle in self across await: abandoning this future
        // cancels the owned command while its supervisor continues cleanup.
        let result = self
            .task
            .as_mut()
            .ok_or_else(|| error("command wait already consumed"))?
            .await
            .map_err(|failure| error(format!("supervisor transport task lost: {failure}")))?;
        self.task.take();
        result
    }
}

impl Drop for RunningSupervisedCommand {
    fn drop(&mut self) {
        if self.task.is_some() {
            self.cancellation.cancel();
        }
    }
}

impl SessionSandbox {
    /// Prepare as the writer: in a hardened container, Agents' commands,
    /// required checks and Axocoatl's own captures run as the writer user.
    pub async fn prepare_supervised_command(
        &self,
        request: ExecRequest,
    ) -> Result<PreparedSupervisedCommand, IsolationError> {
        request.validate_stdin(None).map_err(error)?;
        let (command, runtime, program) =
            self.supervisor_transport_command(None, crate::ExecIdentity::Writer)?;
        prepare_command(command, request, runtime, program).await
    }

    /// Prepare through the owned helper with an environment file for the
    /// helper and the command it launches (the egress credential's proxy
    /// settings). The file is read by the Podman client, so no value of it
    /// appears in any argv.
    pub async fn prepare_supervised_command_with_env(
        &self,
        request: ExecRequest,
        stdin: Option<Vec<u8>>,
        env: crate::egress::ProcessEnv<'_>,
    ) -> Result<PreparedSupervisedCommand, IsolationError> {
        self.prepare_supervised_command_as(request, stdin, env, crate::ExecIdentity::Writer)
            .await
    }

    /// [`Self::prepare_supervised_command_with_env`] as `identity`: in a
    /// hardened container a read-only helper's processes run as the helper
    /// user, which cannot read the writer's processes' environment or
    /// signal them. Without workload users every identity but root is the
    /// image's user.
    pub async fn prepare_supervised_command_as(
        &self,
        request: ExecRequest,
        stdin: Option<Vec<u8>>,
        env: crate::egress::ProcessEnv<'_>,
        identity: crate::ExecIdentity,
    ) -> Result<PreparedSupervisedCommand, IsolationError> {
        request.validate_stdin(stdin.as_deref()).map_err(error)?;
        if let Some(env_file) = env.env_file {
            if !env_file.is_absolute() {
                return Err(error("the process environment file must be absolute"));
            }
        }
        let (command, runtime, program) =
            self.supervisor_transport_command(env.env_file, identity)?;
        prepare_command_with_stdin(
            command,
            request,
            stdin.map(std::sync::Arc::from),
            runtime,
            program,
        )
        .await
    }

    /// Preserve the caller's exact stdin bytes through the owned helper. The
    /// header identity must already describe these bytes; this port never
    /// rewrites argv or embeds the body in a shell program.
    pub async fn prepare_supervised_command_with_stdin(
        &self,
        request: ExecRequest,
        stdin: Vec<u8>,
    ) -> Result<PreparedSupervisedCommand, IsolationError> {
        request.validate_stdin(Some(&stdin)).map_err(error)?;
        let (command, runtime, program) =
            self.supervisor_transport_command(None, crate::ExecIdentity::Writer)?;
        prepare_command_with_stdin(
            command,
            request,
            Some(std::sync::Arc::from(stdin)),
            runtime,
            program,
        )
        .await
    }

    pub(crate) fn supervisor_transport_command(
        &self,
        env_file: Option<&std::path::Path>,
        identity: crate::ExecIdentity,
    ) -> Result<(Command, String, String), IsolationError> {
        let (runtime, root, program) = self.supervised_parts()?;
        let mut command = Command::new("podman");
        command.args(["exec", "-i"]);
        if let Some(env_file) = env_file {
            command.arg("--env-file").arg(env_file);
        }
        command
            .args(self.exec_user(identity))
            .arg("-w")
            .arg(root)
            .arg(&runtime)
            .args(supervisor_serve_args(
                self.workload_users().is_some(),
                identity,
            ));
        Ok((command, runtime, program.sha256().to_owned()))
    }
}

/// The supervisor's arguments for one supervised exec. In a hardened
/// container the workload users' commands (writers' and helpers') get
/// `--harden`: no new privileges, the supervisor's seccomp denylist and a
/// Landlock domain of their own. Root's (readiness and provisioning) do not.
fn supervisor_serve_args(hardened: bool, identity: crate::ExecIdentity) -> Vec<&'static str> {
    let mut args = vec![
        crate::supervisor_program::SUPERVISOR_CONTAINER_PATH,
        "--serve",
    ];
    if hardened && identity != crate::ExecIdentity::Root {
        args.push("--harden");
    }
    args
}

async fn prepare_command(
    command: Command,
    request: ExecRequest,
    runtime_identity: String,
    program_sha256: String,
) -> Result<PreparedSupervisedCommand, IsolationError> {
    prepare_command_with_stdin(command, request, None, runtime_identity, program_sha256).await
}

pub(crate) async fn prepare_command_with_stdin(
    mut command: Command,
    request: ExecRequest,
    stdin: Option<std::sync::Arc<[u8]>>,
    runtime_identity: String,
    program_sha256: String,
) -> Result<PreparedSupervisedCommand, IsolationError> {
    request.validate_stdin(stdin.as_deref()).map_err(error)?;
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let child = command.spawn().map_err(error)?;
    let (dispatch, dispatched) = oneshot::channel();
    let (ready, prepared) = oneshot::channel();
    let (cancel, cancelled) = watch::channel(false);
    let cancellation = SupervisorCancellation(cancel);
    let run_request = request.clone();
    let transport_id = uuid::Uuid::new_v4().to_string();
    let run_transport_id = transport_id.clone();
    let run_runtime = runtime_identity.clone();
    let run_program = program_sha256.clone();
    let task = tokio::spawn(async move {
        supervise_transport(
            child,
            run_request,
            stdin,
            run_runtime,
            run_program,
            run_transport_id,
            dispatched,
            cancelled,
            ready,
        )
        .await
    });
    // This guard also handles cancellation while the helper is being prepared.
    let command = PreparedSupervisedCommand {
        transport_id,
        runtime_identity,
        program_sha256,
        request,
        dispatch: Some(dispatch),
        cancellation,
        task: Some(task),
    };
    match tokio::time::timeout(HANDSHAKE_TIMEOUT + TRANSPORT_GRACE, prepared).await {
        Ok(Ok(Ok(()))) => Ok(command),
        Ok(Ok(Err(message))) => Err(error(message)),
        Ok(Err(_)) => Err(error("helper preparation was lost")),
        Err(_) => Err(error("helper did not acknowledge preparation")),
    }
}

#[allow(clippy::too_many_arguments)]
async fn supervise_transport(
    mut child: Child,
    request: ExecRequest,
    stdin: Option<std::sync::Arc<[u8]>>,
    runtime_identity: String,
    program_sha256: String,
    transport_id: String,
    dispatch: oneshot::Receiver<()>,
    mut cancellation: watch::Receiver<bool>,
    ready: oneshot::Sender<Result<(), String>>,
) -> Result<SupervisedExecution, IsolationError> {
    let mut input = Some(
        child
            .stdin
            .take()
            .ok_or_else(|| error("helper input unavailable"))?,
    );
    let mut output = FrameReader::new(
        child
            .stdout
            .take()
            .ok_or_else(|| error("helper output unavailable"))?,
    );
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| error("helper diagnostics unavailable"))?;
    let diagnostic_bytes = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let mut diagnostics = tokio::spawn(read_diagnostics(stderr, diagnostic_bytes.clone()));
    let handshake = tokio::time::timeout(HANDSHAKE_TIMEOUT, async {
        write_frame(
            input
                .as_mut()
                .ok_or_else(|| error("helper input unavailable"))?,
            &request,
        )
        .await?;
        if let Some(bytes) = stdin.as_deref() {
            let writer = input
                .as_mut()
                .ok_or_else(|| error("helper input unavailable"))?;
            // Already bounded/hashed above. These raw bytes follow the request
            // header, before Ready and before any possible Dispatch control.
            writer.write_all(bytes).await.map_err(error)?;
            writer.flush().await.map_err(error)?;
        }
        let message = output
            .read_frame()
            .await?
            .ok_or_else(|| error("helper closed before Ready"))?;
        message.validate_for(&request).map_err(error)?;
        if !matches!(message, ServerMessage::Ready { .. }) {
            return Err(error("helper dispatched before host admission"));
        }
        Ok::<_, IsolationError>(())
    })
    .await;
    drop(stdin);
    match handshake {
        Ok(Ok(())) => {
            if ready.send(Ok(())).is_err() {
                cancellation.borrow_and_update();
            }
        }
        failure => {
            let mut message = match failure {
                Ok(Err(failure)) => failure.to_string(),
                Err(_) => "helper Ready deadline exceeded".into(),
                _ => unreachable!(),
            };
            drop(input);
            let _ = child.start_kill();
            match tokio::time::timeout(PREPARATION_REAP_TIMEOUT, child.wait()).await {
                Ok(Ok(_)) => (),
                Ok(Err(failure)) => {
                    message.push_str(&format!("; reaping failed preparation: {failure}"))
                }
                Err(_) => message
                    .push_str("; failed preparation transport was not reaped before its deadline"),
            }
            if tokio::time::timeout(PREPARATION_REAP_TIMEOUT, &mut diagnostics)
                .await
                .is_err()
            {
                diagnostics.abort();
                let _ = tokio::time::timeout(PREPARATION_REAP_TIMEOUT, &mut diagnostics).await;
            }
            let diagnostic = diagnostic_bytes
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            if !diagnostic.is_empty() {
                message.push_str("; helper diagnostics: ");
                message.push_str(String::from_utf8_lossy(&diagnostic).trim());
            }
            drop(diagnostic);
            // Ready's outer deadline leaves room for all three bounded cleanup
            // waits. Report the diagnostic only after owning transport cleanup.
            let _ = ready.send(Err(message.clone()));
            return Err(error(message));
        }
    }
    let deadline = Duration::from_millis(request.timeout_ms.saturating_add(CLEANUP_TIMEOUT_MS))
        + TRANSPORT_GRACE;
    let execution = tokio::time::timeout(deadline, async {
        // The helper's own deadline starts when it receives the request. It
        // can therefore finish without launching while the host is still
        // persisting admission. Keep that exact acknowledgment collectable.
        let admission = if *cancellation.borrow() { Admission::Control(false) } else {
            tokio::select! {
                biased;
                _ = cancellation.changed() => Admission::Control(false),
                message = output.read_frame() => Admission::Terminal(Box::new(message?.ok_or_else(|| error("prepared helper ended without a complete result"))?)),
                signal = dispatch => Admission::Control(signal.is_ok() && !*cancellation.borrow()),
            }
        };
        let message = match admission {
            Admission::Terminal(message) => {
                validate_terminal(&message, &request, true)?;
                *message
            }
            Admission::Control(may_dispatch) => {
                let control = if may_dispatch { Control::Dispatch } else { Control::Cancel };
                let failed_write = transmit_control(&mut input, &control).await.err();
                if failed_write.is_some() || !may_dispatch {
                    // EPIPE may race an already-written Finished frame. Close
                    // control on write failure, but still collect that frame.
                    // Pre-dispatch cancellation gets the cleanup bound, not
                    // the potentially day-long command execution allowance.
                    terminal_after_control(&mut output, &request, !may_dispatch, failed_write).await?
                } else {
                    loop {
                        tokio::select! {
                            biased;
                            changed = cancellation.changed() => {
                                if changed.is_err() || *cancellation.borrow() {
                                    let failed_write = transmit_control(&mut input, &Control::Cancel).await.err();
                                    break terminal_after_control(&mut output, &request, false, failed_write).await?;
                                }
                            }
                            message = output.read_frame() => {
                                let message = message?.ok_or_else(|| error("helper ended without a complete result"))?;
                                validate_terminal(&message, &request, false)?;
                                break message;
                            }
                        }
                    }
                }
            }
        };
        drop(input);
        if tokio::time::timeout(TRANSPORT_GRACE, output.read_frame()).await
            .map_err(|_| error("helper control stream did not close"))??.is_some() {
            return Err(error("helper sent unexpected trailing control evidence"));
        }
        let status = tokio::time::timeout(TRANSPORT_GRACE, child.wait()).await
            .map_err(|_| error("helper transport did not exit"))?.map_err(error)?;
        if !status.success() { return Err(error(format!("helper transport ended unsuccessfully: {status}"))); }
        Ok::<_, IsolationError>(message)
    }).await;
    let message = match execution {
        Ok(Ok(message)) => message,
        failure => {
            // kill_on_drop closes transport; none of this claims backend work
            // stopped. The helper also owns its independent execution deadline.
            reap_transport(&mut child).await;
            diagnostics.abort();
            let _ = diagnostics.await;
            return Err(match failure {
                Ok(Err(failure)) => failure,
                Err(_) => error("helper settlement acknowledgment was lost or overdue"),
                _ => unreachable!(),
            });
        }
    };
    // A diagnostic pipe is independent of the authenticated control stream.
    // Bound its drain even when transport has exited.
    if tokio::time::timeout(TRANSPORT_GRACE, &mut diagnostics)
        .await
        .is_err()
    {
        diagnostics.abort();
        let _ = diagnostics.await;
    }
    let settlement = if matches!(
        message,
        ServerMessage::Finished {
            quiescent: true,
            ..
        }
    ) {
        Some(ProcessSettlement {
            transport_id,
            invocation_id: request.invocation_id.clone(),
            request_sha256: request.digest().map_err(error)?,
            runtime_identity: runtime_identity.clone(),
            program_sha256: program_sha256.clone(),
        })
    } else {
        None
    };
    Ok(SupervisedExecution {
        request,
        runtime_identity,
        program_sha256,
        result: message,
        settlement,
    })
}

enum Admission {
    Control(bool),
    Terminal(Box<ServerMessage>),
}

fn validate_terminal(
    message: &ServerMessage,
    request: &ExecRequest,
    before_dispatch: bool,
) -> Result<(), IsolationError> {
    message.validate_for(request).map_err(error)?;
    match message {
        ServerMessage::Finished { launched, .. } if !before_dispatch || !launched => Ok(()),
        ServerMessage::Finished { .. } => Err(error("helper launched before host dispatch")),
        ServerMessage::Ready { .. } => Err(error("unexpected repeated helper readiness")),
    }
}

async fn transmit_control<W: tokio::io::AsyncWrite + Unpin>(
    input: &mut Option<W>,
    control: &Control,
) -> Result<(), IsolationError> {
    let result = match input.as_mut() {
        Some(writer) => write_frame(writer, control).await,
        None => Err(error("helper control input already closed")),
    };
    if result.is_err() {
        // A partial control write is not an acknowledged dispatch or cancel.
        // EOF asks the helper to clean up, while the read side remains owned.
        input.take();
    }
    result
}

async fn terminal_after_control<R: tokio::io::AsyncRead + Unpin>(
    output: &mut FrameReader<R>,
    request: &ExecRequest,
    before_dispatch: bool,
    failed_write: Option<IsolationError>,
) -> Result<ServerMessage, IsolationError> {
    let received = tokio::time::timeout(
        Duration::from_millis(CLEANUP_TIMEOUT_MS) + TRANSPORT_GRACE,
        output.read_frame(),
    )
    .await
    .map_err(|_| error("helper did not confirm settlement after control"))
    .and_then(|result| result)
    .and_then(|message| message.ok_or_else(|| error("helper ended without a complete result")));
    let message = match received {
        Ok(message) => message,
        Err(receive_error) => {
            return Err(match failed_write {
                Some(write_error) => error(format!(
                    "{write_error}; terminal collection: {receive_error}"
                )),
                None => receive_error,
            })
        }
    };
    validate_terminal(&message, request, before_dispatch)?;
    Ok(message)
}

async fn write_frame(
    writer: &mut (impl tokio::io::AsyncWrite + Unpin),
    value: &impl serde::Serialize,
) -> Result<(), IsolationError> {
    let mut bytes = serde_json::to_vec(value).map_err(error)?;
    bytes.push(b'\n');
    tokio::time::timeout(HANDSHAKE_TIMEOUT, async {
        writer.write_all(&bytes).await.map_err(error)?;
        writer.flush().await.map_err(error)
    })
    .await
    .map_err(|_| error("helper control write timed out"))?
}

struct FrameReader<R> {
    reader: BufReader<R>,
    pending: Vec<u8>,
}

impl<R: tokio::io::AsyncRead + Unpin> FrameReader<R> {
    fn new(reader: R) -> Self {
        Self {
            reader: BufReader::new(reader),
            pending: Vec::new(),
        }
    }

    // Partial frame bytes remain owned by this reader when a cancellation
    // notification interrupts the await; the next read resumes the same frame.
    async fn read_frame(&mut self) -> Result<Option<ServerMessage>, IsolationError> {
        loop {
            let available = self.reader.fill_buf().await.map_err(error)?;
            if available.is_empty() {
                return if self.pending.is_empty() {
                    Ok(None)
                } else {
                    Err(error("incomplete helper control frame"))
                };
            }
            let newline = available.iter().position(|byte| *byte == b'\n');
            let take = newline.map_or(available.len(), |at| at + 1);
            if self.pending.len().saturating_add(take) > MAX_RESPONSE_BYTES {
                return Err(error("helper control frame exceeds bound"));
            }
            self.pending.extend_from_slice(&available[..take]);
            self.reader.consume(take);
            if newline.is_some() {
                let frame = std::mem::take(&mut self.pending);
                return serde_json::from_slice(&frame).map(Some).map_err(error);
            }
        }
    }
}

async fn read_diagnostics(
    mut reader: ChildStderr,
    retained: std::sync::Arc<std::sync::Mutex<Vec<u8>>>,
) {
    let mut chunk = [0u8; 4096];
    while let Ok(count) = reader.read(&mut chunk).await {
        if count == 0 {
            break;
        }
        let mut retained = retained.lock().unwrap_or_else(|poison| poison.into_inner());
        let take = STDERR_LIMIT.saturating_sub(retained.len()).min(count);
        retained.extend_from_slice(&chunk[..take]);
    }
}

async fn reap_transport(child: &mut Child) {
    let _ = child.start_kill();
    let _ = tokio::time::timeout(TRANSPORT_GRACE, child.wait()).await;
}

#[cfg(all(test, unix))]
#[path = "supervisor_transport_tests.rs"]
mod tests;
