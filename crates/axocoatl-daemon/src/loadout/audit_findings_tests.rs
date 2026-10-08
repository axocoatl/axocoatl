//! The host's checks of reported findings, against the recorded answers of
//! the 1.3.0 re-smokes and the line counts of their fixture repositories.
use super::*;
use crate::loadout::audit::files::FileKind;
use axocoatl_session::audit_plan::parse_integrated;

/// A recorded answer of the 1.3.0 re-smokes.
fn fixture(name: &str) -> String {
    let path = format!(
        "{}/../axocoatl-session/tests/fixtures/answers/{name}",
        env!("CARGO_MANIFEST_DIR")
    );
    std::fs::read_to_string(&path).unwrap_or_else(|error| panic!("{path}: {error}"))
}

/// The re-smoke's `repo2` as the host listed it (`repo1` is the same
/// without `ops/` and `tests/__init__.py`), with each file's line count.
const REPO2: [(&str, u64, u64); 12] = [
    ("README.md", 120, 3),
    ("auth/__init__.py", 0, 0),
    ("auth/tokens.py", 671, 21),
    ("billing/__init__.py", 0, 0),
    ("billing/pagination.py", 811, 25),
    ("ingest/feed.go", 802, 40),
    ("ingest/go.mod", 30, 3),
    ("notify/__init__.py", 0, 0),
    ("notify/webhook.py", 640, 20),
    ("ops/rotate_keys.py", 340, 12),
    ("tests/__init__.py", 0, 0),
    ("tests/test_billing.py", 400, 16),
];

fn repo2() -> Vec<RepoFile> {
    REPO2
        .iter()
        .map(|(path, size, _)| RepoFile {
            path: (*path).to_owned(),
            size: *size,
            kind: if *size == 0 {
                FileKind::Empty
            } else {
                FileKind::Text
            },
        })
        .collect()
}

fn lines_of(path: &str) -> Option<u64> {
    REPO2
        .iter()
        .find(|(listed, _, _)| *listed == path)
        .map(|(_, _, lines)| *lines)
}

/// A finding's id, location and line.
type Located<'a> = (&'a str, Option<&'a str>, Option<Option<u32>>);

fn located(findings: &[Finding]) -> Vec<Located<'_>> {
    findings
        .iter()
        .map(|finding| {
            (
                finding.id.as_str(),
                finding.location.as_deref(),
                finding.line,
            )
        })
        .collect()
}

/// resmoke10 out4: the planner invented `notify/rotate_keys.py`, the notify
/// worker reported `F2` at it, and the integrator kept it. The host's
/// listing has no such file, so it is a note and not a finding.
#[test]
fn resmoke10_out4_a_finding_at_a_file_the_repository_lacks_is_a_note() {
    let files = repo2();
    let listed = Listed::new(&files, Path::new("/srv/fixtures/repo2"));
    let findings = parse_integrated(&fixture("audit-resmoke10-out4-integrator.txt")).unwrap();
    assert_eq!(findings.len(), 7);
    let (kept, notes) = check_locations(findings.clone(), &listed, lines_of);
    assert_eq!(
        notes,
        [
            "finding at a path that does not exist: notify/rotate_keys.py (Missing Webhook Key \
          Rotation)"
        ]
    );
    let expected: Vec<Finding> = findings
        .into_iter()
        .filter(|finding| finding.id != "notify-F2")
        .collect();
    assert_eq!(kept, expected, "the other six pass unchanged");
    // The same worker's own report, before integration.
    let worker = axocoatl_session::audit_plan::parse_area_report(
        &fixture("audit-resmoke10-out4-worker-notify.txt"),
        "notify",
    )
    .unwrap();
    let (kept, notes) = check_locations(worker.findings, &listed, lines_of);
    assert_eq!(
        located(&kept),
        [
            ("notify-F1", Some("notify/webhook.py:7"), Some(Some(7))),
            ("notify-F3", Some("notify/webhook.py:19"), Some(Some(19)))
        ]
    );
    assert_eq!(notes.len(), 1);
    // The same file in repo1, which has no ops/: the rest worker's
    // ops/rotate_keys.py finding would not exist there either.
    let repo1: Vec<RepoFile> = files
        .into_iter()
        .filter(|file| !file.path.starts_with("ops/") && file.path != "tests/__init__.py")
        .collect();
    let listed = Listed::new(&repo1, Path::new("/srv/fixtures/repo1"));
    let findings = parse_integrated(&fixture("audit-resmoke10-out4-integrator.txt")).unwrap();
    let (kept, notes) = check_locations(findings, &listed, lines_of);
    assert_eq!(kept.len(), 5);
    assert_eq!(
        notes[1],
        "finding at a path that does not exist: ops/rotate_keys.py (Command Injection \
         Vulnerability in rotate_keys.py)"
    );
}

