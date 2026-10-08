//! The supervisor's read-only helper launch (`--serve --harden --helper
//! UID:GID --writer UID:GID --workspace PATH`) through the real binary: the
//! helper reads a Workspace only its writer may enter, cannot write it, and
//! reads nothing else it could not read as an ordinary other user.
//!
//! The launch needs root with `CAP_SETUID`, `CAP_SETGID`, `CAP_SETPCAP` and
//! `CAP_DAC_READ_SEARCH`, and Landlock, so these tests run in a Linux
//! container started with `--cap-add DAC_READ_SEARCH` (root's other
//! capabilities are Podman's defaults); elsewhere each says why it is
//! skipped. The Podman tests in `axocoatl-isolation` (`workload_podman`)
//! check the same properties in a hardened Session container.
#![cfg(target_os = "linux")]

use axocoatl_exec::protocol::{
    read_frame, Control, ExecRequest, HelperView, ProcessOutcome, ServerMessage, WriteRestriction,
    MAX_RESPONSE_BYTES, PROTOCOL_VERSION,
};
use std::collections::BTreeSet;
use std::io::{BufReader, Write};
use std::os::unix::fs::{chown, PermissionsExt};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

const WRITER: (u32, u32) = (2000, 2000);
const HELPER: (u32, u32) = (2001, 2001);
const CAP_DAC_READ_SEARCH: u32 = 2;

/// Why this process cannot launch a helper, or `None` when it can.
fn unable() -> Option<String> {
    // SAFETY: geteuid has no arguments.
    if unsafe { libc::geteuid() } != 0 {
        return Some("not root".into());
    }
    let status = std::fs::read_to_string("/proc/self/status").unwrap();
    let permitted = status
        .lines()
        .find_map(|line| line.strip_prefix("CapPrm:"))
        .map(|value| u64::from_str_radix(value.trim(), 16).unwrap())
        .unwrap();
    for (cap, name) in [
        (CAP_DAC_READ_SEARCH, "CAP_DAC_READ_SEARCH"),
        (6, "CAP_SETGID"),
        (7, "CAP_SETUID"),
        (8, "CAP_SETPCAP"),
    ] {
        if permitted & (1 << cap) == 0 {
            return Some(format!("no {name}"));
        }
    }
    // SAFETY: querying the Landlock ABI takes no attribute pointer.
    let abi = unsafe { libc::syscall(444, std::ptr::null::<u8>(), 0usize, 1u32) };
    if abi < 1 {
        return Some("no Landlock".into());
    }
    None
}

macro_rules! require_launch {
    () => {
        if let Some(reason) = unable() {
            eprintln!(
                "skipped: a helper launch needs root with its capabilities and Landlock ({reason})"
            );
            return;
        }
    };
}

fn landlock_abi() -> i64 {
    // SAFETY: querying the Landlock ABI takes no attribute pointer.
    unsafe { libc::syscall(444, std::ptr::null::<u8>(), 0usize, 1u32) }
}

struct Ran {
    outcome: ProcessOutcome,
    stdout: String,
    stderr: String,
}

impl Ran {
    fn lines(&self) -> BTreeSet<String> {
        self.stdout.lines().map(str::to_owned).collect()
    }

    fn exited(&self) -> &Self {
        assert_eq!(
            self.outcome,
            ProcessOutcome::Exited { code: 0 },
            "{}\n{}",
            self.stdout,
            self.stderr
        );
        self
    }
}

/// Run `script` through the supervisor as the helper of `workspace`, from
/// inside it, as repository tools do.
fn helper(
    workspace: &Path,
    script: &str,
    args: &[&str],
    restriction: Option<WriteRestriction>,
) -> Ran {
    helper_in(workspace, workspace, script, args, restriction)
}

