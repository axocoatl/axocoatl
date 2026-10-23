//! What onboarding finds programs with and runs them through.
//!
//! Every program onboarding runs is found on [`Host::path`] (and, on macOS,
//! in the directories Homebrew, the podman.io package and the Ollama app
//! install to), runs with that `PATH` and `HOME`, and is written to the
//! onboarding log with its exit status. Tests point `path` at a directory of
//! shim programs that log their arguments, and `home` and `root` at
//! temporary directories, so nothing on the machine running the tests is
//! changed or read.

use std::ffi::OsString;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// How long a read-only probe may take.
pub(crate) const PROBE_TIMEOUT: Duration = Duration::from_secs(30);

/// The onboarding log: every command onboarding runs, its exit status and
/// the output of the ones it reads. Never a key.
pub(crate) struct Log {
    path: PathBuf,
    file: Mutex<std::fs::File>,
}

impl Log {
    /// Create `onboard-<timestamp>.log` in `directory` (owner-only).
    pub(crate) fn create(directory: &Path, stamp: &str) -> std::io::Result<Self> {
        Self::create_named(directory, "onboard", stamp)
    }

    /// Create `<name>-<timestamp>.log` in `directory` (owner-only).
    pub(crate) fn create_named(directory: &Path, name: &str, stamp: &str) -> std::io::Result<Self> {
        std::fs::create_dir_all(directory)?;
        let path = directory.join(format!("{name}-{stamp}.log"));
        let mut options = std::fs::OpenOptions::new();
        options.create_new(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let file = options.open(&path)?;
        Ok(Self {
            path,
            file: Mutex::new(file),
        })
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    pub(crate) fn line(&self, text: &str) {
        if let Ok(mut file) = self.file.lock() {
            let _ = writeln!(file, "{text}");
        }
    }
}

/// Where the onboarding log goes: `~/Library/Logs/Axocoatl` on macOS,
/// `$XDG_STATE_HOME/axocoatl` or `~/.local/state/axocoatl` on Linux.
pub(crate) fn log_directory(home: &Path, macos: bool, xdg_state_home: Option<&Path>) -> PathBuf {
    if macos {
        home.join("Library").join("Logs").join("Axocoatl")
    } else {
        match xdg_state_home.filter(|path| path.is_absolute()) {
            Some(state) => state.join("axocoatl"),
            None => home.join(".local").join("state").join("axocoatl"),
        }
    }
}

/// A command onboarding may run, as the person is shown it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Cmd {
    pub(crate) program: String,
    pub(crate) args: Vec<String>,
    /// Variables set for this command only.
    pub(crate) env: Vec<(String, String)>,
    /// Bytes written to its standard input (its standard output is then
    /// discarded, as for `sudo tee FILE`).
    pub(crate) stdin: Option<Vec<u8>>,
    /// Shown instead of the argument list (a shell pipeline).
    pub(crate) shown: Option<String>,
    /// Asks for the person's password through `sudo`, itself or in a script
    /// it runs (Ollama's Linux installer).
    pub(crate) asks_for_password: bool,
}

impl Cmd {
    pub(crate) fn new(program: &str, args: &[&str]) -> Self {
        Self {
            program: program.to_string(),
            args: args.iter().map(|arg| arg.to_string()).collect(),
            env: Vec::new(),
            stdin: None,
            shown: None,
            asks_for_password: program == "sudo",
        }
    }

    /// `sudo program args…`.
    pub(crate) fn sudo(program: &str, args: &[&str]) -> Self {
        let mut all = vec![program];
        all.extend_from_slice(args);
        Self::new("sudo", &all)
    }

    /// `sh -c script`, shown as the script.
    pub(crate) fn shell(script: &str) -> Self {
        let mut command = Self::new("/bin/sh", &["-c", script]);
        command.shown = Some(script.to_string());
        command
    }

    pub(crate) fn env(mut self, name: &str, value: &str) -> Self {
        self.env.push((name.to_string(), value.to_string()));
        self
    }

    pub(crate) fn stdin(mut self, bytes: &[u8]) -> Self {
        self.stdin = Some(bytes.to_vec());
        self
    }

    /// A script that runs `sudo` itself.
    pub(crate) fn with_sudo_inside(mut self) -> Self {
        self.asks_for_password = true;
        self
    }