/// A file written with `./`, an absolute path (the host's repository or a
/// checkout elsewhere) or the repository's directory in front is the
/// listed file; the location then names it as the listing does.
#[test]
fn a_finding_s_path_is_looked_up_after_its_prefix_is_removed() {
    let files = repo2();
    let listed = Listed::new(&files, Path::new("/srv/fixtures/repo2"));
    for written in [
        "notify/webhook.py",
        "./notify/webhook.py",
        "/srv/fixtures/repo2/notify/webhook.py",
        "/workspace/repo/notify/webhook.py",
        "repo2/notify/webhook.py",
        "notify/./webhook.py",
        " `notify/webhook.py` ",
    ] {
        assert_eq!(
            listed.resolve(written).as_deref(),
            Some("notify/webhook.py"),
            "{written:?}"
        );
    }
    for written in [
        "notify/rotate_keys.py",
        "../repo2/notify/webhook.py",
        "/srv/fixtures/repo2/notify",
        "webhook.py",
        "",
    ] {
        assert_eq!(listed.resolve(written), None, "{written:?}");
    }
    let finding = |location: &str| Finding {
        id: "notify-F1".into(),
        source: axocoatl_session::run_outcome::FindingSource::Integrator,
        title: "Hardcoded Webhook Token".into(),
        detail: String::new(),
        severity: None,
        area: Some("notify".into()),
        location: Some(location.into()),
        line: Some(
            axocoatl_session::audit_plan::split_location(location)
                .unwrap()
                .1,
        ),
        repro: None,
    };
    let (kept, notes) = check_locations(
        vec![
            finding("/workspace/repo/notify/webhook.py:7"),
            finding("repo2/notify/webhook.py"),
        ],
        &listed,
        lines_of,
    );
    assert!(notes.is_empty(), "{notes:?}");
    assert_eq!(
        located(&kept),
        [
            ("notify-F1", Some("notify/webhook.py:7"), Some(Some(7))),
            ("notify-F1", Some("notify/webhook.py"), Some(None))
        ]
    );
}

/// No recorded finding of the fixture repositories named a line past the
/// end of its file (resmoke10 out3's `feed.go:18` and `webhook.py:6` are
/// wrong lines inside the files), so the recorded out3 answer passes
/// unchanged; with its `ingest-F1` moved past `feed.go`'s 40 lines, and a
/// finding at an empty file, the file stays and the line becomes unknown.
#[test]
fn resmoke10_out3_a_line_beyond_its_file_becomes_unknown() {
    let files = repo2();
    let listed = Listed::new(&files, Path::new("/srv/fixtures/repo1"));
    let findings = parse_integrated(&fixture("audit-resmoke10-out3-integrator.txt")).unwrap();
    let (kept, notes) = check_locations(findings.clone(), &listed, lines_of);
    assert!(notes.is_empty(), "{notes:?}");
    assert_eq!(kept, findings);

    let mut moved = findings;
    let ingest = moved.iter_mut().find(|f| f.id == "ingest-F1").unwrap();
    assert_eq!(ingest.location.as_deref(), Some("ingest/feed.go:18"));
    ingest.location = Some("ingest/feed.go:41".into());
    ingest.line = Some(Some(41));
    let mut empty = moved[0].clone();
    empty.id = "auth-F2".into();
    empty.title = "Nothing exported".into();
    empty.location = Some("./auth/__init__.py:1".into());
    empty.line = Some(Some(1));
    moved.push(empty);
    let (kept, notes) = check_locations(moved, &listed, |path| {
        assert_ne!(path, "auth/__init__.py", "an empty file is not read");
        lines_of(path)
    });
    assert_eq!(
        notes,
        [
            "finding at line 41 of ingest/feed.go, which has 40 lines: its line is left \
             unknown (Missing error check after JSON unmarshaling)",
            "finding at line 1 of auth/__init__.py, which has 0 lines: its line is left \
             unknown (Nothing exported)"
        ]
    );
    let ingest = kept.iter().find(|f| f.id == "ingest-F1").unwrap();
    assert_eq!(ingest.location.as_deref(), Some("ingest/feed.go"));
    assert_eq!(ingest.line, Some(None));
    let empty = kept.iter().find(|f| f.id == "auth-F2").unwrap();
    assert_eq!(empty.location.as_deref(), Some("auth/__init__.py"));
    assert_eq!(empty.line, Some(None));
    assert_eq!(kept.len(), 10);
}

#[test]
fn the_host_counts_a_file_s_lines() {
    let repo = tempfile::tempdir().unwrap();
    for (name, text, lines) in [
        ("a", "", 0),
        ("b", "one", 1),
        ("c", "one\n", 1),
        ("d", "one\ntwo", 2),
        ("e", "one\ntwo\n\n", 3),
    ] {
        std::fs::write(repo.path().join(name), text).unwrap();
        assert_eq!(host_line_count(repo.path(), name), Some(lines), "{name}");
    }
    assert_eq!(host_line_count(repo.path(), "missing"), None);
    // A file the listing left out (an ignored one) still exists.
    let listed = Listed::new(&[], repo.path());
    assert_eq!(listed.resolve("./d").as_deref(), Some("d"));
    assert_eq!(listed.resolve("z"), None);
}

