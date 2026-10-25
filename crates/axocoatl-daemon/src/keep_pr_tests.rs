//! Keep as PR tests: pure parts, then host git against temporary
//! repositories, a local bare remote and a fake `gh`.
use super::*;
use axocoatl_session::execution_content::{ActivationRepositorySnapshot, RepositorySnapshotPhase};
use axocoatl_session::run_outcome::{
    Adjudication, CheckReport, CheckResult, FailureClass, Finding, LoadoutRef, ModelIdentity,
    NetworkSummary, NotCovered, ReproResult, ReviewFinding, ReviewOutcome, ReviewRound, RunTurnRef,
    RunUsage, RunWarning, TurnState,
};
use std::collections::BTreeMap;
use std::sync::Mutex;

const RUN: &str = "run-0f8c1a2b-3c4d-4e5f-8a9b-0c1d2e3f4a5b";
const SESSION: &str = "ses-11111111-2222-4333-8444-555555555555";

/// A passing fix Outcome written now: the run ended after every change the
/// test made before calling this.
fn outcome() -> RunOutcome {
    RunOutcome {
        schema: RUN_OUTCOME_SCHEMA.into(),
        run_id: RUN.into(),
        session_id: SESSION.into(),
        workspace_id: "wsp-1".into(),
        loadout: LoadoutRef {
            id: "fix".into(),
            version: 1,
            kind: "fix".into(),
            digest: "ab".repeat(32),
            builtin: true,
        },
        task: "Fix the off-by-one in pagination\nSecond line".into(),
        started_at_ms: 1,
        finished_at_ms: now_ms(),
        verdict: RunVerdict::Pass,
        exit_code: 0,
        attention: Vec::new(),
        turns: vec![RunTurnRef {
            turn_id: "turn-1".into(),
            purpose: "run".into(),
            state: TurnState::Completed,
        }],
        checks: vec![CheckResult {
            name: "tests".into(),
            argv: vec!["npm".into(), "test".into()],
            state: CheckState::Passed,
            timeout_ms: 180_000,
            exit_code: Some(0),
            stdout_tail: String::new(),
            stderr_tail: String::new(),
            candidate_sha256: None,
            report: Some(CheckReport {
                format: "junit".into(),
                sha256: "0".repeat(64),
                tests: Vec::new(),
                passed: 12,
                failed: 0,
                skipped: 1,
                errors: 0,
                truncated: 0,
            }),
            reason: None,
        }],
        review: Some(ReviewOutcome {
            reviewer: ModelIdentity {
                provider: "openrouter".into(),
                model: "openai/gpt-oss-120b".into(),
                runtime: "native".into(),
            },
            max_rounds: 3,
            rounds: vec![
                ReviewRound {
                    round: 1,
                    verdict: ReviewVerdictKind::Changes,
                    passed: false,
                    findings_text: "F1 off by one\nF2 missing test".into(),
                    findings: vec![
                        ReviewFinding {
                            id: "F1".into(),
                            text: "off by one".into(),
                        },
                        ReviewFinding {
                            id: "F2".into(),
                            text: "missing test".into(),
                        },
                    ],
                    continued: true,
                    candidate_sha256: None,
                },
                ReviewRound {
                    round: 2,
                    verdict: ReviewVerdictKind::Approve,
                    passed: true,
                    findings_text: String::new(),
                    findings: Vec::new(),
                    continued: false,
                    candidate_sha256: None,
                },
            ],
            passed: true,
            state: "approved".into(),
            reason: String::new(),
        }),
        adjudications: vec![
            Adjudication {
                round: 1,
                finding_id: "F1".into(),
                finding: "off by one in page | size".into(),
                decision: AdjudicationDecision::Accept,
                reason: "fixed the bound; ping @maintainers <script>alert(1)</script>".into(),
                writer_generation: Some(2),
            },
            Adjudication {
                round: 1,
                finding_id: "F2".into(),
                finding: "missing test".into(),
                decision: AdjudicationDecision::Reject,
                reason: "covered by pagination.test.ts already".into(),
                writer_generation: Some(2),
            },
        ],
        findings: vec![Finding {
            id: "B2".into(),
            source: FindingSource::Explorer,
            title: "search ignores accents".into(),
            detail: String::new(),
            severity: Some(Severity::Low),
            area: Some("search".into()),
            location: None,
            line: None,
            repro: Some(ReproResult {
                path: ".axocoatl/qa/b2.spec.ts".into(),
                sha256: None,
                classification: ReproClassification::FailsOnCleanBuild,
                target: None,
                reference: None,
            }),
        }],
        not_covered: Vec::new(),
        unreadable_findings: Vec::new(),
        notes: Vec::new(),
        warnings: vec![RunWarning {
            code: "same_model_reviewer".into(),
            message: "The reviewer runs the writer's model.".into(),
        }],
        usage: RunUsage {
            input_tokens: 1200,
            output_tokens: 300,
            cost_microunits: 12_345,
            complete: false,
            cost_known: true,
            cost_computed: false,
            retries: 1,
            unreported: Vec::new(),
        },
        network: NetworkSummary::default(),
        ollama_versions: Default::default(),
        openrouter_endpoints: Default::default(),
        keep: None,
        error: None,
    }
}

fn manifest(head: Option<String>, started_at_ms: u64) -> RunManifest {
    RunManifest {
        schema: "axocoatl.run-manifest/1".into(),
        run_id: RUN.into(),
        session_id: SESSION.into(),
        workspace_id: "wsp-1".into(),
        loadout: outcome().loadout,
        loadout_text: "schema: axocoatl.loadout/1\n".into(),
        params: BTreeMap::new(),
        credentials: BTreeMap::new(),
        task: outcome().task,
        repo: "/repo".into(),
        repo_head: head,
        dirty_paths: Vec::new(),
        started_at_ms,
        options: serde_json::Value::Null,
    }
}

/// A passing fix Outcome of `run_id` in `session_id`.
pub(crate) fn outcome_for(run_id: &str, session_id: &str) -> RunOutcome {
    let mut outcome = outcome();
    outcome.run_id = run_id.into();
    outcome.session_id = session_id.into();
    outcome
}

/// The manifest of `run_id` in `session_id`, with no HEAD yet.
pub(crate) fn manifest_for(run_id: &str, session_id: &str) -> RunManifest {
    let mut manifest = manifest(None, 0);
    manifest.run_id = run_id.into();
    manifest.session_id = session_id.into();
    manifest
}

fn request() -> KeepPrRequest {
    KeepPrRequest {
        run_id: RUN.into(),
        branch: None,
        open_pr: false,
        remote: None,
        title: None,
    }
}

// ---------------------------------------------------------------------------
// Pure