    /// Runs with `sudo` (or a script that does).
    pub(crate) fn uses_sudo(&self) -> bool {
        self.asks_for_password
    }

    /// The command as the person would type it.
    pub(crate) fn display(&self) -> String {
        if let Some(shown) = &self.shown {
            return shown.clone();
        }
        let mut words: Vec<String> = self
            .env
            .iter()
            .map(|(name, value)| format!("{name}={}", shell_quote(value)))
            .collect();
        words.push(self.program.clone());
        words.extend(self.args.iter().map(|arg| shell_quote(arg)));
        words.join(" ")
    }
}

/// `text` as one POSIX shell word.
pub(crate) fn shell_quote(text: &str) -> String {
    let plain = !text.is_empty()
        && text.chars().all(|c| {
            c.is_ascii_alphanumeric()
                || matches!(c, '-' | '_' | '.' | '/' | ':' | '=' | ',' | '@' | '+')
        });
    if plain {
        text.to_string()
    } else {
        format!("'{}'", text.replace('\'', "'\\''"))
    }
}

/// A read-only probe's result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Probe {
    pub(crate) success: bool,
    pub(crate) code: Option<i32>,
    pub(crate) stdout: String,
    pub(crate) stderr: String,
}

/// What onboarding runs programs with.
#[derive(Clone)]
pub(crate) struct Host {
    /// `PATH` programs are found on and run with.
    pub(crate) path: OsString,
    /// Searched after `path`: where installers put programs that a new
    /// shell may not have on PATH yet.
    pub(crate) extra_dirs: Vec<PathBuf>,
    pub(crate) home: PathBuf,
    /// `/`, or a test's directory standing in for it (`etc/os-release`,
    /// `etc/systemd/system`, `Applications`, `proc`).
    pub(crate) root: PathBuf,
    /// This `axocoatl` executable, for the steps that run it.
    pub(crate) exe: PathBuf,
    pub(crate) macos: bool,
    pub(crate) log: Option<Arc<Log>>,
    /// The Podman connection `CONTAINER_CONNECTION` or `CONTAINER_HOST`
    /// selects in this process's environment (a test's, given instead):
    /// what onboarding reads Podman through.
    pub(crate) podman_connection: axocoatl_isolation::podman::ConnectionOverride,
    /// The person's Axocoatl data directory (`~/.local/share/axocoatl`, the
    /// default data root), where onboarding keeps what it learned about
    /// this computer ([`super::id_map_attempt`]).
    pub(crate) data_home: PathBuf,
}

impl Host {
    /// The host of this process.
    pub(crate) fn from_env(log: Option<Arc<Log>>) -> Result<Self, String> {
        let home = std::env::var_os("HOME")
            .filter(|home| !home.is_empty())
            .map(PathBuf::from)
            .filter(|home| home.is_absolute())
            .ok_or_else(|| "HOME is not set to an absolute path".to_string())?;
        let macos = cfg!(target_os = "macos");
        let extra_dirs = if macos {
            [
                "/opt/homebrew/bin",
                "/usr/local/bin",
                "/opt/podman/bin",
                "/Applications/Ollama.app/Contents/Resources",
            ]
            .iter()
            .map(PathBuf::from)
            .collect()
        } else {
            vec![PathBuf::from("/usr/local/bin")]
        };
        Ok(Self {
            path: std::env::var_os("PATH").unwrap_or_default(),
            extra_dirs,
            root: PathBuf::from("/"),
            exe: std::env::current_exe()
                .map_err(|error| format!("cannot find this axocoatl executable: {error}"))?,
            macos,
            log,
            podman_connection: axocoatl_isolation::podman::ConnectionOverride::from_env(),
            data_home: crate::user_paths::UserPaths::discover()
                .map(|paths| paths.data_dir)
                .unwrap_or_else(|_| home.join(".local/share/axocoatl")),
            home,
        })
    }

    /// A path under [`Host::root`].
    pub(crate) fn at(&self, absolute: &str) -> PathBuf {
        self.root.join(absolute.trim_start_matches('/'))
    }

