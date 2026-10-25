//! Receipt subjects from a run's attribution. Attribution itself is tested
//! with Keep as PR (`keep_pr_tests.rs`).
use super::*;
use axocoatl_session::receipt::source;
use axocoatl_session::run_outcome::CheckState;

fn line(path: &str, kind: &str, digest: &str) -> String {
    use base64::Engine as _;
    format!(
        "{}\t{}\t{kind}\t{digest}\n",
        base64::engine::general_purpose::STANDARD.encode(path),
        if kind == "deleted" { "0" } else { "644" }
    )
}

fn tree(manifest: &str) -> CapturedTree {
    CapturedTree {
        tree_sha256: sha256_hex(manifest.as_bytes()),
        manifest: Some(manifest.to_string()),
        changed: None,
    }
}

fn check(candidate: Option<&str>) -> CheckResult {
    CheckResult {
        name: "tests".into(),
        argv: vec!["pytest".into()],
        state: CheckState::Passed,
        timeout_ms: 180_000,
        exit_code: Some(0),
        stdout_tail: String::new(),
        stderr_tail: String::new(),
        candidate_sha256: candidate.map(str::to_string),
        report: None,
        reason: None,
    }
}

fn paths(names: &[&str]) -> BTreeSet<String> {
    names.iter().map(|name| name.to_string()).collect()
}

fn capture(phase: CapturePhase, manifest: Option<&str>) -> CaptureFacts {
    CaptureFacts {
        phase,
        unavailable: None,
        tree_sha256: Some(
            manifest
                .map(|manifest| sha256_hex(manifest.as_bytes()))
                .unwrap_or_else(|| "t".repeat(64)),
        ),
        judged_sha256: None,
        compared: None,
        manifest: manifest.map(str::to_string),
        patch: None,
        changed: None,
        head: None,
        unread: Vec::new(),
    }
}

/// The judged capture's digests, links and deletions; Git's own files and
/// unattributable activations make the list incomplete.
#[test]
fn subjects_come_from_the_tree_the_checks_judged() {
    let judged = format!(
        "{}{}{}{}",
        line("a.txt", "file", &"1".repeat(64)),
        line("gone.txt", "deleted", "missing"),
        line("link", "link", &"2".repeat(64)),
        line("untouched.txt", "file", &"3".repeat(64)),
    );
    let older = line("a.txt", "file", &"9".repeat(64));
    let attribution = RunAttribution {
        paths: paths(&["a.txt", "gone.txt", "link", "never.txt", ".git/config"]),
        unattributable: vec!["helper (generation 1) in turn t: it has no After capture".into()],
        checked: vec![tree(&older), tree(&judged)],
        last_writer_after: Some(tree(&older)),
        ..RunAttribution::default()
    };
    let files = receipt_files(
        &attribution,
        &[check(Some(&sha256_hex(judged.as_bytes())))],
        Path::new("/nonexistent"),
    );
    assert_eq!(files.facts.source, source::JUDGED_CAPTURE);
    assert_eq!(
        files.facts.tree_sha256.as_deref(),
        Some(sha256_hex(judged.as_bytes()).as_str())
    );
    assert_eq!(
        files.subject,
        vec![
            Subject::sha256("a.txt", "1".repeat(64)),
            Subject::sha256("link", "2".repeat(64)),
        ]
    );
    assert_eq!(files.facts.links, vec!["link"]);
    assert_eq!(files.facts.removed, vec!["gone.txt", "never.txt"]);
    assert!(!files.facts.complete);
    assert_eq!(files.facts.unattributed.len(), 2);
    assert!(files.facts.unattributed[0].starts_with("helper"));
    assert!(files.facts.unattributed[1].starts_with(".git/config: one of Git's own files"));
}

