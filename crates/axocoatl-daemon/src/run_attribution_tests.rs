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
    });
    let mut incomplete = attribution.clone();
    incomplete.last_writer_after = Some(CapturedTree {
        tree_sha256: "f".repeat(64),
        manifest: None,
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