#[test]
fn requests_are_validated_before_anything_is_read() {
    assert!(validate_request(&request()).is_ok());
    let invalid = |mutate: fn(&mut KeepPrRequest)| {
        let mut request = request();
        mutate(&mut request);
        matches!(validate_request(&request), Err(KeepPrError::Invalid(_)))
    };
    assert!(invalid(|r| r.run_id = "run-1".into()));
    assert!(invalid(|r| r.run_id = RUN.to_uppercase()));
    for branch in [
        "",
        "-x",
        "/x",
        ".x",
        "x/",
        "x.",
        "a..b",
        "a//b",
        "a b",
        "a~b",
        "a^b",
        "a:b",
        "a?b",
        "a*b",
        "a[b",
        "a\\b",
        "x@{1}",
        "HEAD",
        "@",
        "a/.hidden",
        "a.lock",
        "é",
    ] {
        let mut request = request();
        request.branch = Some(branch.into());
        assert!(
            matches!(validate_request(&request), Err(KeepPrError::Invalid(_))),
            "{branch:?}"
        );
    }
    let mut long = request();
    long.branch = Some("a".repeat(201));
    assert!(validate_request(&long).is_err());
    for branch in ["axocoatl/fix-0f8c1a2b", "feature/x_y-1.2", "a"] {
        let mut request = request();
        request.branch = Some(branch.into());
        assert!(validate_request(&request).is_ok(), "{branch:?}");
    }
    assert!(invalid(|r| r.remote = Some("-x".into())));
    assert!(invalid(|r| r.remote = Some("a b".into())));
    assert!(invalid(|r| r.remote = Some("x/y".into())));
    assert!(invalid(|r| r.title = Some("  ".into())));
    assert!(invalid(|r| r.title = Some("two\nlines".into())));
    assert!(invalid(|r| r.title = Some("x".repeat(201))));
    let parsed: Result<KeepPrRequest, _> =
        serde_json::from_str(&format!(r#"{{"run_id":"{RUN}","force":true}}"#));
    assert!(parsed.is_err(), "unknown fields are refused");
}

#[test]
fn default_branch_and_title_come_from_the_run() {
    let outcome = outcome();
    assert_eq!(branch_name(&request(), "fix"), "axocoatl/fix-0f8c1a2b");
    let mut named = request();
    named.branch = Some("feature/x".into());
    assert_eq!(branch_name(&named, "fix"), "feature/x");
    assert_eq!(
        commit_title(&request(), &outcome),
        "fix: Fix the off-by-one in pagination"
    );
    let mut long = outcome.clone();
    long.task = "word ".repeat(40);
    let title = commit_title(&request(), &long);
    assert!(
        title.chars().count() <= 73 && title.ends_with('…'),
        "{title}"
    );
    let mut empty = outcome.clone();
    empty.task = "\n \n".into();
    assert_eq!(commit_title(&request(), &empty), format!("fix run {RUN}"));
    let mut titled = request();
    titled.title = Some(" Fix pagination ".into());
    assert_eq!(commit_title(&titled, &outcome), "Fix pagination");
}

#[test]
fn the_body_carries_checks_review_adjudications_not_covered_and_the_record() {
    let body = pr_body(&outcome(), None).unwrap();
    for needle in [
        "### Checks",
        "| tests | passed | 0 | 12 passed, 0 failed, 1 skipped, 0 errors (junit) | npm test |",
        "### Review",
        "`openrouter:openai/gpt-oss-120b`",
        "**approved**",
        "| 1 | changes | F1, F2 | yes |",
        "### Adjudications",
        "| 1 | F1: off by one in page \\| size | accept |",
        "| 1 | F2: missing test | reject | covered by pagination.test.ts already |",
        "### Findings",
        "fails on clean build: .axocoatl/qa/b2.spec.ts",
        "### Not covered",
        "Nothing: every area",
        "### Warnings",
        "`same_model_reviewer`",
        "known subtotal",
        "### Record",
        RUN,
        &format!("axocoatl record verify {RUN}.axorecord.jsonl"),
        &format!("GET /api/runs/{RUN}/record"),
    ] {
        assert!(body.contains(needle), "missing {needle:?} in\n{body}");
    }
    // Quoted run text cannot mention people, inject HTML or break a table.
    assert!(!body.contains("@maintainers"), "{body}");
    assert!(body.contains("&#64;maintainers"));
    assert!(!body.contains("<script>"));
    assert!(body.contains("&lt;script&gt;"));

    let mut failing = outcome();
    failing.not_covered.push(NotCovered {
        area: "gift cards".into(),
        class: FailureClass::ProviderRefusal,
        detail: "classifier stop".into(),
        node_id: None,
        turn_id: None,
    });
    failing.review = None;
    failing.adjudications.clear();
    failing.checks.clear();
    let body = pr_body(&failing, None).unwrap();
    // The Outcome's one rendering of why (`NotCovered::reason`), Markdown
    // escaped.
    assert!(
        body.contains("- **gift cards**: provider\\_refusal: classifier stop"),
        "{body}"
    );
    assert!(body.contains("This run had no required review."));
    assert!(body.contains("This run had no required checks."));
    assert!(body.contains("nothing to adjudicate"));

    let mut huge = outcome();
    huge.adjudications = (0..500)
        .map(|index| Adjudication {
            round: 1,
            finding_id: format!("F{index}"),
            finding: "x".repeat(400),
            decision: AdjudicationDecision::Missing,
            reason: String::new(),
            writer_generation: None,
        })
        .collect();
    let body = pr_body(&huge, None).unwrap();
    assert!(body.chars().count() < MAX_BODY_CHARS + 200);
    assert!(body.contains("…and 400 more adjudications in the run record."));
    assert!(body.contains("**missing**"));
    assert!(body.contains(RUN));

    // A cost the run does not know (a Codex writer's on a model without a
    // price) is what was reserved, never a price; one computed from a Codex
    // writer's reported tokens says so.
    assert!(pr_body(&outcome(), None)
        .unwrap()
        .contains("output tokens, $0.0123"));
    let mut computed = outcome();
    computed.usage.cost_computed = true;
    computed.usage.complete = true;
    let body = pr_body(&computed, None).unwrap();
    assert!(
        body.contains(
            "300 output tokens, $0.0123 (includes cost computed from reported tokens at list \
             prices). Provider"
        ),
        "{body}"
    );
    let mut codex = outcome();
    codex.usage.cost_known = false;
    let body = pr_body(&codex, None).unwrap();
    assert!(
        body.contains("300 output tokens, cost unknown (reserved up to $0.0123)"),
        "{body}"
    );

    let mut other = outcome();
    other.schema = "axocoatl.run-outcome/9".into();
    assert!(matches!(
        pr_body(&other, None),
        Err(KeepPrError::Refused(_))
    ));
}

#[test]
fn keep_results_round_trip_through_run_events() {
    let result = KeepResult {
        branch: "axocoatl/fix-0f8c1a2b".into(),
        commit: "c".repeat(40),
        pull_request_url: None,
        error: None,
    };
    let event = keep_event(&result, 7);
    assert!(matches!(&event, RunEvent::Phase { phase, at_ms: 7, .. } if phase == KEEP_PHASE));
    assert_eq!(keep_result_of(&event), Some(result.clone()));
    assert_eq!(
        keep_result_of(&RunEvent::Phase {
            at_ms: 1,
            phase: "checks".into(),
            detail: "{}".into()
        }),
        None
    );
    let refused = KeepResult {
        branch: "axocoatl/fix-0f8c1a2b".into(),
        commit: String::new(),
        pull_request_url: None,
        error: Some("refused".into()),
    };
    let results = vec![result.clone(), refused];
    assert_eq!(last_kept_branch(&results), Some(&result));
    let error = KeepPrError::AfterBranch {
        branch: "b".into(),
        commit: "c".into(),
        message: "push failed".into(),
    };
    let recorded = error.keep_result("ignored");
    assert_eq!(
        (recorded.branch.as_str(), recorded.commit.as_str()),
        ("b", "c")
    );
    assert!(recorded.error.unwrap().contains("push failed"));
    let recorded = KeepPrError::Refused("no".into()).keep_result("x");
    assert!(recorded.commit.is_empty() && recorded.branch == "x");
}

#[test]
fn errors_map_to_their_http_classes() {
    use crate::DaemonError;
    assert!(matches!(
        DaemonError::from(KeepPrError::Refused("x".into())),
        DaemonError::SessionConflict(_)
    ));
    assert!(matches!(
        DaemonError::from(KeepPrError::Invalid("x".into())),
        DaemonError::InvalidRequest(_)
    ));
    assert!(matches!(
        DaemonError::from(KeepPrError::NotImplemented("x")),
        DaemonError::NotImplemented("x")
    ));
    assert!(matches!(
        DaemonError::from(KeepPrError::Git("x".into())),
        DaemonError::Session(_)
    ));
    assert!(matches!(
        record_error(RunRecordError::NotImplemented("RunRecordStore::open")),
        KeepPrError::NotImplemented(_)
    ));
    assert!(matches!(
        record_error(RunRecordError::NotFound(RUN.into())),
        KeepPrError::Refused(_)
    ));
}

/// An in-memory run record.
#[derive(Default)]
pub(crate) struct MemoryRecord {
    pub manifests: Mutex<BTreeMap<String, RunManifest>>,
    pub outcomes: Mutex<BTreeMap<String, RunOutcome>>,
    pub keeps: Mutex<Vec<(String, KeepResult)>>,
    pub receipts: Mutex<BTreeMap<String, Vec<u8>>>,
}

impl KeepRunRecord for MemoryRecord {
    fn manifest(&self, run_id: &str) -> Result<RunManifest, KeepPrError> {
        self.manifests
            .lock()
            .unwrap()
            .get(run_id)
            .cloned()
            .ok_or_else(|| record_error(RunRecordError::NotFound(run_id.into())))
    }
    fn outcome(&self, run_id: &str) -> Result<Option<RunOutcome>, KeepPrError> {
        Ok(self.outcomes.lock().unwrap().get(run_id).cloned())
    }
    fn keep_results(&self, run_id: &str) -> Result<Vec<KeepResult>, KeepPrError> {
        Ok(self
            .keeps
            .lock()
            .unwrap()
            .iter()
            .filter(|(run, _)| run == run_id)
            .map(|(_, result)| result.clone())
            .collect())
    }
    fn record_keep(&self, run_id: &str, result: &KeepResult) -> Result<(), KeepPrError> {
        self.keeps
            .lock()
            .unwrap()
            .push((run_id.into(), result.clone()));
        Ok(())
    }
    fn receipt(&self, run_id: &str) -> Result<Option<Vec<u8>>, KeepPrError> {
        Ok(self.receipts.lock().unwrap().get(run_id).cloned())
    }
}

#[test]
fn only_a_finished_passing_run_of_the_session_loads() {
    let record = MemoryRecord::default();
    assert!(matches!(
        load_run(&record, SESSION, RUN),
        Err(KeepPrError::Refused(message)) if message.contains("there is no run")
    ));
    record
        .manifests
        .lock()
        .unwrap()
        .insert(RUN.into(), manifest(Some("a".repeat(40)), 1));
    assert!(matches!(
        load_run(&record, "ses-other", RUN),
        Err(KeepPrError::Refused(message)) if message.contains("is not a run of Session")
    ));
    assert!(matches!(
        load_run(&record, SESSION, RUN),
        Err(KeepPrError::Refused(message)) if message.contains("has not finished")
    ));
    let mut attention = outcome();
    attention.verdict = RunVerdict::NeedsAttention;
    attention.exit_code = 2;
    attention.attention = vec!["1 area was not covered".into()];
    record
        .outcomes
        .lock()
        .unwrap()
        .insert(RUN.into(), attention);
    assert!(matches!(
        load_run(&record, SESSION, RUN),
        Err(KeepPrError::Refused(message))
            if message.contains("did not pass") && message.contains("1 area was not covered")
    ));
    record
        .outcomes
        .lock()
        .unwrap()
        .insert(RUN.into(), outcome());
    let (loaded_manifest, loaded_outcome) = load_run(&record, SESSION, RUN).unwrap();
    assert_eq!(loaded_manifest.run_id, RUN);
    assert_eq!(loaded_outcome.verdict, RunVerdict::Pass);
}

fn capture(phase: CapturePhase) -> CaptureFacts {
    CaptureFacts {
        phase,
        unavailable: None,
        tree_sha256: Some("t".repeat(64)),
        judged_sha256: None,
        compared: None,
        manifest: None,
        patch: None,
        changed: None,
        head: None,
        unread: Vec::new(),
    }
}

fn manifest_line(path: &str, digest: &str) -> String {
    use base64::Engine as _;
    format!(
        "{}\t644\tfile\t{digest}\n",
        base64::engine::general_purpose::STANDARD.encode(path)
    )
}

#[test]
fn activation_changes_come_from_its_own_captures() {
    let set = |paths: &[&str]| {
        paths
            .iter()
            .map(|path| path.to_string())
            .collect::<BTreeSet<_>>()
    };
    // A limited writer: the After capture's comparison with its Before.
    let mut before = capture(CapturePhase::Before);
    before.judged_sha256 = Some("j".repeat(64));
    let mut after = capture(CapturePhase::After);
    after.compared = Some((
        "j".repeat(64),
        vec!["lib/a.rs".into(), ".git/config".into()],
    ));
    assert_eq!(
        attribute_activation(&[before.clone(), after.clone()]),
        ActivationChanges::Changed(set(&["lib/a.rs", ".git/config"]))
    );
    let mut other = after.clone();
    other.compared = Some(("k".repeat(64), Vec::new()));
    assert!(matches!(
        attribute_activation(&[before.clone(), other]),
        ActivationChanges::Unknown(_)
    ));
    // Small repositories: complete manifests.
    let mut before = capture(CapturePhase::Before);
    before.manifest = Some(format!(
        "{}{}{}",
        manifest_line("a.txt", "1"),
        manifest_line("b.txt", "2"),
        manifest_line("same.txt", "3")
    ));
    let mut after = capture(CapturePhase::After);
    after.manifest = Some(format!(
        "{}{}{}",
        manifest_line("a.txt", "9"),
        manifest_line("new file.txt", "4"),
        manifest_line("same.txt", "3")
    ));
    // Captures of the checks around it are not the activation's own.
    let mut check = capture(CapturePhase::Check);
    check.manifest = Some(manifest_line("coverage/out.json", "5"));
    assert_eq!(
        attribute_activation(&[before, check, after]),
        ActivationChanges::Changed(set(&["a.txt", "b.txt", "new file.txt"]))
    );
    // Larger repositories: complete patches against HEAD.
    let dirty = "diff --git a/dirty.txt b/dirty.txt\nindex 1..2 100644\n--- a/dirty.txt\n+++ b/dirty.txt\n@@ -1 +1 @@\n-x\n+y\n";
    let edit = "diff --git a/src/lib.rs b/src/lib.rs\nindex 3..4 100644\n--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -1 +1 @@\n--- a/not-a-path\n+fixed\n";
    let untracked = "diff --git a/src/new.rs b/src/new.rs\nnew file mode 100644\nindex 0..5\n--- /dev/null\n+++ b/src/new.rs\n@@ -0,0 +1 @@\n+fn x() {}\n";
    let quoted = "diff --git \"a/sp\\303\\251cial \\\"q\\\"\" \"b/sp\\303\\251cial \\\"q\\\"\"\nold mode 100644\nnew mode 100755\n";
    let binary =
        "diff --git a/img.png b/img.png\nindex 6..7 100644\nGIT binary patch\nliteral 1\nIcmZp\n\n";
    let mut before = capture(CapturePhase::Before);
    before.patch = Some(dirty.as_bytes().to_vec());
    let mut after = capture(CapturePhase::After);
    after.patch = Some(format!("{dirty}{edit}{untracked}{quoted}{binary}").into_bytes());
    assert_eq!(
        attribute_activation(&[before.clone(), after]),
        ActivationChanges::Changed(set(&[
            "src/lib.rs",
            "src/new.rs",
            "spécial \"q\"",
            "img.png"
        ]))
    );
    let mut renamed = capture(CapturePhase::After);
    renamed.patch = Some(
        format!("{dirty}diff --git a/old.rs b/new.rs\nsimilarity index 90%\nrename from old.rs\nrename to new.rs\n")
            .into_bytes(),
    );
    assert_eq!(
        attribute_activation(&[before.clone(), renamed]),
        ActivationChanges::Changed(set(&["old.rs", "new.rs"]))
    );
    // A change that reverts what was dirty before still names that path.
    let mut reverted = capture(CapturePhase::After);
    reverted.patch = Some(Vec::new());
    assert_eq!(
        attribute_activation(&[before.clone(), reverted]),
        ActivationChanges::Changed(set(&["dirty.txt"]))
    );
    let mut garbage = capture(CapturePhase::After);
    garbage.patch = Some(b"warning: something\n".to_vec());
    assert!(matches!(
        attribute_activation(&[before.clone(), garbage]),
        ActivationChanges::Unknown(_)
    ));
    // A change whose patch is too large to record, in a repository whose
    // manifest is too: on the same HEAD, the lines of the paths that differ
    // from HEAD name what the activation changed, whatever was dirty before.
    let mut large_before = capture(CapturePhase::Before);
    large_before.head = Some("h".repeat(40));
    large_before.changed = Some(format!(
        "{}{}",
        manifest_line("dirty.txt", "1"),
        manifest_line("reverted.txt", "2")
    ));
    let mut large_after = capture(CapturePhase::After);
    large_after.head = Some("h".repeat(40));
    large_after.changed = Some(format!(
        "{}{}{}",
        manifest_line("dirty.txt", "1"),
        manifest_line("src/lib.rs", "3"),
        manifest_line("test_fix", "4")
    ));
    assert_eq!(
        attribute_activation(&[large_before.clone(), large_after.clone()]),
        ActivationChanges::Changed(set(&["reverted.txt", "src/lib.rs", "test_fix"]))
    );
    // HEAD moved: the lines are against different trees, and are not compared.
    let mut moved = large_after.clone();
    moved.head = Some("m".repeat(40));
    assert!(matches!(
        attribute_activation(&[large_before.clone(), moved]),
        ActivationChanges::Unknown(_)
    ));
    // Lines left out of either capture are not guessed at.
    let mut unlisted = large_after;
    unlisted.changed = None;
    assert!(matches!(
        attribute_activation(&[large_before, unlisted]),
        ActivationChanges::Unknown(_)
    ));
    // Captures too large to compare, or missing, are never guessed at.
    assert!(matches!(
        attribute_activation(&[capture(CapturePhase::Before), capture(CapturePhase::After)]),
        ActivationChanges::Unknown(_)
    ));
    assert!(matches!(
        attribute_activation(&[before.clone()]),
        ActivationChanges::Unknown(reason) if reason.contains("no After")
    ));
    let mut stopped = capture(CapturePhase::After);
    stopped.unavailable = Some("Execution was stopped before this repository observation".into());
    assert!(matches!(
        attribute_activation(&[before, stopped]),
        ActivationChanges::Unknown(reason) if reason.contains("unavailable")
    ));
    // A profile that permits no capture (a read-only reviewer) changed nothing visible.
    let mut none = capture(CapturePhase::Before);
    none.unavailable = Some("The approved Agent profile does not permit repository capture".into());
    let mut none_after = none.clone();
    none_after.phase = CapturePhase::After;
    assert_eq!(
        attribute_activation(&[none, none_after]),
        ActivationChanges::NotCaptured
    );
    assert_eq!(attribute_activation(&[]), ActivationChanges::NotCaptured);
}

#[test]
fn recorded_snapshots_convert_to_capture_facts() {
    use axocoatl_session::execution_content::RepositoryComparison;
    use axocoatl_session::turn_contract::*;
    use base64::Engine as _;
    let activation = ActivationRef {
        session_id: SessionId::new("ses").unwrap(),
        turn_id: LogicalTurnId::new("turn-1").unwrap(),
        execution_epoch_id: ExecutionEpochId::new("epoch").unwrap(),
        node_id: TurnNodeId::new("writer").unwrap(),
        generation: 1,
        activation_id: ActivationId::new("act").unwrap(),
    };
    let patch = b"diff --git a/a b/a\n".to_vec();
    let snapshot = ActivationRepositorySnapshot {
        activation,
        phase: RepositorySnapshotPhase::After,
        repository: EvidenceRef::new("repo").unwrap(),
        invocation: None,
        condition_run: None,
        outcome: None,
        head: None,
        tree_sha256: Some("t".repeat(64)),
        manifest_bytes: 3,
        manifest_prefix: "abc".into(),
        manifest_complete: true,
        patch_sha256: None,
        patch_bytes: patch.len() as u64,
        patch_prefix: String::new(),
        patch_complete: true,
        patch_base64: Some(base64::engine::general_purpose::STANDARD.encode(&patch)),
        unavailable: None,
        judged_sha256: None,
        baseline: None,
        compared: Some(RepositoryComparison {
            before_sha256: "b".repeat(64),
            changed_paths: vec!["x".into()],
        }),
        changed_manifest: None,
        unread_paths: vec!["nested".into()],
    };
    let facts = CaptureFacts::from_snapshot(&snapshot);
    assert_eq!(facts.phase, CapturePhase::After);
    assert!(facts.usable());
    assert_eq!(facts.manifest.as_deref(), Some("abc"));
    assert_eq!(facts.patch.as_deref(), Some(patch.as_slice()));
    assert_eq!(facts.compared.unwrap().1, vec!["x".to_string()]);
    assert_eq!(facts.unread, vec!["nested".to_string()]);
    let mut partial = snapshot.clone();
    partial.manifest_complete = false;
    partial.patch_complete = false;
    partial.phase = RepositorySnapshotPhase::AfterCheck { index: 0 };
    let facts = CaptureFacts::from_snapshot(&partial);
    assert_eq!(facts.phase, CapturePhase::Check);
    assert!(facts.manifest.is_none() && facts.patch.is_none());
}

#[test]
fn run_attribution_collects_and_refuses_unknowns() {
    let mut attribution = RunAttribution::default();
    let mut before = capture(CapturePhase::Before);
    before.manifest = Some(manifest_line("a.txt", "1"));
    let mut after = capture(CapturePhase::After);
    after.manifest = Some(manifest_line("a.txt", "2"));
    attribution.add_activation("writer", &[before.clone(), after]);
    attribution.add_activation("reviewer", &[]);
    assert!(attribution.unattributable.is_empty());
    attribution.add_activation("writer 2", &[before]);
    assert_eq!(attribution.paths.iter().collect::<Vec<_>>(), vec!["a.txt"]);
    assert_eq!(attribution.unattributable.len(), 1);
    assert!(attribution.unattributable[0].starts_with("writer 2: "));
    attribution.add_legacy_turn("turn-0", vec!["b.txt".into()]);
    assert!(attribution.paths.contains("b.txt"));
    assert_eq!(attribution.notes.len(), 1);
}

#[test]
fn small_helpers() {
    let dirty = vec![
        "a.txt".to_string(),
        "build/".to_string(),
        "old.rs -> new.rs".to_string(),
        "vendor".to_string(),
    ];
    for path in ["a.txt", "build/x.o", "old.rs", "new.rs", "vendor/lib.c"] {
        assert!(was_dirty(path, &dirty), "{path}");
    }
    for path in ["a.txt.bak", "builder", "src/a.txt", "vendors/x"] {
        assert!(!was_dirty(path, &dirty), "{path}");
    }
    assert_eq!(
        porcelain_paths(b" M a.txt\0?? new file.txt\0 D gone\0R  to\0from\0"),
        ["a.txt", "from", "gone", "new file.txt", "to"]
            .iter()
            .map(|path| path.to_string())
            .collect()
    );
    assert!(is_git_internal(".git/config"));
    assert!(is_git_internal("vendor/x/.GIT/hooks/pre-push"));
    assert!(!is_git_internal(".github/workflows/ci.yml"));
    assert!(!is_git_internal(".gitignore"));
    assert!(is_plain_path("a/b.txt"));
    for bad in ["", "/a", "a//b", "a/../b", "./a", "a\0b"] {
        assert!(!is_plain_path(bad), "{bad:?}");
    }
}

// ---------------------------------------------------------------------------
// Host git

fn sh(dir: &Path, script: &str) -> String {
    let output = std::process::Command::new("sh")
        .arg("-c")
        .arg(script)
        .current_dir(dir)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_AUTHOR_NAME", "Setup")
        .env("GIT_AUTHOR_EMAIL", "setup@example.invalid")
        .env("GIT_COMMITTER_NAME", "Setup")
        .env("GIT_COMMITTER_EMAIL", "setup@example.invalid")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{script}\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

fn write(path: &Path, text: &str) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(path, text).unwrap();
}

#[cfg(unix)]
fn executable(path: &Path, text: &str) {
    use std::os::unix::fs::PermissionsExt;
    write(path, text);
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

struct Fixture {
    dir: tempfile::TempDir,
    repo: PathBuf,
    remote: PathBuf,
    data: SecureDir,
    tools: HostTools,
    git_log: PathBuf,
    gh_log: PathBuf,
    gh_body: PathBuf,
    head: String,
}

/// Every file of the repository's working tree (outside `.git`), its HEAD
/// and index bytes, and its current branch.
#[derive(Debug, PartialEq, Eq)]
struct Checkout {
    files: BTreeMap<PathBuf, Vec<u8>>,
    head: Vec<u8>,
    index: Vec<u8>,
    branch: String,
}

fn checkout(repo: &Path) -> Checkout {
    fn walk(root: &Path, dir: &Path, files: &mut BTreeMap<PathBuf, Vec<u8>>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let entry = entry.unwrap();
            let path = entry.path();
            if path.file_name().is_some_and(|name| name == ".git") {
                continue;
            }
            if entry.file_type().unwrap().is_dir() {
                walk(root, &path, files);
            } else {
                files.insert(
                    path.strip_prefix(root).unwrap().to_path_buf(),
                    std::fs::read(&path).unwrap(),
                );
            }
        }
    }
    let mut files = BTreeMap::new();
    walk(repo, repo, &mut files);
    Checkout {
        files,
        head: std::fs::read(repo.join(".git/HEAD")).unwrap(),
        index: std::fs::read(repo.join(".git/index")).unwrap(),
        branch: sh(repo, "git symbolic-ref --short HEAD"),
    }
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::Builder::new()
            .prefix("keep-pr-")
            .tempdir()
            .unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let repo = root.join("repo");
        let remote = root.join("remote.git");
        std::fs::create_dir_all(&repo).unwrap();
        sh(&root, "git init -q --bare -b main remote.git");
        sh(
            &repo,
            "git init -q -b work && git config core.autocrlf false",
        );
        write(&repo.join("a.txt"), "one\n");
        write(&repo.join("b.txt"), "two\n");
        write(&repo.join("docs/c.md"), "# doc\n");
        write(&repo.join(".gitignore"), "target/\n");
        sh(
            &repo,
            "git add -A && git commit -q -m initial && git push -q ../remote.git work:main \
             && git remote add origin https://github.com/acme/widgets.git",
        );
        let head = sh(&repo, "git rev-parse HEAD");
        let global = root.join("gitconfig");
        // The URL rewrite comes from a conditional include, as a per-folder
        // credential or SSH key setup would: Keep must resolve it in the
        // repository's context even though its own Git directory is elsewhere.
        let work = root.join("work.gitconfig");
        write(
            &work,
            &format!(
                "[url \"{}\"]\n\tinsteadOf = https://github.com/acme/widgets.git\n",
                remote.display()
            ),
        );
        write(
            &global,
            &format!(
                "[user]\n\tname = Person\n\temail = person@example.invalid\n[includeIf \"gitdir:{}/\"]\n\tpath = {}\n",
                repo.display(),
                work.display()
            ),
        );
        let real_git = sh(&root, "command -v git");
        let bin = root.join("bin");
        let git_log = root.join("git.log");
        let gh_log = root.join("gh.log");
        let gh_body = root.join("gh-body.md");
        #[cfg(unix)]
        {
            executable(
                &bin.join("git"),
                &format!(
                    "#!/bin/sh\nfor arg in \"$@\"; do printf '%s\\037' \"$arg\"; done >> \"$KEEP_TEST_GIT_LOG\"\nprintf '\\n' >> \"$KEEP_TEST_GIT_LOG\"\nexec '{real_git}' \"$@\"\n"
                ),
            );
            executable(
                &bin.join("gh"),
                "#!/bin/sh\nfor arg in \"$@\"; do printf '%s\\037' \"$arg\"; done >> \"$KEEP_TEST_GH_LOG\"\nprintf '\\ncwd=%s\\n' \"$(pwd -P)\" >> \"$KEEP_TEST_GH_LOG\"\nwhile [ \"$#\" -gt 0 ]; do\n  if [ \"$1\" = --body-file ]; then cp \"$2\" \"$KEEP_TEST_GH_BODY\"; fi\n  shift\ndone\nif [ -n \"$KEEP_TEST_GH_FAIL\" ] && [ ! -e \"$KEEP_TEST_GH_FAIL\" ]; then\n  : > \"$KEEP_TEST_GH_FAIL\"\n  echo 'HTTP 502: try again' >&2\n  exit 1\nfi\necho 'Creating pull request'\necho 'https://github.com/acme/widgets/pull/7'\n",
            );
        }
        let path = format!(
            "{}:{}",
            bin.display(),
            std::env::var("PATH").unwrap_or_default()
        );
        let tools = HostTools {
            path: Some(path.into()),
            env: vec![
                ("GIT_CONFIG_GLOBAL".into(), global.into()),
                ("GIT_CONFIG_NOSYSTEM".into(), "1".into()),
                ("KEEP_TEST_GIT_LOG".into(), git_log.clone().into()),
                ("KEEP_TEST_GH_LOG".into(), gh_log.clone().into()),
                ("KEEP_TEST_GH_BODY".into(), gh_body.clone().into()),
            ],
        };
        let data = SecureDir::open_or_create_all(root.join("data")).unwrap();
        // Repository settings written above predate the run.
        std::thread::sleep(std::time::Duration::from_millis(20));
        Self {
            dir,
            repo,
            remote,
            data,
            tools,
            git_log,
            gh_log,
            gh_body,
            head,
        }
    }

    /// What a run did: edit, add and delete three paths; something else
    /// changed two more.
    fn run_changes(&self) -> RunAttribution {
        write(&self.repo.join("a.txt"), "one, fixed\n");
        write(&self.repo.join("src/new.rs"), "fn new() {}\n");
        std::fs::remove_file(self.repo.join("b.txt")).unwrap();
        write(&self.repo.join("docs/c.md"), "# doc, edited by hand\n");
        write(&self.repo.join("notes.txt"), "scratch\n");
        write(&self.repo.join("target/out.bin"), "ignored\n");
        // The run ends after its changes (an Outcome made later says so).
        std::thread::sleep(std::time::Duration::from_millis(20));
        RunAttribution {
            paths: ["a.txt", "src/new.rs", "b.txt"]
                .iter()
                .map(|path| path.to_string())
                .collect(),
            ..RunAttribution::default()
        }
    }

    fn manifest(&self) -> RunManifest {
        manifest(Some(self.head.clone()), now_ms())
    }

    async fn keep(
        &self,
        request: &KeepPrRequest,
        manifest: &RunManifest,
        outcome: &RunOutcome,
        attribution: &RunAttribution,
        previous: Option<&KeepResult>,
    ) -> Result<KeepPrResponse, KeepPrError> {
        self.keep_signed(request, manifest, outcome, attribution, previous, None)
            .await
    }

    async fn keep_signed(
        &self,
        request: &KeepPrRequest,
        manifest: &RunManifest,
        outcome: &RunOutcome,
        attribution: &RunAttribution,
        previous: Option<&KeepResult>,
        receipt: Option<&OpenedReceipt>,
    ) -> Result<KeepPrResponse, KeepPrError> {
        keep(KeepJob {
            session_root: &self.repo,
            control_root: &self.data,
            request,
            manifest,
            outcome,
            attribution,
            outside: &RunAttribution::default(),
            previous,
            receipt,
            tools: &self.tools,
        })
        .await
    }

    fn branches(&self) -> String {
        sh(
            &self.repo,
            "git for-each-ref --format='%(refname)' refs/heads",
        )
    }

    fn git_calls(&self) -> Vec<Vec<String>> {
        std::fs::read_to_string(&self.git_log)
            .unwrap_or_default()
            .lines()
            .map(|line| {
                line.split('\u{1f}')
                    .filter(|arg| !arg.is_empty())
                    .map(str::to_string)
                    .collect()
            })
            .collect()
    }
}

#[cfg(unix)]
#[tokio::test]
async fn keep_commits_exactly_the_run_paths_without_touching_the_checkout() {
    let fixture = Fixture::new();
    let attribution = fixture.run_changes();
    let before = checkout(&fixture.repo);
    let manifest = fixture.manifest();
    let kept = fixture
        .keep(&request(), &manifest, &outcome(), &attribution, None)
        .await
        .unwrap();
    assert_eq!(kept.branch, "axocoatl/fix-0f8c1a2b");
    assert_eq!(kept.paths, vec!["a.txt", "b.txt", "src/new.rs"]);
    assert_eq!(kept.not_committed, vec!["docs/c.md", "notes.txt"]);
    assert!(kept
        .warnings
        .iter()
        .any(|warning| warning.contains("docs/c.md")));
    assert!(kept.pushed_to.is_none() && kept.pull_request_url.is_none());
    assert_eq!(before, checkout(&fixture.repo), "the checkout changed");

    let repo = &fixture.repo;
    assert_eq!(
        sh(repo, "git rev-parse refs/heads/axocoatl/fix-0f8c1a2b"),
        kept.commit
    );
    assert_eq!(
        sh(repo, &format!("git rev-parse {}^", kept.commit)),
        fixture.head
    );
    assert_eq!(
        sh(
            repo,
            &format!(
                "git diff-tree -r --no-commit-id --name-status {}",
                kept.commit
            )
        ),
        "M\ta.txt\nD\tb.txt\nA\tsrc/new.rs"
    );
    assert_eq!(
        sh(repo, &format!("git show {}:a.txt", kept.commit)),
        "one, fixed"
    );
    assert_eq!(
        sh(repo, &format!("git log -1 --format=%an%n%ae%n%B {}", kept.commit)),
        format!("Person\nperson@example.invalid\nfix: Fix the off-by-one in pagination\n\nAxocoatl run {RUN}\n\nAxocoatl-Run: {RUN}")
    );
    assert_eq!(sh(repo, "git rev-parse HEAD"), fixture.head);
    assert_eq!(
        sh(repo, "git diff --name-only HEAD"),
        "a.txt\nb.txt\ndocs/c.md"
    );
    assert_eq!(
        sh(repo, "git ls-files --others --exclude-standard"),
        "notes.txt\nsrc/new.rs"
    );
    // Nothing was pushed, and the protected Git directory is gone.
    assert_eq!(
        sh(&fixture.remote, "git for-each-ref --format='%(refname)'"),
        "refs/heads/main"
    );
    assert!(
        std::fs::read_dir(fixture.data.path().join("runtime/keep-pr"))
            .unwrap()
            .next()
            .is_none()
    );
    assert!(!fixture
        .git_calls()
        .iter()
        .any(|call| call.iter().any(|arg| arg == "push")));
}

#[cfg(unix)]
#[tokio::test]
async fn open_pr_pushes_without_force_and_gives_gh_the_body_file() {
    let fixture = Fixture::new();
    let attribution = fixture.run_changes();
    let before = checkout(&fixture.repo);
    let mut request = request();
    request.open_pr = true;
    let kept = fixture
        .keep(
            &request,
            &fixture.manifest(),
            &outcome(),
            &attribution,
            None,
        )
        .await
        .unwrap();
    assert_eq!(before, checkout(&fixture.repo), "the checkout changed");
    assert_eq!(
        kept.pull_request_url.as_deref(),
        Some("https://github.com/acme/widgets/pull/7")
    );
    assert_eq!(
        kept.pushed_to.as_deref(),
        Some("origin/axocoatl/fix-0f8c1a2b")
    );
    assert_eq!(kept.base.as_deref(), Some("main"));
    assert_eq!(
        sh(
            &fixture.remote,
            "git rev-parse refs/heads/axocoatl/fix-0f8c1a2b"
        ),
        kept.commit
    );
    assert_eq!(
        sh(&fixture.remote, "git rev-parse refs/heads/main"),
        fixture.head
    );

    let pushes: Vec<Vec<String>> = fixture
        .git_calls()
        .into_iter()
        .filter(|call| call.iter().any(|arg| arg == "push"))
        .collect();
    assert_eq!(pushes.len(), 1, "{pushes:?}");
    let push = &pushes[0];
    let at = push.iter().position(|arg| arg == "push").unwrap();
    assert_eq!(
        &push[at..],
        &[
            "push",
            "--porcelain",
            "origin",
            "axocoatl/fix-0f8c1a2b:refs/heads/axocoatl/fix-0f8c1a2b"
        ]
    );
    for arg in push {
        assert!(
            !arg.starts_with("--force")
                && arg != "-f"
                && arg != "--mirror"
                && arg != "--delete"
                && arg != "-d"
                && !arg.starts_with('+'),
            "{push:?}"
        );
    }

    let gh = std::fs::read_to_string(&fixture.gh_log).unwrap();
    let mut lines = gh.lines();
    let argv: Vec<&str> = lines
        .next()
        .unwrap()
        .split('\u{1f}')
        .filter(|arg| !arg.is_empty())
        .collect();
    assert_eq!(
        &argv[..10],
        &[
            "pr",
            "create",
            "--repo",
            "github.com/acme/widgets",
            "--head",
            "axocoatl/fix-0f8c1a2b",
            "--base",
            "main",
            "--title",
            "fix: Fix the off-by-one in pagination",
        ]
    );
    assert_eq!(argv[10], "--body-file");
    assert!(argv[11].ends_with("pull-request-body.md"));
    // gh ran in an empty directory of Axocoatl's own, not in the repository.
    let cwd = lines.next().unwrap().strip_prefix("cwd=").unwrap();
    assert!(cwd.contains("runtime/keep-pr/keep-"), "{cwd}");
    let body = std::fs::read_to_string(&fixture.gh_body).unwrap();
    for needle in [
        "### Checks",
        "### Review",
        "### Adjudications",
        "### Not covered",
        RUN,
    ] {
        assert!(body.contains(needle), "{needle} missing from\n{body}");
    }
    assert_eq!(body, pr_body(&outcome(), None).unwrap());
}

#[cfg(unix)]
#[tokio::test]
async fn keep_refuses_what_it_must_not_commit_or_push() {
    let fixture = Fixture::new();
    let attribution = fixture.run_changes();
    let manifest = fixture.manifest();
    let before = checkout(&fixture.repo);
    let refused = |result: Result<KeepPrResponse, KeepPrError>, needle: &str| match result {
        Err(KeepPrError::Refused(message)) => {
            assert!(message.contains(needle), "{message:?} lacks {needle:?}")
        }
        other => panic!("expected a refusal containing {needle:?}, got {other:?}"),
    };

    // The run did not pass.
    let mut failing = outcome();
    failing.verdict = RunVerdict::ChecksFailed;
    failing.exit_code = 1;
    refused(
        fixture
            .keep(&request(), &manifest, &failing, &attribution, None)
            .await,
        "did not pass",
    );
    // A run path had uncommitted changes before the run.
    let mut dirty = manifest.clone();
    dirty.dirty_paths = vec!["a.txt".into()];
    refused(
        fixture
            .keep(&request(), &dirty, &outcome(), &attribution, None)
            .await,
        "uncommitted changes before the run started",
    );
    // HEAD moved after the run started.
    let mut moved = manifest.clone();
    moved.repo_head = Some("1".repeat(40));
    refused(
        fixture
            .keep(&request(), &moved, &outcome(), &attribution, None)
            .await,
        "HEAD moved",
    );
    // Captures that cannot establish what an activation changed.
    let mut unknown = attribution.clone();
    unknown
        .unattributable
        .push("writer: its After capture is unavailable".into());
    refused(
        fixture
            .keep(&request(), &manifest, &outcome(), &unknown, None)
            .await,
        "cannot be attributed exactly",
    );
    // A turn outside the run changed a run path too, or cannot be attributed.
    let mut outside = RunAttribution::default();
    outside.paths.insert("a.txt".into());
    let shared = keep(KeepJob {
        session_root: &fixture.repo,
        control_root: &fixture.data,
        request: &request(),
        manifest: &manifest,
        outcome: &outcome(),
        attribution: &attribution,
        outside: &outside,
        previous: None,
        receipt: None,
        tools: &fixture.tools,
    })
    .await;
    refused(shared, "outside the run also changed a.txt");
    let mut outside = RunAttribution::default();
    outside
        .unattributable
        .push("writer in turn-9: it has no After capture".into());
    let unknown_outside = keep(KeepJob {
        session_root: &fixture.repo,
        control_root: &fixture.data,
        request: &request(),
        manifest: &manifest,
        outcome: &outcome(),
        attribution: &attribution,
        outside: &outside,
        previous: None,
        receipt: None,
        tools: &fixture.tools,
    })
    .await;
    refused(
        unknown_outside,
        "outside the run changed files that cannot be attributed",
    );
    // The run changed Git's own files.
    let mut hooks = attribution.clone();
    hooks.paths.insert(".git/hooks/pre-push".into());
    refused(
        fixture
            .keep(&request(), &manifest, &outcome(), &hooks, None)
            .await,
        "Git's own files",
    );
    // Nothing attributed differs from HEAD.
    let mut nothing = RunAttribution::default();
    nothing.paths.insert("docs/missing.md".into());
    refused(
        fixture
            .keep(&request(), &manifest, &outcome(), &nothing, None)
            .await,
        "nothing to keep",
    );
    // An existing local branch.
    sh(&fixture.repo, "git branch axocoatl/taken");
    let mut taken = request();
    taken.branch = Some("axocoatl/taken".into());
    refused(
        fixture
            .keep(&taken, &manifest, &outcome(), &attribution, None)
            .await,
        "already exists",
    );
    // The checked-out branch.
    let mut current = request();
    current.branch = Some("work".into());
    refused(
        fixture
            .keep(&current, &manifest, &outcome(), &attribution, None)
            .await,
        "checked out",
    );
    // The remote's default branch.
    let mut default = request();
    default.branch = Some("main".into());
    default.open_pr = true;
    refused(
        fixture
            .keep(&default, &manifest, &outcome(), &attribution, None)
            .await,
        "default branch of remote origin",
    );
    // A branch that already exists on the remote.
    sh(
        &fixture.repo,
        "git push -q ../remote.git work:refs/heads/axocoatl/remote-only",
    );
    let mut remote_only = request();
    remote_only.branch = Some("axocoatl/remote-only".into());
    remote_only.open_pr = true;
    refused(
        fixture
            .keep(&remote_only, &manifest, &outcome(), &attribution, None)
            .await,
        "already exists on remote origin",
    );
    // A push URL the person's own settings add beside the checked one.
    let global = fixture.dir.path().join("gitconfig");
    let original = std::fs::read_to_string(&global).unwrap();
    std::fs::write(
        &global,
        format!(
            "{original}[remote \"origin\"]\n\tpushurl = {}\n\tpushurl = https://github.com/acme/widgets.git\n",
            fixture.repo.join("planted.git").display()
        ),
    )
    .unwrap();
    std::fs::create_dir_all(fixture.repo.join("planted.git")).unwrap();
    let mut extra = request();
    extra.open_pr = true;
    refused(
        fixture
            .keep(&extra, &manifest, &outcome(), &attribution, None)
            .await,
        "inside the Session's folder",
    );
    std::fs::write(&global, &original).unwrap();
    std::fs::remove_dir_all(fixture.repo.join("planted.git")).unwrap();
    // A remote that does not exist, or that gh cannot open a PR on.
    let mut missing = request();
    missing.open_pr = true;
    missing.remote = Some("upstream".into());
    refused(
        fixture
            .keep(&missing, &manifest, &outcome(), &attribution, None)
            .await,
        "no remote named upstream",
    );
    sh(&fixture.repo, "git remote add local \"$PWD/../remote.git\"");
    std::thread::sleep(std::time::Duration::from_millis(20));
    let after_remote = fixture.manifest();
    let mut local = request();
    local.open_pr = true;
    local.remote = Some("local".into());
    refused(
        fixture
            .keep(&local, &after_remote, &outcome(), &attribution, None)
            .await,
        "does not name a hosted repository",
    );

    // None of that created a branch, pushed or changed the checkout.
    assert_eq!(
        fixture.branches(),
        "refs/heads/axocoatl/taken\nrefs/heads/work"
    );
    assert_eq!(
        sh(&fixture.remote, "git for-each-ref --format='%(refname)'"),
        "refs/heads/axocoatl/remote-only\nrefs/heads/main"
    );
    assert!(!std::path::Path::new(&fixture.gh_log).exists());
    assert_eq!(before, checkout(&fixture.repo));

    // Git's settings changed after the run started.
    let started = manifest.started_at_ms;
    std::thread::sleep(std::time::Duration::from_millis(20));
    sh(
        &fixture.repo,
        "git config remote.origin.url https://github.com/attacker/x.git",
    );
    let mut late = manifest.clone();
    late.started_at_ms = started;
    refused(
        fixture
            .keep(&request(), &late, &outcome(), &attribution, None)
            .await,
        "settings changed after the run started",
    );
}

/// The run's checks and review judged its files as the run left them: a run
/// path edited after the run ended is refused, never committed.
#[cfg(unix)]
#[tokio::test]
async fn a_run_path_changed_after_the_run_ended_is_refused() {
    let fixture = Fixture::new();
    let attribution = fixture.run_changes();
    let manifest = fixture.manifest();
    let ended = outcome();
    std::thread::sleep(std::time::Duration::from_millis(20));
    write(
        &fixture.repo.join("a.txt"),
        "one, fixed, then edited by hand\n",
    );
    // A path the run did not change may change later; it is never committed.
    write(&fixture.repo.join("notes.txt"), "more scratch\n");
    match fixture
        .keep(&request(), &manifest, &ended, &attribution, None)
        .await
    {
        Err(KeepPrError::Refused(message)) => {
            assert!(message.contains("changed after the run ended"), "{message}");
            assert!(message.contains("a.txt"), "{message}");
            assert!(!message.contains("src/new.rs"), "{message}");
            assert!(!message.contains("notes.txt"), "{message}");
        }
        other => panic!("expected a refusal, got {other:?}"),
    }
    assert_eq!(
        fixture.branches(),
        "refs/heads/work",
        "a branch was created"
    );
    // A run that ended after the edit keeps the same paths, the deleted one
    // included.
    std::thread::sleep(std::time::Duration::from_millis(20));
    let kept = fixture
        .keep(&request(), &manifest, &outcome(), &attribution, None)
        .await
        .unwrap();
    assert_eq!(kept.paths, vec!["a.txt", "b.txt", "src/new.rs"]);
}

#[cfg(unix)]
#[tokio::test]
async fn repository_settings_hooks_and_filters_never_run_on_the_host() {
    let fixture = Fixture::new();
    let repo = &fixture.repo;
    let sentinel = fixture.dir.path().join("host-command-ran");
    let helper = fixture.dir.path().join("evil.sh");
    executable(
        &helper,
        &format!(
            "#!/bin/sh\necho \"$0 $*\" >> '{}'\ncat\n",
            sentinel.display()
        ),
    );
    let hooks = fixture.dir.path().join("hooks");
    for hook in [
        "pre-push",
        "reference-transaction",
        "post-commit",
        "pre-commit",
    ] {
        executable(
            &hooks.join(hook),
            &format!("#!/bin/sh\necho {hook} >> '{}'\n", sentinel.display()),
        );
    }
    let helper = helper.display();
    sh(
        repo,
        &format!(
            "printf 'a.txt filter=evil\\n' > .gitattributes && git add .gitattributes \
             && git commit -q -m attributes \
             && git config core.hooksPath '{}' && git config core.fsmonitor '{helper}' \
             && git config filter.evil.clean '{helper}' && git config filter.evil.required true \
             && git config gpg.program '{helper}' && git config commit.gpgSign true \
             && git config core.sshCommand '{helper}' && git config credential.helper '!{helper}' \
             && git config diff.external '{helper}'",
            hooks.display()
        ),
    );
    let head = sh(repo, "git rev-parse HEAD");
    std::thread::sleep(std::time::Duration::from_millis(20));
    let attribution = fixture.run_changes();
    let mut manifest = fixture.manifest();
    manifest.repo_head = Some(head.clone());
    let mut request = request();
    request.open_pr = true;
    let kept = fixture
        .keep(&request, &manifest, &outcome(), &attribution, None)
        .await
        .unwrap();
    assert!(kept.pull_request_url.is_some());
    assert!(
        !sentinel.exists(),
        "a repository-configured program ran on the host: {}",
        std::fs::read_to_string(&sentinel).unwrap_or_default()
    );
    // The committed bytes are the working tree's, unfiltered and unsigned.
    assert_eq!(
        sh(repo, &format!("git show {}:a.txt", kept.commit)),
        "one, fixed"
    );
    assert_eq!(
        sh(
            repo,
            &format!("git cat-file -p {} | grep -c gpgsig || true", kept.commit)
        ),
        "0"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn a_later_keep_continues_from_the_branch_an_earlier_keep_created() {
    let fixture = Fixture::new();
    let attribution = fixture.run_changes();
    let manifest = fixture.manifest();
    let first = fixture
        .keep(&request(), &manifest, &outcome(), &attribution, None)
        .await
        .unwrap();
    let previous = first.keep_result();
    // Keeping the branch again creates nothing new.
    let again = fixture
        .keep(
            &request(),
            &manifest,
            &outcome(),
            &attribution,
            Some(&previous),
        )
        .await
        .unwrap();
    assert_eq!(
        (again.commit.as_str(), again.paths.clone()),
        (first.commit.as_str(), first.paths.clone())
    );
    assert!(again.warnings[0].contains("already created"));
    // Opening the pull request afterwards pushes that same commit; gh fails once.
    let failure_marker = fixture.dir.path().join("gh-failed-once");
    let mut tools = fixture.tools.clone();
    tools
        .env
        .push(("KEEP_TEST_GH_FAIL".into(), failure_marker.clone().into()));
    let mut open = request();
    open.open_pr = true;
    let run_outcome = outcome();
    let job = |previous: Option<KeepResult>| {
        let open = open.clone();
        let manifest = manifest.clone();
        let run_outcome = run_outcome.clone();
        let attribution = attribution.clone();
        let tools = tools.clone();
        let repo = fixture.repo.clone();
        let data = fixture.data.clone();
        async move {
            keep(KeepJob {
                session_root: &repo,
                control_root: &data,
                request: &open,
                manifest: &manifest,
                outcome: &run_outcome,
                attribution: &attribution,
                outside: &RunAttribution::default(),
                previous: previous.as_ref(),
                receipt: None,
                tools: &tools,
            })
            .await
        }
    };
    let failed = job(Some(previous.clone())).await.unwrap_err();
    let KeepPrError::AfterBranch {
        branch,
        commit,
        message,
    } = &failed
    else {
        panic!("{failed:?}");
    };
    assert_eq!(
        (branch.as_str(), commit.as_str()),
        (first.branch.as_str(), first.commit.as_str())
    );
    assert!(message.contains("HTTP 502"), "{message}");
    assert_eq!(
        sh(
            &fixture.remote,
            "git rev-parse refs/heads/axocoatl/fix-0f8c1a2b"
        ),
        first.commit
    );
    // The retry finds its own commit on the remote, does not push again, and opens the PR.
    let recorded = failed.keep_result("unused");
    let opened = job(Some(recorded)).await.unwrap();
    assert_eq!(opened.commit, first.commit);
    assert_eq!(
        opened.pull_request_url.as_deref(),
        Some("https://github.com/acme/widgets/pull/7")
    );
    let pushes = fixture
        .git_calls()
        .into_iter()
        .filter(|call| call.iter().any(|arg| arg == "push"))
        .count();
    assert_eq!(pushes, 1);
    // Once the PR exists, a further request returns it without pushing or calling gh.
    let gh_calls = std::fs::read_to_string(&fixture.gh_log)
        .unwrap()
        .lines()
        .count();
    let done = opened.keep_result();
    let repeat = job(Some(done)).await.unwrap();
    assert_eq!(repeat.pull_request_url, opened.pull_request_url);
    assert_eq!(
        std::fs::read_to_string(&fixture.gh_log)
            .unwrap()
            .lines()
            .count(),
        gh_calls
    );
    // A branch that an earlier Keep created at another commit is refused.
    let mut stale = previous.clone();
    stale.commit = "1".repeat(40);
    assert!(matches!(
        fixture
            .keep(&request(), &manifest, &outcome(), &attribution, Some(&stale))
            .await,
        Err(KeepPrError::Refused(message)) if message.contains("already exists")
    ));
}

#[test]
fn the_persons_settings_are_passed_on_without_the_repositorys() {
    let repo = std::path::Path::new("/w/repo");
    let listing = b"global\0file:/h/.gitconfig\0user.name\nPerson\0global\0file:/h/.gitconfig\0includeif.gitdir:/w/.path\n/w.cfg\0global\0file:/h/.gitconfig\0core.sshcommand\nssh -i ~/.ssh/work\0global\0file:/h/.gitconfig\0credential.helper\nosxkeychain\0global\0file:/h/.gitconfig\0credential.helper\n\0system\0file:/etc/gitconfig\0http.sslverify\0local\0file:.git/config\0core.fsmonitor\n/evil\0command\0command line:\0core.hookspath\n/dev/null\0";
    assert_eq!(
        git_host::person_settings(listing, repo).unwrap(),
        vec![
            "user.name=Person",
            "core.sshcommand=ssh -i ~/.ssh/work",
            "credential.helper=osxkeychain",
            "credential.helper=",
            "http.sslverify",
        ]
    );
    assert_eq!(
        git_host::person_settings(b"global\0file:/h/.gitconfig\0url.a=b.insteadof\nx\0", repo),
        None
    );
    assert_eq!(
        git_host::person_settings(b"global\0file:/h/.gitconfig\0user.name\n\xff\0", repo),
        None
    );
    assert_eq!(git_host::person_settings(b"", repo), Some(Vec::new()));
}

/// Apple's git keeps `credential.helper=osxkeychain` in a configuration file
/// of its installation that git lists with the scope `unknown`, and Keep's
/// processes run with `GIT_CONFIG_NOSYSTEM`; that file's entries are passed on
/// so a push to a private HTTPS remote finds the person's credential. An
/// `unknown` entry from anywhere else, a relative or `..` path, a file inside
/// the repository or one that resolves into it through a link, is not.
#[test]
fn installation_settings_outside_the_repository_are_passed_on() {
    let dir = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(dir.path()).unwrap();
    let repo = root.join("repo");
    let install = root.join("install");
    std::fs::create_dir_all(repo.join(".git")).unwrap();
    std::fs::create_dir_all(&install).unwrap();
    let installed = install.join("gitconfig");
    std::fs::write(&installed, "[credential]\n\thelper = osxkeychain\n").unwrap();
    let planted = repo.join(".git/planted");
    std::fs::write(&planted, "[credential]\n\thelper = !evil\n").unwrap();
    let linked = install.join("link-into-repo");
    std::os::unix::fs::symlink(&planted, &linked).unwrap();
    let entry = |origin: &str, value: &str| {
        let mut bytes = b"unknown\0".to_vec();
        bytes.extend_from_slice(origin.as_bytes());
        bytes.push(0);
        bytes.extend_from_slice(format!("credential.helper\n{value}").as_bytes());
        bytes.push(0);
        bytes
    };
    let mut listing = entry(&format!("file:{}", installed.display()), "osxkeychain");
    for (origin, value) in [
        (format!("file:{}", planted.display()), "!evil-inside"),
        (format!("file:{}", linked.display()), "!evil-linked"),
        (
            format!("file:{}/../repo/.git/planted", install.display()),
            "!evil-dots",
        ),
        ("file:.git/planted".to_string(), "!evil-relative"),
        ("command line:".to_string(), "!evil-command"),
        ("blob:HEAD:.gitconfig".to_string(), "!evil-blob"),
        (
            format!("file:{}", install.join("missing").display()),
            "!evil-missing",
        ),
    ] {
        listing.extend(entry(&origin, value));
    }
    assert_eq!(
        git_host::person_settings(&listing, &repo).unwrap(),
        vec!["credential.helper=osxkeychain"]
    );
}

#[test]
fn push_urls_are_checked_after_the_persons_rewrites() {
    let dir = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(dir.path()).unwrap();
    let session = root.join("session");
    std::fs::create_dir_all(session.join("planted.git")).unwrap();
    std::fs::create_dir_all(root.join("outside.git")).unwrap();
    for ok in [
        "https://github.com/acme/widgets.git".to_string(),
        "git@github.com:acme/widgets.git".to_string(),
        "ssh://git@github.com/acme/widgets".to_string(),
        root.join("outside.git").display().to_string(),
        format!("file://{}", root.join("outside.git").display()),
    ] {
        assert!(git_host::check_push_url(&ok, &session).is_ok(), "{ok}");
    }
    for bad in [
        "ext::sh -c touch% /tmp/pwned".to_string(),
        "fd::17".to_string(),
        "persistent-https::https://example.com/x".to_string(),
        "git://example.com/x".to_string(),
        "relative/path.git".to_string(),
        "-uhoh".to_string(),
        session.join("planted.git").display().to_string(),
        format!("file://{}", session.join("planted.git").display()),
        format!("{}/../session/planted.git", session.display()),
    ] {
        assert!(git_host::check_push_url(&bad, &session).is_err(), "{bad}");
    }
    assert_eq!(
        git_host::redact_url_credentials(
            "fatal: https://x-access-token:ghp_secret@github.com/a/b.git and http://u@h/x"
        ),
        "fatal: https://***@github.com/a/b.git and http://***@h/x"
    );
    assert_eq!(
        git_host::hosted_repository_slug("git@github.com:acme/widgets.git").as_deref(),
        Some("github.com/acme/widgets")
    );
    assert_eq!(
        git_host::hosted_repository_slug("https://ghe.example.com/team/svc").as_deref(),
        Some("ghe.example.com/team/svc")
    );
    assert_eq!(
        git_host::hosted_repository_slug("https://github.com/acme/widgets/extra"),
        None
    );
    assert_eq!(git_host::hosted_repository_slug("/srv/repo.git"), None);
    assert_eq!(
        git_host::hosted_repository_slug("https://github.com/-x/y"),
        None
    );
}

#[cfg(unix)]
#[tokio::test]
async fn a_missing_gh_is_named_and_the_pushed_branch_stays() {
    let fixture = Fixture::new();
    let attribution = fixture.run_changes();
    // The PATH Keep searches holds git and nothing else, so no host's `gh`
    // (a CI runner has one in /usr/bin) can stand in for the missing one.
    // Git finds its own helpers through its exec path, and runs shell
    // commands with an absolute `/bin/sh`.
    let only_git = fixture.dir.path().join("only-git");
    std::fs::create_dir_all(&only_git).unwrap();
    let real_git = sh(fixture.dir.path(), "command -v git");
    std::os::unix::fs::symlink(real_git, only_git.join("git")).unwrap();
    let mut tools = fixture.tools.clone();
    tools.path = Some(only_git.clone().into_os_string());
    assert_eq!(
        std::fs::read_dir(&only_git)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect::<Vec<_>>(),
        ["git"]
    );
    let mut open = request();
    open.open_pr = true;
    let manifest = fixture.manifest();
    let run_outcome = outcome();
    let failed = keep(KeepJob {
        session_root: &fixture.repo,
        control_root: &fixture.data,
        request: &open,
        manifest: &manifest,
        outcome: &run_outcome,
        attribution: &attribution,
        outside: &RunAttribution::default(),
        previous: None,
        receipt: None,
        tools: &tools,
    })
    .await
    .unwrap_err();
    let KeepPrError::AfterBranch {
        commit, message, ..
    } = &failed
    else {
        panic!("{failed:?}");
    };
    assert!(
        message.contains("gh was not found on the daemon's PATH"),
        "{message}"
    );
    assert_eq!(
        sh(
            &fixture.remote,
            "git rev-parse refs/heads/axocoatl/fix-0f8c1a2b"
        ),
        *commit
    );
}

// ---------------------------------------------------------------------------
// Receipts

/// A data root's signing key, in a private directory of its own.
fn signing_key() -> (tempfile::TempDir, crate::receipt_key::SigningKey) {
    let dir = tempfile::tempdir().unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    let (key, _) = crate::receipt_key::load_or_create(dir.path()).unwrap();
    (dir, key)
}

/// A receipt of the passing run [`outcome`] covering `files` (path and
/// content) and `removed`, signed by `key`.
fn signed_receipt(
    key: &crate::receipt_key::SigningKey,
    files: &[(&str, &str)],
    removed: &[&str],
) -> OpenedReceipt {
    use axocoatl_session::receipt::sha256_hex;
    let subject: Vec<serde_json::Value> = files
        .iter()
        .map(|(path, content)| {
            serde_json::json!({"name": path, "digest": {"sha256": sha256_hex(content.as_bytes())}})
        })
        .collect();
    signed_receipt_of(key, subject, removed)
}

/// A receipt of the passing run [`outcome`] whose subjects are `subject`.
fn signed_receipt_of(
    key: &crate::receipt_key::SigningKey,
    subject: Vec<serde_json::Value>,
    removed: &[&str],
) -> OpenedReceipt {
    use axocoatl_session::receipt::{sign_statement, Statement};
    let statement: Statement = serde_json::from_value(serde_json::json!({
        "_type": receipt_v1::STATEMENT_TYPE,
        "subject": subject,
        "predicateType": receipt_v1::PREDICATE_TYPE,
        "predicate": {
            "run": {"run_id": RUN, "session_id": SESSION, "started_at_ms": 1, "finished_at_ms": 2,
                "axocoatl_version": "1.4.0", "task_sha256": "0".repeat(64)},
            "repository": {"dirty_paths": []},
            "loadout": {"id": "fix", "version": 1, "kind": "fix", "digest": "ab".repeat(32), "builtin": true},
            "agents": [{"id": "writer", "role": "writer", "runtime": "native", "provider": "ollama",
                "model": "qwen3-coder:30b"}],
            "sandbox": {"backend": "podman", "network": "egress", "workload": "hardened", "policies": []},
            "network": NetworkSummary::default(),
            "checks": [{"name": "tests", "state": "passed", "exit_code": 0, "timeout_ms": 180000,
                "argv": ["npm", "test"]}],
            "review": {"provider": "openrouter", "model": "openai/gpt-oss-120b", "runtime": "native",
                "verdict": "approved", "passed": true, "rounds": 2, "max_rounds": 2, "findings": 2},
            "adjudications": [],
            "verdict": "pass", "exit_code": 0, "attention": [],
            "not_covered": {"count": 0, "areas": []},
            "warnings": [],
            "usage": {"input_tokens": 1, "output_tokens": 1, "cost_microunits": 0, "cost_usd": 0.0,
                "complete": true, "cost_known": true, "cost_computed": false},
            "files": {"source": "judged_capture", "removed": removed, "complete": true, "unattributed": []},
            "signer": key.public().signer(),
        }
    }))
    .unwrap();
    let (envelope, _) = sign_statement(key.key_pair(), &statement).unwrap();
    receipt_v1::open_envelope(envelope).unwrap()
}

/// What the fixture's run left: an edited, a new and a removed path.
fn run_receipt(key: &crate::receipt_key::SigningKey) -> OpenedReceipt {
    signed_receipt(
        key,
        &[("a.txt", "one, fixed\n"), ("src/new.rs", "fn new() {}\n")],
        &["b.txt"],
    )
}

/// With a receipt, Keep commits what it covers; the commit names the run
/// and the receipt in trailers; the pull request body has a Receipt section
/// near the top and the receipt itself at the end, which parses back to the
/// signed envelope.
#[cfg(unix)]
#[tokio::test]
async fn a_kept_run_with_a_receipt_carries_it_in_trailers_and_the_body() {
    let fixture = Fixture::new();
    let attribution = fixture.run_changes();
    let (_keys, key) = signing_key();
    let receipt = run_receipt(&key);
    let mut open = request();
    open.open_pr = true;
    let kept = fixture
        .keep_signed(
            &open,
            &fixture.manifest(),
            &outcome(),
            &attribution,
            None,
            Some(&receipt),
        )
        .await
        .unwrap();
    assert_eq!(kept.paths, vec!["a.txt", "b.txt", "src/new.rs"]);
    assert_eq!(kept.receipt, Some(receipt.payload_digest()));
    assert!(!kept
        .warnings
        .iter()
        .any(|warning| warning.contains("receipt")));
    let repo = &fixture.repo;
    assert_eq!(
        sh(
            repo,
            &format!(
                "git log -1 --format='%(trailers:key=Axocoatl-Run,valueonly)%(trailers:key=Axocoatl-Receipt,valueonly)' {}",
                kept.commit
            )
        ),
        format!("{RUN}\n{}", receipt.payload_digest())
    );
    let body = std::fs::read_to_string(&fixture.gh_body).unwrap();
    assert_eq!(body, pr_body(&outcome(), Some(&receipt)).unwrap());
    let section = body.find("### Receipt").unwrap();
    assert!(section < body.find("### Record").unwrap());
    for needle in [
        &format!("Signed by key `{}`", key.public().short_id()),
        "- Verdict: **passed** (exit code 0)",
        "- Checks: tests passed",
        "- Review: `openrouter:openai/gpt-oss-120b`: approved",
        "- Writer: `ollama:qwen3-coder:30b`",
        "- Files covered: 2 (as the required checks judged them), removed: 1",
        "axocoatl receipt verify body.md --keys trusted-keys --repo .",
    ] {
        assert!(body.contains(needle), "missing {needle:?} in\n{body}");
    }
    assert!(body.trim_end().ends_with("-->"), "{body}");
    let parsed = receipt_v1::open_receipt(body.as_bytes()).unwrap();
    assert_eq!(parsed.envelope, receipt.envelope);
    assert_eq!(
        receipt_v1::check_signature(&parsed, &[key.public()]),
        receipt_v1::SignatureCheck::Trusted {
            key_id: key.public().key_id()
        }
    );
    // And the files of the branch are the ones it covers.
    let worktree = fixture.dir.path().join("check");
    sh(
        repo,
        &format!("git worktree add -q {} {}", worktree.display(), kept.commit),
    );
    assert!(receipt_v1::check_files(&parsed.statement, &worktree).is_empty());
}

/// A submodule the run moved is committed as the commit it holds (a
/// gitlink), as the run's receipt covers it; one that holds another commit
/// than the receipt's is refused, as is one without a receipt, which no
/// directory time can stand in for. A run whose repository holds a nested
/// Git checkout (not a submodule), whose files no capture reads, is refused
/// and told what to do.
#[cfg(unix)]
#[tokio::test]
async fn a_moved_submodule_is_kept_as_its_receipt_covers_it_and_a_nested_checkout_refused() {
    use axocoatl_session::receipt::sha256_hex;
    let mut fixture = Fixture::new();
    let upstream = fixture.dir.path().join("upstream");
    std::fs::create_dir_all(&upstream).unwrap();
    sh(
        &upstream,
        "git init -q -b main && echo x > x && git add x && git commit -q -m x && echo y > x \
         && git commit -q -am y",
    );
    let older = sh(&upstream, "git rev-parse HEAD~1");
    let newest = sh(&upstream, "git rev-parse HEAD");
    sh(
        &fixture.repo,
        &format!(
            "git -c protocol.file.allow=always submodule add -q {} vendor/sub && git commit -q \
             -m submodule",
            upstream.display()
        ),
    );
    fixture.head = sh(&fixture.repo, "git rev-parse HEAD");
    std::thread::sleep(std::time::Duration::from_millis(20));
    let manifest = fixture.manifest();
    sh(&fixture.repo.join("vendor/sub"), &format!("git checkout -q {older}"));
    write(&fixture.repo.join("a.txt"), "one, fixed\n");
    std::thread::sleep(std::time::Duration::from_millis(20));
    let attribution = RunAttribution {
        paths: ["a.txt", "vendor/sub"]
            .iter()
            .map(|path| path.to_string())
            .collect(),
        ..RunAttribution::default()
    };
    let refusal = |result: Result<KeepPrResponse, KeepPrError>| match result {
        Err(KeepPrError::Refused(message)) => message,
        other => panic!("expected a refusal, got {other:?}"),
    };
    let message = refusal(
        fixture
            .keep(&request(), &manifest, &outcome(), &attribution, None)
            .await,
    );
    assert_eq!(
        message,
        "the run moved the submodule vendor/sub to another commit, and without a receipt that \
         covers it Keep as PR cannot tell that it still holds the commit the run left; commit \
         it yourself"
    );
    let (_keys, key) = signing_key();
    let subjects = |commit: &str| {
        vec![
            serde_json::json!({"name": "a.txt", "digest": {"sha256": sha256_hex(b"one, fixed\n")}}),
            serde_json::json!({"name": "vendor/sub", "digest": {"gitCommit": commit}}),
        ]
    };
    let stale = signed_receipt_of(&key, subjects(&newest), &[]);
    let message = refusal(
        fixture
            .keep_signed(
                &request(),
                &manifest,
                &outcome(),
                &attribution,
                None,
                Some(&stale),
            )
            .await,
    );
    assert!(
        message.contains("vendor/sub: the submodule holds another commit"),
        "{message}"
    );
    assert_eq!(fixture.branches(), "refs/heads/work");
    let receipt = signed_receipt_of(&key, subjects(&older), &[]);
    let kept = fixture
        .keep_signed(
            &request(),
            &manifest,
            &outcome(),
            &attribution,
            None,
            Some(&receipt),
        )
        .await
        .unwrap();
    assert_eq!(kept.paths, vec!["a.txt", "vendor/sub"]);
    assert_eq!(
        sh(&fixture.repo, &format!("git ls-tree {} vendor/sub", kept.commit)),
        format!("160000 commit {older}\tvendor/sub")
    );
    // The person's checkout still has the submodule's old commit staged.
    assert_eq!(
        sh(&fixture.repo, "git ls-files -s vendor/sub"),
        format!("160000 {newest} 0\tvendor/sub")
    );

    let nested = RunAttribution {
        paths: ["a.txt"].iter().map(|path| path.to_string()).collect(),
        unread: ["tools/nested"].iter().map(|path| path.to_string()).collect(),
        ..RunAttribution::default()
    };
    assert_eq!(
        refusal(
            fixture
                .keep(&request(), &manifest, &outcome(), &nested, None)
                .await
        ),
        "tools/nested is a nested Git checkout (not a submodule) whose files Axocoatl's \
         repository captures do not read, so what an Agent changed inside it is not known, so \
         Keep as PR does not guess; ignore it in .gitignore, or make it a submodule"
    );
}

/// A covered file edited after the run (after its checks judged it), a
/// removed path that exists again, or a path the receipt does not cover is
/// refused, naming each, and nothing is created.
#[cfg(unix)]
#[tokio::test]
async fn a_path_that_differs_from_its_receipt_is_refused() {
    let fixture = Fixture::new();
    let attribution = fixture.run_changes();
    let manifest = fixture.manifest();
    let (_keys, key) = signing_key();
    let receipt = run_receipt(&key);
    let ended = outcome();
    std::thread::sleep(std::time::Duration::from_millis(20));
    write(&fixture.repo.join("a.txt"), "one, fixed, then edited\n");
    match fixture
        .keep_signed(
            &request(),
            &manifest,
            &ended,
            &attribution,
            None,
            Some(&receipt),
        )
        .await
    {
        Err(KeepPrError::Refused(message)) => {
            assert!(
                message.starts_with(
                    "a path changed after the run's checks judged it; Keep commits only what \
                     the receipt covers: a.txt: content differs"
                ),
                "{message}"
            );
            assert!(!message.contains("src/new.rs"), "{message}");
        }
        other => panic!("expected a refusal, got {other:?}"),
    }
    assert_eq!(
        fixture.branches(),
        "refs/heads/work",
        "a branch was created"
    );
    write(&fixture.repo.join("a.txt"), "one, fixed\n");

    // A receipt whose subjects differ from the files (judged before a later
    // change the run made without new checks), or that misses a path.
    let stale = signed_receipt(
        &key,
        &[("a.txt", "one, judged\n"), ("src/new.rs", "fn new() {}\n")],
        &["b.txt"],
    );
    let missing = signed_receipt(&key, &[("a.txt", "one, fixed\n")], &["b.txt"]);
    let back = signed_receipt(&key, &[("a.txt", "one, fixed\n")], &["src/new.rs", "b.txt"]);
    std::thread::sleep(std::time::Duration::from_millis(20));
    for (receipt, needle) in [
        (&stale, "a.txt: content differs"),
        (&missing, "the run's receipt does not cover src/new.rs"),
        (&back, "src/new.rs: the run removed it, and it exists"),
    ] {
        match fixture
            .keep_signed(
                &request(),
                &manifest,
                &outcome(),
                &attribution,
                None,
                Some(receipt),
            )
            .await
        {
            Err(KeepPrError::Refused(message)) => {
                assert!(message.contains(needle), "{message} lacks {needle}")
            }
            other => panic!("expected a refusal containing {needle:?}, got {other:?}"),
        }
    }
    assert_eq!(
        fixture.branches(),
        "refs/heads/work",
        "a branch was created"
    );
    // Without a receipt, Keep says the commit carries none.
    let kept = fixture
        .keep(&request(), &manifest, &outcome(), &attribution, None)
        .await
        .unwrap();
    assert!(kept.receipt.is_none());
    assert!(kept
        .warnings
        .iter()
        .any(|warning| warning.contains("no signed receipt")));
}

/// The body's cut never cuts the receipt block; a body without a receipt
/// says so and why; a receipt that cannot be read, or another run's, is
/// refused.
#[test]
fn the_receipt_block_survives_the_body_cut_and_bad_receipts_are_refused() {
    let (_keys, key) = signing_key();
    let receipt = run_receipt(&key);
    let mut huge = outcome();
    huge.adjudications = (0..500)
        .map(|index| Adjudication {
            round: 1,
            finding_id: format!("F{index}"),
            finding: "x".repeat(400),
            decision: AdjudicationDecision::Accept,
            reason: "y".repeat(300),
            writer_generation: None,
        })
        .collect();
    huge.findings = (0..200)
        .map(|index| Finding {
            id: format!("B{index}"),
            source: FindingSource::Reviewer,
            title: "z".repeat(160),
            detail: String::new(),
            severity: None,
            area: None,
            location: None,
            line: None,
            repro: None,
        })
        .collect();
    let body = pr_body(&huge, Some(&receipt)).unwrap();
    assert!(body.contains("…cut to fit a pull request body"), "not cut");
    assert!(body.chars().count() <= MAX_BODY_CHARS + 200);
    assert_eq!(
        receipt_v1::open_receipt(body.as_bytes()).unwrap().envelope,
        receipt.envelope
    );

    let mut unsigned = outcome();
    unsigned.warnings.push(RunWarning {
        code: "receipt_unavailable".into(),
        message: "The run finished, but its receipt could not be signed: the key is unreadable."
            .into(),
    });
    let body = pr_body(&unsigned, None).unwrap();
    assert!(
        body.contains(
            "This run has no signed receipt (The run finished, but its receipt could not be signed"
        ),
        "{body}"
    );
    assert!(!body.contains(receipt_v1::PR_BLOCK_START));

    let record = MemoryRecord::default();
    assert!(load_receipt(&record, RUN).unwrap().is_none());
    record
        .receipts
        .lock()
        .unwrap()
        .insert(RUN.into(), b"not a receipt".to_vec());
    assert!(matches!(
        load_receipt(&record, RUN),
        Err(KeepPrError::Refused(message)) if message.contains("cannot be read")
    ));
    record.receipts.lock().unwrap().insert(
        "run-other".into(),
        receipt_v1::envelope_file_bytes(&receipt.envelope),
    );
    assert!(matches!(
        load_receipt(&record, "run-other"),
        Err(KeepPrError::Refused(message)) if message.contains(&format!("is of run {RUN}"))
    ));
    record.receipts.lock().unwrap().insert(
        RUN.into(),
        receipt_v1::envelope_file_bytes(&receipt.envelope),
    );
    assert_eq!(
        load_receipt(&record, RUN).unwrap().unwrap().envelope,
        receipt.envelope
    );
}
