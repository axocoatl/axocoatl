//! Stress harness for stores reopened while the daemon starts a terminal.
//!
//! A `flock` lock belongs to the open file description. Opening a terminal
//! forks the daemon (`portable_pty` runs a `pre_exec` hook), and until the
//! child execs it holds a duplicate of every descriptor the daemon had open,
//! including a store's locked directory. A store dropped and reopened in that
//! window finds its own lock still held. Store opens therefore wait briefly
//! for such a lock instead of failing at once.
//!
//! The test below opens terminals in a loop through the production PTY path
//! so that other tests can run beside it in the same process. It does nothing
//! unless asked to. With `$STUB` the absolute path of a directory holding an
//! executable `podman` that exits 0, run the daemon's test binary (as built by
//! `cargo test -p axocoatl-daemon --lib --no-run`) with this test and the
//! tests under pressure, for example:
//!
//! ```text
//! AXOCOATL_TEST_PTY_LOOP_MS=1500 AXOCOATL_TEST_FAKE_PODMAN_DIR=$STUB \
//! PATH=$STUB:$PATH <test binary> --exact --include-ignored --test-threads=8 \
//!   lock_inheritance_tests::background_pty_loop \
//!   session_dispatch::conditions::tests::check_read_projection_distinguishes_no_dispatch_from_missing_outcome \
//!   ...
//! ```
//!
//! When this harness was added (macOS, 50 runs beside four lock-sensitive
//! tests), 18 runs failed with os error 35 or a busy lease before store opens
//! waited, and none after.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use axocoatl_isolation::pty::PtyTerminal;

const LOOP_ENV: &str = "AXOCOATL_TEST_PTY_LOOP_MS";
const FAKE_PODMAN_ENV: &str = "AXOCOATL_TEST_FAKE_PODMAN_DIR";

/// Refuse to start real `podman exec` processes: the `podman` that
/// `portable_pty` will run must be the stub.
///
/// This follows `portable_pty`'s own search (`CommandBuilder::search_path`):
/// the first `PATH` entry whose `podman` is not a directory and passes
/// `access(X_OK)` wins, and a relative entry is resolved against the child's
/// working directory, which is `$HOME` because `spawn_podman` sets none. A
/// stub that is not executable is skipped by that search, and a relative
/// entry names a different directory than it does here, so both are refused.
fn require_fake_podman() {
    let fake = std::env::var_os(FAKE_PODMAN_ENV)
        .map(PathBuf::from)
        .unwrap_or_else(|| panic!("{LOOP_ENV} needs {FAKE_PODMAN_ENV}"));
    assert!(
        fake.is_absolute(),
        "{FAKE_PODMAN_ENV} must be an absolute path, not {}",
        fake.display()
    );
    let stub = fake.join("podman");
    assert!(
        stub.is_file() && executable(&stub),
        "{} must be an executable regular file",
        stub.display()
    );
    let path = std::env::var_os("PATH").unwrap_or_else(|| panic!("PATH is not set"));
    for dir in std::env::split_paths(&path) {
        assert!(
            dir.is_absolute(),
            "PATH entry {dir:?} comes before the stub and is relative; \
             portable_pty would resolve it against $HOME"
        );
        let candidate = dir.join("podman");
        if candidate.is_dir() || !executable(&candidate) {
            continue;
        }
        assert_eq!(
            dir, fake,
            "the first podman on PATH must be the stub in {FAKE_PODMAN_ENV}"
        );
        return;
    }
    panic!("{} is not on PATH", fake.display());
}

/// `access(X_OK)`, the check `portable_pty` applies to each candidate.
fn executable(path: &Path) -> bool {
    rustix::fs::access(path, rustix::fs::Access::EXEC_OK).is_ok()
}

#[test]
#[ignore = "stress harness; run with the lock tests under AXOCOATL_TEST_PTY_LOOP_MS"]
fn background_pty_loop() {
    let Some(duration) = std::env::var(LOOP_ENV)
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
    else {
        return;
    };
    require_fake_podman();
    let deadline = Instant::now() + Duration::from_millis(duration);
    let workdir = std::env::temp_dir();
    let mut opened = 0usize;
    while Instant::now() < deadline {
        let terminal = PtyTerminal::spawn_podman(
            format!("lock-loop-{opened}"),
            "axocoatl-lock-loop",
            &workdir,
            "true",
            24,
            80,
        )
        .expect("open a terminal on the podman stub");
        opened += 1;
        let started = Instant::now();
        while terminal.is_alive() && started.elapsed() < Duration::from_secs(5) {
            std::thread::sleep(Duration::from_millis(1));
        }
        drop(terminal);
    }
    eprintln!("background_pty_loop opened {opened} terminals");
    assert!(opened > 0);
}
