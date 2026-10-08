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
             cat src/lib.rs; echo HOME=$HOME TMPDIR=${{TMPDIR-unset}}"
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
    // The file tools write nothing, so they get no scratch directory.
    assert_eq!(
        lines.last(),
        Some(&"HOME=/nonexistent TMPDIR=unset"),
        "{}",
        ran.stdout
    );
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

/// Attempts each write of `$@` (`kind:path`, see [`WRITES`]) and prints
/// `kind:path=ok` or `kind:path=errno-N`.
const WRITES: &str = r#"
import os, sys
def attempt(spec, call):
    try:
        call()
        print(spec + "=ok")
    except OSError as error:
        print(spec + "=errno-" + str(error.errno))
def append(path):
    with open(path, "a") as f:
        f.write("x")
def create(path):
    fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o644)
    os.write(fd, b"x")
    os.close(fd)
def overwrite(path):
    with open(path, "r+") as f:
        f.write("x")
def clobber(path):
    # As a shell's `>` does: O_TRUNC on an existing file.
    os.close(os.open(path, os.O_WRONLY | os.O_TRUNC))
def device(path):
    with open(path, "wb", buffering=0) as f:
        f.write(b"x")
calls = {
    "append": append,
    "create": create,
    "overwrite": overwrite,
    "clobber": clobber,
    "truncate": lambda p: os.truncate(p, 0),
    "remove": os.unlink,
    "mkdir": os.mkdir,
    "rmdir": os.rmdir,
    "rename": lambda p: os.rename(p, p + ".renamed"),
    "link": lambda p: os.link(p, p + ".link"),
    "symlink": lambda p: os.symlink("target", p),
    "fifo": os.mkfifo,
    "chmod": lambda p: os.chmod(p, 0o600),
    "utime": lambda p: os.utime(p, None),
    "xattr": lambda p: os.setxattr(p, "user.axocoatl", b"1"),
    "device": device,
}
for spec in sys.argv[1:]:
    kind, path = spec.split(":", 1)
    attempt(spec, lambda: calls[kind](path))
"#;

/// A Workspace with things any user may change (a `0666` file, a `0777`
/// directory with a file and an empty directory in it), and a directory any
/// user may change inside the writer's private home: what the helper's
/// capability lets it reach.
fn open_targets(fixture: &Fixture) -> (PathBuf, PathBuf, PathBuf) {
    let open_file = fixture.workspace.join("open.txt");
    write_private(&open_file, "open-file\n", WRITER, 0o666);
    let open_dir = fixture.workspace.join("open-dir");
    std::fs::create_dir(&open_dir).unwrap();
    std::fs::create_dir(open_dir.join("empty")).unwrap();
    write_private(&open_dir.join("victim"), "victim\n", WRITER, 0o666);
    let home_dir = fixture.home_file.parent().unwrap().join("open-sub");
    std::fs::create_dir(&home_dir).unwrap();
    for dir in [&open_dir, &open_dir.join("empty"), &home_dir] {
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o777)).unwrap();
        chown(dir, Some(WRITER.0), Some(WRITER.1)).unwrap();
    }
    (open_file, open_dir, home_dir)
}

/// Each write a helper must not make outside its own scratch directory:
/// in the Workspace (where the file modes would allow it), in a directory
/// any user may change inside the writer's private home, in the shared
/// temporary directories and in `/dev`. Setting a file's times to now, which
/// its modes allow any user for a `0666` file and Landlock does not cover,
/// only the file tools are refused (`tools`).
fn forbidden_writes(fixture: &Fixture, tag: &str, tools: bool) -> Vec<String> {
    let (open_file, open_dir, home_dir) = open_targets(fixture);
    let file = open_file.to_string_lossy().into_owned();
    let dir = open_dir.to_string_lossy().into_owned();
    let home = home_dir.to_string_lossy().into_owned();
    let readme = fixture
        .workspace
        .join("README.md")
        .to_string_lossy()
        .into_owned();
    let mut writes: Vec<String> = [
        "append",
        "overwrite",
        "clobber",
        "truncate",
        "chmod",
        "xattr",
    ]
    .iter()
    .map(|kind| format!("{kind}:{file}"))
    .collect();
    if tools {
        writes.push(format!("utime:{file}"));
    }
    writes.extend([
        format!("append:{readme}"),
        format!("create:{}/new.txt", fixture.workspace.display()),
        format!("create:{dir}/new.txt"),
        format!("remove:{dir}/victim"),
        format!("rename:{dir}/victim"),
        format!("link:{dir}/victim"),
        format!("symlink:{dir}/symlink"),
        format!("fifo:{dir}/fifo"),
        format!("mkdir:{dir}/made"),
        format!("rmdir:{dir}/empty"),
        format!("create:{home}/new.txt"),
        format!("mkdir:{home}/made"),
        format!("create:/tmp/axo-helper-view-{tag}"),
        format!("mkdir:/tmp/axo-helper-view-dir-{tag}"),
        format!("create:/var/tmp/axo-helper-view-{tag}"),
        format!("create:/dev/shm/axo-helper-view-{tag}"),
        format!("create:/dev/axo-helper-view-{tag}"),
        format!("fifo:/tmp/axo-helper-view-fifo-{tag}"),
        "device:/dev/full".into(),
    ]);
    writes
}

