//! Signed receipts of loadout runs (docs/design/1.4-receipts.md): an
//! [in-toto Statement v1](https://github.com/in-toto/attestation/blob/main/spec/v1/statement.md)
//! about one finished run, signed by the data root's Ed25519 key inside a
//! [DSSE](https://github.com/secure-systems-lab/dsse/blob/master/envelope.md)
//! envelope.
//!
//! Everything here is pure: the daemon builds and signs a statement with a
//! key it reads from its secret store; `axocoatl receipt show|verify` and
//! Keep as PR read envelopes without a daemon, Podman or the network.
//!
//! A receipt proves that the holder of one key signed exactly these bytes
//! (integrity and signer identity). It does not prove the code is correct,
//! and it says nothing a verifier should trust unless the key is one the
//! verifier already trusts ([`check_signature`]).

use std::collections::BTreeMap;
use std::io::Read;
use std::path::Path;

use base64::Engine as _;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::run_outcome::{NetworkSummary, RunVerdict};

/// `_type` of the statement.
pub const STATEMENT_TYPE: &str = "https://in-toto.io/Statement/v1";
/// `predicateType` of an Axocoatl receipt.
pub const PREDICATE_TYPE: &str = "https://axocoatl.ai/receipt/v1";
/// DSSE `payloadType` of an in-toto statement.
pub const PAYLOAD_TYPE: &str = "application/vnd.in-toto+json";
/// Media type of a DSSE envelope (`GET /api/runs/{run_id}/receipt`).
pub const ENVELOPE_MEDIA_TYPE: &str = "application/vnd.dsse.envelope.v1+json";
/// File name of a run's receipt in its run directory.
pub const RECEIPT_FILE: &str = "receipt.dsse.json";
/// The only signing algorithm.
pub const KEY_ALGORITHM: &str = "ed25519";
/// How the receipt block in a pull request body begins.
pub const PR_BLOCK_START: &str = "<!-- axocoatl-receipt:v1";
/// How it ends.
pub const PR_BLOCK_END: &str = "-->";
/// Largest envelope (or text holding one) read.
pub const MAX_ENVELOPE_BYTES: usize = 16 * 1024 * 1024;
/// Warning code of a finished run the daemon could not sign.
pub const RECEIPT_UNAVAILABLE: &str = "receipt_unavailable";
/// Warning code of a finished run whose receipt does not cover every path
/// the run changed (`predicate.files.complete` is false): what it names is
/// signed, but it does not prove what the run left.
pub const RECEIPT_INCOMPLETE: &str = "receipt_incomplete";

/// `predicate.files.source`: where the subject digests come from.
pub mod source {
    /// The capture of the final candidate the run's required checks ran on.
    pub const JUDGED_CAPTURE: &str = "judged_capture";
    /// The After capture of the run's last writer activation.
    pub const AFTER_CAPTURE: &str = "after_capture";
    /// The files as the host read them from the working tree when the run
    /// finished.
    pub const WORKSPACE_AT_FINISH: &str = "workspace_at_finish";
}

#[derive(Debug, thiserror::Error)]
pub enum ReceiptError {
    /// Not a receipt this version reads: bad JSON, base64, type or field.
    #[error("{0}")]
    Malformed(String),
    /// The statement could not be signed.
    #[error("{0}")]
    Signing(String),
}

fn malformed(message: impl Into<String>) -> ReceiptError {
    ReceiptError::Malformed(message.into())
}

/// Lowercase hex of `bytes`.
pub fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

/// Lowercase hex SHA-256 of `bytes`.
pub fn sha256_hex(bytes: &[u8]) -> String {
    hex(&Sha256::digest(bytes))
}

// ---------------------------------------------------------------------------
// Statement

/// One file the run changed that exists after it, with the SHA-256 of its
/// content (of its target, for a symbolic link), or a submodule it changed
/// with the commit the submodule holds (`gitCommit`, in-toto's name for it).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Subject {
    pub name: String,
    pub digest: BTreeMap<String, String>,
}

impl Subject {
    pub fn sha256(name: impl Into<String>, digest: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            digest: BTreeMap::from([("sha256".to_string(), digest.into())]),
        }
    }

    /// A submodule and the commit it holds ([`submodule_commit`]).
    pub fn git_commit(name: impl Into<String>, commit: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            digest: BTreeMap::from([(GIT_COMMIT.to_string(), commit.into())]),
        }
    }

    /// The SHA-256 digest, when there is one.
    pub fn sha256_digest(&self) -> Option<&str> {
        self.digest.get("sha256").map(String::as_str)
    }

    /// The commit of a submodule subject, when it is one.
    pub fn git_commit_digest(&self) -> Option<&str> {
        self.digest.get(GIT_COMMIT).map(String::as_str)
    }
}

/// The digest name of a submodule subject's commit.
pub const GIT_COMMIT: &str = "gitCommit";

/// The in-toto statement a receipt signs.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Statement {
    #[serde(rename = "_type")]
    pub statement_type: String,
    pub subject: Vec<Subject>,
    #[serde(rename = "predicateType")]
    pub predicate_type: String,
    pub predicate: Predicate,
}

/// What the host observed about the run. Field order is fixed by this
/// struct; maps are `BTreeMap`s; the bytes are `serde_json::to_vec` of the
/// statement, never re-serialized by a verifier.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Predicate {
    pub run: RunFacts,
    pub repository: RepositoryFacts,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub loadout: Option<LoadoutFacts>,
    /// Ordinary dashboard turns carry their own immutable provenance, never
    /// a fabricated loadout identity. Old run receipts omit this field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub native_turn: Option<NativeTurnFacts>,
    pub agents: Vec<AgentFacts>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sandbox: Option<SandboxFacts>,
    /// The Outcome's network summary.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub network: Option<NetworkSummary>,
    pub checks: Vec<CheckFacts>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review: Option<ReviewFacts>,
    pub adjudications: Vec<AdjudicationFacts>,
    pub verdict: RunVerdict,
    pub exit_code: i32,
    /// Why the run needs attention, in words (the Outcome's `attention`).
    pub attention: Vec<String>,
    pub not_covered: NotCoveredFacts,
    /// Warning codes of the Outcome.
    pub warnings: Vec<String>,
    pub usage: UsageFacts,
    pub files: FileFacts,
    /// The key that signed: its public half, so a verifier can say whether
    /// the signature is valid before deciding whether to trust the key.
    /// Trust comes only from the keys the verifier names.
    pub signer: SignerFacts,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunFacts {
    pub run_id: String,
    pub session_id: String,
    pub started_at_ms: u64,
    /// Absent when the retained native evidence does not record closure time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at_ms: Option<u64>,
    /// The Axocoatl version that signed.
    pub axocoatl_version: String,
    /// SHA-256 of the task text (the text is in the record).
    pub task_sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepositoryFacts {
    /// `HEAD` when the run started.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_commit: Option<String>,
    /// Paths with uncommitted changes when the run started.
    pub dirty_paths: Vec<String>,
    /// The `origin` remote as `https://host/path`, without credentials;
    /// absent when there is none or it names no hosted repository.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin_url: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LoadoutFacts {
    pub id: String,
    pub version: u32,
    pub kind: String,
    /// SHA-256 of the exact loadout file.
    pub digest: String,
    pub builtin: bool,
}

/// Evidence identity of an ordinary native Session turn. This records the
/// terminal revision, not today's mutable team or repository configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NativeTurnFacts {
    pub turn_id: String,
    pub turn_revision: u64,
    pub graph_revision: u64,
    pub state: String,
    pub evidence_sha256: String,
    pub configuration_revisions: Vec<u64>,
    pub definition_snapshots: Vec<String>,
    /// Facts that the retained turn cannot establish. Absent policy or
    /// network fields are unknown, never a claim of isolation or zero egress.
    pub limitations: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentFacts {
    pub id: String,
    pub role: String,
    /// `native`, `claude-code` or `codex`.
    pub runtime: String,
    /// The pinned version of an external program; for a native Agent on
    /// Ollama, the Ollama server that served its calls (`ollama 0.40.6`;
    /// several versions comma-separated when Ollama was upgraded during the
    /// run); for one on OpenRouter, the endpoints that answered its calls in
    /// the order they first answered (`openrouter endpoint deepinfra/fp8
    /// (DeepInfra)`; `openrouter endpoints …`, comma-separated, when a call
    /// moved to another endpoint).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub program_version: Option<String>,
    pub provider: String,
    pub model: String,
    /// The stored credential an external writer's route sends (its name,
    /// never its value).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credential: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SandboxFacts {
    /// `podman`.
    pub backend: String,
    /// `egress` or `none`.
    pub network: String,
    /// `hardened`.
    pub workload: String,
    /// The last network policy the Session's record holds for each scope.
    pub policies: Vec<PolicyFacts>,
    /// The image, toolchain, setup and check the run's admission chose and
    /// what its setup did (receipts since 1.5). Absent from older receipts;
    /// a verifier that does not know the field ignores it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub environment: Option<EnvironmentFacts>,
    /// The resource caps the Session's container got, and those its host
    /// could not apply (receipts since 1.5).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limits: Option<crate::SessionResourceLimits>,
}

/// What a loadout run's admission chose for its Session's environment, and
/// what its setup and check probe did (workstream `toolchain`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnvironmentFacts {
    /// `node`, `python`, `rust` or `go`; absent for a run without one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub toolchain: Option<String>,
    /// Why: the files that named it, `--toolchain`, or the loadout.
    pub reason: String,
    /// The image reference the Session started from: a recipe image name,
    /// the loadout's or `--image`'s reference, or the default image.
    pub image: String,
    /// The exact image id, for a recipe image.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image_id: Option<String>,
    /// The recipes the image was built from.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub recipes: Vec<String>,
    /// The command the `detected` checks ran, and where it came from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub check: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub check_source: Option<String>,
    /// The toolchain's dependency install, when the run had one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub setup: Option<SetupFacts>,
    /// The Workspace's toolchain cache volume the Session mounted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_volume: Option<String>,
    /// The check probe before the first model call.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub probe: Option<ProbeFacts>,
}