    /// The `PATH` commands run with: [`Host::path`], then the extra
    /// directories.
    pub(crate) fn run_path(&self) -> OsString {
        let mut directories: Vec<PathBuf> = std::env::split_paths(&self.path).collect();
        for extra in &self.extra_dirs {
            if !directories.contains(extra) {
                directories.push(extra.clone());
            }
        }
        std::env::join_paths(directories).unwrap_or_else(|_| self.path.clone())
    }

    /// `name` as an executable file on the run path.
    pub(crate) fn find(&self, name: &str) -> Option<PathBuf> {
        std::env::split_paths(&self.run_path())
            .filter(|directory| directory.is_absolute())
            .map(|directory| directory.join(name))
            .find(|candidate| is_executable(candidate))
    }

    /// `name` on [`Host::path`] alone: where `axocoatl dev`, the service and
    /// the daemon's own Podman calls find it.
    pub(crate) fn find_on_path(&self, name: &str) -> Option<PathBuf> {
        std::env::split_paths(&self.path)
            .filter(|directory| directory.is_absolute())
            .map(|directory| directory.join(name))
            .find(|candidate| is_executable(candidate))
    }

    pub(crate) fn log(&self, text: &str) {
        if let Some(log) = &self.log {
            log.line(text);
        }
    }

    fn command(&self, program: &str) -> tokio::process::Command {
        let resolved = if program.contains('/') {
            PathBuf::from(program)
        } else {
            self.find(program).unwrap_or_else(|| PathBuf::from(program))
        };
        let mut command = tokio::process::Command::new(resolved);
        command.env("PATH", self.run_path());
        command.env("HOME", &self.home);
        command
    }

    /// Run a read-only command and read what it prints. `None` when the
    /// program is missing, fails to start or does not finish in time.
    pub(crate) async fn probe(&self, program: &str, args: &[&str]) -> Option<Probe> {
        self.probe_env(program, args, &[]).await
    }

    pub(crate) async fn probe_env(
        &self,
        program: &str,
        args: &[&str],
        env: &[(&str, &str)],
    ) -> Option<Probe> {
        if !program.contains('/') && self.find(program).is_none() {
            return None;
        }
        let mut command = self.command(program);
        command
            .args(args)
            .stdin(std::process::Stdio::null())
            .kill_on_drop(true);
        for (name, value) in env {
            command.env(name, value);
        }
        let shown = Cmd::new(program, args).display();
        let output = match tokio::time::timeout(PROBE_TIMEOUT, command.output()).await {
            Ok(Ok(output)) => output,
            Ok(Err(error)) => {
                self.log(&format!("$ {shown}\n  could not run: {error}"));
                return None;
            }
            Err(_) => {
                self.log(&format!(
                    "$ {shown}\n  did not finish within {PROBE_TIMEOUT:?}"
                ));
                return None;
            }
        };
        let probe = Probe {
            success: output.status.success(),
            code: output.status.code(),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        };
        self.log(&format!(
            "$ {shown}\n  exit {}\n{}{}",
            probe
                .code
                .map(|code| code.to_string())
                .unwrap_or_else(|| "by signal".into()),
            indent(&truncate(&probe.stdout, 4000)),
            indent(&truncate(&probe.stderr, 4000)),
        ));
        Some(probe)
    }

