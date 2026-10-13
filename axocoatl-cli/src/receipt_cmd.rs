//! `axocoatl receipt show|verify|key` (docs/design/1.4-receipts.md, 2.5).
//!
//! `show` and `verify` are pure: they read a receipt (a DSSE envelope, or a
//! pull request body holding one) and never need a daemon, Podman or the
//! network. `key` reads, makes or replaces this data root's signing key and
//! prints only its public line.
//!
//! `verify` exit codes: 0 valid and signed by a trusted key (and the files
//! and record match, when asked); 1 malformed or the signature does not
//! verify; 2 a valid signature by a key not given as trusted; 3 usage; 4 the
//! files or the record do not match. When several apply, the first of 1, 2,
//! 4 in that order wins.

use std::io::Read;
use std::path::{Path, PathBuf};

use axocoatl_daemon::receipt_key;
use axocoatl_session::receipt::{
    self, check_files, check_signature, open_receipt, parse_key_line, parse_trusted_keys,
    short_key_id, Envelope, OpenedReceipt, PublicKey, SignatureCheck,
};
use clap::Subcommand;

/// Exit codes of `axocoatl receipt`.
pub mod exit {
    pub const VALID: i32 = 0;
    pub const INVALID: i32 = 1;
    pub const UNTRUSTED: i32 = 2;
    pub const USAGE: i32 = 3;
    pub const MISMATCH: i32 = 4;
    /// `key`: the data root or its secret store cannot be read or written.
    pub const FAILURE: i32 = 5;
}

#[derive(Debug, Subcommand)]
pub enum ReceiptCommands {
    /// Show what a receipt says, in words (it is not verified)
    Show {
        /// The receipt: a DSSE envelope, or a pull request body holding one;
        /// `-` for standard input
        file: String,
        /// Print the signed statement as JSON instead
        #[arg(long)]
        json: bool,
    },
    /// Verify a receipt's signature against trusted keys, and optionally a
    /// checkout's files and a record bundle against it
    Verify {
        /// The receipt: a DSSE envelope, or a pull request body holding one;
        /// `-` for standard input
        file: String,
        /// A trusted key, as `axocoatl receipt key` prints it
        #[arg(long = "key", value_name = "LINE")]
        key: Vec<String>,
        /// A file of trusted keys, one line each (`#` comments)
        #[arg(long = "keys", value_name = "FILE")]
        keys: Vec<PathBuf>,
        /// Check each covered file of this checkout against its digest, and
        /// that each removed path is absent
        #[arg(long, value_name = "DIR")]
        repo: Option<PathBuf>,
        /// Check that this record bundle verifies and carries this receipt
        #[arg(long, value_name = "BUNDLE")]
        record: Option<PathBuf>,
        /// Print the result as one JSON document
        #[arg(long)]
        json: bool,
    },
    /// Print this computer's public key line for a trusted-keys file (the
    /// key is made when there is none)
    Key {
        /// Replace the key with a new one; receipts the old key signed still
        /// verify with its line
        #[arg(long)]
        rotate: bool,
        /// Config file used to find the data directory
        #[arg(short, long, default_value_os_t = crate::default_config_path_for_clap())]
        config: PathBuf,
    },
}

/// Run `axocoatl receipt ...`; returns the process exit code.
pub async fn cmd_receipt(command: ReceiptCommands) -> i32 {
    match command {
        ReceiptCommands::Show { file, json } => show(&file, json, &mut std::io::stdout()),
        ReceiptCommands::Verify {
            file,
            key,
            keys,
            repo,
            record,
            json,
        } => {
            let request = VerifyRequest {
                file,
                key,
                keys,
                repo,
                record,
            };
            let report = verify(&request);
            print_report(&report, json, &mut std::io::stdout());
            report.exit_code
        }
        ReceiptCommands::Key { rotate, config } => key(&config, rotate),
    }
}

/// The bytes of `file`, or of standard input for `-`, at most the largest
/// receipt.
fn read_input(file: &str) -> Result<Vec<u8>, String> {
    let limit = receipt::MAX_ENVELOPE_BYTES as u64 + 1;
    let mut bytes = Vec::new();
    if file == "-" {
        std::io::stdin()
            .take(limit)
            .read_to_end(&mut bytes)
            .map_err(|error| format!("standard input: {error}"))?;
    } else {
        std::fs::File::open(file)
            .and_then(|opened| opened.take(limit).read_to_end(&mut bytes))
            .map_err(|error| format!("{file}: {error}"))?;
    }
    if bytes.len() as u64 >= limit {
        return Err(format!(
            "{file}: larger than {} bytes",
            receipt::MAX_ENVELOPE_BYTES
        ));
    }
    Ok(bytes)
}

