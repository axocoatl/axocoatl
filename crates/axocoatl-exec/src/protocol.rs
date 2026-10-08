use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::io::BufRead;

// Version 2 adds an optional length/digest header and exact raw stdin bytes
// before Ready. Version 1 peers cannot mistake payload bytes for controls.
// Version 3 adds an optional kernel write restriction for the launched tree.
// Within version 3 that restriction may also refuse TCP (`deny_network`). The
// field is omitted while false, so a request without it keeps its exact bytes
// and digest, and a supervisor that predates it rejects the unknown field
// instead of launching without it.
pub const PROTOCOL_VERSION: u32 = 3;
/// Placeholder for the supervisor's own `HOME` in a write restriction.
pub const HOME_PLACEHOLDER: &str = "$HOME";
const MAX_RESTRICTION_PATHS: usize = 16;
pub const SUPERVISOR_VERSION: &str = env!("CARGO_PKG_VERSION");
/// Existing EditFile source limit plus its oversize-detection sentinel.
pub const MAX_FILE_CAPTURE_BYTES: usize = 8 * 1024 * 1024 + 1;
/// One complete EditFile source and the existing foreground stderr allowance.
/// Actual requests still select their own bounded captures; checks keep their
/// stricter durable output limits.
pub const MAX_CAPTURE_BYTES: usize = MAX_FILE_CAPTURE_BYTES + 1024 * 1024;
pub const MAX_REQUEST_BYTES: usize = 512 * 1024;
/// Existing WriteFile/EditFile content ceiling. This separate raw payload never
/// enlarges the bounded JSON request/control frames or encodes bytes as argv.
pub const MAX_STDIN_BYTES: usize = 8 * 1024 * 1024;
pub const MAX_RESPONSE_BYTES: usize = MAX_CAPTURE_BYTES * 2 + 16 * 1024;
pub const MAX_CONTROL_BYTES: usize = 128;
pub const MAX_TIMEOUT_MS: u64 = 24 * 60 * 60 * 1000;
pub const CLEANUP_TIMEOUT_MS: u64 = 10_000;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecRequest {
    pub protocol: u32,
    pub invocation_id: String,
    pub argv: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stdin: Option<StdinDescriptor>,
    pub timeout_ms: u64,
    pub stdout_bytes: usize,
    pub stderr_bytes: usize,
    /// Launch the command tree under a kernel write restriction. A supervisor
    /// that cannot apply it refuses to launch rather than run unrestricted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub write_restriction: Option<WriteRestriction>,
}

/// Writes are allowed only beneath `writable`; everything else, including the
/// repository, is read-only. A writable entry equal to, inside, or containing
/// a `protected` path is dropped, so a scratch or home directory can never
/// reopen the protected tree. Reads and execution are unaffected. A
/// read-only helper's command ([`HelperView`]) gets none of `writable`: only
/// its own scratch directory and a few devices.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WriteRestriction {
    pub writable: Vec<String>,
    pub protected: Vec<String>,
    /// Also refuse every TCP connect and bind, on any address and port,
    /// loopback included. A supervisor whose kernel cannot refuse both
    /// refuses to launch rather than run with the network open.
    #[serde(default, skip_serializing_if = "is_false")]
    pub deny_network: bool,
}

fn is_false(value: &bool) -> bool {
    !*value
}

impl WriteRestriction {
    pub fn validate(&self) -> Result<(), String> {
        let path = |path: &String| {
            (path == HOME_PLACEHOLDER || path.starts_with('/'))
                && path.len() <= 4096
                && !path.contains('\0')
                && !path.split('/').any(|part| part == "..")
        };
        if self.writable.len() > MAX_RESTRICTION_PATHS
            || self.protected.is_empty()
            || self.protected.len() > MAX_RESTRICTION_PATHS
            || !self.writable.iter().all(path)
            || !self
                .protected
                .iter()
                .all(|entry| entry != HOME_PLACEHOLDER && path(entry))
        {
            return Err("invalid supervisor write restriction".into());
        }
        Ok(())
    }

    /// The writable roots that remain after dropping any overlap with a
    /// protected path, with `$HOME` resolved by the caller.
    pub fn effective_writable(&self, home: Option<&str>) -> Vec<String> {
        let within = |inner: &str, outer: &str| {
            let outer = outer.trim_end_matches('/');
            inner == outer || inner.starts_with(&format!("{outer}/")) || outer.is_empty()
        };
        self.writable
            .iter()
            .filter_map(|entry| {
                if entry == HOME_PLACEHOLDER {
                    home.map(str::to_owned)
                } else {
                    Some(entry.clone())
                }
            })
            .filter(|entry| entry.starts_with('/'))
            .filter(|entry| {
                !self
                    .protected
                    .iter()
                    .any(|protected| within(entry, protected) || within(protected, entry))
            })
            .collect()
    }
}