/// One toolchain setup: the command, how it ended, and the network it had.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SetupFacts {
    /// The command as approved (`npm ci ...`, `pip install ...`, `--setup`).
    pub command: String,
    /// `detected`, `--setup` or `loadout`.
    pub source: String,
    /// SHA-256 of the exact shell script that ran (it wraps the command
    /// with the toolchain's preparation, such as Python's virtual
    /// environment).
    pub script_sha256: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    pub duration_ms: u64,
    /// Whether the Workspace's cache already held this exact setup, so
    /// nothing was installed.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub unchanged: bool,
    /// The package registry hosts the setup alone could reach, opened
    /// before it ran and closed before the first model call.
    pub registry_hosts: Vec<String>,
    /// Connections the setup's own processes opened, by the network record.
    pub allowed_connections: u64,
    pub refused_connections: u64,
    /// Why the setup did not finish, when it did not.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// The probe of the check before the first model call: whether its program
/// is in the image, and what the check did on the untouched tree.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProbeFacts {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub program: Option<String>,
    pub program_found: bool,
    /// The check's exit code on the untouched tree (a failing check is what
    /// a fix run is for).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub baseline_exit: Option<i32>,
    pub duration_ms: u64,
    /// The check outlasted the probe's time and was stopped (with every
    /// process it started); it could run, so the run went on.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub timed_out: bool,
}

/// One scope's network policy as the Session's network record holds it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PolicyFacts {
    /// `session`, `provisioning` or `browser`.
    pub scope: String,
    pub revision: u64,
    /// SHA-256 of the canonical compiled policy, routes included.
    pub digest: String,
    /// The allowlist hosts and each route (host, ports, rules, credential
    /// name, who may use it), as the record renders them.
    pub rules: Vec<String>,
    /// Each route of `rules`, and whether it checked model-call bodies.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub routes: Vec<RouteFacts>,
}