fn show(file: &str, json: bool, out: &mut dyn std::io::Write) -> i32 {
    let bytes = match read_input(file) {
        Ok(bytes) => bytes,
        Err(error) => {
            eprintln!("axocoatl receipt: {error}");
            return exit::USAGE;
        }
    };
    let opened = match open_receipt(&bytes) {
        Ok(opened) => opened,
        Err(error) => {
            eprintln!("axocoatl receipt: not a receipt: {error}");
            return exit::INVALID;
        }
    };
    if json {
        let value: serde_json::Value =
            serde_json::from_slice(&opened.payload).unwrap_or(serde_json::Value::Null);
        let _ = writeln!(
            out,
            "{}",
            serde_json::to_string_pretty(&value).unwrap_or_default()
        );
    } else {
        let _ = writeln!(
            out,
            "Not verified: this is what the receipt says. `axocoatl receipt verify` checks its \
             signature against keys you trust."
        );
        for line in receipt::describe(&opened.statement) {
            let _ = writeln!(out, "{line}");
        }
        let _ = writeln!(out, "Statement {}", opened.payload_digest());
    }
    exit::VALID
}

/// What `verify` was asked.
#[derive(Debug, Default)]
pub struct VerifyRequest {
    pub file: String,
    pub key: Vec<String>,
    pub keys: Vec<PathBuf>,
    pub repo: Option<PathBuf>,
    pub record: Option<PathBuf>,
}

/// What `verify` found: the JSON document `--json` prints.
#[derive(Debug, serde::Serialize)]
pub struct VerifyReport {
    pub exit_code: i32,
    /// `valid`, `malformed`, `invalid`, `untrusted` or `mismatch`, or
    /// `usage`.
    pub result: String,
    /// Why, in words, for any result but `valid`.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub problems: Vec<String>,
    /// `trusted`, `untrusted` or `invalid`, once the receipt could be read.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub signature: Option<String>,
    /// The key that signed, or the key the receipt names.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub key_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub verdict: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub statement_sha256: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub files: Option<FilesReport>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub record: Option<RecordReport>,
    /// What the receipt says, in words (not a check).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub statement: Vec<String>,
}

#[derive(Debug, serde::Serialize)]
pub struct FilesReport {
    pub repo: String,
    pub covered: usize,
    pub removed: usize,
    /// `judged_capture`, `after_capture` or `workspace_at_finish`.
    pub source: String,
    /// Whether the receipt covers every path the run changed.
    pub complete: bool,
    pub mismatches: Vec<FileProblem>,
}

#[derive(Debug, serde::Serialize)]
pub struct FileProblem {
    pub path: String,
    pub problem: String,
}

#[derive(Debug, serde::Serialize)]
pub struct RecordReport {
    pub file: String,
    pub problems: Vec<String>,
}

impl VerifyReport {
    fn new(exit_code: i32, result: &str, problem: String) -> Self {
        Self {
            exit_code,
            result: result.into(),
            problems: vec![problem],
            signature: None,
            key_id: None,
            run_id: None,
            verdict: None,
            statement_sha256: None,
            files: None,
            record: None,
            statement: Vec::new(),
        }
    }
}

/// The trusted keys of `--key` lines and `--keys` files.
fn trusted_keys(request: &VerifyRequest) -> Result<Vec<PublicKey>, String> {
    let mut keys = Vec::new();
    for line in &request.key {
        match parse_key_line(line) {
            Ok(Some(key)) => keys.push(key),
            Ok(None) => return Err("--key: an empty line or a comment is not a key".into()),
            Err(error) => return Err(format!("--key: {error}")),
        }
    }
    for file in &request.keys {
        let text = std::fs::read_to_string(file)
            .map_err(|error| format!("--keys {}: {error}", file.display()))?;
        keys.extend(
            parse_trusted_keys(&text)
                .map_err(|error| format!("--keys {}: {error}", file.display()))?,
        );
    }
    Ok(keys)
}