/// How `--serve --harden` launches a read-only helper's command in a
/// hardened container (`--helper UID:GID --writer UID:GID --workspace
/// PATH`). The supervisor starts as root and launches the command as the
/// helper user with `CAP_DAC_READ_SEARCH` as its only capability, in a
/// Landlock domain that lets it open, list and execute the Workspace whatever
/// its file modes, and outside it only what any user could read already.
/// Without a write restriction (its file tools) it writes nothing: it may
/// open only `/dev/null` for writing, and its seccomp filter refuses changing
/// a file's mode, owner or times. With one (its shell) it writes only
/// beneath a scratch directory of its own (`HOME`, `TMPDIR`) and to a few
/// devices, whatever the restriction's `writable` roots name. Its seccomp
/// filter also refuses Unix sockets (other than stream socket pairs),
/// `inotify` and `fanotify` watches and extended attribute changes, which
/// the capability would otherwise extend. These are transport arguments,
/// chosen by the host with the container's users, not part of the request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HelperView {
    /// The helper's `(uid, gid)`: the command runs as them.
    pub helper: (u32, u32),
    /// The writer's `(uid, gid)`: nothing the writer owns outside the
    /// Workspace is opened to the helper.
    pub writer: (u32, u32),
    /// The Workspace, readable to the helper whatever its file modes.
    pub workspace: String,
}

impl HelperView {
    pub fn validate(&self) -> Result<(), String> {
        let (helper, writer) = (self.helper, self.writer);
        let ids = [helper.0, helper.1, writer.0, writer.1];
        if ids.contains(&0)
            || [helper.0, helper.1]
                .iter()
                .any(|id| *id == writer.0 || *id == writer.1)
            || !self.workspace.starts_with('/')
            || self.workspace == "/"
            || self.workspace.len() > 4096
            || self.workspace.contains('\0')
            || self.workspace.split('/').any(|part| part == "..")
        {
            return Err("invalid helper view".into());
        }
        Ok(())
    }

    /// The supervisor arguments that follow `--serve --harden`.
    pub fn args(&self) -> Vec<String> {
        vec![
            "--helper".into(),
            format!("{}:{}", self.helper.0, self.helper.1),
            "--writer".into(),
            format!("{}:{}", self.writer.0, self.writer.1),
            "--workspace".into(),
            self.workspace.clone(),
        ]
    }

    /// The view [`Self::args`] wrote, exactly in that order.
    pub fn parse(args: &[String]) -> Result<Self, String> {
        let ids = |value: &str| -> Result<(u32, u32), String> {
            let (uid, gid) = value.split_once(':').ok_or("invalid helper view ids")?;
            let id = |part: &str| {
                if part.is_empty() || !part.bytes().all(|byte| byte.is_ascii_digit()) {
                    return Err("invalid helper view ids".to_string());
                }
                part.parse::<u32>()
                    .map_err(|_| "invalid helper view ids".to_string())
            };
            Ok((id(uid)?, id(gid)?))
        };
        let [helper_flag, helper, writer_flag, writer, workspace_flag, workspace] = args else {
            return Err("invalid helper view arguments".into());
        };
        if helper_flag != "--helper" || writer_flag != "--writer" || workspace_flag != "--workspace"
        {
            return Err("invalid helper view arguments".into());
        }
        let view = Self {
            helper: ids(helper)?,
            writer: ids(writer)?,
            workspace: workspace.clone(),
        };
        view.validate()?;
        Ok(view)
    }
}

impl ExecRequest {
    pub fn validate(&self) -> Result<(), String> {
        if self.protocol != PROTOCOL_VERSION
            || self.invocation_id.is_empty()
            || self.invocation_id.len() > 128
            || self.invocation_id.chars().any(char::is_control)
            || self.argv.is_empty()
            || self.argv.len() > 128
            || self.argv[0].is_empty()
            || self.argv.iter().any(|arg| arg.contains('\0'))
            || self
                .argv
                .iter()
                .try_fold(0usize, |n, arg| n.checked_add(arg.len()))
                .is_none_or(|n| n > 64 * 1024)
            || self
                .stdin
                .as_ref()
                .is_some_and(|input| input.byte_len > MAX_STDIN_BYTES || !is_digest(&input.sha256))
            || self
                .write_restriction
                .as_ref()
                .is_some_and(|restriction| restriction.validate().is_err())
            || self.timeout_ms == 0
            || self.timeout_ms > MAX_TIMEOUT_MS
            || self
                .stdout_bytes
                .checked_add(self.stderr_bytes)
                .is_none_or(|n| n > MAX_CAPTURE_BYTES)
        {
            return Err("invalid supervisor request version, identity or bounds".into());
        }
        Ok(())
    }

