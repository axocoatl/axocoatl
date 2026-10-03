//! A store's `flock` is held by its open file description, so a child that
//! inherited the descriptor keeps it locked until the child execs. These tests
//! pin that mechanism and the bounded wait that store opens use for it.

use super::*;

use std::os::unix::process::CommandExt;
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use rustix::fd::{AsRawFd, BorrowedFd};

fn store_dir() -> (tempfile::TempDir, PathBuf) {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("store");
    std::fs::create_dir(&path).unwrap();
    (root, path)
}

fn would_block(result: io::Result<()>) -> bool {
    matches!(result, Err(error) if error.kind() == io::ErrorKind::WouldBlock)
}

#[test]
fn a_duplicate_descriptor_keeps_a_dropped_store_locked_until_it_closes() {
    let (_root, path) = store_dir();
    let holder = SecureDir::open(&path).unwrap();
    holder.try_lock_exclusive().unwrap();
    // What a forked child holds between fork and exec: another descriptor
    // for the same locked open file description.
    let duplicate = holder.fd.try_clone().unwrap();
    drop(holder);

    let reopened = SecureDir::open(&path).unwrap();
    assert!(
        would_block(reopened.try_lock_exclusive()),
        "the duplicate still holds the dropped store's lock"
    );

    let released = Arc::new(AtomicBool::new(false));
    let releaser = std::thread::spawn({
        let released = released.clone();
        move || {
            std::thread::sleep(Duration::from_millis(50));
            released.store(true, Ordering::SeqCst);
            drop(duplicate);
        }
    });
    let started = Instant::now();
    reopened
        .lock_exclusive_waiting(Duration::from_secs(5))
        .expect("the lock is taken once the duplicate closes");
    assert!(released.load(Ordering::SeqCst));
    assert!(started.elapsed() >= Duration::from_millis(40));
    releaser.join().unwrap();

    let contender = SecureDir::open(&path).unwrap();
    assert!(would_block(contender.try_lock_exclusive()));
}

#[test]
fn a_lock_held_past_the_grace_reports_would_block() {
    let (_root, path) = store_dir();
    let holder = SecureDir::open(&path).unwrap();
    holder.try_lock_exclusive().unwrap();
    let contender = SecureDir::open(&path).unwrap();
    let started = Instant::now();
    let error = contender
        .lock_exclusive_waiting(LOCK_INHERITANCE_GRACE)
        .unwrap_err();
    let waited = started.elapsed();
    assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
    assert!(waited >= LOCK_INHERITANCE_GRACE, "waited {waited:?}");
    assert!(
        waited < LOCK_INHERITANCE_GRACE + Duration::from_secs(2),
        "waited {waited:?}"
    );
    drop(holder);
    contender
        .lock_exclusive_waiting(LOCK_INHERITANCE_GRACE)
        .unwrap();
}

#[test]
fn a_lock_file_duplicate_is_waited_for_and_a_real_owner_is_reported() {
    let (_root, path) = store_dir();
    let dir = SecureDir::open(&path).unwrap();
    let holder = dir.open_lock_file("owner.lock").unwrap();
    lock_file_exclusive_waiting(&holder, LOCK_INHERITANCE_GRACE).unwrap();
    let duplicate = holder.try_clone().unwrap();
    drop(holder);

    let reopened = dir.open_lock_file("owner.lock").unwrap();
    let error = lock_file_exclusive_waiting(&reopened, Duration::ZERO).unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::WouldBlock);

    let releaser = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(50));
        drop(duplicate);
    });
    lock_file_exclusive_waiting(&reopened, Duration::from_secs(5)).unwrap();
    releaser.join().unwrap();

    let contender = dir.open_lock_file("owner.lock").unwrap();
    let started = Instant::now();
    let error = lock_file_exclusive_waiting(&contender, LOCK_INHERITANCE_GRACE).unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
    assert!(started.elapsed() >= LOCK_INHERITANCE_GRACE);
}

#[test]
fn a_child_between_fork_and_exec_holds_the_lock_of_a_dropped_store() {
    let (_root, path) = store_dir();
    let holder = SecureDir::open(&path).unwrap();
    holder.try_lock_exclusive().unwrap();

    let (mut forked, forked_writer) = std::io::pipe().unwrap();
    let writer_fd = forked_writer.as_raw_fd();
    let spawner = std::thread::spawn(move || {
        let mut command = Command::new("/usr/bin/true");
        // SAFETY: the hook only calls write(2) and nanosleep(2), both
        // async-signal-safe, on a descriptor that outlives the spawn.
        unsafe {
            command.pre_exec(move || {
                let writer = BorrowedFd::borrow_raw(writer_fd);
                rustix::io::write(writer, b"f")?;
                std::thread::sleep(Duration::from_millis(150));
                Ok(())
            });
        }
        let status = command.status().unwrap();
        drop(forked_writer);
        status
    });

    // The child has forked and holds a copy of the holder's descriptor.
    let mut signal = [0u8; 1];
    forked.read_exact(&mut signal).unwrap();
    drop(holder);
    let reopened = SecureDir::open(&path).unwrap();
    assert!(
        would_block(reopened.try_lock_exclusive()),
        "the forked child keeps the dropped store's lock until it execs"
    );
    reopened
        .lock_exclusive_waiting(Duration::from_secs(5))
        .expect("exec closes the child's copy and releases the lock");
    assert!(spawner.join().unwrap().success());
}