    /// Run a command that changes the machine, its output on this terminal.
    pub(crate) async fn run(&self, cmd: &Cmd) -> Result<(), String> {
        self.log(&format!("$ {}", cmd.display()));
        let mut command = self.command(&cmd.program);
        command.args(&cmd.args);
        for (name, value) in &cmd.env {
            command.env(name, value);
        }
        if cmd.stdin.is_some() {
            command
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::null());
        }
        let mut child = command
            .spawn()
            .map_err(|error| format!("`{}` could not start: {error}", cmd.display()))?;
        if let (Some(bytes), Some(mut stdin)) = (&cmd.stdin, child.stdin.take()) {
            use tokio::io::AsyncWriteExt;
            stdin
                .write_all(bytes)
                .await
                .map_err(|error| format!("`{}`: {error}", cmd.display()))?;
            drop(stdin);
        }
        let status = child
            .wait()
            .await
            .map_err(|error| format!("`{}`: {error}", cmd.display()))?;
        self.log(&format!("  exit {}", exit_text(status.code())));
        if status.success() {
            Ok(())
        } else {
            Err(format!(
                "`{}` failed (exit {})",
                cmd.display(),
                exit_text(status.code())
            ))
        }
    }

    /// Run a command with this terminal and return its exit code (`None`
    /// when a signal ended it). Dropping the future (onboarding stopped by
    /// a signal) kills the command.
    pub(crate) async fn status(&self, cmd: &Cmd) -> Result<Option<i32>, String> {
        self.log(&format!("$ {}", cmd.display()));
        let mut command = self.command(&cmd.program);
        command.args(&cmd.args).kill_on_drop(true);
        for (name, value) in &cmd.env {
            command.env(name, value);
        }
        let status = command
            .status()
            .await
            .map_err(|error| format!("`{}` could not start: {error}", cmd.display()))?;
        self.log(&format!("  exit {}", exit_text(status.code())));
        Ok(status.code())
    }

    /// Run a command that changes the machine, showing its output here and
    /// keeping its last lines: the error a program such as `ollama pull`
    /// printed when it failed.
    pub(crate) async fn run_keeping_tail(&self, cmd: &Cmd) -> Result<(), (String, String)> {
        self.run_teeing(cmd, false).await.map(|_| ())
    }

    /// Run a command that changes the machine with this terminal as its
    /// input (an installer that asks for a password), showing its output
    /// here and returning the last 8 KiB of it: what the installer said
    /// about this computer (Ollama's: no GPU, so CPU only).
    pub(crate) async fn run_showing_output(&self, cmd: &Cmd) -> Result<String, (String, String)> {
        self.run_teeing(cmd, true).await
    }

    async fn run_teeing(
        &self,
        cmd: &Cmd,
        terminal_input: bool,
    ) -> Result<String, (String, String)> {
        use tokio::io::AsyncReadExt;
        self.log(&format!("$ {}", cmd.display()));
        let mut command = self.command(&cmd.program);
        command
            .args(&cmd.args)
            .stdin(if terminal_input {
                std::process::Stdio::inherit()
            } else {
                std::process::Stdio::null()
            })
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        for (name, value) in &cmd.env {
            command.env(name, value);
        }
        let mut child = command.spawn().map_err(|error| {
            (
                format!("`{}` could not start: {error}", cmd.display()),
                String::new(),
            )
        })?;
        let tail = Arc::new(Mutex::new(Vec::<u8>::new()));
        let mut readers = Vec::new();
        for (stream, to_stderr) in [
            (
                child
                    .stdout
                    .take()
                    .map(|s| Box::new(s) as Box<dyn tokio::io::AsyncRead + Unpin + Send>),
                false,
            ),
            (
                child
                    .stderr
                    .take()
                    .map(|s| Box::new(s) as Box<dyn tokio::io::AsyncRead + Unpin + Send>),
                true,
            ),
        ] {
            let Some(mut stream) = stream else { continue };
            let tail = tail.clone();
            readers.push(tokio::spawn(async move {
                let mut buffer = [0u8; 4096];
                while let Ok(read) = stream.read(&mut buffer).await {
                    if read == 0 {
                        break;
                    }
                    let chunk = &buffer[..read];
                    if to_stderr {
                        let _ = std::io::stderr().write_all(chunk);
                    } else {
                        let _ = std::io::stdout().write_all(chunk);
                    }
                    if let Ok(mut tail) = tail.lock() {
                        tail.extend_from_slice(chunk);
                        let excess = tail.len().saturating_sub(8192);
                        tail.drain(..excess);
                    }
                }
            }));
        }
        let status = child
            .wait()
            .await
            .map_err(|error| (format!("`{}`: {error}", cmd.display()), String::new()))?;
        for reader in readers {
            let _ = reader.await;
        }
        let tail = tail
            .lock()
            .map(|tail| String::from_utf8_lossy(&tail).into_owned())
            .unwrap_or_default();
        self.log(&format!(
            "  exit {}\n{}",
            exit_text(status.code()),
            indent(&last_lines(&tail, 5))
        ));
        if status.success() {
            Ok(tail)
        } else {
            Err((
                format!(
                    "`{}` failed (exit {})",
                    cmd.display(),
                    exit_text(status.code())
                ),
                last_error_line(&tail),
            ))
        }
    }
}