/// Whether the bundle at `file` verifies and carries `opened` for its run.
fn check_record(opened: &OpenedReceipt, file: &Path) -> Vec<String> {
    let opened_file = match std::fs::File::open(file) {
        Ok(opened_file) => opened_file,
        Err(error) => return vec![format!("{}: {error}", file.display())],
    };
    let summary = match axocoatl_session::record_bundle::verify_bundle(std::io::BufReader::new(
        opened_file,
    )) {
        Ok(summary) => summary,
        Err(error) => return vec![format!("the record bundle does not verify: {error}")],
    };
    let mut problems = Vec::new();
    let run_id = &opened.statement.predicate.run.run_id;
    if summary.header.run_id != *run_id {
        problems.push(format!(
            "the record bundle is of run {}, the receipt of run {run_id}",
            summary.header.run_id
        ));
    }
    match summary.receipt {
        None => problems.push(format!(
            "the record bundle (schema {}) carries no receipt",
            summary.header.schema
        )),
        Some(value) => match serde_json::from_value::<Envelope>(value) {
            Ok(envelope) if envelope == opened.envelope => {}
            Ok(_) => problems.push("the record bundle carries another receipt".into()),
            Err(error) => problems.push(format!(
                "the record bundle's receipt section is not an envelope: {error}"
            )),
        },
    }
    problems
}

/// `axocoatl receipt verify`, without printing.
pub fn verify(request: &VerifyRequest) -> VerifyReport {
    let trusted = match trusted_keys(request) {
        Ok(trusted) => trusted,
        Err(error) => return VerifyReport::new(exit::USAGE, "usage", error),
    };
    if let Some(repo) = &request.repo {
        if !repo.is_dir() {
            return VerifyReport::new(
                exit::USAGE,
                "usage",
                format!("--repo {}: not a directory", repo.display()),
            );
        }
    }
    let bytes = match read_input(&request.file) {
        Ok(bytes) => bytes,
        Err(error) => return VerifyReport::new(exit::USAGE, "usage", error),
    };
    let opened = match open_receipt(&bytes) {
        Ok(opened) => opened,
        Err(error) => {
            return VerifyReport::new(exit::INVALID, "malformed", format!("not a receipt: {error}"))
        }
    };
    let statement = &opened.statement;
    let mut report = VerifyReport {
        exit_code: exit::VALID,
        result: "valid".into(),
        problems: Vec::new(),
        signature: None,
        key_id: None,
        run_id: Some(statement.predicate.run.run_id.clone()),
        verdict: serde_json::to_value(statement.predicate.verdict)
            .ok()
            .and_then(|value| value.as_str().map(str::to_string)),
        statement_sha256: Some(opened.payload_digest()),
        files: None,
        record: None,
        statement: receipt::describe(statement),
    };
    let mut untrusted = false;
    match check_signature(&opened, &trusted) {
        SignatureCheck::Trusted { key_id } => {
            report.signature = Some("trusted".into());
            report.key_id = Some(key_id);
        }
        SignatureCheck::Untrusted { key_id } => {
            report.signature = Some("untrusted".into());
            report.problems.push(format!(
                "the signature is valid, but key {} ({key_id}) is not one you trust: give its \
                 line with --key or --keys",
                short_key_id(&key_id)
            ));
            report.key_id = Some(key_id);
            untrusted = true;
        }
        SignatureCheck::Invalid(reason) => {
            report.signature = Some("invalid".into());
            report.key_id = Some(statement.predicate.signer.key_id.clone());
            report.exit_code = exit::INVALID;
            report.result = "invalid".into();
            report.problems.push(format!("the signature does not verify: {reason}"));
            // Nothing else the receipt says is worth checking.
            return report;
        }
    }
    let mut mismatched = false;
    if let Some(repo) = &request.repo {
        let mismatches: Vec<FileProblem> = check_files(statement, repo)
            .into_iter()
            .map(|mismatch| FileProblem {
                path: mismatch.path,
                problem: mismatch.problem,
            })
            .collect();
        if !mismatches.is_empty() {
            mismatched = true;
            report.problems.push(format!(
                "{} of the receipt's {} paths differ in {}",
                mismatches.len(),
                statement.subject.len() + statement.predicate.files.removed.len(),
                repo.display()
            ));
        }
        report.files = Some(FilesReport {
            repo: repo.display().to_string(),
            covered: statement.subject.len(),
            removed: statement.predicate.files.removed.len(),
            source: statement.predicate.files.source.clone(),
            complete: statement.predicate.files.complete,
            mismatches,
        });
    }
    if let Some(file) = &request.record {
        let problems = check_record(&opened, file);
        if !problems.is_empty() {
            mismatched = true;
            report
                .problems
                .push(format!("the record {} does not match", file.display()));
        }
        report.record = Some(RecordReport {
            file: file.display().to_string(),
            problems,
        });
    }
    if untrusted {
        report.exit_code = exit::UNTRUSTED;
        report.result = "untrusted".into();
    } else if mismatched {
        report.exit_code = exit::MISMATCH;
        report.result = "mismatch".into();
    }
    report
}

