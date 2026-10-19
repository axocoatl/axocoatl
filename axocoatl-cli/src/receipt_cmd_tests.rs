//! `axocoatl receipt` without a daemon: receipts signed with a key made in
//! a temporary data root, verified against a checkout and a record bundle.
use super::*;
use axocoatl_session::receipt::{
    envelope_file_bytes, pr_block, sha256_hex, sign_statement, source, AgentFacts, CheckFacts,
    FileFacts, LoadoutFacts, NotCoveredFacts, PolicyFacts, Predicate, RepositoryFacts, ReviewFacts,
    RunFacts, SandboxFacts, Statement, Subject, UsageFacts, PREDICATE_TYPE, STATEMENT_TYPE,
};
use axocoatl_session::run_outcome::{NetworkSummary, RunVerdict};

const RUN: &str = "run-0c1d0000-0000-4000-8000-000000000001";

struct Fixture {
    _dir: tempfile::TempDir,
    dir: PathBuf,
    repo: PathBuf,
    key: receipt_key::SigningKey,
    receipt: PathBuf,
    envelope: Envelope,
}

fn statement(signer: &receipt_key::SigningKey) -> Statement {
    Statement {
        statement_type: STATEMENT_TYPE.into(),
        subject: vec![
            Subject::sha256("calc.py", sha256_hex(b"def add(a, b):\n    return a + b\n")),
            Subject::sha256("docs/link", sha256_hex(b"../calc.py")),
        ],
        predicate_type: PREDICATE_TYPE.into(),
        predicate: Predicate {
            run: RunFacts {
                run_id: RUN.into(),
                session_id: "ses-0c1d0000-0000-4000-8000-000000000002".into(),
                started_at_ms: 1,
                finished_at_ms: 2,
                axocoatl_version: "1.4.0".into(),
                task_sha256: sha256_hex(b"fix add"),
            },
            repository: RepositoryFacts {
                base_commit: Some("a".repeat(40)),
                dirty_paths: Vec::new(),
                origin_url: None,
            },
            loadout: LoadoutFacts {
                id: "fix".into(),
                version: 1,
                kind: "fix".into(),
                digest: "b".repeat(64),
                builtin: true,
            },
            agents: vec![AgentFacts {
                id: "writer".into(),
                role: "writer".into(),
                runtime: "native".into(),
                program_version: None,
                provider: "ollama".into(),
                model: "qwen3-coder:30b".into(),
                credential: None,
            }],
            sandbox: SandboxFacts {
                backend: "podman".into(),
                network: "egress".into(),
                workload: "hardened".into(),
                policies: vec![PolicyFacts {
                    scope: "session".into(),
                    revision: 1,
                    digest: "c".repeat(64),
                    rules: Vec::new(),
                    routes: Vec::new(),
                }],
            },
            network: NetworkSummary::default(),
            checks: vec![CheckFacts {
                name: "tests".into(),
                state: "passed".into(),
                exit_code: Some(0),
                timeout_ms: 180_000,
                argv: vec!["python3".into(), "-m".into(), "pytest".into()],
                candidate_sha256: Some("d".repeat(64)),
            }],
            review: Some(ReviewFacts {
                provider: "ollama".into(),
                model: "gpt-oss:20b".into(),
                runtime: "native".into(),
                program_version: None,
                verdict: "approved".into(),
                passed: true,
                rounds: 1,
                max_rounds: 2,
                findings: 0,
            }),
            adjudications: Vec::new(),
            verdict: RunVerdict::Pass,
            exit_code: 0,
            attention: Vec::new(),
            not_covered: NotCoveredFacts {
                count: 0,
                areas: Vec::new(),
            },
            warnings: Vec::new(),
            usage: UsageFacts {
                input_tokens: 1,
                output_tokens: 1,
                cost_microunits: 0,
                cost_usd: 0.0,
                complete: true,
                cost_known: true,
                cost_computed: false,
            },
            files: FileFacts {
                source: source::JUDGED_CAPTURE.into(),
                tree_sha256: Some("d".repeat(64)),
                links: vec!["docs/link".into()],
                removed: vec!["old.py".into()],
                read_at_finish: Vec::new(),
                complete: true,
                unattributed: Vec::new(),
            },
            signer: signer.public().signer(),
        },
    }
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let data = root.join("data");
        std::fs::create_dir(&data).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&data, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        let (key, created) = receipt_key::load_or_create(&data).unwrap();
        assert!(created);
        let repo = root.join("repo");
        std::fs::create_dir_all(repo.join("docs")).unwrap();
        std::fs::write(repo.join("calc.py"), "def add(a, b):\n    return a + b\n").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink("../calc.py", repo.join("docs/link")).unwrap();
        let (envelope, _) = sign_statement(key.key_pair(), &statement(&key)).unwrap();
        let receipt = root.join("r.json");
        std::fs::write(&receipt, envelope_file_bytes(&envelope)).unwrap();
        Self {
            _dir: dir,
            dir: root,
            repo,
            key,
            receipt,
            envelope,
        }
    }

    fn line(&self) -> String {
        self.key.public().line("axocoatl@test")
    }

    fn keys_file(&self) -> PathBuf {
        let path = self.dir.join("trusted-keys");
        std::fs::write(&path, format!("# reviewers\n{}\n", self.line())).unwrap();
        path
    }

    fn request(&self) -> VerifyRequest {
        VerifyRequest {
            file: self.receipt.display().to_string(),
            ..VerifyRequest::default()
        }
    }

    /// A record bundle of `run_id` whose receipt section is `envelope`.
    fn bundle(&self, run_id: &str, envelope: Option<&Envelope>) -> PathBuf {
        use axocoatl_session::record_bundle::{BundleHeader, BundleWriter, RECORD_BUNDLE_SCHEMA};
        let mut writer = BundleWriter::new(
            Vec::new(),
            &BundleHeader {
                schema: RECORD_BUNDLE_SCHEMA.into(),
                run_id: run_id.into(),
                session_id: "ses-1".into(),
                created_at_ms: 2,
                axocoatl_version: "1.4.0".into(),
            },
        )
        .unwrap();
        writer
            .section("manifest", &serde_json::json!({"run_id": run_id}))
            .unwrap();
        if let Some(envelope) = envelope {
            writer
                .section("receipt", &serde_json::to_value(envelope).unwrap())
                .unwrap();
        }
        let path = self
            .dir
            .join(format!("{}.axorecord.jsonl", uuid::Uuid::new_v4()));
        std::fs::write(&path, writer.finish().unwrap()).unwrap();
        path
    }
}