/// After a helper's attempts: nothing it may not write has changed.
fn assert_unchanged(fixture: &Fixture, tag: &str) {
    let workspace = &fixture.workspace;
    assert_eq!(
        std::fs::read_to_string(workspace.join("README.md")).unwrap(),
        "workspace-readme\n"
    );
    let open = std::fs::metadata(workspace.join("open.txt")).unwrap();
    assert_eq!(
        std::fs::read_to_string(workspace.join("open.txt")).unwrap(),
        "open-file\n"
    );
    assert_eq!(open.permissions().mode() & 0o7777, 0o666);
    let mut names: Vec<String> = std::fs::read_dir(workspace.join("open-dir"))
        .unwrap()
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    assert_eq!(names, ["empty", "victim"]);
    assert!(!workspace.join("new.txt").exists());
    let home = fixture.home_file.parent().unwrap().join("open-sub");
    assert_eq!(std::fs::read_dir(home).unwrap().count(), 0);
    for leftover in [
        format!("/tmp/axo-helper-view-{tag}"),
        format!("/tmp/axo-helper-view-dir-{tag}"),
        format!("/tmp/axo-helper-view-fifo-{tag}"),
        format!("/var/tmp/axo-helper-view-{tag}"),
        format!("/dev/shm/axo-helper-view-{tag}"),
        format!("/dev/axo-helper-view-{tag}"),
    ] {
        let exists = std::fs::symlink_metadata(&leftover).is_ok();
        let _ = std::fs::remove_file(&leftover);
        let _ = std::fs::remove_dir(&leftover);
        assert!(!exists, "{leftover}");
    }
}

fn require_python() -> bool {
    if Command::new("python3").arg("-V").output().is_err() {
        eprintln!("skipped: python3 is not in this image");
        return false;
    }
    true
}

/// A helper's file tools (no write restriction) write nothing: not the
/// Workspace, where its file modes would let any user write, not a
/// directory any user may change inside the writer's private home, which
/// the capability takes it through, not `/tmp`, `/var/tmp`, `/dev/shm` or
/// `/dev`. They cannot create, write, truncate, remove, rename or link a
/// file, or change its mode, times or extended attributes; only
/// `/dev/null` takes what they discard. They get no scratch directory.
#[test]
fn a_helpers_file_tools_cannot_create_write_truncate_or_remove_anything() {
    require_launch!();
    if !require_python() {
        return;
    }
    let fixture = Fixture::new();
    let tag = format!("tools-{}", std::process::id());
    let mut writes = forbidden_writes(&fixture, &tag, true);
    writes.extend(["device:/dev/zero".into(), "device:/dev/urandom".into()]);
    let mut args: Vec<&str> = vec![WRITES];
    args.extend(writes.iter().map(String::as_str));
    args.push("device:/dev/null");
    let ran = helper(
        &fixture.workspace,
        "p=$1; shift; python3 -c \"$p\" \"$@\"; echo discarded > /dev/null && echo shell-null=ok",
        &args,
        None,
    );
    ran.exited();
    let got = ran.lines();
    for write in &writes {
        let refused = got
            .iter()
            .any(|line| line.starts_with(&format!("{write}=errno-")));
        assert!(refused, "{write}: {}\n{}", ran.stdout, ran.stderr);
    }
    assert!(got.contains("device:/dev/null=ok"), "{}", ran.stdout);
    assert!(got.contains("shell-null=ok"), "{}", ran.stdout);
    assert_unchanged(&fixture, &tag);
}