/// One route of a scope's network policy, as its recorded rules name it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RouteFacts {
    /// `route#<n>`, or `host_ollama`.
    pub id: String,
    pub host: String,
    /// `refuse`: the route read each request body before it left and
    /// refused one that asked the model provider to reach other hosts or
    /// run tools itself (fetch a URL, search the web, connect to an MCP
    /// server, run a provider-hosted tool); `allow`: `provider_fetch:
    /// allow` sent bodies unread. Absent for a route to a host that is not
    /// a model API, whose bodies pass unread.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_fetch: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckFacts {
    pub name: String,
    /// `passed`, `failed`, `timed_out`, `not_run` or `unavailable`.
    pub state: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
    pub argv: Vec<String>,
    /// The repository tree the check ran on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub candidate_sha256: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewFacts {
    pub provider: String,
    pub model: String,
    pub runtime: String,
    /// For a native reviewer, the Ollama server that served its calls
    /// (`ollama 0.40.6`) or the OpenRouter endpoints that answered them, as
    /// [`AgentFacts::program_version`] says it. Absent otherwise; a verifier
    /// that does not know the field ignores it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub program_version: Option<String>,
    /// `approved`, `changes`, ... (the Outcome's review state).
    pub verdict: String,
    pub passed: bool,
    pub rounds: u32,
    pub max_rounds: u32,
    pub findings: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdjudicationFacts {
    pub round: u32,
    pub id: String,
    /// `accept`, `reject` or `missing`.
    pub decision: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NotCoveredFacts {
    pub count: u64,
    pub areas: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UsageFacts {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cost_microunits: u64,
    pub cost_usd: f64,
    pub complete: bool,
    pub cost_known: bool,
    pub cost_computed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileFacts {
    /// [`source`]: `judged_capture`, `after_capture` or
    /// `workspace_at_finish`.
    pub source: String,
    /// The capture's tree digest, for a capture source.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tree_sha256: Option<String>,
    /// Subjects that are symbolic links (their digest is of the target).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub links: Vec<String>,
    /// Paths the run changed that do not exist after it.
    pub removed: Vec<String>,
    /// Subjects (or removed paths) whose state the host read from the
    /// working tree when the run finished, although `source` is a capture:
    /// the capture's lines of the paths that differ from `HEAD` did not
    /// hold them, so in that tree they were as in `HEAD` (a change the run
    /// made and undid).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub read_at_finish: Vec<String>,
    /// Every path the run changed is a subject or removed.
    pub complete: bool,
    /// What could not be attributed or read, and why.
    pub unattributed: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SignerFacts {
    pub algorithm: String,
    /// Base64 of the 32-byte raw public key.
    pub public_key: String,
    /// `sha256:` + hex SHA-256 of the raw public key.
    pub key_id: String,
}

// ---------------------------------------------------------------------------
// Keys

/// An Ed25519 public key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PublicKey([u8; 32]);

impl PublicKey {
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, ReceiptError> {
        let raw: [u8; 32] = bytes
            .try_into()
            .map_err(|_| malformed("an Ed25519 public key is 32 bytes"))?;
        Ok(Self(raw))
    }

    pub fn from_base64(text: &str) -> Result<Self, ReceiptError> {
        let bytes = decode_base64(text).map_err(|_| malformed("the public key is not base64"))?;
        Self::from_bytes(&bytes)
    }

    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    pub fn base64(&self) -> String {
        base64::engine::general_purpose::STANDARD.encode(self.0)
    }

    /// `sha256:` + hex SHA-256 of the raw key.
    pub fn key_id(&self) -> String {
        format!("sha256:{}", sha256_hex(&self.0))
    }

    /// The first 16 hex characters of the key id: how people see it.
    pub fn short_id(&self) -> String {
        short_key_id(&self.key_id())
    }

    /// `ed25519 <base64> <comment> <short id>`: one line of a trusted-keys
    /// file.
    pub fn line(&self, comment: &str) -> String {
        format!(
            "{KEY_ALGORITHM} {} {comment} {}",
            self.base64(),
            self.short_id()
        )
    }

    pub fn signer(&self) -> SignerFacts {
        SignerFacts {
            algorithm: KEY_ALGORITHM.into(),
            public_key: self.base64(),
            key_id: self.key_id(),
        }
    }

    /// Whether `signature` is this key's signature of `message`.
    pub fn verifies(&self, message: &[u8], signature: &[u8]) -> bool {
        ring::signature::UnparsedPublicKey::new(&ring::signature::ED25519, &self.0)
            .verify(message, signature)
            .is_ok()
    }
}

/// The first 16 hex characters of a `sha256:<hex>` key id.
pub fn short_key_id(key_id: &str) -> String {
    key_id
        .strip_prefix("sha256:")
        .unwrap_or(key_id)
        .chars()
        .take(16)
        .collect()
}

/// The public key of a key pair.
pub fn public_key_of(key_pair: &ring::signature::Ed25519KeyPair) -> PublicKey {
    use ring::signature::KeyPair as _;
    let mut raw = [0u8; 32];
    raw.copy_from_slice(key_pair.public_key().as_ref());
    PublicKey(raw)
}

/// Parse one trusted-keys line: `ed25519 <base64> [comment...]`. `None`
/// for a blank line or a `#` comment.
pub fn parse_key_line(line: &str) -> Result<Option<PublicKey>, ReceiptError> {
    let line = line.trim();
    if line.is_empty() || line.starts_with('#') {
        return Ok(None);
    }
    let mut words = line.split_whitespace();
    let algorithm = words.next().unwrap_or_default();
    if algorithm != KEY_ALGORITHM {
        return Err(malformed(format!(
            "a trusted key line starts with {KEY_ALGORITHM}, not {:?}",
            algorithm.chars().take(40).collect::<String>()
        )));
    }
    let key = words
        .next()
        .ok_or_else(|| malformed("a trusted key line has no public key after ed25519"))?;
    PublicKey::from_base64(key).map(Some)
}

/// Every key of a trusted-keys file (`#` comments and blank lines skipped).
/// A line that is not a key is an error naming its number.
pub fn parse_trusted_keys(text: &str) -> Result<Vec<PublicKey>, ReceiptError> {
    let mut keys = Vec::new();
    for (index, line) in text.lines().enumerate() {
        match parse_key_line(line) {
            Ok(Some(key)) => keys.push(key),
            Ok(None) => {}
            Err(error) => return Err(malformed(format!("line {}: {error}", index + 1))),
        }
    }
    Ok(keys)
}

// ---------------------------------------------------------------------------
// Envelope

/// A DSSE envelope.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Envelope {
    #[serde(rename = "payloadType")]
    pub payload_type: String,
    /// Base64 of the statement bytes.
    pub payload: String,
    pub signatures: Vec<EnvelopeSignature>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnvelopeSignature {
    #[serde(default)]
    pub keyid: String,
    /// Base64 of the Ed25519 signature of the PAE.
    pub sig: String,
}

/// DSSE's pre-authentication encoding:
/// `"DSSEv1" SP LEN(type) SP type SP LEN(body) SP body`, lengths in ASCII
/// decimal bytes.
pub fn pae(payload_type: &str, payload: &[u8]) -> Vec<u8> {
    let mut out = format!(
        "DSSEv1 {} {payload_type} {} ",
        payload_type.len(),
        payload.len()
    )
    .into_bytes();
    out.extend_from_slice(payload);
    out
}

/// Standard or URL-safe base64, with or without padding (DSSE allows
/// either).
fn decode_base64(text: &str) -> Result<Vec<u8>, base64::DecodeError> {
    use base64::engine::general_purpose::{STANDARD, STANDARD_NO_PAD, URL_SAFE, URL_SAFE_NO_PAD};
    STANDARD
        .decode(text)
        .or_else(|_| STANDARD_NO_PAD.decode(text))
        .or_else(|_| URL_SAFE.decode(text))
        .or_else(|_| URL_SAFE_NO_PAD.decode(text))
}

/// The statement's bytes, as signed.
pub fn statement_bytes(statement: &Statement) -> Result<Vec<u8>, ReceiptError> {
    serde_json::to_vec(statement).map_err(|error| ReceiptError::Signing(error.to_string()))
}

fn validate_provenance(statement: &Statement) -> Result<(), ReceiptError> {
    let predicate = &statement.predicate;
    match (&predicate.loadout, &predicate.native_turn) {
        (Some(_), None) if predicate.sandbox.is_some() && predicate.network.is_some() => Ok(()),
        (None, Some(turn)) => {
            let digest = &turn.evidence_sha256;
            if turn.turn_id.is_empty()
                || turn.turn_id != predicate.run.run_id
                || predicate.run.session_id.is_empty()
                || turn.turn_revision == 0
                || turn.graph_revision == 0
                || digest.len() != 64
                || !digest.bytes().all(|b| b.is_ascii_hexdigit())
                || !matches!(turn.state.as_str(), "completed" | "cancelled" | "finished")
            {
                return Err(malformed(
                    "native receipt provenance is inconsistent or not terminal",
                ));
            }
            let expected = match turn.state.as_str() {
                "completed" => (RunVerdict::Pass, 0),
                "cancelled" => (RunVerdict::Interrupted, 6),
                _ => (RunVerdict::NeedsAttention, 2),
            };
            if (predicate.verdict, predicate.exit_code) != expected {
                return Err(malformed(
                    "native receipt verdict disagrees with its terminal state",
                ));
            }
            if turn.state == "completed"
                && (predicate.checks.iter().any(|check| check.state != "passed")
                    || predicate
                        .review
                        .as_ref()
                        .is_some_and(|review| !review.passed))
            {
                return Err(malformed("a completed native receipt cannot contain failed or unavailable completion conditions"));
            }
            Ok(())
        }
        _ => Err(malformed(
            "a receipt must name exactly one complete loadout or native turn provenance",
        )),
    }
}

/// Sign `statement` with `key_pair`. Its `predicate.signer` must name that
/// key. Returns the envelope and the exact statement bytes it carries.
pub fn sign_statement(
    key_pair: &ring::signature::Ed25519KeyPair,
    statement: &Statement,
) -> Result<(Envelope, Vec<u8>), ReceiptError> {
    let key = public_key_of(key_pair);
    if statement.predicate.signer != key.signer() {
        return Err(ReceiptError::Signing(
            "the statement names another signer than the key signing it".into(),
        ));
    }
    if statement.statement_type != STATEMENT_TYPE || statement.predicate_type != PREDICATE_TYPE {
        return Err(ReceiptError::Signing(
            "the statement's _type or predicateType is not a receipt's".into(),
        ));
    }
    validate_provenance(statement)?;
    let payload = statement_bytes(statement)?;
    let signature = key_pair.sign(&pae(PAYLOAD_TYPE, &payload));
    let envelope = Envelope {
        payload_type: PAYLOAD_TYPE.into(),
        payload: base64::engine::general_purpose::STANDARD.encode(&payload),
        signatures: vec![EnvelopeSignature {
            keyid: key.key_id(),
            sig: base64::engine::general_purpose::STANDARD.encode(signature.as_ref()),
        }],
    };
    Ok((envelope, payload))
}

/// The bytes of an envelope as stored (`receipt.dsse.json`): compact JSON
/// and a final newline.
pub fn envelope_file_bytes(envelope: &Envelope) -> Vec<u8> {
    let mut bytes = serde_json::to_vec(envelope).unwrap_or_default();
    bytes.push(b'\n');
    bytes
}

/// An envelope read and its statement parsed. Nothing here is verified
/// yet: see [`check_signature`].
#[derive(Debug, Clone, PartialEq)]
pub struct OpenedReceipt {
    pub envelope: Envelope,
    /// The exact statement bytes the signature covers.
    pub payload: Vec<u8>,
    pub statement: Statement,
}

impl OpenedReceipt {
    /// `sha256:` + hex SHA-256 of the statement bytes (Keep's
    /// `Axocoatl-Receipt` trailer).
    pub fn payload_digest(&self) -> String {
        format!("sha256:{}", sha256_hex(&self.payload))
    }

    /// The signer's public key, as the statement names it.
    pub fn signer(&self) -> Result<PublicKey, ReceiptError> {
        let signer = &self.statement.predicate.signer;
        if signer.algorithm != KEY_ALGORITHM {
            return Err(malformed(format!(
                "the signer's algorithm is {:?}, not {KEY_ALGORITHM}",
                signer.algorithm
            )));
        }
        let key = PublicKey::from_base64(&signer.public_key)?;
        if key.key_id() != signer.key_id {
            return Err(malformed(
                "the signer's key id is not the SHA-256 of its public key",
            ));
        }
        Ok(key)
    }
}

/// Read a receipt from `bytes`: a DSSE envelope as JSON, or text (a pull
/// request body) holding a [`PR_BLOCK_START`] block. The payload type,
/// `_type` and `predicateType` must be a receipt's.
pub fn open_receipt(bytes: &[u8]) -> Result<OpenedReceipt, ReceiptError> {
    if bytes.len() > MAX_ENVELOPE_BYTES {
        return Err(malformed(format!(
            "the receipt is larger than {MAX_ENVELOPE_BYTES} bytes"
        )));
    }
    let text = std::str::from_utf8(bytes).map_err(|_| malformed("the receipt is not UTF-8"))?;
    let trimmed = text.trim_start_matches('\u{feff}').trim();
    let envelope_bytes: std::borrow::Cow<'_, [u8]> = if trimmed.starts_with('{') {
        std::borrow::Cow::Borrowed(trimmed.as_bytes())
    } else {
        match extract_pr_block(text) {
            Some(Ok(bytes)) => std::borrow::Cow::Owned(bytes),
            Some(Err(error)) => return Err(error),
            None => {
                return Err(malformed(format!(
                    "neither a DSSE envelope (JSON) nor text holding an `{PR_BLOCK_START}` block"
                )))
            }
        }
    };
    let envelope: Envelope = serde_json::from_slice(&envelope_bytes)
        .map_err(|error| malformed(format!("the envelope is not a DSSE envelope: {error}")))?;
    open_envelope(envelope)
}

/// [`open_receipt`] of a parsed envelope.
pub fn open_envelope(envelope: Envelope) -> Result<OpenedReceipt, ReceiptError> {
    if envelope.payload_type != PAYLOAD_TYPE {
        return Err(malformed(format!(
            "the payload type is {:?}, not {PAYLOAD_TYPE}",
            envelope.payload_type.chars().take(80).collect::<String>()
        )));
    }
    if envelope.signatures.is_empty() {
        return Err(malformed("the envelope has no signature"));
    }
    let payload = decode_base64(&envelope.payload)
        .map_err(|_| malformed("the envelope's payload is not base64"))?;
    let value: serde_json::Value = serde_json::from_slice(&payload)
        .map_err(|error| malformed(format!("the statement is not JSON: {error}")))?;
    let statement_type = value.get("_type").and_then(|value| value.as_str());
    if statement_type != Some(STATEMENT_TYPE) {
        return Err(malformed(format!(
            "the statement's _type is {statement_type:?}, not {STATEMENT_TYPE}"
        )));
    }
    let predicate_type = value.get("predicateType").and_then(|value| value.as_str());
    if predicate_type != Some(PREDICATE_TYPE) {
        return Err(malformed(format!(
            "the statement's predicateType is {predicate_type:?}, not {PREDICATE_TYPE}"
        )));
    }
    let statement: Statement = serde_json::from_value(value)
        .map_err(|error| malformed(format!("the statement is not a receipt: {error}")))?;
    validate_provenance(&statement)?;
    Ok(OpenedReceipt {
        envelope,
        payload,
        statement,
    })
}

/// What the signature check found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SignatureCheck {
    /// A key the verifier trusts signed it.
    Trusted { key_id: String },
    /// The key the statement names signed it, and the verifier does not
    /// trust that key.
    Untrusted { key_id: String },
    /// No signature verifies: the bytes changed, or the signature is not
    /// the named key's.
    Invalid(String),
}

/// Check the envelope's signatures against `trusted` keys, then, when none
/// of them signed it, against the key the statement names.
pub fn check_signature(opened: &OpenedReceipt, trusted: &[PublicKey]) -> SignatureCheck {
    let message = pae(&opened.envelope.payload_type, &opened.payload);
    let signatures: Vec<(String, Vec<u8>)> = opened
        .envelope
        .signatures
        .iter()
        .filter_map(|signature| {
            decode_base64(&signature.sig)
                .ok()
                .map(|bytes| (signature.keyid.clone(), bytes))
        })
        .collect();
    let signer = match opened.signer() {
        Ok(signer) => signer,
        Err(error) => return SignatureCheck::Invalid(error.to_string()),
    };
    for key in trusted {
        if signatures
            .iter()
            .any(|(_, signature)| key.verifies(&message, signature))
        {
            if *key != signer {
                return SignatureCheck::Invalid(format!(
                    "the receipt names key {} as its signer, and key {} signed it",
                    signer.short_id(),
                    key.short_id()
                ));
            }
            return SignatureCheck::Trusted {
                key_id: key.key_id(),
            };
        }
    }
    let key_id = signer.key_id();
    let named = signatures
        .iter()
        .filter(|(keyid, _)| keyid.is_empty() || *keyid == key_id);
    for (_, signature) in named {
        if signer.verifies(&message, signature) {
            return SignatureCheck::Untrusted { key_id };
        }
    }
    SignatureCheck::Invalid(format!(
        "no signature verifies with the key the receipt names ({}): the receipt was changed after it was signed, or another key signed it",
        signer.short_id()
    ))
}

// ---------------------------------------------------------------------------
// Pull request block

/// The receipt block of a pull request body: [`PR_BLOCK_START`], the
/// envelope's JSON as base64 in lines of 76 characters, [`PR_BLOCK_END`].
pub fn pr_block(envelope: &Envelope) -> String {
    let json = serde_json::to_vec(envelope).unwrap_or_default();
    let encoded = base64::engine::general_purpose::STANDARD.encode(json);
    let mut block = String::with_capacity(encoded.len() + encoded.len() / 76 + 64);
    block.push_str(PR_BLOCK_START);
    block.push('\n');
    for chunk in encoded.as_bytes().chunks(76) {
        block.push_str(std::str::from_utf8(chunk).unwrap_or_default());
        block.push('\n');
    }
    block.push_str(PR_BLOCK_END);
    block
}

/// The envelope bytes of the first receipt block in `text`; `None` when
/// there is none.
pub fn extract_pr_block(text: &str) -> Option<Result<Vec<u8>, ReceiptError>> {
    let start = text.find(PR_BLOCK_START)?;
    let rest = &text[start + PR_BLOCK_START.len()..];
    let Some(end) = rest.find(PR_BLOCK_END) else {
        return Some(Err(malformed("the receipt block has no closing -->")));
    };
    let encoded: String = rest[..end]
        .chars()
        .filter(|ch| !ch.is_whitespace())
        .collect();
    Some(decode_base64(&encoded).map_err(|_| malformed("the receipt block is not base64")))
}

// ---------------------------------------------------------------------------
// Files

/// What a path is in a checkout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PathState {
    /// A regular file and the SHA-256 of its content.
    File(String),
    /// A symbolic link and the SHA-256 of its target.
    Link(String),
    /// A submodule and the commit it holds ([`submodule_commit`]).
    Commit(String),
    Absent,
    /// Something a receipt cannot cover, and why.
    Other(String),
}

/// Why a receipt does not cover a nested Git checkout that is not a
/// submodule: no repository capture reads its files.
pub const NESTED_CHECKOUT_NOT_COVERED: &str = "a nested Git checkout (not a submodule), whose \
     files Axocoatl's repository captures do not read, so a receipt does not cover it or \
     anything inside it";

/// The commit the submodule at `path` under the checkout `root` holds, as
/// Axocoatl's repository captures record it and Git compares a submodule:
/// the commit its own checkout has as HEAD, or, when it has none Git can
/// read (it is not checked out, or its HEAD is unborn), the commit the
/// index records for it. `Ok(None)` when the index has no submodule at
/// `path`. Git reads the index and that HEAD; nothing either repository
/// configures runs.
pub fn submodule_commit(root: &Path, path: &str) -> Result<Option<String>, String> {
    if !is_plain_path(path) {
        return Ok(None);
    }
    let listed = read_git(
        root,
        &["ls-files", "-s", "-z", "--", &format!(":(literal){path}")],
        None,
    )?;
    let recorded = listed.split('\0').find_map(|entry| {
        let (head, name) = entry.split_once('\t')?;
        let mut fields = head.split(' ');
        match (fields.next(), fields.next(), fields.next(), fields.next()) {
            (Some("160000"), Some(commit), Some("0"), None) if name == path => {
                Some(commit.to_owned())
            }
            _ => None,
        }
    });
    let Some(recorded) = recorded else {
        return Ok(None);
    };
    let checkout = root.join(path);
    let own = if std::fs::symlink_metadata(checkout.join(".git")).is_ok() {
        // Git does not look above the submodule for a repository.
        std::fs::canonicalize(&checkout).ok().and_then(|checkout| {
            read_git(
                &checkout,
                &["rev-parse", "--verify", "-q", "HEAD"],
                checkout.parent(),
            )
            .ok()
        })
    } else {
        None
    };
    Ok(Some(
        own.map(|commit| commit.trim().to_owned())
            .filter(|commit| is_commit(commit))
            .unwrap_or(recorded),
    ))
}

/// A Git commit id, SHA-1 or SHA-256, in lowercase hex.
pub fn is_commit(value: &str) -> bool {
    (value.len() == 40 || value.len() == 64)
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

/// What `git ARGS` prints in `dir`, with nothing a repository configures
/// run (no file system monitor, no hooks) and no prompt, waiting at most 30
/// seconds. `ceiling` keeps Git from looking above it for a repository.
fn read_git(dir: &Path, args: &[&str], ceiling: Option<&Path>) -> Result<String, String> {
    // Git trusts a repository another user owns only by its absolute path.
    let dir = std::fs::canonicalize(dir).map_err(|error| format!("cannot be read: {error}"))?;
    let mut command = std::process::Command::new("git");
    command
        .arg("-C")
        .arg(&dir)
        .arg("-c")
        .arg(format!("safe.directory={}", dir.display()))
        .args([
            "-c",
            "core.fsmonitor=false",
            "-c",
            "core.hooksPath=/dev/null",
        ])
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_NO_LAZY_FETCH", "1")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null());
    if let Some(ceiling) = ceiling {
        command.env("GIT_CEILING_DIRECTORIES", ceiling);
    }
    let mut child = command
        .spawn()
        .map_err(|error| format!("git could not start: {error}"))?;
    let mut stdout = child.stdout.take().expect("piped");
    let reader = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let _ = stdout.read_to_end(&mut bytes);
        bytes
    });
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if std::time::Instant::now() < deadline => {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err("git did not answer within 30 seconds".into());
            }
            Err(error) => return Err(format!("git could not be waited for: {error}")),
        }
    };
    let bytes = reader.join().unwrap_or_default();
    if !status.success() {
        return Err(format!("git {} failed", args[0]));
    }
    String::from_utf8(bytes).map_err(|_| "git printed a name that is not UTF-8".into())
}