    /// Both sides validate the exact payload before issuing/accepting Ready.
    /// None is the existing /dev/null input path; Some(empty) delivers pipe EOF.
    pub fn validate_stdin(&self, bytes: Option<&[u8]>) -> Result<(), String> {
        self.validate()?;
        match (&self.stdin, bytes) {
            (None, None) => Ok(()),
            (Some(expected), Some(bytes))
                if bytes.len() == expected.byte_len && sha256(bytes) == expected.sha256 =>
            {
                Ok(())
            }
            _ => Err("supervisor stdin differs from its exact request length or digest".into()),
        }
    }

    pub fn digest(&self) -> Result<String, String> {
        self.validate()?;
        Ok(sha256(
            &serde_json::to_vec(self).map_err(|error| error.to_string())?,
        ))
    }
}

/// Exact input identity retained in the executable request digest. Body bytes
/// travel raw after the header, before Ready, and are never a control frame.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StdinDescriptor {
    pub byte_len: usize,
    pub sha256: String,
}

impl StdinDescriptor {
    pub fn for_bytes(bytes: &[u8]) -> Result<Self, String> {
        if bytes.len() > MAX_STDIN_BYTES {
            return Err("supervisor stdin exceeds the file-tool input bound".into());
        }
        Ok(Self {
            byte_len: bytes.len(),
            sha256: sha256(bytes),
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Control {
    Dispatch,
    Cancel,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ProcessOutcome {
    Exited { code: i32 },
    Signalled { signal: i32 },
    TimedOut,
    Cancelled,
    LaunchFailed { message: String },
    Failed { message: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum PrimaryExit {
    Exited { code: i32 },
    Signalled { signal: i32 },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapturedOutput {
    pub retained_hex: String,
    pub observed_bytes: u64,
    pub observed_sha256: String,
    pub complete: bool,
}

impl CapturedOutput {
    pub fn retained_bytes(&self, capacity: usize) -> Result<Vec<u8>, String> {
        if capacity > MAX_CAPTURE_BYTES
            || self.retained_hex.len() > capacity.saturating_mul(2)
            || !self.retained_hex.len().is_multiple_of(2)
            || !is_digest(&self.observed_sha256)
        {
            return Err("invalid captured output bounds or digest".into());
        }
        let mut bytes = Vec::with_capacity(self.retained_hex.len() / 2);
        for pair in self.retained_hex.as_bytes().chunks_exact(2) {
            let a = nibble(pair[0]).ok_or("invalid output encoding")?;
            let b = nibble(pair[1]).ok_or("invalid output encoding")?;
            bytes.push(a * 16 + b);
        }
        if self.observed_bytes < bytes.len() as u64
            || (self.observed_bytes == bytes.len() as u64 && sha256(&bytes) != self.observed_sha256)
        {
            return Err("captured bytes conflict with their observed digest or length".into());
        }
        Ok(bytes)
    }
}

/// Bounded storage, while the reader still counts and hashes every byte it sees.
pub struct OutputCapture {
    retained: Vec<u8>,
    capacity: usize,
    observed: u64,
    digest: Sha256,
}

impl OutputCapture {
    pub fn new(capacity: usize) -> Result<Self, String> {
        if capacity > MAX_CAPTURE_BYTES {
            return Err("capture limit exceeded".into());
        }
        Ok(Self {
            retained: Vec::new(),
            capacity,
            observed: 0,
            digest: Sha256::new(),
        })
    }

    pub fn observe(&mut self, bytes: &[u8]) -> Result<(), String> {
        self.observed = self
            .observed
            .checked_add(bytes.len() as u64)
            .ok_or("output byte count overflow")?;
        self.digest.update(bytes);
        let take = self
            .capacity
            .saturating_sub(self.retained.len())
            .min(bytes.len());
        self.retained.extend_from_slice(&bytes[..take]);
        Ok(())
    }

    pub fn finish(self, complete: bool) -> CapturedOutput {
        CapturedOutput {
            retained_hex: hex(&self.retained),
            observed_bytes: self.observed,
            observed_sha256: format!("{:x}", self.digest.finalize()),
            complete,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ServerMessage {
    Ready {
        protocol: u32,
        invocation_id: String,
        request_sha256: String,
        supervisor_version: String,
    },
    Finished {
        protocol: u32,
        invocation_id: String,
        request_sha256: String,
        outcome: ProcessOutcome,
        primary_exit: Option<PrimaryExit>,
        launched: bool,
        stdout: CapturedOutput,
        stderr: CapturedOutput,
        quiescent: bool,
    },
}

impl ServerMessage {
    pub fn validate_for(&self, request: &ExecRequest) -> Result<(), String> {
        let (protocol, invocation_id, request_sha256) = match self {
            Self::Ready {
                protocol,
                invocation_id,
                request_sha256,
                supervisor_version,
            } => {
                if supervisor_version != SUPERVISOR_VERSION {
                    return Err("supervisor version differs".into());
                }
                (protocol, invocation_id, request_sha256)
            }
            Self::Finished {
                protocol,
                invocation_id,
                request_sha256,
                stdout,
                stderr,
                outcome,
                primary_exit,
                launched,
                ..
            } => {
                stdout.retained_bytes(request.stdout_bytes)?;
                stderr.retained_bytes(request.stderr_bytes)?;
                if primary_exit.is_some() && !launched {
                    return Err("process exit without command launch".into());
                }
                match primary_exit {
                    Some(PrimaryExit::Exited { code }) if !(0..=255).contains(code) => {
                        return Err("invalid primary process exit".into())
                    }
                    Some(PrimaryExit::Signalled { signal }) if !(1..=64).contains(signal) => {
                        return Err("invalid primary process signal".into())
                    }
                    _ => (),
                }
                match outcome {
                    ProcessOutcome::Exited { code }
                        if *primary_exit != Some(PrimaryExit::Exited { code: *code }) =>
                    {
                        return Err("primary exit disagrees with command outcome".into())
                    }
                    ProcessOutcome::Signalled { signal }
                        if *primary_exit != Some(PrimaryExit::Signalled { signal: *signal }) =>
                    {
                        return Err("primary signal disagrees with command outcome".into())
                    }
                    ProcessOutcome::LaunchFailed { .. } if *launched => {
                        return Err("launched command cannot be a launch refusal".into())
                    }
                    _ => (),
                }
                match outcome {
                    ProcessOutcome::Exited { code } if !(0..=255).contains(code) => {
                        return Err("invalid process exit code".into())
                    }
                    ProcessOutcome::Signalled { signal } if !(1..=64).contains(signal) => {
                        return Err("invalid process signal".into())
                    }
                    ProcessOutcome::LaunchFailed { message }
                    | ProcessOutcome::Failed { message }
                        if message.len() > 512 =>
                    {
                        return Err("supervision diagnostic exceeds limit".into())
                    }
                    _ => (),
                }
                (protocol, invocation_id, request_sha256)
            }
        };
        if *protocol != PROTOCOL_VERSION
            || invocation_id != &request.invocation_id
            || request_sha256 != &request.digest()?
        {
            return Err("supervisor response belongs to another command".into());
        }
        Ok(())
    }
}

/// Read only one bounded newline-delimited frame; EOF before newline is loss of
/// a complete acknowledgment, not a shorter valid response.
pub fn read_frame(reader: &mut impl BufRead, limit: usize) -> Result<Option<Vec<u8>>, String> {
    let mut bytes = Vec::new();
    loop {
        let available = reader.fill_buf().map_err(|error| error.to_string())?;
        if available.is_empty() {
            return if bytes.is_empty() {
                Ok(None)
            } else {
                Err("incomplete supervisor frame".into())
            };
        }
        let newline = available.iter().position(|byte| *byte == b'\n');
        let take = newline.map_or(available.len(), |at| at + 1);
        if bytes.len().saturating_add(take) > limit {
            return Err("supervisor frame exceeds limit".into());
        }
        bytes.extend_from_slice(&available[..take]);
        reader.consume(take);
        if newline.is_some() {
            bytes.pop();
            return Ok(Some(bytes));
        }
    }
}

pub fn sha256(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

pub fn bounded_message(mut value: String) -> String {
    let mut end = value.len().min(512);
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    value.truncate(end);
    value
}

fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut value = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        value.push(DIGITS[(byte >> 4) as usize] as char);
        value.push(DIGITS[(byte & 15) as usize] as char);
    }
    value
}
fn nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        _ => None,
    }
}
fn is_digest(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| nibble(byte).is_some())
}

#[cfg(test)]
mod helper_view_tests {
    use super::HelperView;

    fn view() -> HelperView {
        HelperView {
            helper: (1001, 1001),
            writer: (1000, 1000),
            workspace: "/work/repo".into(),
        }
    }

    #[test]
    fn a_helper_view_round_trips_through_its_arguments() {
        let args = view().args();
        assert_eq!(
            args,
            [
                "--helper",
                "1001:1001",
                "--writer",
                "1000:1000",
                "--workspace",
                "/work/repo"
            ]
        );
        assert_eq!(HelperView::parse(&args).unwrap(), view());
    }

    #[test]
    fn a_helper_view_refuses_root_shared_ids_and_other_paths() {
        for invalid in [
            HelperView {
                helper: (0, 1001),
                ..view()
            },
            HelperView {
                writer: (1000, 0),
                ..view()
            },
            HelperView {
                helper: (1000, 1001),
                ..view()
            },
            HelperView {
                helper: (1001, 1000),
                ..view()
            },
            HelperView {
                workspace: "relative".into(),
                ..view()
            },
            HelperView {
                workspace: "/".into(),
                ..view()
            },
            HelperView {
                workspace: "/work/../etc".into(),
                ..view()
            },
        ] {
            assert!(invalid.validate().is_err(), "{invalid:?}");
            assert!(HelperView::parse(&invalid.args()).is_err(), "{invalid:?}");
        }
        let mut args = view().args();
        args.swap(0, 2);
        assert!(HelperView::parse(&args).is_err());
        for ids in ["1001", "1001:", ":1001", "+1001:1001", "1001:1001:1", "x:1"] {
            let mut args = view().args();
            args[1] = ids.into();
            assert!(HelperView::parse(&args).is_err(), "{ids}");
        }
        assert!(HelperView::parse(&view().args()[..4]).is_err());
    }
}

#[cfg(test)]
mod write_restriction_tests {
    use super::WriteRestriction;

    #[test]
    fn writable_roots_never_overlap_a_protected_tree() {
        let restriction = WriteRestriction {
            writable: vec![
                "/tmp".into(),
                "/".into(),
                "$HOME".into(),
                "/work/repo/cache".into(),
                "/work".into(),
            ],
            protected: vec!["/work/repo".into()],
            deny_network: false,
        };
        restriction.validate().unwrap();
        assert_eq!(
            restriction.effective_writable(Some("/root")),
            ["/tmp", "/root"]
        );
        assert_eq!(
            restriction.effective_writable(Some("/work/repo/home")),
            ["/tmp"]
        );
        assert_eq!(restriction.effective_writable(None), ["/tmp"]);
        let mut invalid = restriction.clone();
        invalid.protected.clear();
        assert!(invalid.validate().is_err());
        invalid.protected = vec!["$HOME".into()];
        assert!(invalid.validate().is_err());
        invalid.protected = vec!["relative".into()];
        assert!(invalid.validate().is_err());
    }

    /// A restriction that leaves the network open keeps the exact bytes, and
    /// so the request digest, it had before `deny_network` existed. One that
    /// refuses it names the field, which a supervisor predating it rejects
    /// as unknown instead of launching with the network open.
    #[test]
    fn deny_network_is_omitted_while_false_and_never_silently_dropped() {
        let earlier = r#"{"writable":["/tmp"],"protected":["/work/repo"]}"#;
        let parsed: WriteRestriction = serde_json::from_str(earlier).unwrap();
        assert!(!parsed.deny_network);
        assert_eq!(serde_json::to_string(&parsed).unwrap(), earlier);
        let denied = WriteRestriction {
            deny_network: true,
            ..parsed
        };
        let encoded = serde_json::to_string(&denied).unwrap();
        assert_eq!(
            encoded,
            r#"{"writable":["/tmp"],"protected":["/work/repo"],"deny_network":true}"#
        );
        assert_eq!(
            serde_json::from_str::<WriteRestriction>(&encoded).unwrap(),
            denied
        );

        /// The version 3 restriction as it was before `deny_network`.
        #[derive(Debug, serde::Deserialize)]
        #[serde(deny_unknown_fields)]
        #[allow(dead_code)]
        struct EarlierRestriction {
            writable: Vec<String>,
            protected: Vec<String>,
        }
        assert!(serde_json::from_str::<EarlierRestriction>(&encoded).is_err());
        assert!(serde_json::from_str::<EarlierRestriction>(earlier).is_ok());
    }
}