fn print_report(report: &VerifyReport, json: bool, out: &mut dyn std::io::Write) {
    if json {
        let _ = writeln!(
            out,
            "{}",
            serde_json::to_string_pretty(report).unwrap_or_default()
        );
        return;
    }
    if let Some(run) = &report.run_id {
        let _ = writeln!(
            out,
            "receipt of run {run}, verdict {}",
            report.verdict.as_deref().unwrap_or("?")
        );
    }
    match (report.signature.as_deref(), report.key_id.as_deref()) {
        (Some("trusted"), Some(key)) => {
            let _ = writeln!(
                out,
                "signature: valid, by trusted key {} ({key})",
                short_key_id(key)
            );
        }
        (Some("untrusted"), Some(key)) => {
            let _ = writeln!(
                out,
                "signature: valid, by key {} ({key}), which is not one you trust",
                short_key_id(key)
            );
        }
        (Some("invalid"), _) => {
            let _ = writeln!(out, "signature: INVALID");
        }
        _ => {}
    }
    if let Some(files) = &report.files {
        if files.mismatches.is_empty() {
            let _ = writeln!(
                out,
                "files: {} covered and {} removed match {} ({}{})",
                files.covered,
                files.removed,
                files.repo,
                files.source,
                if files.complete {
                    ""
                } else {
                    "; the receipt does not cover every path the run changed"
                }
            );
        } else {
            let _ = writeln!(out, "files: {} differ in {}:", files.mismatches.len(), files.repo);
            for mismatch in &files.mismatches {
                let _ = writeln!(out, "  {}: {}", mismatch.path, mismatch.problem);
            }
        }
    }
    if let Some(record) = &report.record {
        if record.problems.is_empty() {
            let _ = writeln!(
                out,
                "record: {} verifies and carries this receipt",
                record.file
            );
        } else {
            for problem in &record.problems {
                let _ = writeln!(out, "record: {problem}");
            }
        }
    }
    for problem in &report.problems {
        let _ = writeln!(out, "problem: {problem}");
    }
    let _ = writeln!(
        out,
        "result: {} (exit {})",
        report.result, report.exit_code
    );
}

fn key(config: &Path, rotate: bool) -> i32 {
    let data_dir = match crate::secret_cmd::data_dir(config) {
        Ok(data_dir) => data_dir,
        Err(error) => {
            eprintln!("axocoatl receipt: {error}");
            return exit::FAILURE;
        }
    };
    let comment = receipt_key::host_comment();
    if rotate {
        return match receipt_key::rotate(&data_dir) {
            Ok((key, previous)) => {
                match previous {
                    Some(previous) => eprintln!(
                        "Replaced receipt signing key {} with {}. Receipts the old key signed \
                         still verify with its line; keep it where they are verified, or remove \
                         it there to stop trusting them.",
                        previous.short_id(),
                        key.public().short_id()
                    ),
                    None => eprintln!(
                        "Made a new receipt signing key {} for {}.",
                        key.public().short_id(),
                        data_dir.display()
                    ),
                }
                println!("{}", key.public().line(&comment));
                exit::VALID
            }
            Err(error) => {
                eprintln!("axocoatl receipt: {error}");
                exit::FAILURE
            }
        };
    }
    match receipt_key::load_or_create(&data_dir) {
        Ok((key, created)) => {
            if created {
                eprintln!(
                    "Made a new receipt signing key {} for {}; the daemon signs every finished run \
                     with it.",
                    key.public().short_id(),
                    data_dir.display()
                );
            }
            println!("{}", key.public().line(&comment));
            exit::VALID
        }
        Err(error) => {
            eprintln!("axocoatl receipt: {error}");
            exit::FAILURE
        }
    }
}

#[cfg(test)]
#[path = "receipt_cmd_tests.rs"]
mod tests;
