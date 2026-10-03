#![cfg(unix)]
//! Memory stores reopen while a starting child process still shares the lock
//! of the store that was just dropped (see the Session crate's test of the
//! same name for the mechanism).

use std::io::{Read, Write};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, ExitStatus};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use axocoatl_core::SecureDir;
use axocoatl_memory::activation_state::ActivationStateStore;
use axocoatl_memory::knowledge::KnowledgeStore;
use axocoatl_session::execution_namespace::ExecutionComponent;
use axocoatl_session::execution_ownership::LegacyFormatOwnership;
use axocoatl_session::execution_store::{ExecutionStoreOwner, SessionExecutionStore};
use axocoatl_session::turn_contract::SessionId;

/// Start `/usr/bin/true` with a `pre_exec` hook that pauses for `hold`, and
/// return once the child has forked, when it holds a copy of every
/// descriptor this process had open.
fn child_between_fork_and_exec(hold: Duration) -> JoinHandle<ExitStatus> {
    let (mut forked, mut writer) = std::io::pipe().unwrap();
    let spawner = std::thread::spawn(move || {
        let mut command = Command::new("/usr/bin/true");
        // SAFETY: the hook only calls write(2) and nanosleep(2).
        unsafe {
            command.pre_exec(move || {
                writer.write_all(b"f")?;
                std::thread::sleep(hold);
                Ok(())
            });
        }
        command.status().unwrap()
    });
    let mut signal = [0u8; 1];
    forked.read_exact(&mut signal).unwrap();
    spawner
}

/// Drop `store` while a freshly forked child holds its descriptors, and
/// reopen it with `open` before the child execs. A refused reopen is recorded
/// and retried after the child is gone, so the later steps still run.
fn reopen_beside_child<T, E: std::fmt::Debug>(
    name: &str,
    store: T,
    held: Option<&Path>,
    open: impl Fn() -> Result<T, E>,
    failures: &mut Vec<String>,
) -> T {
    let child = child_between_fork_and_exec(Duration::from_millis(100));
    drop(store);
    if let Some(path) = held {
        assert!(
            would_block(path),
            "{name}: the starting child shares the dropped store's lock"
        );
    }
    let reopened = open();
    assert!(child.join().unwrap().success());
    reopened.unwrap_or_else(|error| {
        failures.push(format!("{name}: {error:?}"));
        open().unwrap()
    })
}

fn would_block(path: &Path) -> bool {
    SecureDir::open(path)
        .unwrap()
        .try_lock_exclusive()
        .is_err_and(|error| error.kind() == std::io::ErrorKind::WouldBlock)
}

fn session() -> SessionId {
    SessionId::new("session-locks").unwrap()
}

#[test]
fn memory_stores_reopen_while_a_starting_child_holds_their_locks() {
    let state = tempfile::tempdir().unwrap();
    let knowledge = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let canonical = SessionExecutionStore::open(
        Arc::new(
            LegacyFormatOwnership::acquire(root.path())
                .unwrap()
                .upgrade()
                .unwrap(),
        ),
        ExecutionStoreOwner {
            workspace_id: "workspace-locks".into(),
            session_id: session(),
        },
    )
    .unwrap();
    let isolated = || ActivationStateStore::open(state.path(), session());
    let notes = || KnowledgeStore::open(SecureDir::open_or_create_all(knowledge.path()).unwrap());
    let owned = || {
        canonical
            .component_namespace(ExecutionComponent::ActivationState)
            .map_err(|error| format!("{error:?}"))
            .and_then(|namespace| {
                ActivationStateStore::open_owned(namespace).map_err(|error| format!("{error:?}"))
            })
    };

    let mut failures = Vec::new();
    let first = isolated().unwrap();
    let held = Some(state.path());
    drop(reopen_beside_child(
        "isolated activation state",
        first,
        held,
        isolated,
        &mut failures,
    ));
    let first = notes().unwrap();
    let held = knowledge.path().join("v1");
    drop(reopen_beside_child(
        "knowledge",
        first,
        Some(&held),
        notes,
        &mut failures,
    ));
    let first = owned().unwrap();
    drop(reopen_beside_child(
        "owned activation state",
        first,
        None,
        owned,
        &mut failures,
    ));
    assert!(failures.is_empty(), "{failures:#?}");
}