/// [`helper`] from the directory `cwd`.
fn helper_in(
    cwd: &Path,
    workspace: &Path,
    script: &str,
    args: &[&str],
    restriction: Option<WriteRestriction>,
) -> Ran {
    let view = HelperView {
        helper: HELPER,
        writer: WRITER,
        workspace: workspace.to_string_lossy().into_owned(),
    };
    let mut argv = vec!["sh".to_string(), "-c".into(), script.into(), "sh".into()];
    argv.extend(args.iter().map(|arg| (*arg).to_string()));
    let request = ExecRequest {
        protocol: PROTOCOL_VERSION,
        invocation_id: format!("helper-view-{}", std::process::id()),
        argv,
        stdin: None,
        timeout_ms: 60_000,
        stdout_bytes: 512 * 1024,
        stderr_bytes: 64 * 1024,
        write_restriction: restriction,
    };
    let mut process = Command::new(env!("CARGO_BIN_EXE_axocoatl-exec-supervisor"))
        .args(["--serve", "--harden"])
        .args(view.args())
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap();
    let mut input = process.stdin.take().unwrap();
    let mut output = BufReader::new(process.stdout.take().unwrap());
    let mut send = |bytes: Vec<u8>| {
        input.write_all(&bytes).unwrap();
        input.write_all(b"\n").unwrap();
        input.flush().unwrap();
    };
    send(serde_json::to_vec(&request).unwrap());
    let mut next = || -> ServerMessage {
        let frame = read_frame(&mut output, MAX_RESPONSE_BYTES)
            .unwrap()
            .expect("a supervisor message");
        let message: ServerMessage = serde_json::from_slice(&frame).unwrap();
        message.validate_for(&request).unwrap();
        message
    };
    assert!(matches!(next(), ServerMessage::Ready { .. }));
    send(serde_json::to_vec(&Control::Dispatch).unwrap());
    let ServerMessage::Finished {
        outcome,
        stdout,
        stderr,
        ..
    } = next()
    else {
        panic!("no terminal message");
    };
    drop(input);
    let _ = process.wait();
    let text = |captured: &axocoatl_exec::protocol::CapturedOutput, limit| {
        String::from_utf8_lossy(&captured.retained_bytes(limit).unwrap()).into_owned()
    };
    Ran {
        outcome,
        stdout: text(&stdout, request.stdout_bytes),
        stderr: text(&stderr, request.stderr_bytes),
    }
}

/// A read-only helper's shell restriction, as the daemon builds it.
fn shell_restriction(workspace: &Path) -> WriteRestriction {
    WriteRestriction {
        writable: vec!["/tmp".into(), "/var/tmp".into(), "/dev".into()],
        protected: vec![workspace.to_string_lossy().into_owned()],
        deny_network: true,
    }
}

/// A private Workspace as `mkdtemp` and `umask 077` make one, owned by the
/// writer, beneath a directory only root may enter; and the writer's
/// private files outside it (a home directory and a file in `/tmp`).
struct Fixture {
    _base: tempfile::TempDir,
    workspace: PathBuf,
    home_file: PathBuf,
    tmp_file: tempfile::NamedTempFile,
}

fn write_private(path: &Path, text: &str, owner: (u32, u32), mode: u32) {
    std::fs::write(path, text).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
    chown(path, Some(owner.0), Some(owner.1)).unwrap();
}

fn private_dir(path: &Path, owner: (u32, u32)) {
    std::fs::create_dir_all(path).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
    chown(path, Some(owner.0), Some(owner.1)).unwrap();
}

