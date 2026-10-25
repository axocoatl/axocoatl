//! Pipes to a child process that no other program can inherit.
//!
//! On macOS the standard library makes a child's pipes with `pipe()` and
//! then marks each end close-on-exec with `ioctl` (macOS has no `pipe2`), and
//! `posix_spawn` passes every descriptor not so marked to the new program. A
//! program another thread starts between those calls therefore inherits the
//! ends and holds the write end open for as long as it runs: the child's
//! output then has no end of file when the child exits, and a bounded command
//! reads "incomplete output" from a child that finished at once. Measured on
//! a Mac: with four threads starting `sleep 3` beside it, 6 of 16,000 bounded
//! `/bin/echo hi` commands failed with "collecting output timed out before
//! transport EOF", and an onboarding test failed once under load after such
//! a `podman machine list`.
//!
//! A FIFO opened with `O_CLOEXEC` has no such moment, so on macOS a child's
//! ends are a FIFO's, made in a directory only this user can write and
//! removed before the child starts. macOS's kqueue does not report the end
//! of file of a FIFO whose last writer closed (it does for a pipe), so this
//! process uses nonblocking reads and writes, retrying after a short async
//! wait. The future owns its end, so cancellation closes it even if a
//! descendant keeps the other end open. The child's ends remain blocking.
//! Elsewhere `pipe2(O_CLOEXEC)` already makes both ends close-on-exec at
//! once, and the standard library's pipes are used.

use std::io;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::process::{Child, Command};

#[cfg(target_os = "macos")]
const FIFO_RETRY_DELAY: std::time::Duration = std::time::Duration::from_millis(10);

/// Our end of a child's output.
pub(crate) enum OutputEnd {
    /// A pipe, read through the runtime.
    Async(Box<dyn AsyncRead + Unpin + Send>),
    /// A nonblocking FIFO (see the module's note).
    #[cfg(target_os = "macos")]
    Fifo(std::fs::File),
}

/// Our end of a child's input.
pub(crate) enum InputEnd {
    Async(Box<dyn AsyncWrite + Unpin + Send>),
    #[cfg(target_os = "macos")]
    Fifo(std::fs::File),
}

/// The ends of a child's pipes that stay with this process, set up on its
/// command before it starts ([`Pipes::attach`]) and taken once it has
/// ([`Pipes::ends`]).
pub(crate) struct Pipes {
    #[cfg(target_os = "macos")]
    fifo: Option<fifo::Ends>,
}

impl Pipes {
    /// Give `command` pipes for its standard output and error, and for its
    /// standard input when `input`. The command keeps the child's ends until
    /// it is dropped, so drop it once the child has started: until then the
    /// child's output has no end of file.
    pub(crate) fn attach(command: &mut Command, input: bool) -> Self {
        #[cfg(target_os = "macos")]
        {
            match fifo::Ends::attach(command, input) {
                Ok(ends) => return Self { fifo: Some(ends) },
                Err(error) => fifo::warn_once(&error),
            }
        }
        command
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        if input {
            command.stdin(std::process::Stdio::piped());
        }
        Self {
            #[cfg(target_os = "macos")]
            fifo: None,
        }
    }

    /// Our ends once `child` has started: its input (when attached with
    /// `input`), its output and its error output.
    pub(crate) fn ends(
        self,
        child: &mut Child,
    ) -> io::Result<(Option<InputEnd>, OutputEnd, OutputEnd)> {
        #[cfg(target_os = "macos")]
        if let Some(ends) = self.fifo {
            return Ok((
                ends.stdin.map(InputEnd::Fifo),
                OutputEnd::Fifo(ends.stdout),
                OutputEnd::Fifo(ends.stderr),
            ));
        }
        let missing = |what: &str| io::Error::other(format!("the child's {what} was not captured"));
        let stdin = child
            .stdin
            .take()
            .map(|stdin| InputEnd::Async(Box::new(stdin)));
        let stdout = child.stdout.take().ok_or_else(|| missing("stdout"))?;
        let stderr = child.stderr.take().ok_or_else(|| missing("stderr"))?;
        Ok((
            stdin,
            OutputEnd::Async(Box::new(stdout)),
            OutputEnd::Async(Box::new(stderr)),
        ))
    }
}

