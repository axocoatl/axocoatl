#![cfg(unix)]
//! A child process shares every open file description of the daemon from
//! `fork` until `exec`, including the locked descriptors of open stores. Every
//! Session store must therefore reopen while such a child still holds the
//! lock of the store that was just dropped, instead of failing with
//! `WouldBlock` (os error 35) or reporting its owner busy.

use std::fmt::Debug;
use std::io::{Read, Write};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, ExitStatus};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use axocoatl_core::SecureDir;
use axocoatl_session::control_authority::ControlAuthority;
use axocoatl_session::control_command::{ControlCommandOwner, ControlCommandStore};
use axocoatl_session::execution_content::ExecutionContentStore;
use axocoatl_session::execution_namespace::ExecutionComponent;
use axocoatl_session::execution_ownership::{LegacyFormatOwnership, UpgradedFormatOwnership};
use axocoatl_session::execution_store::{
    ExecutionStoreError, ExecutionStoreOwner, SessionExecutionStore,
};
use axocoatl_session::invocation_audit::{InvocationAudit, InvocationAuditOwner};
use axocoatl_session::turn_contract::{LogicalTurnId, SessionId};

/// How long the child pauses between fork and exec: well inside
/// `LOCK_INHERITANCE_GRACE`, far longer than the reopen takes.
const HOLD: Duration = Duration::from_millis(100);