#[cfg(unix)]
#[test]
fn a_receipt_by_a_trusted_key_verifies_against_its_checkout_and_record() {
    let fixture = Fixture::new();
    let report = verify(&VerifyRequest {
        keys: vec![fixture.keys_file()],
        repo: Some(fixture.repo.clone()),
        record: Some(fixture.bundle(RUN, Some(&fixture.envelope))),
        ..fixture.request()
    });
    assert_eq!(report.exit_code, exit::VALID, "{report:?}");
    assert_eq!(report.result, "valid");
    assert_eq!(report.signature.as_deref(), Some("trusted"));
    assert_eq!(report.key_id, Some(fixture.key.public().key_id()));
    let files = report.files.as_ref().unwrap();
    assert_eq!((files.covered, files.removed), (2, 1));
    assert!(files.mismatches.is_empty());
    assert!(report.record.as_ref().unwrap().problems.is_empty());
    // --key takes the same line.
    let report = verify(&VerifyRequest {
        key: vec![fixture.line()],
        ..fixture.request()
    });
    assert_eq!(report.exit_code, exit::VALID, "{report:?}");
    let mut text = Vec::new();
    print_report(&report, false, &mut text);
    let text = String::from_utf8(text).unwrap();
    assert!(text.contains("signature: valid, by trusted key"), "{text}");
    assert!(text.ends_with("result: valid (exit 0)\n"), "{text}");
}

#[cfg(unix)]
#[test]
fn one_changed_byte_of_a_covered_file_or_a_removed_path_back_is_exit_four() {
    let fixture = Fixture::new();
    std::fs::write(
        fixture.repo.join("calc.py"),
        "def add(a, b):\n    return a - b\n",
    )
    .unwrap();
    std::fs::write(fixture.repo.join("old.py"), "back\n").unwrap();
    let report = verify(&VerifyRequest {
        keys: vec![fixture.keys_file()],
        repo: Some(fixture.repo.clone()),
        ..fixture.request()
    });
    assert_eq!(report.exit_code, exit::MISMATCH, "{report:?}");
    let mismatches = &report.files.as_ref().unwrap().mismatches;
    assert_eq!(mismatches.len(), 2);
    assert_eq!(mismatches[0].path, "calc.py");
    assert!(mismatches[0].problem.starts_with("content differs"));
    assert_eq!(mismatches[1].path, "old.py");
    let mut text = Vec::new();
    print_report(&report, false, &mut text);
    let text = String::from_utf8(text).unwrap();
    assert!(text.contains("  calc.py: content differs"), "{text}");
    // As JSON: one document naming the file.
    let mut json = Vec::new();
    print_report(&report, true, &mut json);
    let value: serde_json::Value = serde_json::from_slice(&json).unwrap();
    assert_eq!(value["exit_code"], 4);
    assert_eq!(value["files"]["mismatches"][0]["path"], "calc.py");
}

#[test]
fn without_a_trusted_key_a_valid_signature_is_exit_two() {
    let fixture = Fixture::new();
    let report = verify(&fixture.request());
    assert_eq!(report.exit_code, exit::UNTRUSTED, "{report:?}");
    assert_eq!(report.signature.as_deref(), Some("untrusted"));
    assert!(report.problems[0].contains("not one you trust"));
    // Another key is no better.
    let other = tempfile::tempdir().unwrap();
    let (other_key, _) = receipt_key::load_or_create(other.path()).unwrap();
    let report = verify(&VerifyRequest {
        key: vec![other_key.public().line("someone")],
        ..fixture.request()
    });
    assert_eq!(report.exit_code, exit::UNTRUSTED);
    // Untrusted wins over a file that differs: the subjects are only claims.
    let report = verify(&VerifyRequest {
        repo: Some(fixture.dir.clone()),
        ..fixture.request()
    });
    assert_eq!(report.exit_code, exit::UNTRUSTED);
}