impl OutputEnd {
    /// Read to the end, calling `chunk` with each piece read until it says
    /// to stop; `Err` when a read failed. Dropping the future closes the
    /// reader and drops its callback, including while a FIFO has no data.
    pub(crate) async fn read_chunks(
        self,
        mut chunk: impl FnMut(&[u8]) -> bool + Send + 'static,
    ) -> io::Result<()> {
        match self {
            OutputEnd::Async(mut reader) => {
                // Keep the buffer out of the future: callers can nest several
                // process probes on a runtime worker's limited stack.
                let mut buffer = vec![0_u8; 16 * 1024];
                loop {
                    let read = reader.read(&mut buffer).await?;
                    if read == 0 || !chunk(&buffer[..read]) {
                        return Ok(());
                    }
                }
            }
            #[cfg(target_os = "macos")]
            OutputEnd::Fifo(mut file) => {
                use std::io::Read;
                let mut buffer = vec![0_u8; 16 * 1024];
                loop {
                    match file.read(&mut buffer) {
                        Ok(0) => return Ok(()),
                        Ok(read) => {
                            if !chunk(&buffer[..read]) {
                                return Ok(());
                            }
                            // A continuously ready FIFO must still let its
                            // owner run the deadline or cancel this task.
                            tokio::task::yield_now().await;
                        }
                        Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                            tokio::time::sleep(FIFO_RETRY_DELAY).await;
                        }
                        Err(error) if error.kind() == io::ErrorKind::Interrupted => {
                            tokio::task::yield_now().await;
                        }
                        Err(error) => return Err(error),
                    }
                }
            }
        }
    }
}

impl InputEnd {
    /// Write all of `bytes`, then close the child's input.
    pub(crate) async fn write_all_and_close(self, bytes: Vec<u8>) -> io::Result<()> {
        match self {
            InputEnd::Async(mut writer) => {
                writer.write_all(&bytes).await?;
                writer.shutdown().await
            }
            #[cfg(target_os = "macos")]
            InputEnd::Fifo(mut file) => {
                use std::io::Write;
                let mut sent = 0;
                while sent < bytes.len() {
                    let end = sent + (bytes.len() - sent).min(16 * 1024);
                    match file.write(&bytes[sent..end]) {
                        Ok(0) => {
                            return Err(io::Error::new(
                                io::ErrorKind::WriteZero,
                                "the child's input accepted no bytes",
                            ));
                        }
                        Ok(written) => {
                            sent += written;
                            tokio::task::yield_now().await;
                        }
                        Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                            tokio::time::sleep(FIFO_RETRY_DELAY).await;
                        }
                        Err(error) if error.kind() == io::ErrorKind::Interrupted => {
                            tokio::task::yield_now().await;
                        }
                        Err(error) => return Err(error),
                    }
                }
                Ok(())
            }
        }
    }
}

/// What [`tokio::process::Command::output`] returns, through [`Pipes`]: run
/// `command` to its end and read all its output.
pub async fn output(mut command: Command) -> io::Result<std::process::Output> {
    let pipes = Pipes::attach(&mut command, false);
    let mut child = command.spawn()?;
    // The child's ends now live only in the child.
    drop(command);
    let (_, stdout, stderr) = pipes.ends(&mut child)?;
    let (status, out, err) = tokio::join!(child.wait(), read_all(stdout), read_all(stderr));
    Ok(std::process::Output {
        status: status?,
        stdout: out?,
        stderr: err?,
    })
}

async fn read_all(end: OutputEnd) -> io::Result<Vec<u8>> {
    let bytes = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let sink = bytes.clone();
    end.read_chunks(move |chunk| {
        sink.lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .extend_from_slice(chunk);
        true
    })
    .await?;
    let mut bytes = bytes.lock().unwrap_or_else(|poison| poison.into_inner());
    Ok(std::mem::take(&mut *bytes))
}

#[cfg(target_os = "macos")]
mod fifo {
    use std::fs::{File, OpenOptions};
    use std::io;
    use std::os::fd::{AsRawFd, OwnedFd};
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
    use std::path::Path;

    use tokio::process::Command;

    /// This process's ends of a child's FIFOs, all nonblocking.
    pub(super) struct Ends {
        pub(super) stdin: Option<File>,
        pub(super) stdout: File,
        pub(super) stderr: File,
    }