/// Start `/usr/bin/true` with a `pre_exec` hook that pauses for `HOLD`, and
/// return once the child has forked, when it holds a copy of every
/// descriptor this process had open.
fn child_between_fork_and_exec() -> JoinHandle<ExitStatus> {
    let (mut forked, mut writer) = std::io::pipe().unwrap();
    let spawner = std::thread::spawn(move || {
        let mut command = Command::new("/usr/bin/true");
        // SAFETY: the hook only calls write(2) and nanosleep(2).
        unsafe {
            command.pre_exec(move || {
                writer.write_all(b"f")?;
                std::thread::sleep(HOLD);
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
fn reopen_beside_child<T, E: Debug>(
    name: &str,
    store: T,
    open: impl Fn() -> Result<T, E>,
    failures: &mut Vec<String>,
) -> T {
    let child = child_between_fork_and_exec();
    drop(store);
    let started = Instant::now();
    let reopened = open();
    let waited = started.elapsed();
    assert!(child.join().unwrap().success());
    match reopened {
        Ok(store) => {
            eprintln!("{name}: reopened after {waited:?}");
            store
        }
        Err(error) => {
            failures.push(format!("{name}: {error:?}"));
            open().unwrap()
        }
    }
}

fn debug(error: impl Debug) -> String {
    format!("{error:?}")
}

fn would_block(path: &Path) -> bool {
    SecureDir::open_existing_all(path)
        .unwrap()
        .try_lock_exclusive()
        .is_err_and(|error| error.kind() == std::io::ErrorKind::WouldBlock)
}

fn session() -> SessionId {
    SessionId::new("session-locks").unwrap()
}

fn turn() -> LogicalTurnId {
    LogicalTurnId::new("turn-locks").unwrap()
}

fn owner() -> ExecutionStoreOwner {
    ExecutionStoreOwner {
        workspace_id: "workspace-locks".into(),
        session_id: session(),
    }
}

fn upgraded(root: &Path) -> Arc<UpgradedFormatOwnership> {
    Arc::new(
        LegacyFormatOwnership::acquire(root)
            .unwrap()
            .upgrade()
            .unwrap(),
    )
}

#[test]
fn every_owned_session_store_reopens_while_a_starting_child_holds_its_lock() {
    let root = tempfile::tempdir().unwrap();
    let ownership = upgraded(root.path());
    let canonical = SessionExecutionStore::open(ownership.clone(), owner()).unwrap();
    let mut failures = Vec::new();
    {
        // Taking the component namespace is the lock to wait for; the store
        // then opens through the namespace's own descriptor.
        let namespace = |component| canonical.component_namespace(component).map_err(debug);
        let content = || {
            namespace(ExecutionComponent::ExecutionContent)
                .and_then(|owned| ExecutionContentStore::open_owned(owned).map_err(debug))
        };
        let audit = || {
            namespace(ExecutionComponent::InvocationAudit)
                .and_then(|owned| InvocationAudit::open_owned(owned).map_err(debug))
        };
        let authority = || {
            namespace(ExecutionComponent::ControlAuthority { turn_id: turn() })
                .and_then(|owned| ControlAuthority::open_owned(owned).map_err(debug))
        };
        let commands = || {
            namespace(ExecutionComponent::ControlCommands { turn_id: turn() })
                .and_then(|owned| ControlCommandStore::open_owned(owned).map_err(debug))
        };
        let first = content().unwrap();
        drop(reopen_beside_child(
            "execution content",
            first,
            content,
            &mut failures,
        ));
        let first = audit().unwrap();
        drop(reopen_beside_child(
            "invocation audit",
            first,
            audit,
            &mut failures,
        ));
        let first = authority().unwrap();
        drop(reopen_beside_child(
            "control authority",
            first,
            authority,
            &mut failures,
        ));
        let first = commands().unwrap();
        drop(reopen_beside_child(
            "control commands",
            first,
            commands,
            &mut failures,
        ));
    }
    let canonical = reopen_beside_child(
        "session journal",
        canonical,
        || SessionExecutionStore::open(ownership.clone(), owner()),
        &mut failures,
    );
    drop(canonical);

    // The data root: external lease, in-root lease and data-root inode.
    let child = child_between_fork_and_exec();
    drop(ownership);
    assert!(
        would_block(root.path()),
        "the starting child shares the dropped data root's lock"
    );
    match UpgradedFormatOwnership::open(root.path()) {
        Ok(reopened) => drop(reopened),
        Err(error) => failures.push(format!("data-root ownership: {error:?}")),
    }
    assert!(child.join().unwrap().success());

    assert!(failures.is_empty(), "{failures:#?}");
}

#[test]
fn every_standalone_session_store_reopens_while_a_starting_child_holds_its_lock() {
    let root = tempfile::tempdir().unwrap();
    let canonical = SessionExecutionStore::open(upgraded(root.path()), owner()).unwrap();
    let identity = canonical.identity().unwrap();
    let dirs: Vec<_> = (0..4).map(|_| tempfile::tempdir().unwrap()).collect();
    let content = || ExecutionContentStore::open(dirs[0].path(), identity.clone());
    let audit = || {
        InvocationAudit::open(
            dirs[1].path(),
            InvocationAuditOwner {
                workspace_id: "workspace-locks".into(),
                session_id: session(),
            },
        )
    };
    let authority = || ControlAuthority::open(dirs[2].path(), session(), turn());
    let commands = || {
        ControlCommandStore::open(
            dirs[3].path(),
            ControlCommandOwner {
                workspace_id: "workspace-locks".into(),
                session_id: session(),
                turn_id: turn(),
            },
        )
    };

    let mut failures = Vec::new();
    let first = content().unwrap();
    drop(reopen_beside_child(
        "execution content",
        first,
        content,
        &mut failures,
    ));
    let first = audit().unwrap();
    drop(reopen_beside_child(
        "invocation audit",
        first,
        audit,
        &mut failures,
    ));
    let first = authority().unwrap();
    drop(reopen_beside_child(
        "control authority",
        first,
        authority,
        &mut failures,
    ));
    let first = commands().unwrap();
    drop(reopen_beside_child(
        "control commands",
        first,
        commands,
        &mut failures,
    ));
    assert!(failures.is_empty(), "{failures:#?}");
}

#[test]
fn a_store_still_held_by_its_owner_is_reported_busy_after_the_grace() {
    let root = tempfile::tempdir().unwrap();
    let ownership = upgraded(root.path());
    let canonical = SessionExecutionStore::open(ownership.clone(), owner()).unwrap();
    let started = Instant::now();
    assert!(matches!(
        SessionExecutionStore::open(ownership, owner()),
        Err(ExecutionStoreError::Io(error)) if error.kind() == std::io::ErrorKind::WouldBlock
    ));
    assert!(started.elapsed() >= axocoatl_core::LOCK_INHERITANCE_GRACE);
    drop(canonical);
}