/// resmoke10 out3: the integrator kept the billing and rest workers'
/// findings of `billing/pagination.py` side by side. `billing-F3` and
/// `rest-F1` are one claim at line 8 and become one; the two pairs at
/// lines 15/16 and 24 make different claims (overflow against IndexError,
/// overflow against missing keys) and stay.
#[test]
fn resmoke10_out3_near_duplicates_after_integration_are_removed() {
    let findings = parse_integrated(&fixture("audit-resmoke10-out3-integrator.txt")).unwrap();
    assert_eq!(findings.len(), 9);
    let (kept, note) = remove_near_duplicates(findings);
    let ids: Vec<&str> = kept.iter().map(|finding| finding.id.as_str()).collect();
    // rest-F1 says more (297 bytes of detail against 279) and takes
    // billing-F3's place.
    assert_eq!(
        ids,
        [
            "auth-F1",
            "billing-F1",
            "billing-F2",
            "rest-F1",
            "ingest-F1",
            "notify-F1",
            "rest-F2",
            "rest-F3"
        ]
    );
    assert_eq!(
        note.unwrap(),
        "the host removed 1 finding that repeats another at the same file and line (at most 2 \
         lines apart) with a similar title, keeping the more specific: billing-F3 (kept rest-F1)"
    );
}

/// resmoke9 out3 kept `notify-F1` "Hardcoded API Token" and `rest-F1`
/// "Hardcoded API token in webhook module" at `webhook.py:7`; resmoke10 out1
/// kept the off-by-one at lines 15 and 16 under two different claims.
#[test]
fn recorded_integrations_lose_only_repeated_claims() {
    let findings = parse_integrated(&fixture("audit-resmoke9-out3-integrator.txt")).unwrap();
    let total = findings.len();
    let (kept, note) = remove_near_duplicates(findings);
    assert_eq!(kept.len(), total - 1);
    assert!(kept.iter().all(|finding| finding.id != "rest-F1"));
    assert!(note.unwrap().ends_with(": rest-F1 (kept notify-F1)"));
    // "Hardcoded Webhook URL" and "Insecure webhook URL domain" at line 5
    // share two of five words: both stay.
    assert!(kept.iter().any(|finding| finding.id == "notify-F2"));
    assert!(kept.iter().any(|finding| finding.id == "rest-F4"));

    let findings = parse_integrated(&fixture("audit-resmoke10-out1-integrator.txt")).unwrap();
    let (kept, note) = remove_near_duplicates(findings.clone());
    assert_eq!(kept, findings);
    assert_eq!(note, None);
}

#[test]
fn titles_are_similar_when_most_of_their_words_are_shared() {
    assert!(similar_titles(
        "Potential Integer Overflow in page_count function",
        "Integer Overflow in page_count Function"
    ));
    assert!(similar_titles(
        "Hardcoded API Token",
        "Hardcoded API token in webhook module"
    ));
    // Recorded pairs that name different defects.
    for (a, b) in [
        ("Hardcoded Webhook Token", "Hardcoded Webhook URL"),
        (
            "Potential Integer Overflow in get_page function",
            "Potential IndexError in get_page Function",
        ),
        (
            "Potential Integer Overflow in invoice_total function",
            "Missing Input Validation in invoice_total Function",
        ),
        ("Potential issue", "Possible bug"),
    ] {
        assert!(!similar_titles(a, b), "{a} / {b}");
    }
}

/// Lines more than two apart, one line unknown, or another file: not one
/// finding. Both lines unknown at one file with one title: one.
#[test]
fn near_duplicates_need_the_same_file_and_a_nearby_line() {
    let finding = |id: &str, location: &str, detail: &str| {
        let (location, line) = axocoatl_session::audit_plan::split_location(location).unwrap();
        Finding {
            id: id.into(),
            source: axocoatl_session::run_outcome::FindingSource::Integrator,
            title: "Off-by-one in get_page".into(),
            detail: detail.into(),
            severity: None,
            area: None,
            location: Some(location),
            line: Some(line),
            repro: None,
        }
    };
    let findings = vec![
        finding("a", "billing/pagination.py:16", "end drops the last item"),
        finding("b", "billing/pagination.py:19", ""),
        finding("c", "billing/pagination.py", ""),
        finding("d", "billing/other.py:16", ""),
        finding("e", "billing/pagination.py:14", ""),
        finding("f", "billing/pagination.py", "longer detail"),
    ];
    let (kept, note) = remove_near_duplicates(findings);
    let ids: Vec<&str> = kept.iter().map(|finding| finding.id.as_str()).collect();
    assert_eq!(ids, ["a", "b", "f", "d"]);
    assert!(note.unwrap().contains("e (kept a), c (kept f)"));
}