#[test]
fn reopening_beside_a_fork_loop_never_fails_with_the_wait() {
    // At least this many reopens, and enough of them finding the lock held
    // for the wait to be what is tested: a quick run can otherwise pass with
    // no contended reopen at all.
    const MIN_REOPENS: usize = 500;
    const MIN_CONTENDED: usize = 20;
    const TIME_LIMIT: Duration = Duration::from_secs(5);
    let (_root, path) = store_dir();
    let done = Arc::new(AtomicBool::new(false));
    let spawned = Arc::new(AtomicUsize::new(0));
    let spawner = std::thread::spawn({
        let done = done.clone();
        let spawned = spawned.clone();
        move || {
            while !done.load(Ordering::SeqCst) {
                let mut command = Command::new("/usr/bin/true");
                // SAFETY: an empty hook; it only forces fork and exec in
                // place of posix_spawn, as a PTY spawn does.
                unsafe {
                    command.pre_exec(|| Ok(()));
                }
                assert!(command.status().unwrap().success());
                spawned.fetch_add(1, Ordering::SeqCst);
            }
        }
    });
    while spawned.load(Ordering::SeqCst) == 0 {
        std::thread::sleep(Duration::from_millis(1));
    }

    let started = Instant::now();
    let mut reopens = 0usize;
    let mut contended = 0usize;
    let mut failures = Vec::new();
    while (reopens < MIN_REOPENS || contended < MIN_CONTENDED) && started.elapsed() < TIME_LIMIT {
        let dir = SecureDir::open(&path).unwrap();
        if would_block(dir.try_lock_exclusive()) {
            contended += 1;
            if let Err(error) = dir.lock_exclusive_waiting(LOCK_INHERITANCE_GRACE) {
                failures.push(format!("reopen {reopens}: {error}"));
            }
        }
        drop(dir);
        reopens += 1;
    }
    let elapsed = started.elapsed();
    done.store(true, Ordering::SeqCst);
    spawner.join().unwrap();
    eprintln!(
        "{reopens} reopens in {elapsed:?} beside {} fork-and-exec children: \
         {contended} found the lock inherited",
        spawned.load(Ordering::SeqCst)
    );
    assert!(failures.is_empty(), "{failures:?}");
    assert!(
        contended >= MIN_CONTENDED,
        "only {contended} of {reopens} reopens found the lock inherited in {elapsed:?}, \
         so the wait was not exercised enough"
    );
}

#[test]
fn only_would_block_is_retried_and_the_last_attempt_lands_on_the_deadline() {
    let calls = AtomicUsize::new(0);
    let error = retry_inherited_lock(LOCK_INHERITANCE_GRACE, || {
        calls.fetch_add(1, Ordering::SeqCst);
        Err(io::Error::from(io::ErrorKind::PermissionDenied))
    })
    .unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
    assert_eq!(calls.load(Ordering::SeqCst), 1);

    calls.store(0, Ordering::SeqCst);
    let error = retry_inherited_lock(Duration::ZERO, || {
        calls.fetch_add(1, Ordering::SeqCst);
        Err(io::Error::from(io::ErrorKind::WouldBlock))
    })
    .unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
    assert_eq!(calls.load(Ordering::SeqCst), 1);

    calls.store(0, Ordering::SeqCst);
    retry_inherited_lock(LOCK_INHERITANCE_GRACE, || {
        if calls.fetch_add(1, Ordering::SeqCst) < 3 {
            Err(io::Error::from(io::ErrorKind::WouldBlock))
        } else {
            Ok(())
        }
    })
    .unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 4);

    // 1 + 2 + 4 + 8 + 16 + 32 + 32 … ms: a 250 ms grace allows about a dozen
    // attempts, the last one at the deadline.
    calls.store(0, Ordering::SeqCst);
    let started = Instant::now();
    let mut last = Duration::ZERO;
    retry_inherited_lock(LOCK_INHERITANCE_GRACE, || {
        calls.fetch_add(1, Ordering::SeqCst);
        last = started.elapsed();
        Err(io::Error::from(io::ErrorKind::WouldBlock))
    })
    .unwrap_err();
    let attempts = calls.load(Ordering::SeqCst);
    assert!((6..=14).contains(&attempts), "{attempts} attempts");
    assert!(last >= LOCK_INHERITANCE_GRACE, "last attempt at {last:?}");
}