fn exit_text(code: Option<i32>) -> String {
    code.map(|code| code.to_string())
        .unwrap_or_else(|| "by signal".to_string())
}

fn indent(text: &str) -> String {
    text.lines()
        .map(|line| format!("  | {line}\n"))
        .collect::<String>()
}

fn truncate(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_string();
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &text[..end])
}

/// The last `count` lines of `text`, carriage-return progress collapsed.
pub(crate) fn last_lines(text: &str, count: usize) -> String {
    let lines: Vec<&str> = text
        .lines()
        .map(|line| line.rsplit('\r').next().unwrap_or(line).trim_end())
        .filter(|line| !line.is_empty())
        .collect();
    lines[lines.len().saturating_sub(count)..].join("\n")
}

/// The last line of `text` that says what failed: one starting with
/// `Error` (Ollama: `Error: pull model manifest: …`), else the last line.
pub(crate) fn last_error_line(text: &str) -> String {
    let lines = last_lines(text, 50);
    lines
        .lines()
        .rev()
        .find(|line| line.trim_start().to_ascii_lowercase().starts_with("error"))
        .or_else(|| lines.lines().last())
        .unwrap_or_default()
        .trim()
        .to_string()
}

/// Is `path` a regular file someone may execute?
pub(crate) fn is_executable(path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path)
            .map(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
    }
    #[cfg(not(unix))]
    {
        path.is_file()
    }
}

/// Free bytes on the file system that holds `path` (or its nearest
/// existing parent).
pub(crate) fn free_bytes(path: &Path) -> Option<u64> {
    let mut existing = path;
    while !existing.exists() {
        existing = existing.parent()?;
    }
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        let name = std::ffi::CString::new(existing.as_os_str().as_bytes()).ok()?;
        let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
        // SAFETY: `name` is a valid NUL-terminated path and `stat` a valid,
        // writable statvfs.
        if unsafe { libc::statvfs(name.as_ptr(), &mut stat) } != 0 {
            return None;
        }
        #[allow(clippy::unnecessary_cast)]
        Some(stat.f_bavail as u64 * stat.f_frsize as u64)
    }
    #[cfg(not(unix))]
    {
        let _ = existing;
        None
    }
}

/// A port nothing listens on at the IPv4 or IPv6 loopback address.
pub(crate) fn port_free(port: u16) -> bool {
    let v4 = std::net::TcpListener::bind(("127.0.0.1", port)).is_ok();
    let v6 = match std::net::TcpListener::bind(("::1", port)) {
        Ok(_) => true,
        Err(error) => error.kind() == std::io::ErrorKind::AddrNotAvailable,
    };
    v4 && v6
}

/// `port` when it is free, else the first free port after it (up to 100
/// further), else `port`.
pub(crate) fn first_free_port(port: u16, free: impl Fn(u16) -> bool) -> u16 {
    (port..=port.saturating_add(100))
        .find(|candidate| free(*candidate))
        .unwrap_or(port)
}

/// `YYYYMMDD-HHMMSS` in UTC, for log and backup names.
pub(crate) fn stamp() -> String {
    chrono::Utc::now().format("%Y%m%d-%H%M%S").to_string()
}

#[cfg(test)]
pub(crate) mod testing {
    //! Shim programs for onboarding's tests: each logs its arguments, one
    //! line per call, to `calls.log` in its directory, and does what its
    //! script says.

    use super::*;

    pub(crate) struct Shims {
        pub(crate) dir: tempfile::TempDir,
        pub(crate) home: tempfile::TempDir,
        pub(crate) root: tempfile::TempDir,
    }

    impl Shims {
        /// Shims whose root gives the tests' user, alex, the usual range
        /// of subordinate ids in `etc/subuid` and `etc/subgid`, with a
        /// setuid `newuidmap` and `newgidmap` on PATH.
        pub(crate) fn new() -> Self {
            let shims = Self {
                dir: tempfile::tempdir().unwrap(),
                home: tempfile::tempdir().unwrap(),
                root: tempfile::tempdir().unwrap(),
            };
            shims.subordinate_ids(Some("alex:100000:65536\n"));
            shims.id_map_tools(Some(0o4755));
            shims
        }