/// A submodule the run moved is a subject with the commit it holds, as a
/// file is with its content's digest; a nested Git checkout (not a
/// submodule), whose files no capture reads, is named as one the receipt
/// does not cover, whether its own entry changed or not, so the receipt is
/// incomplete and says why: in a whole manifest and in a large repository's
/// changed lines alike.
#[test]
fn a_submodule_is_covered_by_its_commit_and_a_nested_checkout_is_named_as_not_covered() {
    let commit = "5".repeat(40);
    let manifest = format!(
        "{}{}{}",
        line("a.txt", "file", &"1".repeat(64)),
        line("tools", SPECIAL_KIND, UNREAD_DIGEST),
        line("vendor/sub", SPECIAL_KIND, &commit),
    );
    let whole = tree(&manifest);
    let lines = CapturedTree {
        tree_sha256: "c".repeat(64),
        manifest: None,
        changed: Some(manifest.clone()),
    };
    for (judged, candidate) in [
        (whole, sha256_hex(manifest.as_bytes())),
        (lines, "c".repeat(64)),
    ] {
        let attribution = RunAttribution {
            paths: paths(&["a.txt", "tools", "vendor/sub"]),
            checked: vec![judged],
            unread: paths(&["old-clone", "tools"]),
            ..RunAttribution::default()
        };
        let files = receipt_files(
            &attribution,
            &[check(Some(&candidate))],
            Path::new("/nonexistent"),
        );
        assert_eq!(files.facts.source, source::JUDGED_CAPTURE);
        assert_eq!(
            files.subject,
            vec![
                Subject::sha256("a.txt", "1".repeat(64)),
                Subject::git_commit("vendor/sub", commit.clone()),
            ]
        );
        assert!(!files.facts.complete);
        let not_covered = "a nested Git checkout (not a submodule), whose files Axocoatl's \
                           repository captures do not read, so a receipt does not cover it or \
                           anything inside it";
        assert_eq!(
            files.facts.unattributed,
            vec![
                format!("tools: {not_covered}"),
                format!("old-clone: {not_covered}"),
            ]
        );
    }
}

/// Without one judged tree (no check named one, or the checks named two),
/// the last writer's After capture; a manifest whose digest is not its
/// tree's is never read.
#[test]
fn without_a_judged_tree_the_last_writers_after_capture_then_the_workspace() {
    let after = line("a.txt", "file", &"4".repeat(64));
    let attribution = RunAttribution {
        paths: paths(&["a.txt"]),
        checked: vec![tree(&line("a.txt", "file", &"5".repeat(64)))],
        last_writer_after: Some(tree(&after)),
        ..RunAttribution::default()
    };
    for checks in [
        vec![check(None)],
        vec![check(Some(&"a".repeat(64))), check(Some(&"b".repeat(64)))],
        Vec::new(),
    ] {
        let files = receipt_files(&attribution, &checks, Path::new("/nonexistent"));
        assert_eq!(files.facts.source, source::AFTER_CAPTURE);
        assert_eq!(
            files.subject,
            vec![Subject::sha256("a.txt", "4".repeat(64))]
        );
        assert!(files.facts.complete);
    }

    // An incomplete or forged manifest leaves the files to be read now.
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.txt"), "now\n").unwrap();
    let mut forged = attribution.clone();
    forged.last_writer_after = Some(CapturedTree {
        tree_sha256: "f".repeat(64),
        manifest: Some(after.clone()),
        changed: None,
    });
    let mut incomplete = attribution.clone();
    incomplete.last_writer_after = Some(CapturedTree {
        tree_sha256: "f".repeat(64),
        manifest: None,
        changed: None,
    });
    for attribution in [forged, incomplete] {
        let files = receipt_files(&attribution, &[], dir.path());
        assert_eq!(files.facts.source, source::WORKSPACE_AT_FINISH);
        assert_eq!(files.facts.tree_sha256, None);
        assert_eq!(
            files.subject,
            vec![Subject::sha256("a.txt", sha256_hex(b"now\n"))]
        );
    }
}

#[cfg(unix)]
#[test]
fn the_workspace_is_read_without_following_links() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::create_dir(root.join("src")).unwrap();
    std::fs::write(root.join("src/lib.rs"), "fn f() {}\n").unwrap();
    std::os::unix::fs::symlink("lib.rs", root.join("src/alias.rs")).unwrap();
    std::os::unix::fs::symlink("/etc", root.join("etc")).unwrap();
    let attribution = RunAttribution {
        paths: paths(&["src/lib.rs", "src/alias.rs", "etc/hosts", "src", "gone"]),
        ..RunAttribution::default()
    };
    let files = receipt_files(&attribution, &[], root);
    assert_eq!(files.facts.source, source::WORKSPACE_AT_FINISH);
    assert_eq!(
        files.subject,
        vec![
            Subject::sha256("src/alias.rs", sha256_hex(b"lib.rs")),
            Subject::sha256("src/lib.rs", sha256_hex(b"fn f() {}\n")),
        ]
    );
    assert_eq!(files.facts.links, vec!["src/alias.rs"]);
    assert_eq!(files.facts.removed, vec!["gone"]);
    assert_eq!(
        files.facts.unattributed.len(),
        2,
        "{:?}",
        files.facts.unattributed
    );
    assert!(files.facts.unattributed[0].starts_with("etc/hosts: etc is a symbolic link"));
    assert!(files.facts.unattributed[1].starts_with("src: is a directory"));
    assert!(!files.facts.complete);
}