impl Fixture {
    fn new() -> Self {
        let base = tempfile::Builder::new()
            .prefix("axo-helper-view-")
            .tempdir()
            .unwrap();
        let workspace = base.path().join("repo");
        private_dir(&workspace, WRITER);
        write_private(
            &workspace.join("README.md"),
            "workspace-readme\n",
            WRITER,
            0o600,
        );
        private_dir(&workspace.join("src"), WRITER);
        write_private(
            &workspace.join("src/lib.rs"),
            "workspace-source\n",
            WRITER,
            0o600,
        );
        write_private(
            &workspace.join("run.sh"),
            "echo workspace-script\n",
            WRITER,
            0o700,
        );
        let home = base.path().join("home");
        private_dir(&home, WRITER);
        let home_file = home.join("token");
        write_private(&home_file, "writer-home-token\n", WRITER, 0o600);
        let tmp_file = tempfile::Builder::new()
            .prefix("axo-helper-view-writer-")
            .tempfile_in("/tmp")
            .unwrap();
        write_private(tmp_file.path(), "writer-tmp-token\n", WRITER, 0o600);
        Self {
            _base: base,
            workspace,
            home_file,
            tmp_file,
        }
    }
}

/// `cat`s each of `$@`, printing `path=read` or `path=denied`.
const READS: &str = "for p in \"$@\"; do if cat -- \"$p\" >/dev/null 2>&1; then echo \"$p=read\"; \
                     else echo \"$p=denied\"; fi; done";

#[test]
fn a_helper_reads_a_private_workspace_and_no_private_file_outside_it() {
    require_launch!();
    let fixture = Fixture::new();
    let ws = |name: &str| fixture.workspace.join(name).to_string_lossy().into_owned();
    let home = fixture.home_file.to_string_lossy().into_owned();
    let tmp = fixture.tmp_file.path().to_string_lossy().into_owned();
    let ran = helper(
        &fixture.workspace,
        &format!(
            "id -u; id -G; {READS}; ls -a . src | tr '\\n' ' '; echo; sh run.sh; \
             [ -r README.md ] && [ -x src ] && echo access=read; [ -w README.md ] || echo access=no-write; \
             ./run.sh 2>/dev/null || echo owner-only-execute=refused; \
             (echo nope > README.md) 2>/dev/null || echo write=refused; \
             cat src/lib.rs; echo HOME=$HOME TMPDIR=$TMPDIR"
        ),
        &[
            &ws("README.md"),
            &ws("src/lib.rs"),
            &home,
            &tmp,
            "/etc/shadow",
            "/etc/passwd",
            "/proc/self/status",
            "/proc/1/status",
            "/proc/cpuinfo",
        ],
        None,
    );
    ran.exited();
    let lines: Vec<&str> = ran.stdout.lines().collect();
    assert_eq!(lines[0], HELPER.0.to_string(), "{}", ran.stdout);
    assert_eq!(lines[1], HELPER.1.to_string(), "{}", ran.stdout);
    let got = ran.lines();
    for expected in [
        format!("{}=read", ws("README.md")),
        format!("{}=read", ws("src/lib.rs")),
        format!("{home}=denied"),
        format!("{tmp}=denied"),
        "/etc/passwd=read".into(),
        "/proc/self/status=denied".into(),
        "/proc/1/status=denied".into(),
        "/proc/cpuinfo=read".into(),
        "workspace-script".into(),
        "access=read".into(),
        "access=no-write".into(),
        "owner-only-execute=refused".into(),
        "workspace-source".into(),
        "write=refused".into(),
    ] {
        assert!(
            got.contains(&expected),
            "{expected}: {}\n{}",
            ran.stdout,
            ran.stderr
        );
    }
    if Path::new("/etc/shadow").exists() {
        assert!(got.contains("/etc/shadow=denied"), "{}", ran.stdout);
    }
    assert!(
        ran.stdout.contains(". .. README.md run.sh src"),
        "{}",
        ran.stdout
    );
    assert!(ran.stdout.contains("lib.rs"), "{}", ran.stdout);
    let scratch = lines
        .last()
        .and_then(|line| line.strip_prefix("HOME="))
        .and_then(|rest| rest.split_once(" TMPDIR="))
        .unwrap();
    assert_eq!(scratch.0, scratch.1);
    assert!(
        scratch.0.starts_with("/tmp/axocoatl-helper."),
        "{scratch:?}"
    );
    // Removed when the command ends.
    assert!(!Path::new(scratch.0).exists(), "{scratch:?}");
    assert_eq!(
        std::fs::read_to_string(fixture.workspace.join("README.md")).unwrap(),
        "workspace-readme\n"
    );
}