/// A repository-relative path a receipt can name: not empty, not absolute,
/// no empty, `.` or `..` segment, no NUL.
pub fn is_plain_path(path: &str) -> bool {
    !path.is_empty()
        && !path.starts_with('/')
        && !path.contains('\0')
        && path
            .split('/')
            .all(|segment| !segment.is_empty() && segment != "." && segment != "..")
}

/// Read `path` under `root` without following a symbolic link anywhere on
/// the way: a regular file's content digest, a link's target digest (as
/// Axocoatl's repository captures record them, `sha256sum` of the bytes or
/// of `readlink -n`), a submodule's commit, or why none.
pub fn hash_repo_path(root: &Path, path: &str) -> PathState {
    if !is_plain_path(path) {
        return PathState::Other("not a plain repository path".into());
    }
    let segments: Vec<&str> = path.split('/').collect();
    let mut current = root.to_path_buf();
    for segment in &segments[..segments.len() - 1] {
        current.push(segment);
        match std::fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return PathState::Other(format!(
                    "{} is a symbolic link",
                    current.strip_prefix(root).unwrap_or(&current).display()
                ))
            }
            Ok(metadata) if metadata.is_dir() => {}
            Ok(_) => return PathState::Absent,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return PathState::Absent,
            Err(error) => return PathState::Other(format!("cannot be read: {error}")),
        }
    }
    current.push(segments[segments.len() - 1]);
    let metadata = match std::fs::symlink_metadata(&current) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return PathState::Absent,
        Err(error) => return PathState::Other(format!("cannot be read: {error}")),
    };
    if metadata.file_type().is_symlink() {
        return match std::fs::read_link(&current) {
            Ok(target) => PathState::Link(sha256_hex(&os_bytes(target.as_os_str()))),
            Err(error) => PathState::Other(format!("its link cannot be read: {error}")),
        };
    }
    if metadata.is_dir() {
        return match submodule_commit(root, path) {
            Ok(Some(commit)) => PathState::Commit(commit),
            Ok(None) if std::fs::symlink_metadata(current.join(".git")).is_ok() => {
                PathState::Other(NESTED_CHECKOUT_NOT_COVERED.into())
            }
            Ok(None) => PathState::Other("is a directory".into()),
            Err(error) => PathState::Other(format!(
                "is a directory, and whether it is a submodule cannot be read ({error})"
            )),
        };
    }
    if !metadata.is_file() {
        return PathState::Other("is not a regular file".into());
    }
    let mut file = match std::fs::File::open(&current) {
        Ok(file) => file,
        Err(error) => return PathState::Other(format!("cannot be opened: {error}")),
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        // The file opened is the one inspected, not a link put in its place.
        match file.metadata() {
            Ok(opened) if opened.dev() == metadata.dev() && opened.ino() == metadata.ino() => {}
            _ => return PathState::Other("changed while it was read".into()),
        }
    }
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 64 * 1024];
    loop {
        match file.read(&mut buffer) {
            Ok(0) => break,
            Ok(read) => hasher.update(&buffer[..read]),
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(error) => return PathState::Other(format!("cannot be read: {error}")),
        }
    }
    PathState::File(hex(&hasher.finalize()))
}

#[cfg(unix)]
fn os_bytes(text: &std::ffi::OsStr) -> Vec<u8> {
    use std::os::unix::ffi::OsStrExt;
    text.as_bytes().to_vec()
}

#[cfg(not(unix))]
fn os_bytes(text: &std::ffi::OsStr) -> Vec<u8> {
    text.to_string_lossy().as_bytes().to_vec()
}

/// One way a checkout differs from what a receipt covers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileMismatch {
    pub path: String,
    pub problem: String,
}

impl std::fmt::Display for FileMismatch {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}: {}", self.path, self.problem)
    }
}

fn short(digest: &str) -> String {
    digest.chars().take(16).collect()
}

/// Whether `path` in `root` is what `statement` says the run left: each
/// subject a file (or, for one in `files.links`, a link) with its digest, or
/// a submodule holding its commit; each removed path absent.
pub fn check_path(statement: &Statement, root: &Path, path: &str) -> Option<FileMismatch> {
    let mismatch = |problem: String| {
        Some(FileMismatch {
            path: path.to_string(),
            problem,
        })
    };
    let state = hash_repo_path(root, path);
    if let Some(subject) = statement
        .subject
        .iter()
        .find(|subject| subject.name == path)
    {
        if let Some(expected) = subject.git_commit_digest() {
            return match state {
                PathState::Commit(found) if found == expected => None,
                PathState::Commit(found) => mismatch(format!(
                    "the submodule holds another commit (receipt {}…, found {}…)",
                    short(expected),
                    short(&found)
                )),
                PathState::Absent => mismatch("missing".into()),
                PathState::Other(why) => mismatch(why),
                PathState::File(_) | PathState::Link(_) => {
                    mismatch("the receipt covers a submodule; this is not one".into())
                }
            };
        }
        let Some(expected) = subject.sha256_digest() else {
            return mismatch("the receipt gives it no sha256 digest".into());
        };
        let link = statement
            .predicate
            .files
            .links
            .iter()
            .any(|link| link == path);
        return match (state, link) {
            (PathState::File(found), false) | (PathState::Link(found), true) => {
                if found == expected {
                    None
                } else {
                    mismatch(format!(
                        "content differs (receipt sha256:{}…, found sha256:{}…)",
                        short(expected),
                        short(&found)
                    ))
                }
            }
            (PathState::File(_), true) => {
                mismatch("the receipt covers a symbolic link; this is a file".into())
            }
            (PathState::Link(_), false) => {
                mismatch("the receipt covers a file; this is a symbolic link".into())
            }
            (PathState::Commit(_), _) => {
                mismatch("the receipt covers a file; this is a submodule".into())
            }
            (PathState::Absent, _) => mismatch("missing".into()),
            (PathState::Other(why), _) => mismatch(why),
        };
    }
    if statement
        .predicate
        .files
        .removed
        .iter()
        .any(|removed| removed == path)
    {
        return match state {
            PathState::Absent => None,
            _ => mismatch("the run removed it, and it exists".into()),
        };
    }
    mismatch("the receipt does not cover it".into())
}