#[test]
fn a_changed_or_malformed_receipt_is_exit_one_and_bad_flags_exit_three() {
    let fixture = Fixture::new();
    let mut tampered = fixture.envelope.clone();
    let mut payload = base64::Engine::decode(
        &base64::engine::general_purpose::STANDARD,
        &tampered.payload,
    )
    .unwrap();
    let at = payload
        .windows(4)
        .position(|window| window == b"pass")
        .unwrap();
    payload[at] = b'P';
    tampered.payload = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &payload);
    std::fs::write(&fixture.receipt, envelope_file_bytes(&tampered)).unwrap();
    let report = verify(&VerifyRequest {
        keys: vec![fixture.keys_file()],
        ..fixture.request()
    });
    assert_eq!(report.exit_code, exit::INVALID, "{report:?}");
    assert_eq!(report.signature.as_deref(), Some("invalid"));
    std::fs::write(&fixture.receipt, "{\"payloadType\":\"x\"}").unwrap();
    assert_eq!(verify(&fixture.request()).exit_code, exit::INVALID);
    assert_eq!(verify(&fixture.request()).result, "malformed");
    for request in [
        VerifyRequest {
            file: fixture.dir.join("missing").display().to_string(),
            ..VerifyRequest::default()
        },
        VerifyRequest {
            key: vec!["ssh-ed25519 AAAA".into()],
            ..fixture.request()
        },
        VerifyRequest {
            keys: vec![fixture.dir.join("no-such-keys")],
            ..fixture.request()
        },
        VerifyRequest {
            repo: Some(fixture.dir.join("no-such-repo")),
            ..fixture.request()
        },
    ] {
        assert_eq!(verify(&request).exit_code, exit::USAGE, "{request:?}");
    }
}

#[test]
fn a_record_must_verify_and_carry_this_receipt_for_its_run() {
    let fixture = Fixture::new();
    let request = |record: PathBuf| VerifyRequest {
        keys: vec![fixture.keys_file()],
        record: Some(record),
        ..fixture.request()
    };
    let other_run = verify(&request(
        fixture.bundle("run-other", Some(&fixture.envelope)),
    ));
    assert_eq!(other_run.exit_code, exit::MISMATCH);
    assert!(other_run.record.unwrap().problems[0].contains("run-other"));
    let none = verify(&request(fixture.bundle(RUN, None)));
    assert_eq!(none.exit_code, exit::MISMATCH);
    assert!(none.record.unwrap().problems[0].contains("carries no receipt"));
    let mut another = fixture.envelope.clone();
    another.signatures[0].keyid = "sha256:00".into();
    let different = verify(&request(fixture.bundle(RUN, Some(&another))));
    assert!(different.record.unwrap().problems[0].contains("another receipt"));
    // One changed byte of the bundle: it no longer verifies.
    let path = fixture.bundle(RUN, Some(&fixture.envelope));
    let mut bytes = std::fs::read(&path).unwrap();
    let at = bytes.windows(8).position(|w| w == b"manifest").unwrap();
    bytes[at] = b'M';
    std::fs::write(&path, bytes).unwrap();
    let broken = verify(&request(path));
    assert_eq!(broken.exit_code, exit::MISMATCH);
    assert!(broken.record.unwrap().problems[0].contains("does not verify"));
}

#[test]
fn a_pull_request_body_holding_the_block_is_a_receipt() {
    let fixture = Fixture::new();
    let body = fixture.dir.join("body.md");
    std::fs::write(
        &body,
        format!(
            "## Axocoatl run\n\n### Receipt\n\nsigned\n\n{}\n",
            pr_block(&fixture.envelope)
        ),
    )
    .unwrap();
    let report = verify(&VerifyRequest {
        file: body.display().to_string(),
        key: vec![fixture.line()],
        ..VerifyRequest::default()
    });
    assert_eq!(report.exit_code, exit::VALID, "{report:?}");
    let mut out = Vec::new();
    assert_eq!(show(&body.display().to_string(), false, &mut out), 0);
    let text = String::from_utf8(out).unwrap();
    assert!(text.starts_with("Not verified:"), "{text}");
    assert!(
        text.contains(&format!("Run {RUN} (fix@1), verdict pass")),
        "{text}"
    );
    assert!(text.contains("  docs/link  sha256:"), "{text}");
    let mut out = Vec::new();
    assert_eq!(
        show(&fixture.receipt.display().to_string(), true, &mut out),
        0
    );
    let value: serde_json::Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(value["predicateType"], PREDICATE_TYPE);
    let mut out = Vec::new();
    assert_eq!(
        show(
            &fixture.dir.join("missing").display().to_string(),
            false,
            &mut out
        ),
        3
    );
}