/// Git finds and reads a repository only its owner may enter: it asks
/// `access` about the repository's directories, which answers as an `open`
/// would for the helper.
#[test]
fn a_helper_runs_git_in_a_private_repository() {
    require_launch!();
    if Command::new("git").arg("--version").output().is_err() {
        eprintln!("skipped: git is not in this image");
        return;
    }
    let fixture = Fixture::new();
    let git = |args: &[&str]| {
        let status = Command::new("git")
            .args([
                "-c",
                "user.name=Writer",
                "-c",
                "user.email=writer@example.invalid",
            ])
            .args(args)
            .current_dir(&fixture.workspace)
            .env("HOME", &fixture.workspace)
            .uid(WRITER.0)
            .gid(WRITER.1)
            .pre_exec_umask()
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?}");
    };
    git(&["init", "-q", "."]);
    git(&["add", "README.md", "src/lib.rs"]);
    git(&["commit", "-qm", "Add the fixture"]);
    let ran = helper(
        &fixture.workspace,
        "git -c safe.directory='*' log --format=%s; git -c safe.directory='*' status --short --untracked-files=no; \
         git -c safe.directory='*' show HEAD:src/lib.rs",
        &[],
        None,
    );
    ran.exited();
    assert_eq!(
        ran.stdout.lines().collect::<Vec<_>>(),
        ["Add the fixture", "workspace-source"],
        "{}",
        ran.stderr
    );
}

/// `umask 077` for a command the test starts, as a private checkout's.
trait PreExecUmask {
    fn pre_exec_umask(&mut self) -> &mut Self;
}

impl PreExecUmask for Command {
    fn pre_exec_umask(&mut self) -> &mut Self {
        // SAFETY: umask is async-signal-safe and allocates nothing.
        unsafe {
            self.pre_exec(|| {
                libc::umask(0o077);
                Ok(())
            })
        }
    }
}