/// Each check capture is a tree the checks may have judged; the last
/// activation that changed a file gives the writer's After capture, and an
/// activation that changed nothing does not replace it.
#[test]
fn attribution_keeps_the_checked_trees_and_the_last_writers_after_capture() {
    let before = line("a.txt", "file", &"1".repeat(64));
    let after = line("a.txt", "file", &"2".repeat(64));
    let checked = format!("{after}{}", line("b.txt", "file", &"3".repeat(64)));
    let mut attribution = RunAttribution::default();
    attribution.add_activation(
        "writer",
        &[
            capture(CapturePhase::Before, Some(&before)),
            capture(CapturePhase::Check, Some(&checked)),
            capture(CapturePhase::After, Some(&after)),
        ],
    );
    attribution.add_activation(
        "helper",
        &[
            capture(CapturePhase::Before, Some(&after)),
            capture(CapturePhase::After, Some(&after)),
        ],
    );
    assert_eq!(attribution.paths, paths(&["a.txt"]));
    assert_eq!(attribution.checked, vec![tree(&checked)]);
    assert_eq!(attribution.last_writer_after, Some(tree(&after)));
    let mut unusable = capture(CapturePhase::Check, Some(&checked));
    unusable.unavailable = Some("Execution was stopped".into());
    attribution.add_activation("checks", &[unusable]);
    assert_eq!(attribution.checked.len(), 1);
    // A later activation whose changes are unknown may have changed files
    // after that writer: its After capture is no longer what the run left.
    attribution.add_activation("writer 2", &[capture(CapturePhase::Before, Some(&after))]);
    assert_eq!(attribution.unattributable.len(), 1);
    assert_eq!(attribution.last_writer_after, None);
}

/// In a repository too large for a whole manifest, the judged capture's
/// lines of the paths that differ from HEAD give the digests; a path they
/// do not hold was as in HEAD in that tree (a change the run undid), and is
/// read when the run finishes and named as such.
#[test]
fn a_large_repository_takes_digests_from_the_changed_lines_of_the_judged_tree() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("reverted.txt"), "as in HEAD\n").unwrap();
    let changed = format!(
        "{}{}{}",
        line("a.txt", "file", &"1".repeat(64)),
        line("gone.txt", "deleted", "missing"),
        line("new/link", "link", &"2".repeat(64)),
    );
    let judged = CapturedTree {
        tree_sha256: "c".repeat(64),
        manifest: None,
        changed: Some(changed),
    };
    let attribution = RunAttribution {
        paths: paths(&["a.txt", "gone.txt", "new/link", "reverted.txt"]),
        checked: vec![judged],
        ..RunAttribution::default()
    };
    let files = receipt_files(&attribution, &[check(Some(&"c".repeat(64)))], dir.path());
    assert_eq!(files.facts.source, source::JUDGED_CAPTURE);
    assert_eq!(
        files.facts.tree_sha256.as_deref(),
        Some("c".repeat(64).as_str())
    );
    assert_eq!(
        files.subject,
        vec![
            Subject::sha256("a.txt", "1".repeat(64)),
            Subject::sha256("new/link", "2".repeat(64)),
            Subject::sha256("reverted.txt", sha256_hex(b"as in HEAD\n")),
        ]
    );
    assert_eq!(files.facts.links, vec!["new/link"]);
    assert_eq!(files.facts.removed, vec!["gone.txt"]);
    assert_eq!(files.facts.read_at_finish, vec!["reverted.txt"]);
    assert!(files.facts.complete);
    // A capture that left the lines out (too many changed paths) gives no
    // digests: the files are read when the run finishes.
    let mut without = attribution.clone();
    without.checked[0].changed = None;
    let files = receipt_files(&without, &[check(Some(&"c".repeat(64)))], dir.path());
    assert_eq!(files.facts.source, source::WORKSPACE_AT_FINISH);
    assert!(files.facts.read_at_finish.is_empty());
}