/// A helper's shell writes only beneath its own scratch directory (its
/// `HOME` and `TMPDIR`, removed when it ends) and to `/dev/null`,
/// `/dev/zero`, `/dev/tty` and `/dev/urandom`: whatever its restriction's
/// `writable` roots name, not `/tmp`, `/var/tmp`, `/dev/shm` or `/dev`
/// themselves, nor the Workspace or anything the capability reaches. In its
/// scratch directory ordinary work goes on; it reads the 0700 Workspace.
#[test]
fn a_helper_shell_writes_only_its_scratch_directory_and_a_few_devices() {
    require_launch!();
    if !require_python() {
        return;
    }
    let fixture = Fixture::new();
    let tag = format!("shell-{}", std::process::id());
    let writes = forbidden_writes(&fixture, &tag, false);
    let mut args: Vec<&str> = vec![WRITES];
    args.extend(writes.iter().map(String::as_str));
    let ran = helper(
        &fixture.workspace,
        "set -e; p=$1; shift; python3 -c \"$p\" \"$@\"; s=$TMPDIR; echo HOME=$HOME TMPDIR=$TMPDIR; \
         python3 -c \"$p\" create:$s/f append:$s/f overwrite:$s/f clobber:$s/f truncate:$s/f \
           mkdir:$s/d rmdir:$s/d rename:$s/f link:$s/f.renamed symlink:$s/l fifo:$s/p \
           chmod:$s/f.renamed utime:$s/f.renamed remove:$s/f.renamed device:/dev/null \
           device:/dev/zero device:/dev/urandom xattr:$s/f.renamed.link; \
         echo to-stderr > /dev/stderr; cat README.md; mktemp >/dev/null && echo mktemp=ok",
        &args,
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
    for write in &writes {
        let refused = got
            .iter()
            .any(|line| line.starts_with(&format!("{write}=errno-")));
        assert!(refused, "{write}: {}\n{}", ran.stdout, ran.stderr);
    }
    let scratch = ran
        .stdout
        .lines()
        .find_map(|line| line.strip_prefix("HOME="))
        .and_then(|rest| rest.split_once(" TMPDIR="))
        .unwrap();
    assert_eq!(scratch.0, scratch.1);
    assert!(
        scratch.0.starts_with("/tmp/axocoatl-helper."),
        "{scratch:?}"
    );
    let s = scratch.0;
    for allowed in [
        format!("create:{s}/f"),
        format!("append:{s}/f"),
        format!("overwrite:{s}/f"),
        format!("clobber:{s}/f"),
        format!("truncate:{s}/f"),
        format!("mkdir:{s}/d"),
        format!("rmdir:{s}/d"),
        format!("rename:{s}/f"),
        format!("link:{s}/f.renamed"),
        format!("symlink:{s}/l"),
        format!("fifo:{s}/p"),
        format!("chmod:{s}/f.renamed"),
        format!("utime:{s}/f.renamed"),
        format!("remove:{s}/f.renamed"),
        "device:/dev/null".into(),
        "device:/dev/zero".into(),
        "device:/dev/urandom".into(),
        "workspace-readme".into(),
        "mktemp=ok".into(),
    ] {
        assert!(
            got.contains(&format!("{allowed}=ok")) || got.contains(&allowed),
            "{allowed}: {}\n{}",
            ran.stdout,
            ran.stderr
        );
    }
    // Extended attributes stay refused even there.
    assert!(
        got.contains(&format!("xattr:{s}/f.renamed.link=errno-{}", libc::EPERM)),
        "{}",
        ran.stdout
    );
    assert!(ran.stderr.contains("to-stderr"), "{}", ran.stderr);
    // Removed when the command ends.
    assert!(!Path::new(s).exists(), "{s}");
    assert_unchanged(&fixture, &tag);
}

/// Ordinary read-only work runs in a helper's shell, in a Workspace only
/// the writer may enter: Git, grep, ripgrep when the image has it, Python
/// with its temporary files, and `cargo check` with its target directory
/// and Cargo home in the scratch directory.
#[test]
fn a_helper_shell_runs_common_tools_in_a_private_workspace() {
    require_launch!();
    if landlock_abi() < 4 {
        eprintln!("skipped: a helper's shell needs Landlock ABI 4");
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
    private_dir(&fixture.workspace.join("probe/src"), WRITER);
    private_dir(&fixture.workspace.join("probe"), WRITER);
    write_private(
        &fixture.workspace.join("probe/Cargo.toml"),
        "[package]\nname = \"probe\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[workspace]\n",
        WRITER,
        0o600,
    );
    write_private(
        &fixture.workspace.join("probe/Cargo.lock"),
        "version = 4\n\n[[package]]\nname = \"probe\"\nversion = \"0.1.0\"\n",
        WRITER,
        0o600,
    );
    write_private(
        &fixture.workspace.join("probe/src/main.rs"),
        "fn main() { println!(\"probe\"); }\n",
        WRITER,
        0o600,
    );
    git(&["init", "-q", "."]);
    git(&["add", "."]);
    git(&["commit", "-qm", "Add the fixture"]);
    let tools = [
        ("git", "git -c safe.directory='*' log --format=%s && git -c safe.directory='*' status --short && echo git=ok"),
        ("grep", "grep -rq workspace-source src && echo grep=ok"),
        ("rg", "if command -v rg >/dev/null; then rg -q workspace-source src && echo rg=ok; else echo rg=ok; fi"),
        (
            "python",
            "python3 -c 'import tempfile, os, subprocess; f = tempfile.NamedTemporaryFile(); \
             f.write(b\"x\"); f.flush(); d = tempfile.mkdtemp(); \
             open(os.path.expanduser(\"~/.cache-probe\"), \"w\").write(\"x\"); \
             subprocess.run([\"true\"], check=True); print(open(\"src/lib.rs\").read().strip())' && echo python=ok",
        ),
        (
            "cargo",
            "if command -v cargo >/dev/null; then CARGO_HOME=$TMPDIR/cargo-home CARGO_TARGET_DIR=$TMPDIR/target \
             cargo check --offline --locked -q --manifest-path probe/Cargo.toml && echo cargo=ok; \
             else echo cargo=ok; fi",
        ),
    ];
    for (name, script) in tools {
        let ran = helper(
            &fixture.workspace,
            script,
            &[],
            Some(shell_restriction(&fixture.workspace)),
        );
        ran.exited();
        assert!(
            ran.lines().contains(&format!("{name}=ok")),
            "{name}: {}\n{}",
            ran.stdout,
            ran.stderr
        );
        if name == "python" {
            assert!(ran.lines().contains("workspace-source"), "{}", ran.stdout);
        }
    }
    assert!(!fixture.workspace.join("probe/target").exists());
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
    // Observed from outside: the command cannot read its own /proc entries,
    // and its file tools' kind cannot write anywhere to say which it is: the
    // observer looks for the helper user's `sleep`.
    let observer = std::thread::spawn(move || {
        for _ in 0..200 {
            for entry in std::fs::read_dir("/proc").unwrap().flatten() {
                let name = entry.file_name().to_string_lossy().into_owned();
                if !name.bytes().all(|byte| byte.is_ascii_digit()) {
                    continue;
                }
                let Ok(text) = std::fs::read_to_string(entry.path().join("status")) else {
                    continue;
                };
                let helper_sleep = text.lines().any(|line| line == "Name:\tsleep")
                    && text.lines().any(|line| {
                        line.strip_prefix("Uid:")
                            .and_then(|ids| ids.split_whitespace().next())
                            == Some(&HELPER.0.to_string())
                    });
                if helper_sleep {
                    // SAFETY: a plain kill of the observed process.
                    unsafe { libc::kill(name.parse().unwrap(), libc::SIGKILL) };
                    return text;
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        panic!("no helper process to observe");
    });
    let _ = helper(&fixture.workspace, "sleep 30", &[], None);
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
/// filter refuses both, `inotify` and `fanotify` alike, from its file tools
/// and its shell, while connected socket pairs (stream and sequenced-packet,
/// which child processes' pipes and Rust's process spawning use) still work.
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
        attempt('pair_seqpacket', lambda: socket.socketpair(socket.AF_UNIX, socket.SOCK_SEQPACKET))\n\
        attempt('pair_dgram', lambda: socket.socketpair(socket.AF_UNIX, socket.SOCK_DGRAM))\n\
        libc = ctypes.CDLL(None, use_errno=True)\n\
        def watch():\n\
        \x20   fd = libc.inotify_init1(0)\n\
        \x20   if fd < 0 or libc.inotify_add_watch(fd, b'.', 0x100) < 0:\n\
        \x20       raise OSError(ctypes.get_errno(), 'inotify')\n\
        attempt('inotify', watch)\n\
        def fanotify():\n\
        \x20   if libc.fanotify_init(0, 0) < 0:\n\
        \x20       raise OSError(ctypes.get_errno(), 'fanotify_init')\n\
        attempt('fanotify_init', fanotify)\n\
        def mark():\n\
        \x20   mark = libc.fanotify_mark\n\
        \x20   mark.argtypes = [ctypes.c_int, ctypes.c_uint, ctypes.c_uint64, ctypes.c_int, ctypes.c_char_p]\n\
        \x20   if mark(-1, 1, 0x1, -100, b'.') < 0:\n\
        \x20       raise OSError(ctypes.get_errno(), 'fanotify_mark')\n\
        attempt('fanotify_mark', mark)\n";
    for restriction in [None, Some(shell_restriction(&fixture.workspace))] {
        let shell = restriction.is_some();
        let ran = helper(
            &fixture.workspace,
            "python3 -c \"$1\"",
            &[script],
            restriction,
        );
        if shell && landlock_abi() < 4 {
            continue;
        }
        ran.exited();
        let got = ran.lines();
        let eperm = format!("errno-{}", libc::EPERM);
        let eacces = format!("errno-{}", libc::EACCES);
        for expected in [
            format!("unix={eperm}"),
            format!("unix_dgram={eperm}"),
            "pair_stream=ok".into(),
            "pair_seqpacket=ok".into(),
            format!("pair_dgram={eperm}"),
            format!("inotify={eperm}"),
            // Refused by the filter, not by a bad descriptor (EBADF).
            format!("fanotify_mark={eperm}"),
        ] {
            assert!(
                got.contains(&expected),
                "{shell}: {expected}: {}\n{}",
                ran.stdout,
                ran.stderr
            );
        }
        assert!(
            got.contains(&format!("fanotify_init={eperm}"))
                || got.contains(&format!("fanotify_init={eacces}")),
            "{shell}: {}",
            ran.stdout
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
    let home = fixture.home_file.to_string_lossy().into_owned();
    let home_dir = fixture
        .home_file
        .parent()
        .unwrap()
        .to_string_lossy()
        .into_owned();
    for restriction in [None, Some(shell_restriction(&fixture.workspace))] {
        let shell = restriction.is_some();
        let ran = helper(
            &fixture.workspace,
            "for f in environ maps mem cmdline status; do cat /proc/$1/$f >/dev/null 2>&1; \
             echo $f=$?; done; ls /proc/$1 >/dev/null 2>&1; echo list=$?; \
             kill -0 $1 2>/dev/null; echo kill=$?; cat \"$2\" 2>/dev/null; echo home=$?; \
             ls \"$3\" >/dev/null 2>&1; echo home-list=$?",
            &[&pid, &home, &home_dir],
            restriction,
        );
        if shell && landlock_abi() < 4 {
            continue;
        }
        ran.exited();
        for name in [
            "environ", "maps", "mem", "cmdline", "status", "kill", "home",
        ] {
            assert!(
                ran.lines().contains(&format!("{name}=1")),
                "{shell}: {name}: {}",
                ran.stdout
            );
        }
        for name in ["list", "home-list"] {
            assert!(
                ran.lines().contains(&format!("{name}=1"))
                    || ran.lines().contains(&format!("{name}=2")),
                "{shell}: {name}: {}",
                ran.stdout
            );
        }
        assert!(!ran.stdout.contains("writer-environment"));
        assert!(!ran.stdout.contains("writer-home-token"));
    }
    let alive = writer.try_wait().unwrap().is_none();
    let _ = writer.kill();
    let _ = writer.wait();
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