/// Every subject and removed path of `statement` checked in `root`.
pub fn check_files(statement: &Statement, root: &Path) -> Vec<FileMismatch> {
    statement
        .subject
        .iter()
        .map(|subject| subject.name.as_str())
        .chain(statement.predicate.files.removed.iter().map(String::as_str))
        .filter_map(|path| check_path(statement, root, path))
        .collect()
}

// ---------------------------------------------------------------------------
// Words

fn verdict_words(verdict: RunVerdict) -> &'static str {
    match verdict {
        RunVerdict::Pass => "pass",
        RunVerdict::ChecksFailed => "checks failed",
        RunVerdict::NeedsAttention => "needs attention",
        RunVerdict::Interrupted => "interrupted",
        RunVerdict::Error => "error",
    }
}

fn source_words(source: &str) -> &'static str {
    match source {
        source::JUDGED_CAPTURE => "as the required checks judged them",
        source::AFTER_CAPTURE => "as the last writer left them (no check judged them)",
        source::WORKSPACE_AT_FINISH => {
            "as the host read them when the run finished (not as captured for the checks)"
        }
        _ => "from an unknown source",
    }
}

/// The statement in words, for `axocoatl receipt show`, the run summary
/// and the pull request body.
pub fn describe(statement: &Statement) -> Vec<String> {
    let predicate = &statement.predicate;
    let run = &predicate.run;
    let identity = match (&predicate.loadout, &predicate.native_turn) {
        (Some(loadout), None) => format!(
            "Run {} ({}@{}), verdict {}, exit code {}",
            run.run_id,
            loadout.id,
            loadout.version,
            verdict_words(predicate.verdict),
            predicate.exit_code
        ),
        (None, Some(turn)) => format!(
            "Session turn {} (revision {}, {}), verdict {}, exit code {}",
            turn.turn_id,
            turn.turn_revision,
            turn.state,
            verdict_words(predicate.verdict),
            predicate.exit_code
        ),
        _ => format!("Run {} (unrecognized provenance)", run.run_id),
    };
    let mut lines = vec![
        identity,
        format!(
            "Signed by key {} ({} {})",
            short_key_id(&predicate.signer.key_id),
            predicate.signer.algorithm,
            predicate.signer.key_id
        ),
        match run.finished_at_ms {
            Some(finished) => format!(
                "Axocoatl {}, Session {}, finished at {} ms",
                run.axocoatl_version, run.session_id, finished
            ),
            None => format!(
                "Axocoatl {}, Session {} (closure timestamp not retained)",
                run.axocoatl_version, run.session_id
            ),
        },
    ];
    if let Some(base) = &predicate.repository.base_commit {
        let origin = predicate
            .repository
            .origin_url
            .as_deref()
            .map(|url| format!(" of {url}"))
            .unwrap_or_default();
        lines.push(format!("Base commit {base}{origin}"));
    }
    for agent in &predicate.agents {
        let program = agent
            .program_version
            .as_deref()
            .map(|version| format!(" {version}"))
            .unwrap_or_default();
        lines.push(format!(
            "Agent {} ({}): {}{program}, {}:{}",
            agent.id, agent.role, agent.runtime, agent.provider, agent.model
        ));
    }
    if predicate.checks.is_empty() {
        lines.push("Checks: none required".into());
    }
    for check in &predicate.checks {
        let exit = check
            .exit_code
            .map(|code| format!(", exit {code}"))
            .unwrap_or_default();
        lines.push(format!(
            "Check {}: {}{exit} ({})",
            check.name,
            check.state.replace('_', " "),
            check.argv.join(" ")
        ));
    }
    match &predicate.review {
        Some(review) => lines.push(format!(
            "Review by {}:{}{}: {} ({}), {} of {} rounds, {} findings",
            review.provider,
            review.model,
            review
                .program_version
                .as_deref()
                .map(|version| format!(" ({version})"))
                .unwrap_or_default(),
            review.verdict,
            if review.passed {
                "passed"
            } else {
                "not passed"
            },
            review.rounds,
            review.max_rounds,
            review.findings
        )),
        None => lines.push("Review: none required".into()),
    }
    if !predicate.adjudications.is_empty() {
        let answered: Vec<String> = predicate
            .adjudications
            .iter()
            .map(|item| format!("{} {}", item.id, item.decision))
            .collect();
        lines.push(format!("Adjudications: {}", answered.join(", ")));
    }
    if predicate.not_covered.count > 0 {
        lines.push(format!(
            "Not covered: {} ({})",
            predicate.not_covered.count,
            predicate.not_covered.areas.join(", ")
        ));
    }
    for reason in &predicate.attention {
        lines.push(format!("Needs attention: {reason}"));
    }
    if !predicate.warnings.is_empty() {
        lines.push(format!("Warnings: {}", predicate.warnings.join(", ")));
    }
    let files = &predicate.files;
    lines.push(format!(
        "Files: {} covered, {} removed, {}{}{}",
        statement.subject.len(),
        files.removed.len(),
        source_words(&files.source),
        if files.read_at_finish.is_empty() {
            String::new()
        } else {
            format!(
                "; read when the run finished (as in HEAD when judged): {}",
                files.read_at_finish.join(", ")
            )
        },
        if files.complete {
            String::new()
        } else {
            format!("; incomplete: {}", files.unattributed.join("; "))
        }
    ));
    for subject in &statement.subject {
        if let Some(commit) = subject.git_commit_digest() {
            lines.push(format!("  {}  commit {commit} (submodule)", subject.name));
            continue;
        }
        lines.push(format!(
            "  {}  sha256:{}{}",
            subject.name,
            subject.sha256_digest().unwrap_or("?"),
            if files.links.contains(&subject.name) {
                " (link)"
            } else {
                ""
            }
        ));
    }
    for removed in &files.removed {
        lines.push(format!("  {removed}  removed"));
    }
    if let Some(sandbox) = &predicate.sandbox {
        for policy in &sandbox.policies {
            lines.push(format!(
                "Network policy ({} scope, revision {}): {}",
                policy.scope,
                policy.revision,
                if policy.rules.is_empty() {
                    "nothing allowed".to_string()
                } else {
                    policy.rules.join("; ")
                }
            ));
            for route in &policy.routes {
                let check = match route.provider_fetch.as_deref() {
                    Some("refuse") => "model-call bodies checked, provider fetches refused",
                    Some("allow") => "model-call bodies sent unread (provider_fetch: allow)",
                    Some(_) => "model-call body check unknown",
                    None => "bodies not checked (not a model API)",
                };
                lines.push(format!("  {} {}: {check}", route.id, route.host));
            }
        }
        lines.push(format!(
            "Sandbox: {}, network {}, {} workload",
            sandbox.backend, sandbox.network, sandbox.workload
        ));
        if let Some(limits) = &sandbox.limits {
            lines.push(limits_line(limits));
        }
    }
    if let Some(turn) = &predicate.native_turn {
        lines.extend(
            turn.limitations
                .iter()
                .map(|why| format!("Not established: {why}")),
        );
    }
    lines
}

/// `Resource caps: memory 2g, 2 CPUs, 512 processes; not applied: ...`.
fn limits_line(limits: &crate::SessionResourceLimits) -> String {
    let mut applied = Vec::new();
    if let Some(memory) = &limits.memory {
        applied.push(format!("memory {memory}"));
    }
    if let Some(cpus) = &limits.cpus {
        applied.push(format!("{cpus} CPUs"));
    }
    if let Some(pids) = limits.pids {
        applied.push(format!("{pids} processes"));
    }
    let mut line = format!(
        "Resource caps: {}",
        if applied.is_empty() {
            "none".to_string()
        } else {
            applied.join(", ")
        }
    );
    if !limits.not_applied.is_empty() {
        line.push_str(&format!(
            "; not applied, this host does not delegate their cgroup controllers: {}",
            limits.not_applied.join(", ")
        ));
    }
    line
}

#[cfg(test)]
mod tests {
    use super::*;
    use ring::signature::{Ed25519KeyPair, KeyPair};

    /// RFC 8032 section 7.1, test 1.
    const RFC8032_SEED: &str = "9d61b19deffd5a60ba844af492ec2cc44449c5697b326919703bac031cae7f60";
    const RFC8032_PUBLIC: &str = "d75a980182b10ab7d54bfed3c964073a0ee172f3daa62325af021a68f707511a";
    const RFC8032_SIGNATURE: &str = "e5564300c360ac729086e2cc806e828a84877f1eb8e5d974d873e065224901555fb8821590a33bacc61e39701cf9b46bd25bf5f0595bbe24655141438e7a100b";

    fn unhex(text: &str) -> Vec<u8> {
        (0..text.len())
            .step_by(2)
            .map(|index| u8::from_str_radix(&text[index..index + 2], 16).unwrap())
            .collect()
    }

    fn key(seed: &str) -> Ed25519KeyPair {
        Ed25519KeyPair::from_seed_unchecked(&unhex(seed)).unwrap()
    }

    fn other_key() -> Ed25519KeyPair {
        key("4ccd089b28ff96da9db6c346ec114e0f5b8a319f35aba624da8cf6ed4fb8a6fb")
    }