/// What the helper reads with its view is what an ordinary other user reads
/// (the same command run as the helper's user without the supervisor), plus
/// the Workspace: across the configuration, the writer's and root's private
/// directories, `/tmp`, `/run` and every running process's `/proc` entries.
#[test]
fn a_helper_reads_nothing_outside_the_workspace_that_another_user_could_not() {
    require_launch!();
    let fixture = Fixture::new();
    // A running writer process, with a marker in its environment.
    let mut writer = Command::new("sleep")
        .arg("30")
        .env("AXO_WRITER_SECRET", "writer-environment")
        .uid(WRITER.0)
        .gid(WRITER.1)
        .spawn()
        .unwrap();
    let mut candidates = Vec::new();
    let mut dirs = Vec::new();
    let mut roots: Vec<PathBuf> = [
        "/etc",
        "/root",
        "/home",
        "/tmp",
        "/var",
        "/run",
        "/dev/shm",
        "/proc/1",
        "/proc/sys",
        "/opt",
        "/srv",
        "/mnt",
    ]
    .iter()
    .map(PathBuf::from)
    .collect();
    roots.push(PathBuf::from(format!("/proc/{}", writer.id())));
    fn collect(path: &Path, depth: usize, files: &mut Vec<String>, dirs: &mut Vec<String>) {
        let Ok(metadata) = std::fs::symlink_metadata(path) else {
            return;
        };
        let Some(text) = path.to_str().filter(|text| !text.contains('\n')) else {
            return;
        };
        if metadata.is_dir() {
            dirs.push(text.to_owned());
            if depth > 6 || text.starts_with("/proc/sys/net") && depth > 3 {
                return;
            }
            if let Ok(entries) = std::fs::read_dir(path) {
                for entry in entries.flatten() {
                    collect(&entry.path(), depth + 1, files, dirs);
                }
            }
        } else if metadata.is_file() {
            files.push(text.to_owned());
        }
    }
    for root in &roots {
        collect(root, 0, &mut candidates, &mut dirs);
    }
    let list = fixture.workspace.join("candidates");
    let dir_list = fixture.workspace.join("dirs");
    std::fs::write(&list, candidates.join("\n") + "\n").unwrap();
    std::fs::write(&dir_list, dirs.join("\n") + "\n").unwrap();
    for path in [&list, &dir_list] {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o644)).unwrap();
    }
    let probe =
        "while IFS= read -r p; do head -c 1 -- \"$p\" >/dev/null 2>&1 && echo \"read $p\"; \
                 done < \"$1\"; while IFS= read -r p; do ls -- \"$p\" >/dev/null 2>&1 && \
                 echo \"list $p\"; done < \"$2\"; true";
    let with_view = helper(
        &fixture.workspace,
        probe,
        &[list.to_str().unwrap(), dir_list.to_str().unwrap()],
        None,
    );
    with_view.exited();
    // Today's helper: the same user and group, no capability, no view; the
    // lists are copied where it may read them.
    let open = tempfile::tempdir().unwrap();
    std::fs::set_permissions(open.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
    for (from, name) in [(&list, "candidates"), (&dir_list, "dirs")] {
        std::fs::copy(from, open.path().join(name)).unwrap();
    }
    let plain = Command::new("sh")
        .args(["-c", probe, "sh"])
        .arg(open.path().join("candidates"))
        .arg(open.path().join("dirs"))
        .uid(HELPER.0)
        .gid(HELPER.1)
        .current_dir("/")
        .output()
        .unwrap();
    let _ = writer.kill();
    let _ = writer.wait();
    let plain: BTreeSet<String> = String::from_utf8_lossy(&plain.stdout)
        .lines()
        .map(str::to_owned)
        .collect();
    let with_lines = with_view.lines();
    let workspace = fixture.workspace.to_string_lossy().into_owned();
    let in_workspace = |line: &str| {
        line.split_once(' ').is_some_and(|(_, path)| {
            path == workspace || path.starts_with(&format!("{workspace}/"))
        })
    };
    let gained: Vec<&String> = with_lines
        .iter()
        .filter(|line| !plain.contains(*line) && !in_workspace(line))
        .collect();
    // The Workspace is what it gains: the writer's private files in it.
    for line in [
        format!("list {workspace}"),
        format!("list {workspace}/src"),
        format!("read {workspace}/src/lib.rs"),
    ] {
        assert!(with_lines.contains(&line), "{line}: {}", with_view.stdout);
        assert!(!plain.contains(&line), "{line}");
    }
    assert!(
        gained.is_empty(),
        "read with the view but not by another user: {gained:?}"
    );
    // The view reads less than an ordinary user, never more: still the
    // configuration, never a process's /proc entries.
    assert!(
        with_view.lines().contains("read /etc/passwd"),
        "{}",
        with_view.stdout
    );
    let writer_proc = format!("/proc/{}/", writer.id());
    assert!(
        !with_view.stdout.contains(&writer_proc),
        "{}",
        with_view.stdout
    );
    assert!(
        plain.contains(&format!("read /proc/{}/cmdline", writer.id())),
        "{plain:?}"
    );
}