    impl Ends {
        pub(super) fn attach(command: &mut Command, input: bool) -> io::Result<Self> {
            let (stdout, child_stdout) = fifo()?;
            let (stderr, child_stderr) = fifo()?;
            set_nonblocking(&stdout)?;
            set_nonblocking(&stderr)?;
            let stdin = match input {
                true => {
                    let (child_stdin, stdin) = fifo()?;
                    set_nonblocking(&stdin)?;
                    command.stdin(OwnedFd::from(child_stdin));
                    Some(stdin)
                }
                false => None,
            };
            command
                .stdout(OwnedFd::from(child_stdout))
                .stderr(OwnedFd::from(child_stderr));
            Ok(Self {
                stdin,
                stdout,
                stderr,
            })
        }
    }

    /// A FIFO's read end and write end, both blocking and close-on-exec from
    /// the moment they are opened (the standard library opens every file
    /// with `O_CLOEXEC`).
    pub(super) fn fifo() -> io::Result<(File, File)> {
        fifo_in(&std::env::temp_dir())
    }

    /// [`fifo`], made in a new directory in `base` that only this user can
    /// write to; the FIFO and the directory are removed once its ends are
    /// open.
    pub(super) fn fifo_in(base: &Path) -> io::Result<(File, File)> {
        for _ in 0..8 {
            let mut suffix = [0_u8; 8];
            getrandom::fill(&mut suffix).map_err(io::Error::other)?;
            let suffix: String = suffix.iter().map(|byte| format!("{byte:02x}")).collect();
            let directory = base.join(format!("axocoatl-pipe-{suffix}"));
            match std::fs::DirBuilder::new().mode(0o700).create(&directory) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error),
            }
            let path = directory.join("fifo");
            let opened = make_fifo(&path).and_then(|()| {
                // The read end opens without waiting for a writer, and then
                // the write end opens at once.
                let read = OpenOptions::new()
                    .read(true)
                    .custom_flags(libc::O_NONBLOCK)
                    .open(&path)?;
                let write = OpenOptions::new().write(true).open(&path)?;
                set_blocking(&read)?;
                Ok((read, write))
            });
            let _ = std::fs::remove_file(&path);
            let _ = std::fs::remove_dir(&directory);
            return opened;
        }
        Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!(
                "could not make a new directory for a pipe in {}",
                base.display()
            ),
        ))
    }

    fn make_fifo(path: &Path) -> io::Result<()> {
        let path = std::ffi::CString::new(path.as_os_str().as_bytes())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "a NUL in the pipe path"))?;
        // SAFETY: `path` is a NUL-terminated string that outlives the call.
        if unsafe { libc::mkfifo(path.as_ptr(), 0o600) } == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }

    fn set_blocking(file: &File) -> io::Result<()> {
        set_nonblocking_flag(file, false)
    }

    pub(super) fn set_nonblocking(file: &File) -> io::Result<()> {
        set_nonblocking_flag(file, true)
    }

    fn set_nonblocking_flag(file: &File, nonblocking: bool) -> io::Result<()> {
        let fd = file.as_raw_fd();
        // SAFETY: `fd` is open for the duration of both calls.
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        if flags < 0 {
            return Err(io::Error::last_os_error());
        }
        let flags = if nonblocking {
            flags | libc::O_NONBLOCK
        } else {
            flags & !libc::O_NONBLOCK
        };
        // SAFETY: as above.
        if unsafe { libc::fcntl(fd, libc::F_SETFL, flags) } < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// The temporary directory cannot hold a FIFO: the standard library's
    /// pipes are used instead, and that is said once.
    pub(super) fn warn_once(error: &io::Error) {
        static WARNED: std::sync::Once = std::sync::Once::new();
        WARNED.call_once(|| {
            tracing::warn!(
                %error,
                directory = %std::env::temp_dir().display(),
                "could not make a FIFO for a child's output; using ordinary pipes, which a \
                 program started at the same moment can inherit"
            );
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A child gets its input and gives its output and error output through
    /// the pipes, each read to its end.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_child_reads_its_input_and_writes_its_output_through_the_pipes() {
        let mut command = Command::new("/bin/sh");
        command.args(["-c", "cat; printf err >&2"]);
        let pipes = Pipes::attach(&mut command, true);
        let mut child = command.spawn().unwrap();
        drop(command);
        let (stdin, stdout, stderr) = pipes.ends(&mut child).unwrap();
        stdin
            .unwrap()
            .write_all_and_close(b"in and out".to_vec())
            .await
            .unwrap();
        let (status, out, err) = tokio::join!(child.wait(), read_all(stdout), read_all(stderr));
        assert!(status.unwrap().success());
        assert_eq!(out.unwrap(), b"in and out");
        assert_eq!(err.unwrap(), b"err");
    }

    /// [`output`] is `Command::output`: the status and all of both outputs.
    #[cfg(unix)]
    #[tokio::test]
    async fn output_returns_the_status_and_both_outputs() {
        let mut command = Command::new("/bin/sh");
        command.args([
            "-c",
            "printf out; sleep 0.2; printf more; printf err >&2; exit 3",
        ]);
        let output = output(command).await.unwrap();
        assert_eq!(output.status.code(), Some(3));
        assert_eq!(output.stdout, b"outmore");
        assert_eq!(output.stderr, b"err");
    }

    /// Both ends of a FIFO are close-on-exec, and neither the FIFO nor its
    /// directory is left behind.
    #[cfg(target_os = "macos")]
    #[test]
    fn a_fifos_ends_close_on_exec_and_leave_nothing_behind() {
        use std::os::fd::AsRawFd;
        let base = tempfile::tempdir().unwrap();
        let (read, write) = fifo::fifo_in(base.path()).unwrap();
        for end in [&read, &write] {
            // SAFETY: the descriptor is open for the call.
            let flags = unsafe { libc::fcntl(end.as_raw_fd(), libc::F_GETFD) };
            assert_eq!(flags & libc::FD_CLOEXEC, libc::FD_CLOEXEC);
            // SAFETY: the descriptor is open for the call.
            let flags = unsafe { libc::fcntl(end.as_raw_fd(), libc::F_GETFL) };
            assert_eq!(flags & libc::O_NONBLOCK, 0);
        }
        fifo::set_nonblocking(&read).unwrap();
        // Changing our read end leaves the child's write end blocking.
        // SAFETY: both descriptors remain open for these calls.
        assert_ne!(
            unsafe { libc::fcntl(read.as_raw_fd(), libc::F_GETFL) } & libc::O_NONBLOCK,
            0
        );
        // SAFETY: as above.
        assert_eq!(
            unsafe { libc::fcntl(write.as_raw_fd(), libc::F_GETFL) } & libc::O_NONBLOCK,
            0
        );
        assert_eq!(std::fs::read_dir(base.path()).unwrap().count(), 0);
    }

    /// A timeout drops a reader's callback immediately, even while a child
    /// or descendant still holds the output open. A detached blocking
    /// worker would retain the callback until that writer closed.
    #[cfg(target_os = "macos")]
    #[tokio::test(flavor = "current_thread")]
    async fn cancelling_an_empty_fifo_read_releases_its_callback() {
        use std::future::Future;
        use std::sync::Arc;
        use std::task::{Context, Waker};

        let (reader, writer) = fifo::fifo().unwrap();
        fifo::set_nonblocking(&reader).unwrap();
        let owner = Arc::new(());
        let callback_owner = Arc::clone(&owner);
        let mut read = Box::pin(OutputEnd::Fifo(reader).read_chunks(move |_| {
            let _keep_alive = &callback_owner;
            true
        }));
        // Polling once establishes that the reader really started and is
        // waiting for data; cancellation is not dependent on a short timer.
        assert!(read
            .as_mut()
            .poll(&mut Context::from_waker(Waker::noop()))
            .is_pending());
        assert_eq!(Arc::strong_count(&owner), 2);
        drop(read);
        assert_eq!(Arc::strong_count(&owner), 1);
        drop(writer);
    }

    /// A full FIFO must not keep accepting the rest of a request after
    /// cancellation. Once the caller drops its write, draining the bytes
    /// already buffered reaches EOF without any of the cancelled payload.
    #[cfg(target_os = "macos")]
    #[tokio::test(flavor = "current_thread")]
    async fn cancelling_a_full_fifo_write_closes_it_without_sending_more() {
        use std::future::Future;
        use std::io::Write;
        use std::task::{Context, Waker};
        use std::time::Duration;

        let (reader, mut writer) = fifo::fifo().unwrap();
        fifo::set_nonblocking(&reader).unwrap();
        fifo::set_nonblocking(&writer).unwrap();
        let buffered = [0xaa; 16 * 1024];
        let mut filled = 0;
        loop {
            match writer.write(&buffered) {
                Ok(0) => panic!("a FIFO write made no progress"),
                Ok(written) => filled += written,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => panic!("filling the FIFO: {error}"),
            }
        }
        assert!(filled > 0);
        let mut write = Box::pin(InputEnd::Fifo(writer).write_all_and_close(vec![0xbb; 16 * 1024]));
        assert!(write
            .as_mut()
            .poll(&mut Context::from_waker(Waker::noop()))
            .is_pending());
        drop(write);
        let drained =
            tokio::time::timeout(Duration::from_secs(10), read_all(OutputEnd::Fifo(reader)))
                .await
                .expect("the cancelled writer must close, even while its reader stays open")
                .unwrap();
        assert_eq!(drained, vec![0xaa; filled]);
    }

    /// A callback that refills every chunk keeps the FIFO continuously
    /// readable. One poll must still yield so its caller can cancel it;
    /// retrying only on WouldBlock would consume the second chunk instead.
    #[cfg(target_os = "macos")]
    #[tokio::test(flavor = "current_thread")]
    async fn a_ready_fifo_reader_yields_so_it_can_be_cancelled() {
        use std::future::Future;
        use std::io::Write;
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;
        use std::task::{Context, Waker};

        let (reader, mut writer) = fifo::fifo().unwrap();
        fifo::set_nonblocking(&reader).unwrap();
        writer.write_all(b"ready").unwrap();
        let chunks = Arc::new(AtomicUsize::new(0));
        let observed = Arc::clone(&chunks);
        let mut read = Box::pin(OutputEnd::Fifo(reader).read_chunks(move |chunk| {
            writer.write_all(chunk).unwrap();
            // The second chunk terminates even a reader that forgot to
            // yield, so that regression fails without hanging the test.
            observed.fetch_add(1, Ordering::SeqCst) == 0
        }));
        assert!(read
            .as_mut()
            .poll(&mut Context::from_waker(Waker::noop()))
            .is_pending());
        assert_eq!(chunks.load(Ordering::SeqCst), 1);
        drop(read);
        assert_eq!(Arc::strong_count(&chunks), 1);
    }

    /// The point of these pipes: a program another thread starts while a
    /// command runs does not hold the command's output open. Four threads
    /// start `sleep 3` without pause beside 2,000 bounded `/bin/echo hi`
    /// commands. With the standard library's pipes on macOS, 6 of 16,000
    /// such commands waited for a `sleep` to end and failed. Ignored by
    /// default: it starts hundreds of processes.
    #[cfg(unix)]
    #[test]
    #[ignore = "starts hundreds of processes; run it alone"]
    fn programs_started_beside_a_command_do_not_hold_its_output_open() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;
        use std::time::Duration;

        let rounds: usize = std::env::var("AXOCOATL_PIPE_ROUNDS")
            .ok()
            .and_then(|rounds| rounds.parse().ok())
            .unwrap_or(250);
        let stop = Arc::new(AtomicBool::new(false));
        let starters: Vec<_> = (0..4)
            .map(|_| {
                let stop = stop.clone();
                std::thread::spawn(move || {
                    let mut children = std::collections::VecDeque::new();
                    while !stop.load(Ordering::Relaxed) {
                        children.push_back(
                            std::process::Command::new("/bin/sleep")
                                .arg("3")
                                .stdin(std::process::Stdio::null())
                                .stdout(std::process::Stdio::null())
                                .stderr(std::process::Stdio::null())
                                .spawn()
                                .unwrap(),
                        );
                        while children.len() > 300 {
                            children.pop_front().unwrap().wait().unwrap();
                        }
                    }
                    for mut child in children {
                        let _ = child.kill();
                        let _ = child.wait();
                    }
                })
            })
            .collect();
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(4)
            .enable_all()
            .build()
            .unwrap();
        let (failures, total) = runtime.block_on(async {
            let mut failures = Vec::new();
            let mut total = 0;
            for _ in 0..rounds {
                let commands: Vec<_> = (0..8)
                    .map(|_| {
                        tokio::spawn(async {
                            let mut command = Command::new("/bin/echo");
                            command.arg("hi");
                            crate::SessionSandbox::run_bounded_command(
                                command,
                                Duration::from_secs(30),
                            )
                            .await
                        })
                    })
                    .collect();
                for command in commands {
                    total += 1;
                    match command.await.unwrap() {
                        Ok(output) if output.stdout == b"hi\n" => {}
                        Ok(output) => {
                            failures.push(String::from_utf8_lossy(&output.stdout).into_owned())
                        }
                        Err(error) => failures.push(error.to_string()),
                    }
                }
            }
            (failures, total)
        });
        stop.store(true, Ordering::Relaxed);
        for starter in starters {
            starter.join().unwrap();
        }
        eprintln!("{} of {total} commands failed", failures.len());
        assert!(failures.is_empty(), "{failures:?}");
    }
}