    pub(crate) fn statement(signer: SignerFacts) -> Statement {
        Statement {
            statement_type: STATEMENT_TYPE.into(),
            subject: vec![
                Subject::sha256("src/lib.rs", "a".repeat(64)),
                Subject::sha256("tests/test_x.py", "b".repeat(64)),
            ],
            predicate_type: PREDICATE_TYPE.into(),
            predicate: Predicate {
                run: RunFacts {
                    run_id: "run-00000000-0000-4000-8000-000000000001".into(),
                    session_id: "ses-00000000-0000-4000-8000-000000000002".into(),
                    started_at_ms: 1_000,
                    finished_at_ms: Some(2_000),
                    axocoatl_version: "1.4.0".into(),
                    task_sha256: sha256_hex(b"fix it"),
                },
                repository: RepositoryFacts {
                    base_commit: Some("c".repeat(40)),
                    dirty_paths: Vec::new(),
                    origin_url: Some("https://github.com/acme/widgets.git".into()),
                },
                loadout: Some(LoadoutFacts {
                    id: "fix".into(),
                    version: 1,
                    kind: "fix".into(),
                    digest: "d".repeat(64),
                    builtin: true,
                }),
                native_turn: None,
                agents: vec![AgentFacts {
                    id: "writer".into(),
                    role: "writer".into(),
                    runtime: "native".into(),
                    program_version: None,
                    provider: "ollama".into(),
                    model: "qwen3-coder:30b".into(),
                    credential: None,
                }],
                sandbox: Some(SandboxFacts {
                    backend: "podman".into(),
                    network: "egress".into(),
                    workload: "hardened".into(),
                    policies: vec![PolicyFacts {
                        scope: "session".into(),
                        revision: 1,
                        digest: "e".repeat(64),
                        rules: vec!["ollama.host.axocoatl.internal:443 (host_ollama to 127.0.0.1:11434: access full, for agent)".into()],
                        routes: Vec::new(),
                    }],
                    environment: None,
                    limits: None,
                }),
                network: Some(NetworkSummary::default()),
                checks: vec![CheckFacts {
                    name: "tests".into(),
                    state: "passed".into(),
                    exit_code: Some(0),
                    timeout_ms: Some(180_000),
                    argv: vec!["python3".into(), "-m".into(), "pytest".into()],
                    candidate_sha256: Some("f".repeat(64)),
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
                    input_tokens: 10,
                    output_tokens: 5,
                    cost_microunits: 0,
                    cost_usd: 0.0,
                    complete: true,
                    cost_known: true,
                    cost_computed: false,
                },
                files: FileFacts {
                    source: source::JUDGED_CAPTURE.into(),
                    tree_sha256: Some("f".repeat(64)),
                    links: Vec::new(),
                    removed: vec!["old.txt".into()],
                    read_at_finish: Vec::new(),
                    complete: true,
                    unattributed: Vec::new(),
                },
                signer,
            },
        }
    }

    fn signed(key_pair: &Ed25519KeyPair) -> (Envelope, Vec<u8>) {
        sign_statement(key_pair, &statement(public_key_of(key_pair).signer())).unwrap()
    }

    fn opened(envelope: &Envelope) -> Result<OpenedReceipt, ReceiptError> {
        open_receipt(&envelope_file_bytes(envelope))
    }

    #[test]
    fn ring_ed25519_is_rfc_8032() {
        let key_pair = key(RFC8032_SEED);
        assert_eq!(hex(key_pair.public_key().as_ref()), RFC8032_PUBLIC);
        assert_eq!(hex(key_pair.sign(b"").as_ref()), RFC8032_SIGNATURE);
        let public = public_key_of(&key_pair);
        assert!(public.verifies(b"", &unhex(RFC8032_SIGNATURE)));
    }

    /// DSSE protocol.md's own example.
    #[test]
    fn pae_is_the_dsse_encoding() {
        assert_eq!(
            pae("http://example.com/HelloWorld", b"hello world"),
            b"DSSEv1 29 http://example.com/HelloWorld 11 hello world".to_vec()
        );
        assert_eq!(pae("", b""), b"DSSEv1 0  0 ".to_vec());
        // Lengths are bytes, not characters.
        assert_eq!(
            pae("é", "ü".as_bytes()),
            "DSSEv1 2 é 2 ü".as_bytes().to_vec()
        );
    }

    /// A fixed key and a fixed statement give fixed bytes: Ed25519 is
    /// deterministic, and the statement serializes in struct order.
    #[test]
    fn a_fixed_key_signs_a_fixed_statement_to_fixed_bytes() {
        let key_pair = key(RFC8032_SEED);
        let public = public_key_of(&key_pair);
        assert_eq!(
            public.key_id(),
            format!("sha256:{}", sha256_hex(&unhex(RFC8032_PUBLIC)))
        );
        let (envelope, payload) = signed(&key_pair);
        let (again, _) = signed(&key_pair);
        assert_eq!(envelope, again);
        assert_eq!(envelope.payload_type, PAYLOAD_TYPE);
        assert_eq!(envelope.signatures.len(), 1);
        assert_eq!(envelope.signatures[0].keyid, public.key_id());
        let text = String::from_utf8(payload.clone()).unwrap();
        assert!(text.starts_with(
            "{\"_type\":\"https://in-toto.io/Statement/v1\",\"subject\":[{\"name\":\"src/lib.rs\",\"digest\":{\"sha256\":\"aaaa"
        ), "{text}");
        assert!(text.contains(
            "\"predicateType\":\"https://axocoatl.ai/receipt/v1\",\"predicate\":{\"run\":{"
        ));
        let signature = base64::engine::general_purpose::STANDARD
            .decode(&envelope.signatures[0].sig)
            .unwrap();
        assert_eq!(
            signature,
            key_pair
                .sign(&pae(PAYLOAD_TYPE, &payload))
                .as_ref()
                .to_vec()
        );
        // The vector: change the statement's shape and this changes.
        assert_eq!(sha256_hex(&payload), FIXED_PAYLOAD_SHA256, "{text}");
        assert_eq!(hex(&signature), FIXED_SIGNATURE);
        let opened = opened(&envelope).unwrap();
        assert_eq!(opened.payload, payload);
        assert_eq!(
            check_signature(&opened, &[public]),
            SignatureCheck::Trusted {
                key_id: public.key_id()
            }
        );
    }

    /// SHA-256 of the statement bytes of [`statement`] signed by the RFC 8032
    /// test key.
    const FIXED_PAYLOAD_SHA256: &str =
        "a624cc13905e140a19c9731b2b78bd4e0ede580d31cbabf6dabc2f99e3217c15";
    /// Its signature over the PAE. An independent Ed25519 gives the same
    /// bytes: OpenSSL 3.6's `openssl pkeyutl -sign -rawin` with the same
    /// seed over the same PAE.
    const FIXED_SIGNATURE: &str = "61b99fe7e00f01ff848c7afb1e1525654a6ffb6848df2793bfff712b79c3323720e0170343091638bb3321b46b108e86ea735837e274c8fc7b9ef2d86da2ad0d";

    /// The resource caps (1.5) are an optional field of the sandbox facts:
    /// absent, the statement is 1.4's; present, they are signed and said,
    /// with the caps the host could not apply.
    #[test]
    fn resource_caps_are_optional_and_said_with_those_not_applied() {
        let key_pair = key(RFC8032_SEED);
        let mut statement = statement(public_key_of(&key_pair).signer());
        let plain = serde_json::to_string(&statement.predicate.sandbox).unwrap();
        assert!(!plain.contains("limits"), "{plain}");
        assert!(!describe(&statement)
            .iter()
            .any(|line| line.starts_with("Resource caps")));
        statement.predicate.sandbox.as_mut().unwrap().limits = Some(crate::SessionResourceLimits {
            memory: Some("2g".into()),
            cpus: None,
            pids: Some(512),
            not_applied: vec!["CPU (2 CPUs, cgroup controller cpu)".into()],
        });
        let text = serde_json::to_string(&statement.predicate.sandbox).unwrap();
        assert!(text.contains(r#""limits":{"memory":"2g","pids":512,"not_applied":["CPU (2 CPUs, cgroup controller cpu)"]}"#), "{text}");
        let read: SandboxFacts = serde_json::from_str(&text).unwrap();
        assert_eq!(Some(read), statement.predicate.sandbox);
        let said = describe(&statement);
        assert!(said.contains(&"Resource caps: memory 2g, 512 processes; not applied, this host does not delegate their cgroup controllers: CPU (2 CPUs, cgroup controller cpu)".to_string()), "{said:?}");
    }

    /// Environment facts (1.5) are an optional field of the sandbox facts:
    /// absent, a statement is byte for byte what 1.4 signed; present, a
    /// reader that knows only 1.4's fields still reads the statement, and
    /// the signature covers them.
    #[test]
    fn environment_facts_are_optional_and_ignored_by_older_readers() {
        #[derive(Deserialize)]
        struct SandboxFacts14 {
            backend: String,
            policies: Vec<PolicyFacts>,
        }
        let key_pair = key(RFC8032_SEED);
        let mut statement = statement(public_key_of(&key_pair).signer());
        let plain = serde_json::to_string(&statement.predicate.sandbox).unwrap();
        assert!(!plain.contains("environment"), "{plain}");
        statement.predicate.sandbox.as_mut().unwrap().environment = Some(EnvironmentFacts {
            toolchain: Some("python".into()),
            reason: "pyproject.toml at the repository root".into(),
            image: "localhost/axocoatl-recipe-python:eb24246cd089".into(),
            image_id: Some("8".repeat(64)),
            recipes: vec!["python".into()],
            check: Some("pytest".into()),
            check_source: Some("detected from the repository".into()),
            setup: Some(SetupFacts {
                command: "pip install -e . pytest".into(),
                source: "detected".into(),
                script_sha256: "9".repeat(64),
                exit_code: Some(0),
                duration_ms: 4200,
                unchanged: false,
                registry_hosts: vec!["pypi.org".into(), "files.pythonhosted.org".into()],
                allowed_connections: 12,
                refused_connections: 0,
                error: None,
            }),
            cache_volume: Some("axo-tc-0123456789abcdef01234567".into()),
            probe: Some(ProbeFacts {
                program: Some("pytest".into()),
                program_found: true,
                baseline_exit: Some(1),
                duration_ms: 900,
                timed_out: false,
            }),
        });
        let text = serde_json::to_string(&statement.predicate.sandbox).unwrap();
        let older: SandboxFacts14 = serde_json::from_str(&text).unwrap();
        assert_eq!(older.backend, "podman");
        assert_eq!(older.policies.len(), 1);
        let (envelope, _) = sign_statement(&key_pair, &statement).unwrap();
        let opened = opened(&envelope).unwrap();
        assert_eq!(
            opened
                .statement
                .predicate
                .sandbox
                .as_ref()
                .unwrap()
                .environment,
            statement.predicate.sandbox.as_ref().unwrap().environment
        );
        assert!(matches!(
            check_signature(&opened, &[public_key_of(&key_pair)]),
            SignatureCheck::Trusted { .. }
        ));
    }

    #[test]
    fn one_changed_byte_of_payload_signature_or_type_fails() {
        let key_pair = key(RFC8032_SEED);
        let public = public_key_of(&key_pair);
        let (envelope, payload) = signed(&key_pair);

        // Payload: one byte of the statement, re-encoded.
        let at = payload
            .windows(6)
            .position(|window| window == b"passed")
            .unwrap();
        let mut changed = payload.clone();
        changed[at] = b'P';
        let mut tampered = envelope.clone();
        tampered.payload = base64::engine::general_purpose::STANDARD.encode(&changed);
        let receipt = opened(&tampered).unwrap();
        assert!(matches!(
            check_signature(&receipt, &[public]),
            SignatureCheck::Invalid(_)
        ));
        assert!(matches!(
            check_signature(&receipt, &[]),
            SignatureCheck::Invalid(_)
        ));

        // Signature: one byte.
        let mut signature = base64::engine::general_purpose::STANDARD
            .decode(&envelope.signatures[0].sig)
            .unwrap();
        signature[10] ^= 1;
        let mut tampered = envelope.clone();
        tampered.signatures[0].sig = base64::engine::general_purpose::STANDARD.encode(&signature);
        assert!(matches!(
            check_signature(&opened(&tampered).unwrap(), &[public]),
            SignatureCheck::Invalid(_)
        ));

        // Payload type: refused as another kind of envelope, and the
        // signature covers it anyway.
        let mut tampered = envelope.clone();
        tampered.payload_type = "application/vnd.in-toto+jsoN".into();
        assert!(matches!(opened(&tampered), Err(ReceiptError::Malformed(_))));
        let receipt = OpenedReceipt {
            envelope: tampered,
            payload: payload.clone(),
            statement: statement(public.signer()),
        };
        assert!(matches!(
            check_signature(&receipt, &[public]),
            SignatureCheck::Invalid(_)
        ));
    }

    #[test]
    fn another_key_is_untrusted_and_a_lying_signer_is_invalid() {
        let key_pair = key(RFC8032_SEED);
        let other = public_key_of(&other_key());
        let (envelope, _) = signed(&key_pair);
        let receipt = opened(&envelope).unwrap();
        let signer = public_key_of(&key_pair);
        assert_eq!(
            check_signature(&receipt, &[other]),
            SignatureCheck::Untrusted {
                key_id: signer.key_id()
            }
        );
        assert_eq!(
            check_signature(&receipt, &[]),
            SignatureCheck::Untrusted {
                key_id: signer.key_id()
            }
        );
        // A statement naming another signer than the key signing it is
        // never signed, and one that names the trusted key while another
        // key signed it does not verify.
        assert!(sign_statement(&key_pair, &statement(other.signer())).is_err());
        let mut forged = statement(other.signer());
        forged.predicate.signer = other.signer();
        let payload = serde_json::to_vec(&forged).unwrap();
        let signature = key_pair.sign(&pae(PAYLOAD_TYPE, &payload));
        let envelope = Envelope {
            payload_type: PAYLOAD_TYPE.into(),
            payload: base64::engine::general_purpose::STANDARD.encode(&payload),
            signatures: vec![EnvelopeSignature {
                keyid: other.key_id(),
                sig: base64::engine::general_purpose::STANDARD.encode(signature.as_ref()),
            }],
        };
        let receipt = opened(&envelope).unwrap();
        assert!(matches!(
            check_signature(&receipt, &[other]),
            SignatureCheck::Invalid(_)
        ));
        // The trusted key signed a statement that names another signer.
        let mut misnamed = statement(other.signer());
        misnamed.predicate.signer = other.signer();
        let payload = serde_json::to_vec(&misnamed).unwrap();
        let receipt = OpenedReceipt {
            envelope: Envelope {
                payload_type: PAYLOAD_TYPE.into(),
                payload: base64::engine::general_purpose::STANDARD.encode(&payload),
                signatures: vec![EnvelopeSignature {
                    keyid: signer.key_id(),
                    sig: base64::engine::general_purpose::STANDARD
                        .encode(key_pair.sign(&pae(PAYLOAD_TYPE, &payload)).as_ref()),
                }],
            },
            payload,
            statement: misnamed,
        };
        assert!(matches!(
            check_signature(&receipt, &[signer]),
            SignatureCheck::Invalid(reason) if reason.contains("names key")
        ));
        // A signer whose key id is not its key's.
        let mut lying = statement(signer.signer());
        lying.predicate.signer.key_id = other.key_id();
        let receipt = OpenedReceipt {
            payload: serde_json::to_vec(&lying).unwrap(),
            statement: lying,
            envelope: envelope.clone(),
        };
        assert!(matches!(
            check_signature(&receipt, &[]),
            SignatureCheck::Invalid(reason) if reason.contains("key id")
        ));
    }

    #[test]
    fn unknown_types_and_garbage_are_refused() {
        let key_pair = key(RFC8032_SEED);
        let (envelope, payload) = signed(&key_pair);
        for (from, to) in [
            (STATEMENT_TYPE, "https://in-toto.io/Statement/v0.1"),
            (PREDICATE_TYPE, "https://example.com/other/v1"),
        ] {
            let text = String::from_utf8(payload.clone())
                .unwrap()
                .replace(from, to);
            let mut changed = envelope.clone();
            changed.payload = base64::engine::general_purpose::STANDARD.encode(text);
            let error = opened(&changed).unwrap_err().to_string();
            assert!(error.contains(to), "{error}");
        }
        for bad in [&b"not json"[..], b"{}", b"{\"payloadType\":1}", b""] {
            assert!(open_receipt(bad).is_err());
        }
        let mut unsigned = envelope.clone();
        unsigned.signatures.clear();
        assert!(opened(&unsigned).is_err());
        let mut unencoded = envelope;
        unencoded.payload = "!!!".into();
        assert!(opened(&unencoded).is_err());
    }

    #[test]
    fn the_pull_request_block_round_trips_through_a_body() {
        let key_pair = key(RFC8032_SEED);
        let (envelope, payload) = signed(&key_pair);
        let block = pr_block(&envelope);
        assert!(block.starts_with("<!-- axocoatl-receipt:v1\n"));
        assert!(block.ends_with("\n-->"));
        assert!(block
            .lines()
            .all(|line| line.len() <= 76 || line.starts_with("<!--")));
        let body = format!("## Run\n\nSome text with --> in it before.\n\n{block}\n\ntrailing\n");
        let receipt = open_receipt(body.as_bytes()).unwrap();
        assert_eq!(receipt.envelope, envelope);
        assert_eq!(receipt.payload, payload);
        assert_eq!(
            receipt.payload_digest(),
            format!("sha256:{}", sha256_hex(&payload))
        );
        assert!(open_receipt(b"no block here").is_err());
        assert!(open_receipt(b"<!-- axocoatl-receipt:v1\nAAAA").is_err());
    }

    #[test]
    fn key_lines_round_trip_and_bad_lines_name_their_number() {
        let public = public_key_of(&key(RFC8032_SEED));
        let line = public.line("axocoatl@host");
        assert_eq!(
            line,
            format!(
                "ed25519 {} axocoatl@host {}",
                public.base64(),
                &public.key_id()[7..23]
            )
        );
        assert_eq!(parse_key_line(&line).unwrap(), Some(public));
        let other = public_key_of(&other_key());
        let file = format!("# trusted\n\n{line}\n  {}  \n", other.line("ci"));
        assert_eq!(parse_trusted_keys(&file).unwrap(), vec![public, other]);
        let error = parse_trusted_keys(&format!("{line}\nssh-rsa AAAA x\n"))
            .unwrap_err()
            .to_string();
        assert!(error.starts_with("line 2:"), "{error}");
        assert!(parse_trusted_keys("ed25519 AAAA\n").is_err());
        assert!(parse_trusted_keys("ed25519\n").is_err());
    }

    #[cfg(unix)]
    #[test]
    fn files_are_hashed_as_captures_hash_them_and_checked() {
        let dir = crate::private_tempdir_test_support::private_tempdir();
        let root = dir.path();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/lib.rs"), b"fn main() {}\n").unwrap();
        std::os::unix::fs::symlink("lib.rs", root.join("src/link")).unwrap();
        std::os::unix::fs::symlink("/etc", root.join("outside")).unwrap();
        assert_eq!(
            hash_repo_path(root, "src/lib.rs"),
            PathState::File(sha256_hex(b"fn main() {}\n"))
        );
        // `readlink -n | sha256sum`.
        assert_eq!(
            hash_repo_path(root, "src/link"),
            PathState::Link(sha256_hex(b"lib.rs"))
        );
        assert_eq!(hash_repo_path(root, "missing"), PathState::Absent);
        assert_eq!(hash_repo_path(root, "src/lib.rs/x"), PathState::Absent);
        assert!(matches!(hash_repo_path(root, "src"), PathState::Other(_)));
        assert!(matches!(
            hash_repo_path(root, "outside/passwd"),
            PathState::Other(why) if why.contains("symbolic link")
        ));
        for bad in ["", "/etc/passwd", "a/../b", "./a", "a//b"] {
            assert!(
                matches!(hash_repo_path(root, bad), PathState::Other(_)),
                "{bad:?}"
            );
        }

        let key_pair = key(RFC8032_SEED);
        let mut covered = statement(public_key_of(&key_pair).signer());
        covered.subject = vec![
            Subject::sha256("src/lib.rs", sha256_hex(b"fn main() {}\n")),
            Subject::sha256("src/link", sha256_hex(b"lib.rs")),
        ];
        covered.predicate.files.links = vec!["src/link".into()];
        covered.predicate.files.removed = vec!["old.txt".into()];
        assert_eq!(check_files(&covered, root), Vec::new());
        std::fs::write(root.join("src/lib.rs"), b"fn main() {}\n\n").unwrap();
        std::fs::write(root.join("old.txt"), b"back").unwrap();
        let problems = check_files(&covered, root);
        assert_eq!(problems.len(), 2, "{problems:?}");
        assert_eq!(problems[0].path, "src/lib.rs");
        assert!(problems[0].problem.starts_with("content differs"));
        assert_eq!(problems[1].path, "old.txt");
        assert_eq!(
            check_path(&covered, root, "README.md").unwrap().problem,
            "the receipt does not cover it"
        );
        covered.predicate.files.links.clear();
        assert!(check_path(&covered, root, "src/link")
            .unwrap()
            .problem
            .contains("symbolic link"));
    }

    /// A submodule is covered by the commit it holds, as a repository
    /// capture records it: the commit its checkout has as HEAD, or, not
    /// checked out, the one the index records. A nested Git checkout that
    /// is not a submodule is named as not covered, and a plain directory is
    /// no subject. A receipt's submodule subject is checked by that commit
    /// and shown with it.
    #[cfg(unix)]
    #[test]
    fn a_submodule_is_covered_by_its_commit() {
        let git = |dir: &Path, args: &[&str]| {
            let output = std::process::Command::new("git")
                .args([
                    "-c",
                    "user.name=t",
                    "-c",
                    "user.email=t@example.invalid",
                    "-c",
                    "protocol.file.allow=always",
                    "-c",
                    "init.defaultBranch=main",
                ])
                .args(args)
                .current_dir(dir)
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .output()
                .unwrap();
            assert!(output.status.success(), "{output:?}");
            String::from_utf8(output.stdout).unwrap().trim().to_owned()
        };
        let upstream = tempfile::tempdir().unwrap();
        git(upstream.path(), &["init", "--quiet"]);
        for content in ["x\n", "y\n"] {
            std::fs::write(upstream.path().join("x"), content).unwrap();
            git(upstream.path(), &["add", "-A"]);
            git(upstream.path(), &["commit", "--quiet", "-m", content]);
        }
        let newest = git(upstream.path(), &["rev-parse", "HEAD"]);
        let older = git(upstream.path(), &["rev-parse", "HEAD~1"]);
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        git(root, &["init", "--quiet"]);
        std::fs::write(root.join("a.txt"), "a\n").unwrap();
        git(root, &["add", "-A"]);
        git(root, &["commit", "--quiet", "-m", "a"]);
        git(
            root,
            &[
                "submodule",
                "add",
                "--quiet",
                upstream.path().to_str().unwrap(),
                "vendor/sub",
            ],
        );
        git(root, &["commit", "--quiet", "-m", "sub"]);
        assert_eq!(
            hash_repo_path(root, "vendor/sub"),
            PathState::Commit(newest.clone())
        );
        git(&root.join("vendor/sub"), &["checkout", "--quiet", &older]);
        assert_eq!(
            hash_repo_path(root, "vendor/sub"),
            PathState::Commit(older.clone())
        );

        let key_pair = key(RFC8032_SEED);
        let mut covered = statement(public_key_of(&key_pair).signer());
        covered.subject = vec![Subject::git_commit("vendor/sub", older.clone())];
        covered.predicate.files.removed.clear();
        assert_eq!(check_files(&covered, root), Vec::new());
        assert!(describe(&covered)
            .iter()
            .any(|line| line == &format!("  vendor/sub  commit {older} (submodule)")));
        git(&root.join("vendor/sub"), &["checkout", "--quiet", &newest]);
        let problems = check_files(&covered, root);
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(
            problems[0]
                .problem
                .starts_with("the submodule holds another commit"),
            "{problems:?}"
        );
        // Not checked out (as in a pull request's checkout without its
        // submodules): the commit the index records.
        git(
            root,
            &["submodule", "deinit", "--quiet", "--force", "vendor/sub"],
        );
        assert_eq!(
            hash_repo_path(root, "vendor/sub"),
            PathState::Commit(newest.clone())
        );
        // A file subject at a submodule, a submodule subject at a file.
        covered.subject = vec![
            Subject::sha256("vendor/sub", "a".repeat(64)),
            Subject::git_commit("a.txt", newest.clone()),
        ];
        let problems = check_files(&covered, root);
        assert_eq!(
            problems
                .iter()
                .map(|problem| problem.problem.as_str())
                .collect::<Vec<_>>(),
            [
                "the receipt covers a file; this is a submodule",
                "the receipt covers a submodule; this is not one"
            ]
        );
        // A nested checkout that is not a submodule, and a plain directory.
        let nested = root.join("tools/nested");
        std::fs::create_dir_all(&nested).unwrap();
        git(&nested, &["init", "--quiet"]);
        assert_eq!(
            hash_repo_path(root, "tools/nested"),
            PathState::Other(NESTED_CHECKOUT_NOT_COVERED.into())
        );
        assert_eq!(
            hash_repo_path(root, "tools"),
            PathState::Other("is a directory".into())
        );
    }

    #[test]
    fn each_routes_body_check_is_in_the_statement_and_in_words() {
        let key_pair = key(RFC8032_SEED);
        let mut statement = statement(public_key_of(&key_pair).signer());
        // Without routes the field is left out, so the fixed vector above
        // is unchanged.
        let bare = String::from_utf8(statement_bytes(&statement).unwrap()).unwrap();
        assert!(bare.contains("access full, for agent)\"]}]}"), "{bare}");
        statement.predicate.sandbox.as_mut().unwrap().policies[0].routes = vec![
            RouteFacts {
                id: "host_ollama".into(),
                host: "ollama.host.axocoatl.internal".into(),
                provider_fetch: Some("refuse".into()),
            },
            RouteFacts {
                id: "route#0".into(),
                host: "api.anthropic.com".into(),
                provider_fetch: Some("allow".into()),
            },
            RouteFacts {
                id: "route#1".into(),
                host: "registry.npmjs.org".into(),
                provider_fetch: None,
            },
        ];
        let text = String::from_utf8(statement_bytes(&statement).unwrap()).unwrap();
        assert!(text.contains(
            "\"routes\":[{\"id\":\"host_ollama\",\"host\":\"ollama.host.axocoatl.internal\",\"provider_fetch\":\"refuse\"},{\"id\":\"route#0\",\"host\":\"api.anthropic.com\",\"provider_fetch\":\"allow\"},{\"id\":\"route#1\",\"host\":\"registry.npmjs.org\"}]"
        ), "{text}");
        let (envelope, _) = sign_statement(&key_pair, &statement).unwrap();
        let opened = opened(&envelope).unwrap();
        assert_eq!(opened.statement, statement);
        let words = describe(&statement).join("\n");
        for needle in [
            "  host_ollama ollama.host.axocoatl.internal: model-call bodies checked, provider fetches refused",
            "  route#0 api.anthropic.com: model-call bodies sent unread (provider_fetch: allow)",
            "  route#1 registry.npmjs.org: bodies not checked (not a model API)",
        ] {
            assert!(words.contains(needle), "{needle}\n{words}");
        }
    }

    #[test]
    fn the_statement_in_words_names_what_a_reviewer_needs() {
        let key_pair = key(RFC8032_SEED);
        let text = describe(&statement(public_key_of(&key_pair).signer())).join("\n");
        for needle in [
            "Run run-00000000-0000-4000-8000-000000000001 (fix@1), verdict pass, exit code 0",
            "Signed by key ",
            "Agent writer (writer): native, ollama:qwen3-coder:30b",
            "Check tests: passed, exit 0 (python3 -m pytest)",
            "Review by ollama:gpt-oss:20b: approved (passed), 1 of 2 rounds, 0 findings",
            "Files: 2 covered, 1 removed, as the required checks judged them",
            "  old.txt  removed",
        ] {
            assert!(text.contains(needle), "{needle}\n{text}");
        }
    }

    fn native_statement(signer: SignerFacts) -> Statement {
        let mut value = statement(signer);
        value.predicate.loadout = None;
        value.predicate.sandbox = None;
        value.predicate.network = None;
        value.predicate.run.finished_at_ms = None;
        value.predicate.native_turn = Some(NativeTurnFacts {
            turn_id: value.predicate.run.run_id.clone(),
            turn_revision: 12,
            graph_revision: 1,
            state: "completed".into(),
            evidence_sha256: "a".repeat(64),
            configuration_revisions: vec![2],
            definition_snapshots: vec!["retained-lead".into()],
            limitations: vec!["Historical sandbox and network facts are not recorded.".into()],
        });
        value
    }

    #[test]
    fn native_turn_receipts_verify_without_fabricated_loadout_or_policy() {
        let pair = key(RFC8032_SEED);
        let public = public_key_of(&pair);
        let value = native_statement(public.signer());
        let (envelope, payload) = sign_statement(&pair, &value).unwrap();
        let opened = open_envelope(envelope).unwrap();
        assert!(matches!(
            check_signature(&opened, &[public]),
            SignatureCheck::Trusted { .. }
        ));
        assert_eq!(opened.statement, value);
        let json: serde_json::Value = serde_json::from_slice(&payload).unwrap();
        assert!(json["predicate"].get("loadout").is_none());
        assert!(json["predicate"].get("sandbox").is_none());
        assert!(json["predicate"].get("network").is_none());
        assert!(json["predicate"]["run"].get("finished_at_ms").is_none());
        let words = describe(&value).join("\n");
        assert!(words.contains("Session turn"));
        assert!(words.contains("closure timestamp not retained"));
        assert!(!words.contains("hardened"));
    }

    #[test]
    fn native_provenance_rejects_ambiguous_stale_and_inconsistent_assertions() {
        let pair = key(RFC8032_SEED);
        let good = native_statement(public_key_of(&pair).signer());
        let mutate: &[fn(&mut Statement)] = &[
            |s| s.predicate.loadout = statement(Default::default()).predicate.loadout,
            |s| s.predicate.native_turn = None,
            |s| s.predicate.native_turn.as_mut().unwrap().turn_id = "another-turn".into(),
            |s| s.predicate.native_turn.as_mut().unwrap().turn_revision = 0,
            |s| s.predicate.native_turn.as_mut().unwrap().graph_revision = 0,
            |s| s.predicate.native_turn.as_mut().unwrap().evidence_sha256 = "not-a-digest".into(),
            |s| s.predicate.native_turn.as_mut().unwrap().state = "running".into(),
            |s| s.predicate.native_turn.as_mut().unwrap().state = "cancelled".into(),
            |s| s.predicate.checks[0].state = "not_run".into(),
            |s| s.predicate.review.as_mut().unwrap().passed = false,
        ];
        for change in mutate {
            let mut invalid = good.clone();
            change(&mut invalid);
            assert!(sign_statement(&pair, &invalid).is_err());
            let (mut envelope, _) = sign_statement(&pair, &good).unwrap();
            envelope.payload = base64::engine::general_purpose::STANDARD
                .encode(statement_bytes(&invalid).unwrap());
            assert!(open_envelope(envelope).is_err());
        }
        let mut cancelled = good;
        cancelled.predicate.native_turn.as_mut().unwrap().state = "cancelled".into();
        cancelled.predicate.verdict = RunVerdict::Interrupted;
        cancelled.predicate.exit_code = 6;
        cancelled.predicate.checks[0].state = "not_run".into();
        cancelled.predicate.review.as_mut().unwrap().passed = false;
        assert!(sign_statement(&pair, &cancelled).is_ok());
    }
}