/// The shell's restriction still holds under the view: no write beneath the
/// Workspace, nothing read back from the shared `/tmp`, its own scratch
/// directory for both, and no TCP.
#[test]
fn a_helper_shell_cannot_write_the_workspace_and_keeps_to_its_scratch_directory() {
    require_launch!();
    let fixture = Fixture::new();
    let ran = helper(
        &fixture.workspace,
        "(echo nope >> README.md) 2>/dev/null || echo append=refused; \
         (echo new > new.txt) 2>/dev/null || echo create=refused; \
         echo mine > \"$TMPDIR/mine\" && cat \"$TMPDIR/mine\"; \
         echo shared > /var/tmp/axo-helper-view-shared-$$ && echo shared-write=0; \
         cat /var/tmp/axo-helper-view-shared-$$ >/dev/null 2>&1 || echo shared-read=refused",
        &[],
        Some(shell_restriction(&fixture.workspace)),
    );
    if landlock_abi() < 4 {
        let ProcessOutcome::LaunchFailed { message } = &ran.outcome else {
            panic!("{:?}", ran.outcome);
        };
        assert!(
            message.starts_with("write restriction unavailable"),
            "{message}"
        );
        return;
    }
    ran.exited();
    let got = ran.lines();
    for expected in [
        "append=refused",
        "create=refused",
        "mine",
        "shared-write=0",
        "shared-read=refused",
    ] {
        assert!(
            got.contains(expected),
            "{expected}: {}\n{}",
            ran.stdout,
            ran.stderr
        );
    }
    assert!(!fixture.workspace.join("new.txt").exists());
    for entry in std::fs::read_dir("/var/tmp").unwrap().flatten() {
        if entry
            .file_name()
            .to_string_lossy()
            .starts_with("axo-helper-view-shared-")
        {
            let _ = std::fs::remove_file(entry.path());
        }
    }
    assert_eq!(
        std::fs::read_to_string(fixture.workspace.join("README.md")).unwrap(),
        "workspace-readme\n"
    );
}