/// A change past what a capture's manifest and patch record is attributed
/// from the captures' lines of the paths that differ from HEAD, on one
/// HEAD. When HEAD moved, or a capture lists none of those paths (more than
/// the 10,000 it lists), the reason says which, and names that limit.
#[test]
fn a_change_past_the_capture_bounds_is_attributed_from_its_changed_lines_or_says_why_not() {
    let lines = |digest: &str| {
        format!(
            "{}{}",
            line("a.txt", "file", &digest.repeat(64)),
            line("b.txt", "file", &"9".repeat(64))
        )
    };
    let mut before = capture(CapturePhase::Before, None);
    before.changed = Some(lines("1"));
    before.head = Some("h1".into());
    let mut after = capture(CapturePhase::After, None);
    after.changed = Some(lines("2"));
    after.head = Some("h1".into());
    assert_eq!(
        attribute_activation(&[before.clone(), after.clone()]),
        ActivationChanges::Changed(["a.txt".to_owned()].into())
    );
    let mut moved = after.clone();
    moved.head = Some("h2".into());
    assert_eq!(
        attribute_activation(&[before.clone(), moved]),
        ActivationChanges::Unknown(
            "HEAD moved between its captures, and a patch is not recorded whole (a capture \
             records at most 512 KiB of it, within its output), so they cannot be compared"
                .into()
        )
    );
    for (name, before, after) in [
        (
            "After",
            before.clone(),
            CaptureFacts {
                changed: None,
                ..after.clone()
            },
        ),
        (
            "Before",
            CaptureFacts {
                changed: None,
                ..before.clone()
            },
            after.clone(),
        ),
    ] {
        let ActivationChanges::Unknown(reason) = attribute_activation(&[before, after]) else {
            panic!("attributed");
        };
        assert_eq!(
            reason,
            format!(
                "its {name} capture lists none of the paths that differ from HEAD (it lists at \
                 most 10,000, within its output), and a patch is not recorded whole (a capture \
                 records at most 512 KiB of it, within its output), so the captures cannot be \
                 compared"
            )
        );
        // With a team node's name and a turn's id, the receipt's warning
        // names it whole.
        let named = format!(
            "team-node-2b98db5db047e10c209d5b91 (generation 10) in turn \
             turn-823c34fb-e018-4c9e-80ff-6a7cea65d4ad: {reason}"
        );
        let warning = crate::bootstrap::receipts::receipt_incomplete(std::slice::from_ref(&named));
        assert!(
            warning.message.contains(&format!("{named}. Review")),
            "{}",
            warning.message
        );
    }
}

/// Source Control's Last turn for a native turn: the paths its activations'
/// captures show they changed, sorted, and a reason instead of an empty
/// list when an activation's changes cannot be known.
#[test]
fn last_turn_names_the_changed_paths_or_why_they_are_unknown() {
    let before = format!(
        "{}{}",
        line("calc.py", "file", &"1".repeat(64)),
        line("README.md", "file", &"4".repeat(64))
    );
    let after = format!(
        "{}{}{}",
        line("calc.py", "file", &"2".repeat(64)),
        line("README.md", "file", &"4".repeat(64)),
        line("tests/test_calc.py", "file", &"5".repeat(64))
    );
    let mut attribution = RunAttribution::default();
    attribution.add_activation(
        "lead (generation 1) in turn t",
        &[
            capture(CapturePhase::Before, Some(&before)),
            capture(CapturePhase::After, Some(&after)),
        ],
    );
    assert_eq!(
        attribution.clone().last_turn(),
        (
            vec!["calc.py".to_string(), "tests/test_calc.py".to_string()],
            None
        )
    );

    // An After capture that could not record the repository says why in
    // its own words, without the host's internal layers.
    let mut unavailable = capture(CapturePhase::After, None);
    unavailable.unavailable = Some(
        "Session dispatch: IO error: repository command did not complete normally: TimedOut."
            .into(),
    );
    assert_eq!(
        attribute_activation(&[capture(CapturePhase::Before, Some(&before)), unavailable]),
        ActivationChanges::Unknown(
            "its After capture is unavailable (repository command did not complete normally: \
             TimedOut)"
                .into()
        )
    );

    // A helper whose After capture is missing: its changes are unknown, and
    // Last turn says so rather than read as complete.
    attribution.add_activation(
        "scout (generation 1) in turn t",
        &[capture(CapturePhase::Before, Some(&after))],
    );
    let (paths, unattributed) = attribution.last_turn();
    assert_eq!(
        paths,
        vec!["calc.py".to_string(), "tests/test_calc.py".to_string()]
    );
    let unattributed = unattributed.unwrap();
    assert!(
        unattributed.contains("scout (generation 1) in turn t: it has no After capture"),
        "{unattributed}"
    );
    assert!(
        unattributed.ends_with("Review All changes."),
        "{unattributed}"
    );

    // A turn none of whose activations could capture changes nothing it
    // can name, and nothing is reported unknown.
    let mut uncaptured = RunAttribution::default();
    uncaptured.add_activation("reader", &[]);
    assert_eq!(uncaptured.last_turn(), (Vec::new(), None));
}