        /// Put `newuidmap` and `newgidmap` on PATH with `mode` (`0o4755`:
        /// setuid; `0o755`: they cannot map), or remove them. They are
        /// never run: onboarding only looks at them.
        pub(crate) fn id_map_tools(&self, mode: Option<u32>) {
            for name in ["newuidmap", "newgidmap"] {
                let path = self.dir.path().join(name);
                match mode {
                    #[cfg(unix)]
                    Some(mode) => {
                        use std::os::unix::fs::PermissionsExt;
                        std::fs::write(&path, "#!/bin/sh\nexit 1\n").unwrap();
                        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode))
                            .unwrap();
                    }
                    #[cfg(not(unix))]
                    Some(_) => std::fs::write(&path, "").unwrap(),
                    None => {
                        let _ = std::fs::remove_file(&path);
                    }
                }
            }
        }

        /// Write `etc/subuid` and `etc/subgid` under the root with `lines`,
        /// or remove both.
        pub(crate) fn subordinate_ids(&self, lines: Option<&str>) {
            let etc = self.root.path().join("etc");
            std::fs::create_dir_all(&etc).unwrap();
            for name in ["subuid", "subgid"] {
                match lines {
                    Some(lines) => std::fs::write(etc.join(name), lines).unwrap(),
                    None => {
                        let _ = std::fs::remove_file(etc.join(name));
                    }
                }
            }
        }

        /// Add a program `name` running `body` (POSIX sh) after logging
        /// `name args…`.
        pub(crate) fn add(&self, name: &str, body: &str) {
            self.write_program(
                name,
                &format!(
                    "#!/bin/sh\nPATH=\"$PATH:/usr/bin:/bin\"\nprintf '%s\\n' \"{name} $*\" >> \"{}\"\n{body}\n",
                    self.dir.path().join("calls.log").display()
                ),
            );
        }

        /// Write the program `name` (mode 755) with `text`, in a child
        /// process, so this multi-threaded test process never holds a
        /// descriptor open for writing it. On Linux a test on another
        /// thread that forks while one is open passes it to its child, and
        /// running the program before that child execs fails with ETXTBSY
        /// ("Text file busy"), which made these tests flaky.
        pub(crate) fn write_program(&self, name: &str, text: &str) {
            #[cfg(unix)]
            {
                use std::io::Write;
                let path = self.dir.path().join(name);
                let mut child = std::process::Command::new("/bin/sh")
                    .arg("-c")
                    .arg("cat > \"$1.new\" && chmod 755 \"$1.new\" && mv -f \"$1.new\" \"$1\"")
                    .arg("sh")
                    .arg(&path)
                    .stdin(std::process::Stdio::piped())
                    .spawn()
                    .unwrap();
                child
                    .stdin
                    .take()
                    .unwrap()
                    .write_all(text.as_bytes())
                    .unwrap();
                assert!(child.wait().unwrap().success(), "{}", path.display());
            }
            #[cfg(not(unix))]
            std::fs::write(self.dir.path().join(name), text).unwrap();
        }

        pub(crate) fn calls(&self) -> Vec<String> {
            std::fs::read_to_string(self.dir.path().join("calls.log"))
                .unwrap_or_default()
                .lines()
                .map(str::to_string)
                .collect()
        }

        /// A host whose PATH holds only the shims: a program without a
        /// shim is not installed. The shims themselves find `cat`, `sleep`
        /// and the like in the system directories. Neither
        /// `CONTAINER_HOST` nor `CONTAINER_CONNECTION` is read from the
        /// environment running the tests.
        pub(crate) fn host(&self, macos: bool) -> Host {
            Host {
                path: self.dir.path().as_os_str().to_owned(),
                extra_dirs: Vec::new(),
                home: self.home.path().to_path_buf(),
                root: self.root.path().to_path_buf(),
                exe: self.dir.path().join("axocoatl"),
                macos,
                log: None,
                podman_connection: axocoatl_isolation::podman::ConnectionOverride::None,
                data_home: self.home.path().join(".local/share/axocoatl"),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::testing::Shims;
    use super::*;

    #[test]
    fn commands_are_shown_as_typed() {
        assert_eq!(
            Cmd::new("podman", &["machine", "init", "--cpus", "6"]).display(),
            "podman machine init --cpus 6"
        );
        assert_eq!(
            Cmd::new("ollama", &["pull", "qwen3-coder:30b"])
                .env("OLLAMA_HOST", "http://127.0.0.1:11434")
                .display(),
            "OLLAMA_HOST=http://127.0.0.1:11434 ollama pull qwen3-coder:30b"
        );
        let sudo = Cmd::sudo("apt-get", &["install", "-y", "podman"]);
        assert_eq!(sudo.display(), "sudo apt-get install -y podman");
        assert!(sudo.uses_sudo());
        let script =
            Cmd::shell("curl -fsSL https://ollama.com/install.sh | OLLAMA_VERSION=0.40.6 sh");
        assert_eq!(
            script.display(),
            "curl -fsSL https://ollama.com/install.sh | OLLAMA_VERSION=0.40.6 sh"
        );
        assert!(!script.uses_sudo());
        assert!(script.with_sudo_inside().uses_sudo());
        assert_eq!(shell_quote("a b'c"), "'a b'\\''c'");
    }

    #[test]
    fn logs_go_where_each_platform_keeps_them() {
        let home = Path::new("/Users/alex");
        assert_eq!(
            log_directory(home, true, None),
            Path::new("/Users/alex/Library/Logs/Axocoatl")
        );
        assert_eq!(
            log_directory(Path::new("/home/alex"), false, None),
            Path::new("/home/alex/.local/state/axocoatl")
        );
        assert_eq!(
            log_directory(Path::new("/home/alex"), false, Some(Path::new("/state"))),
            Path::new("/state/axocoatl")
        );
    }

    #[test]
    fn the_first_free_port_from_8080() {
        assert_eq!(first_free_port(8080, |_| true), 8080);
        assert_eq!(first_free_port(8080, |port| port != 8080), 8081);
        assert_eq!(first_free_port(8080, |port| port > 8083), 8084);
        assert_eq!(first_free_port(8080, |_| false), 8080);
    }

    #[test]
    fn a_failed_command_keeps_the_line_that_says_why() {
        let tail =
            "pulling manifest\rpulling 1.0 GB\r\nError: pull model manifest: file does not exist\n";
        assert_eq!(
            last_error_line(tail),
            "Error: pull model manifest: file does not exist"
        );
        assert_eq!(last_error_line("only this\n"), "only this");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn commands_run_through_the_path_and_home_given_and_are_logged() {
        let shims = Shims::new();
        shims.add(
            "tool",
            "echo \"home=$HOME\"; echo 'Error: it broke' >&2; exit 3",
        );
        let log_dir = tempfile::tempdir().unwrap();
        let log = Arc::new(Log::create(log_dir.path(), "test").unwrap());
        let mut host = shims.host(true);
        host.log = Some(log.clone());
        let probe = host.probe("tool", &["--version"]).await.unwrap();
        assert!(!probe.success);
        assert_eq!(probe.code, Some(3));
        assert_eq!(
            probe.stdout.trim(),
            format!("home={}", shims.home.path().display())
        );
        let error = host.run(&Cmd::new("tool", &["install"])).await.unwrap_err();
        assert_eq!(error, "`tool install` failed (exit 3)");
        let (error, why) = host
            .run_keeping_tail(&Cmd::new("tool", &["pull", "x"]))
            .await
            .unwrap_err();
        assert_eq!(error, "`tool pull x` failed (exit 3)");
        assert_eq!(why, "Error: it broke");
        assert_eq!(
            shims.calls(),
            ["tool --version", "tool install", "tool pull x"]
        );
        let logged = std::fs::read_to_string(log.path()).unwrap();
        assert!(logged.contains("$ tool --version\n  exit 3"), "{logged}");
        assert!(logged.contains("$ tool install\n  exit 3"), "{logged}");
        assert!(host.probe("missing-tool", &[]).await.is_none());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn stdin_reaches_a_command_like_sudo_tee() {
        let shims = Shims::new();
        let target = shims.root.path().join("drop-in.conf");
        shims.add("tee", &format!("cat > \"{}\"", target.display()));
        let host = shims.host(false);
        host.run(&Cmd::new("tee", &["x"]).stdin(b"[Service]\n"))
            .await
            .unwrap();
        assert_eq!(std::fs::read_to_string(target).unwrap(), "[Service]\n");
    }
}