/// The command holds `CAP_DAC_READ_SEARCH` and nothing else, in every set
/// and its bounding set, with no new privileges and the helper's seccomp
/// filter; a setuid program gains nothing.
#[test]
fn a_helper_holds_only_dac_read_search_and_cannot_gain_more() {
    require_launch!();
    let fixture = Fixture::new();
    // Observed from outside: the command cannot read its own /proc entries.
    // A directory anyone may write, where the command leaves its pid.
    let drop = tempfile::tempdir().unwrap();
    std::fs::set_permissions(drop.path(), std::fs::Permissions::from_mode(0o777)).unwrap();
    let pid_file = drop.path().join("pid");
    let script = format!(
        "sleep 30 & pid=$!; echo $pid > {}; wait $pid",
        pid_file.display()
    );
    let observer = std::thread::spawn(move || {
        for _ in 0..200 {
            if let Ok(pid) = std::fs::read_to_string(&pid_file) {
                let pid = pid.trim().to_string();
                if let Ok(text) = std::fs::read_to_string(format!("/proc/{pid}/status")) {
                    // SAFETY: a plain kill of the observed process.
                    unsafe { libc::kill(pid.parse().unwrap(), libc::SIGKILL) };
                    return text;
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        panic!("no helper process to observe");
    });
    let _ = helper(&fixture.workspace, &script, &[], None);
    let text = observer.join().unwrap();
    let field = |name: &str| {
        text.lines()
            .find_map(|line| line.strip_prefix(&format!("{name}:")))
            .map(|value| value.trim().to_string())
            .unwrap_or_else(|| panic!("no {name}: {text}"))
    };
    let only = format!("{:016x}", 1u64 << CAP_DAC_READ_SEARCH);
    for set in ["CapInh", "CapPrm", "CapEff", "CapBnd", "CapAmb"] {
        assert_eq!(field(set), only, "{set}: {text}");
    }
    assert_eq!(field("NoNewPrivs"), "1", "{text}");
    assert_eq!(field("Seccomp"), "2", "{text}");
    assert!(
        field("Uid")
            .split_whitespace()
            .all(|id| id == HELPER.0.to_string()),
        "{text}"
    );
    assert!(
        field("Gid")
            .split_whitespace()
            .all(|id| id == HELPER.1.to_string()),
        "{text}"
    );
    assert_eq!(field("Groups"), "", "{text}");
}

/// The capability lets the helper pass directories it could not enter
/// before, which Landlock does not cover for Unix sockets or watches; its
/// filter refuses both, while stream socket pairs (child processes' pipes)
/// still work.
#[test]
fn a_helper_cannot_open_unix_sockets_or_watch_paths() {
    require_launch!();
    if Command::new("python3").arg("-V").output().is_err() {
        eprintln!("skipped: python3 is not in this image");
        return;
    }
    let fixture = Fixture::new();
    let script = "import socket, ctypes, os\n\
        def attempt(name, call):\n\
        \x20   try:\n\
        \x20       call(); print(name + '=ok')\n\
        \x20   except OSError as error:\n\
        \x20       print(name + '=errno-' + str(error.errno))\n\
        attempt('unix', lambda: socket.socket(socket.AF_UNIX, socket.SOCK_STREAM))\n\
        attempt('unix_dgram', lambda: socket.socket(socket.AF_UNIX, socket.SOCK_DGRAM))\n\
        attempt('pair_stream', lambda: socket.socketpair(socket.AF_UNIX, socket.SOCK_STREAM))\n\
        attempt('pair_dgram', lambda: socket.socketpair(socket.AF_UNIX, socket.SOCK_DGRAM))\n\
        libc = ctypes.CDLL(None, use_errno=True)\n\
        def watch():\n\
        \x20   fd = libc.inotify_init1(0)\n\
        \x20   if fd < 0 or libc.inotify_add_watch(fd, b'.', 0x100) < 0:\n\
        \x20       raise OSError(ctypes.get_errno(), 'inotify')\n\
        attempt('inotify', watch)\n";
    let ran = helper(&fixture.workspace, "python3 -c \"$1\"", &[script], None);
    ran.exited();
    let got = ran.lines();
    let eperm = format!("errno-{}", libc::EPERM);
    for expected in [
        format!("unix={eperm}"),
        format!("unix_dgram={eperm}"),
        "pair_stream=ok".into(),
        format!("pair_dgram={eperm}"),
        format!("inotify={eperm}"),
    ] {
        assert!(
            got.contains(&expected),
            "{expected}: {}\n{}",
            ran.stdout,
            ran.stderr
        );
    }
}

/// Reading a writer process's environment needs ptrace access, which the
/// helper's user and domain do not have, capability or not; neither can it
/// signal the process.
#[test]
fn a_helper_cannot_read_or_signal_a_writer_process() {
    require_launch!();
    let fixture = Fixture::new();
    let mut writer = Command::new("sleep")
        .arg("30")
        .env("AXO_WRITER_SECRET", "writer-environment")
        .uid(WRITER.0)
        .gid(WRITER.1)
        .spawn()
        .unwrap();
    let pid = writer.id().to_string();
    let ran = helper(
        &fixture.workspace,
        "for f in environ maps mem cmdline status; do cat /proc/$1/$f >/dev/null 2>&1; \
         echo $f=$?; done; kill -0 $1 2>/dev/null; echo kill=$?",
        &[&pid],
        None,
    );
    let alive = writer.try_wait().unwrap().is_none();
    let _ = writer.kill();
    let _ = writer.wait();
    ran.exited();
    for name in ["environ", "maps", "mem", "cmdline", "status", "kill"] {
        assert!(
            ran.lines().contains(&format!("{name}=1")),
            "{name}: {}",
            ran.stdout
        );
    }
    assert!(!ran.stdout.contains("writer-environment"));
    assert!(alive);
}

/// Without its Workspace the launch is refused and says why; the command
/// never runs in another way instead.
#[test]
fn a_helper_launch_without_its_workspace_is_refused() {
    require_launch!();
    let base = tempfile::tempdir().unwrap();
    let missing = base.path().join("absent");
    let ran = helper_in(Path::new("/"), &missing, "id -u", &[], None);
    let ProcessOutcome::LaunchFailed { message } = &ran.outcome else {
        panic!("{:?}: {}", ran.outcome, ran.stdout);
    };
    assert!(
        message.starts_with("helper view unavailable: the Workspace")
            && message.contains("does not exist"),
        "{message}"
    );
}
