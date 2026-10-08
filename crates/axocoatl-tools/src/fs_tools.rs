//! Native file + shell tools for directory sessions.
//!
//! Each tool runs its work as a command *inside* the session's OCI container
//! (see `axocoatl_isolation::SessionSandbox`). The container is the security
//! boundary: the session directory is bind-mounted, nothing else is reachable.
//! Paths supplied by the model are passed as positional arguments to `sh`, not
//! interpolated into a script, so they cannot inject shell syntax.
//!
//! As defense-in-depth, the structured file tools (`read_file`, `write_file`,
//! `edit_file`, `list_dir`, `grep`) additionally confine model-supplied paths
//! to the session root via [`confine`], so `../../` and absolute paths can't
//! reach beyond the project even inside the container. The `bash` tool is the
//! explicit escape hatch for anything outside that.

use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use axocoatl_isolation::session_sandbox::{ExecResult, Sandbox};

use crate::builtin::BuiltinTool;
use crate::error::ToolError;
use crate::executor::ToolExecutor;

/// Timeout for quick filesystem operations.
const FS_TIMEOUT: Duration = Duration::from_secs(30);
/// Timeout for shell commands (builds, test runs, …).
const SHELL_TIMEOUT: Duration = Duration::from_secs(180);
/// Longest model-supplied path accepted by a structured file tool.
const PATH_ARG_MAX_BYTES: usize = 4 * 1024;
/// Longest model-supplied grep or glob expression.
const SEARCH_ARG_MAX_BYTES: usize = 16 * 1024;
/// Longest shell command accepted by bash/background/terminal tools.
const COMMAND_ARG_MAX_BYTES: usize = 64 * 1024;
/// Largest complete file body accepted by write/edit.
const FILE_WRITE_MAX_BYTES: usize = 8 * 1024 * 1024;
/// Largest exact edit needle. Large generated files should be replaced with
/// `write_file` rather than duplicated in an edit request.
const EDIT_OLD_MAX_BYTES: usize = 1024 * 1024;
/// Maximum text returned in one structured tool field. This bounds the JSON
/// passed back to the model even when the sandbox command emitted much more.
const TOOL_TEXT_OUTPUT_MAX_BYTES: usize = 64 * 1024;
/// The most bytes of a file one `read_file` call reads: the largest `limit`,
/// and the default window of an Agent whose model context is unknown or has
/// at least four times as many tokens ([`read_file_window`]). A longer file is read to
/// its end across calls, each at the previous call's `next_offset`. The
/// audit's host-checked coverage counts the bytes each succeeded call
/// returned (`offset` and `returned_bytes` in its result).
pub const READ_FILE_WINDOW_BYTES: usize = TOOL_TEXT_OUTPUT_MAX_BYTES;
/// A default window is at most the model's context in tokens divided by
/// this, as bytes: counted at one token per byte, a read without `limit`
/// fills at most a quarter of the context.
pub const READ_FILE_CONTEXT_FRACTION: usize = 4;
/// The smallest default window, whatever the context.
pub const READ_FILE_MIN_WINDOW_BYTES: usize = 512;

/// The bytes a `read_file` without `limit` returns for an Agent whose model
/// has a context of `context_tokens` (0 when unknown): 64 KiB, or a quarter
/// of the context counted at one token per byte when that is smaller (8 KiB
/// for a 32,768-token context), and never under 512 bytes.
pub fn read_file_window(context_tokens: usize) -> usize {
    if context_tokens == 0 {
        return READ_FILE_WINDOW_BYTES;
    }
    (context_tokens / READ_FILE_CONTEXT_FRACTION)
        .clamp(READ_FILE_MIN_WINDOW_BYTES, READ_FILE_WINDOW_BYTES)
}

/// `bytes` as `read_file` states a window: `64 KiB`, or `1500 bytes`.
fn window_words(bytes: usize) -> String {
    if bytes.is_multiple_of(1024) {
        format!("{} KiB", bytes / 1024)
    } else {
        format!("{bytes} bytes")
    }
}
/// `bash` has two independently useful streams; split the overall text budget
/// between them so one result still remains bounded.
const SHELL_STREAM_OUTPUT_MAX_BYTES: usize = TOOL_TEXT_OUTPUT_MAX_BYTES / 2;
/// Tool errors are model-facing too and must not echo an unlimited stderr.
const TOOL_ERROR_MAX_BYTES: usize = 8 * 1024;
/// Terminal identifiers are generated and short; a huge caller value has no
/// useful meaning and should not be reflected into errors/results.
const TERMINAL_ID_MAX_BYTES: usize = 256;
/// Bound terminal inventory JSON independently of the number of stale handles.
const TERMINAL_LIST_MAX_ENTRIES: usize = 128;
const TERMINAL_COMMAND_PREVIEW_MAX_BYTES: usize = 256;
const TERMINAL_TAIL_MAX_LINES: u64 = 10_000;

#[derive(Debug, PartialEq, Eq)]
struct BoundedText {
    text: String,
    truncated: bool,
    original_bytes: usize,
}

fn truncate_utf8(mut text: String, max_bytes: usize) -> BoundedText {
    let original_bytes = text.len();
    if original_bytes <= max_bytes {
        return BoundedText {
            text,
            truncated: false,
            original_bytes,
        };
    }

    let mut end = max_bytes;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text.truncate(end);
    BoundedText {
        text,
        truncated: true,
        original_bytes,
    }
}

fn bounded_reason(reason: impl Into<String>) -> String {
    let bounded = truncate_utf8(reason.into(), TOOL_ERROR_MAX_BYTES);
    if bounded.truncated {
        format!(
            "{}\n[error detail truncated: captured {} bytes; limit {} bytes]",
            bounded.text, bounded.original_bytes, TOOL_ERROR_MAX_BYTES
        )
    } else {
        bounded.text
    }
}

fn exec_err(tool: &str, e: axocoatl_isolation::IsolationError) -> ToolError {
    ToolError::ExecutionFailed {
        tool: tool.to_string(),
        reason: bounded_reason(e.to_string()),
    }
}

/// Map a non-zero exit to a `ToolError`, otherwise return the result.
fn require_ok(tool: &str, r: ExecResult) -> Result<ExecResult, ToolError> {
    if r.ok() {
        Ok(r)
    } else {
        Err(ToolError::ExecutionFailed {
            tool: tool.to_string(),
            reason: if r.stderr.trim().is_empty() {
                format!("exit code {}", r.exit_code)
            } else {
                bounded_reason(r.stderr.trim().to_string())
            },
        })
    }
}

fn str_arg<'a>(args: &'a serde_json::Value, key: &str, tool: &str) -> Result<&'a str, ToolError> {
    args.get(key)
        .and_then(|v| v.as_str())
        .ok_or_else(|| ToolError::InvalidArgs {
            tool: tool.to_string(),
            reason: format!("expected string field '{key}'"),
        })
}

fn bounded_str_arg<'a>(
    args: &'a serde_json::Value,
    key: &str,
    tool: &str,
    max_bytes: usize,
) -> Result<&'a str, ToolError> {
    let value = str_arg(args, key, tool)?;
    validate_str_arg(value, key, tool, max_bytes)?;
    Ok(value)
}

fn optional_bounded_str_arg<'a>(
    args: &'a serde_json::Value,
    key: &str,
    default: &'a str,
    tool: &str,
    max_bytes: usize,
) -> Result<&'a str, ToolError> {
    match args.get(key) {
        None | Some(serde_json::Value::Null) => Ok(default),
        Some(value) => {
            let value = value.as_str().ok_or_else(|| ToolError::InvalidArgs {
                tool: tool.to_string(),
                reason: format!("expected string field '{key}'"),
            })?;
            validate_str_arg(value, key, tool, max_bytes)?;
            Ok(value)
        }
    }
}

fn validate_str_arg(value: &str, key: &str, tool: &str, max_bytes: usize) -> Result<(), ToolError> {
    if value.len() > max_bytes {
        return Err(ToolError::InvalidArgs {
            tool: tool.to_string(),
            reason: format!(
                "field '{key}' is {} bytes; the limit is {max_bytes} bytes. Narrow or split the operation.",
                value.len()
            ),
        });
    }
    if value.contains('\0') {
        return Err(ToolError::InvalidArgs {
            tool: tool.to_string(),
            reason: format!(
                "field '{key}' contains a NUL byte, which command arguments cannot represent"
            ),
        });
    }
    Ok(())
}

fn validate_content_arg(
    value: &str,
    key: &str,
    tool: &str,
    max_bytes: usize,
) -> Result<(), ToolError> {
    if value.len() <= max_bytes {
        return Ok(());
    }
    Err(ToolError::InvalidArgs {
        tool: tool.to_string(),
        reason: format!(
            "field '{key}' is {} bytes; the file-tool limit is {max_bytes} bytes. Use a repository-native generator or split the write.",
            value.len()
        ),
    })
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

fn bounded_text_fields(text: String, limit: usize) -> (String, bool, usize) {
    let bounded = truncate_utf8(text, limit);
    (bounded.text, bounded.truncated, bounded.original_bytes)
}

fn terminal_dimension(
    args: &serde_json::Value,
    key: &str,
    default: u16,
    minimum: u16,
    maximum: u16,
) -> Result<u16, ToolError> {
    let Some(value) = args.get(key).filter(|value| !value.is_null()) else {
        return Ok(default);
    };
    let value = value.as_u64().ok_or_else(|| ToolError::InvalidArgs {
        tool: "spawn_terminal".to_string(),
        reason: format!("field '{key}' must be an integer"),
    })?;
    if value < u64::from(minimum) || value > u64::from(maximum) {
        return Err(ToolError::InvalidArgs {
            tool: "spawn_terminal".to_string(),
            reason: format!("field '{key}' must be between {minimum} and {maximum}"),
        });
    }
    Ok(value as u16)
}

fn optional_tail_lines(args: &serde_json::Value) -> Result<Option<usize>, ToolError> {
    let Some(value) = args.get("tail_lines").filter(|value| !value.is_null()) else {
        return Ok(None);
    };
    let value = value.as_u64().ok_or_else(|| ToolError::InvalidArgs {
        tool: "read_terminal".to_string(),
        reason: "field 'tail_lines' must be a positive integer".to_string(),
    })?;
    if value == 0 || value > TERMINAL_TAIL_MAX_LINES {
        return Err(ToolError::InvalidArgs {
            tool: "read_terminal".to_string(),
            reason: format!("field 'tail_lines' must be between 1 and {TERMINAL_TAIL_MAX_LINES}"),
        });
    }
    Ok(Some(value as usize))
}

fn grep_args<'a>(pattern: &'a str, path: &'a str) -> [&'a str; 6] {
    // The public tool contract promises a regex. Use extended regular
    // expressions so common model-generated patterns such as `foo|bar`
    // behave as advertised instead of silently producing no matches.
    ["grep", "-Ern", "-e", pattern, "--", path]
}

const BOUNDED_STATUS_MARKER: &str = "\n__AXOCOATL_TOOL_EXIT_8F431C2D__:";
const BOUNDED_STDOUT_SCRIPT: &str = r#"limit=$1
shift
{
  "$@"
  axo_status=$?
  printf '\n__AXOCOATL_TOOL_EXIT_8F431C2D__:%s\n' "$axo_status" >&2
} | {
  head -c "$limit"
  cat >/dev/null
}"#;

/// Execute daemon-authored argv while allowing at most `max_bytes + 1` bytes
/// to cross the sandbox stdout transport. The drain after `head` lets the
/// command finish normally instead of changing its behavior with SIGPIPE. A
/// final stderr sentinel preserves the left side's real exit status despite
/// the POSIX pipeline reporting the drain's status.
async fn exec_bounded_stdout(
    sandbox: &dyn Sandbox,
    argv: &[&str],
    timeout: Duration,
    tool: &str,
    max_bytes: usize,
) -> Result<ExecResult, ToolError> {
    let capture_bytes = (max_bytes + 1).to_string();
    let mut owned = vec![
        "sh".to_string(),
        "-c".to_string(),
        BOUNDED_STDOUT_SCRIPT.to_string(),
        "sh".to_string(),
        capture_bytes,
    ];
    owned.extend(argv.iter().map(|value| (*value).to_string()));
    let borrowed: Vec<&str> = owned.iter().map(String::as_str).collect();
    let result = sandbox
        .exec(&borrowed, timeout)
        .await
        .map_err(|error| exec_err(tool, error))?;
    with_reported_status(tool, result)
}

/// `result` with the exit status the command reported after
/// [`BOUNDED_STATUS_MARKER`] on standard error, and that report removed.
fn with_reported_status(tool: &str, mut result: ExecResult) -> Result<ExecResult, ToolError> {
    let marker =
        result
            .stderr
            .rfind(BOUNDED_STATUS_MARKER)
            .ok_or_else(|| ToolError::ExecutionFailed {
                tool: tool.to_string(),
                reason: "bounded command wrapper did not report an exit status".to_string(),
            })?;
    let status_text = result.stderr[marker + BOUNDED_STATUS_MARKER.len()..].trim();
    let exit_code = status_text
        .parse::<i32>()
        .map_err(|_| ToolError::ExecutionFailed {
            tool: tool.to_string(),
            reason: "bounded command wrapper reported an invalid exit status".to_string(),
        })?;
    result.stderr.truncate(marker);
    result.exit_code = exit_code;
    Ok(result)
}

/// Ends `read_file`'s hex dump with the dump's own exit status.
const READ_END_MARKER: &str = "__AXOCOATL_READ_END_5B2E91A7__:";
/// `read_file`'s window, as hexadecimal bytes so the result is exact even
/// when the file is not UTF-8: `$1` the path, `$2` the 1-based byte to start
/// at (empty: the start, read with `head` so nothing is drained), `$3` the
/// most bytes to dump. The reading command's status follows
/// [`BOUNDED_STATUS_MARKER`] on standard error; the dump's follows
/// [`READ_END_MARKER`] on standard output. The drain after `head` lets
/// `tail` finish normally instead of dying of SIGPIPE.
const READ_WINDOW_SCRIPT: &str = r#"{
  if [ -n "$2" ]; then tail -c "+$2" -- "$1"; else head -c "$3" -- "$1"; fi
  printf '\n__AXOCOATL_TOOL_EXIT_8F431C2D__:%s\n' "$?" >&2
} | {
  head -c "$3" | od -An -v -tx1
  printf '__AXOCOATL_READ_END_5B2E91A7__:%s\n' "$?"
  cat >/dev/null
}"#;

/// The bytes of `read_file`'s hex dump ([`READ_WINDOW_SCRIPT`]).
fn decode_window(stdout: &str) -> Result<Vec<u8>, String> {
    let (dump, status) = stdout
        .rsplit_once(READ_END_MARKER)
        .ok_or("the window's byte dump did not finish")?;
    match status.trim() {
        "0" => {}
        "127" => {
            return Err(
                "the sandbox image has no `od`, which read_file needs to read a file".into(),
            )
        }
        other => return Err(format!("the window's byte dump failed (status {other})")),
    }
    let mut bytes = Vec::with_capacity(dump.len() / 3);
    for pair in dump.split_ascii_whitespace() {
        if pair.len() != 2 {
            return Err(format!("unexpected byte dump field '{pair}'"));
        }
        let byte = u8::from_str_radix(pair, 16)
            .map_err(|_| format!("unexpected byte dump field '{pair}'"))?;
        bytes.push(byte);
    }
    Ok(bytes)
}

/// The length of `bytes` without a last character cut short: when the
/// window ends inside a UTF-8 sequence that was valid so far, the window
/// ends before it, so the next read shows that character whole. Never 0
/// for a non-empty window: a window shorter than one character keeps its
/// bytes.
fn complete_utf8_prefix(bytes: &[u8]) -> usize {
    let len = bytes.len();
    let Some(start) = (len.saturating_sub(3)..len)
        .rev()
        .find(|index| bytes[*index] & 0b1100_0000 != 0b1000_0000)
    else {
        return len;
    };
    match std::str::from_utf8(&bytes[start..]) {
        Err(error) if error.valid_up_to() == 0 && error.error_len().is_none() && start > 0 => start,
        _ => len,
    }
}

/// Lexically resolve `.` and `..` segments without touching the filesystem.
fn lexical_normalize(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for comp in p.components() {
        match comp {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Confine a model-supplied path to the session root. Returns the original path
/// (to hand to the in-container command) when it stays inside the session
/// directory, or an `InvalidArgs` error when it would escape.
///
/// Defense-in-depth on top of the container boundary: a confused or adversarial
/// model can otherwise read or write through `../../` or an absolute path
/// (`/etc/passwd`) that resolves inside the container. The structured file
/// tools have no legitimate need to leave the project root; the `bash` tool
/// remains the explicit escape hatch for anything else.
///
/// Resolution is lexical, so it does not follow symlinks — those stay contained
/// by the sandbox's filesystem namespace.
fn confine<'a>(root: &Path, path: &'a str, tool: &str) -> Result<&'a str, ToolError> {
    let candidate = if Path::new(path).is_absolute() {
        PathBuf::from(path)
    } else {
        root.join(path)
    };
    let normalized = lexical_normalize(&candidate);
    let root_norm = lexical_normalize(root);
    if normalized.starts_with(&root_norm) {
        Ok(path)
    } else {
        Err(ToolError::InvalidArgs {
            tool: tool.to_string(),
            reason: format!(
                "path '{path}' escapes the session directory; file tools are \
                 confined to the project root. Use the bash tool for paths \
                 outside it."
            ),
        })
    }
}

/// Register the full session toolset (file ops + shell) into `executor`,
/// each tool bound to `sandbox`, with `read_file`'s default window at its
/// largest ([`READ_FILE_WINDOW_BYTES`]).
pub fn register_session_tools(executor: &mut ToolExecutor, sandbox: Arc<dyn Sandbox>) {
    register_session_tools_with_read_window(executor, sandbox, READ_FILE_WINDOW_BYTES);
}

/// [`register_session_tools`] for an Agent whose `read_file` without `limit`
/// returns `read_window` bytes ([`read_file_window`] of its model's context).
pub fn register_session_tools_with_read_window(
    executor: &mut ToolExecutor,
    sandbox: Arc<dyn Sandbox>,
    read_window: usize,
) {
    executor.register_builtin(
        "read_file",
        Arc::new(ReadFileTool::new(sandbox.clone(), read_window)),
    );
    executor.register_builtin(
        "write_file",
        Arc::new(WriteFileTool {
            sandbox: sandbox.clone(),
        }),
    );
    executor.register_builtin(
        "edit_file",
        Arc::new(EditFileTool {
            sandbox: sandbox.clone(),
        }),
    );
    executor.register_builtin(
        "list_dir",
        Arc::new(ListDirTool {
            sandbox: sandbox.clone(),
        }),
    );
    executor.register_builtin(
        "grep",
        Arc::new(GrepTool {
            sandbox: sandbox.clone(),
        }),
    );
    executor.register_builtin(
        "glob",
        Arc::new(GlobTool {
            sandbox: sandbox.clone(),
        }),
    );
    executor.register_builtin(
        "bash",
        Arc::new(BashTool {
            sandbox: sandbox.clone(),
        }),
    );
    executor.register_builtin(
        "bash_background",
        Arc::new(BashBackgroundTool {
            sandbox: sandbox.clone(),
        }),
    );
    // Visible-to-user terminal tools.  Unlike bash / bash_background, these
    // surface in the dashboard's Terminals pane via the existing PTY
    // bridge — the user can watch live, scroll back, and interact.
    executor.register_builtin(
        "spawn_terminal",
        Arc::new(SpawnTerminalTool {
            sandbox: sandbox.clone(),
        }),
    );
    executor.register_builtin(
        "list_terminals",
        Arc::new(ListTerminalsTool {
            sandbox: sandbox.clone(),
        }),
    );
    executor.register_builtin(
        "read_terminal",
        Arc::new(ReadTerminalTool {
            sandbox: sandbox.clone(),
        }),
    );
    executor.register_builtin("kill_terminal", Arc::new(KillTerminalTool { sandbox }));
}

// ── read_file ───────────────────────────────────────────────────────────

pub struct ReadFileTool {
    sandbox: Arc<dyn Sandbox>,
    /// Bytes a read without `limit` returns, and the most any read returns.
    window: usize,
    description: String,
}

impl ReadFileTool {
    /// A `read_file` whose default window is `window` bytes (1 to
    /// [`READ_FILE_WINDOW_BYTES`]; [`read_file_window`] of the calling
    /// Agent's model context).
    pub fn new(sandbox: Arc<dyn Sandbox>, window: usize) -> Self {
        let window = window.clamp(1, READ_FILE_WINDOW_BYTES);
        let description = format!(
            "Read up to {} of a file in the session directory, from its start or from byte \
             `offset`, or fewer bytes with `limit`. When more of the file follows, the result \
             says `truncated: true` and gives `next_offset`: read the rest of a longer file with \
             further calls at each `next_offset` until `truncated` is false.",
            window_words(window)
        );
        Self {
            sandbox,
            window,
            description,
        }
    }

    /// Bytes a read without `limit` returns.
    pub fn window(&self) -> usize {
        self.window
    }
}

#[async_trait::async_trait]
impl BuiltinTool for ReadFileTool {
    fn description(&self) -> &str {
        &self.description
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "File path to read (maximum 4 KiB)" },
                "offset": { "type": "integer", "minimum": 0, "description": "Byte of the file to start at (default 0); the previous result's next_offset reads on" },
                "limit": { "type": "integer", "minimum": 1, "maximum": self.window, "description": format!("Most bytes to read (default and most {})", self.window) }
            },
            "required": ["path"]
        })
    }
    /// The window from `offset`: at most `limit` bytes (a larger `limit`,
    /// up to 65,536, reads the default window). The bytes travel as a hex
    /// dump, so `returned_bytes` and `next_offset` count the file's own
    /// bytes even when it is not UTF-8; only `content` is decoded, with
    /// U+FFFD for bytes that are not UTF-8 (`invalid_utf8: true`). A window
    /// that would end inside a character ends before it.
    async fn execute(&self, args: serde_json::Value) -> Result<serde_json::Value, ToolError> {
        let path = bounded_str_arg(&args, "path", "read_file", PATH_ARG_MAX_BYTES)?;
        let offset = read_number(&args, "offset")?.unwrap_or(0);
        let limit = match read_number(&args, "limit")? {
            None => self.window,
            Some(limit) if (1..=READ_FILE_WINDOW_BYTES as u64).contains(&limit) => {
                (limit as usize).min(self.window)
            }
            Some(_) => {
                return Err(ToolError::InvalidArgs {
                    tool: "read_file".to_string(),
                    reason: format!(
                        "field 'limit' must be between 1 and {READ_FILE_WINDOW_BYTES} bytes"
                    ),
                })
            }
        };
        let path = confine(self.sandbox.root(), path, "read_file")?;
        // `tail -c +N` starts at byte N, counted from 1; a read from the
        // start uses `head`. One byte past the window says whether more of
        // the file follows.
        let start = if offset == 0 {
            String::new()
        } else {
            offset.saturating_add(1).to_string()
        };
        let capture_bytes = (limit + 1).to_string();
        let r = self
            .sandbox
            .exec(
                &[
                    "sh",
                    "-c",
                    READ_WINDOW_SCRIPT,
                    "sh",
                    path,
                    &start,
                    &capture_bytes,
                ],
                FS_TIMEOUT,
            )
            .await
            .map_err(|e| exec_err("read_file", e))?;
        let r = require_ok("read_file", with_reported_status("read_file", r)?)?;
        let mut bytes = decode_window(&r.stdout).map_err(|reason| ToolError::ExecutionFailed {
            tool: "read_file".to_string(),
            reason,
        })?;
        let captured_bytes = bytes.len();
        let truncated = captured_bytes > limit;
        bytes.truncate(limit);
        if truncated {
            bytes.truncate(complete_utf8_prefix(&bytes));
        }
        let returned_bytes = bytes.len();
        let invalid_utf8 = std::str::from_utf8(&bytes).is_err();
        let content = String::from_utf8_lossy(&bytes).into_owned();
        let mut result = serde_json::json!({
            "content": content,
            "truncated": truncated,
            "returned_bytes": returned_bytes,
            "captured_bytes": captured_bytes,
            "output_limit_bytes": limit,
        });
        if offset > 0 {
            result["offset"] = offset.into();
        }
        if truncated {
            result["next_offset"] = offset.saturating_add(returned_bytes as u64).into();
        }
        if invalid_utf8 {
            result["invalid_utf8"] = true.into();
        }
        Ok(result)
    }
}

/// `read_file`'s optional `offset` or `limit`: a byte count, as an integer
/// or a string of digits; `None` when absent.
fn read_number(args: &serde_json::Value, key: &str) -> Result<Option<u64>, ToolError> {
    let invalid = || ToolError::InvalidArgs {
        tool: "read_file".to_string(),
        reason: format!("field '{key}' must be a whole number of bytes"),
    };
    match args.get(key) {
        None | Some(serde_json::Value::Null) => Ok(None),
        Some(serde_json::Value::Number(number)) => number.as_u64().map(Some).ok_or_else(invalid),
        Some(serde_json::Value::String(text)) => {
            text.trim().parse().map(Some).map_err(|_| invalid())
        }
        Some(_) => Err(invalid()),
    }
}

// ── write_file ──────────────────────────────────────────────────────────

pub struct WriteFileTool {
    sandbox: Arc<dyn Sandbox>,
}

#[async_trait::async_trait]
impl BuiltinTool for WriteFileTool {
    fn concurrency_policy(&self) -> axocoatl_llm::ConcurrencyPolicy {
        axocoatl_llm::ConcurrencyPolicy::Exclusive
    }

    fn description(&self) -> &str {
        "Write (creating or overwriting) a file of up to 8 MiB in the session directory"
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "File path to write (maximum 4 KiB)" },
                "content": { "type": "string", "description": "Full file content (maximum 8 MiB)" }
            },
            "required": ["path", "content"]
        })
    }
    async fn execute(&self, args: serde_json::Value) -> Result<serde_json::Value, ToolError> {
        let path = bounded_str_arg(&args, "path", "write_file", PATH_ARG_MAX_BYTES)?;
        let path = confine(self.sandbox.root(), path, "write_file")?;
        let content = str_arg(&args, "content", "write_file")?;
        validate_content_arg(content, "content", "write_file", FILE_WRITE_MAX_BYTES)?;
        // `sh -c 'cat > "$1"' sh <path>` — path is $1, never interpolated.
        let r = self
            .sandbox
            .exec_stdin(
                &["sh", "-c", "cat > \"$1\"", "sh", path],
                content,
                FS_TIMEOUT,
            )
            .await
            .map_err(|e| exec_err("write_file", e))?;
        require_ok("write_file", r)?;
        Ok(serde_json::json!({ "ok": true, "path": path, "bytes": content.len() }))
    }
}

// ── edit_file ───────────────────────────────────────────────────────────

pub struct EditFileTool {
    sandbox: Arc<dyn Sandbox>,
}

#[async_trait::async_trait]
impl BuiltinTool for EditFileTool {
    fn concurrency_policy(&self) -> axocoatl_llm::ConcurrencyPolicy {
        axocoatl_llm::ConcurrencyPolicy::Exclusive
    }

    fn description(&self) -> &str {
        "Replace an exact substring in a file with new text. The old text must match \
         exactly once unless 'all' is set; source and result files are limited to 8 MiB"
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "File path to edit (maximum 4 KiB)" },
                "old": { "type": "string", "description": "Exact non-empty text to replace (maximum 1 MiB). Must appear exactly once — include surrounding lines to make it unique." },
                "new": { "type": "string", "description": "Replacement text (maximum 8 MiB; the resulting file must also fit)" },
                "all": { "type": "boolean", "description": "Replace every occurrence instead of requiring a unique match. Default false." }
            },
            "required": ["path", "old", "new"]
        })
    }
    async fn execute(&self, args: serde_json::Value) -> Result<serde_json::Value, ToolError> {
        let path = bounded_str_arg(&args, "path", "edit_file", PATH_ARG_MAX_BYTES)?;
        let path = confine(self.sandbox.root(), path, "edit_file")?;
        let old = str_arg(&args, "old", "edit_file")?;
        let new = str_arg(&args, "new", "edit_file")?;
        validate_content_arg(old, "old", "edit_file", EDIT_OLD_MAX_BYTES)?;
        validate_content_arg(new, "new", "edit_file", FILE_WRITE_MAX_BYTES)?;
        if old.is_empty() {
            return Err(ToolError::InvalidArgs {
                tool: "edit_file".to_string(),
                reason: "field 'old' must not be empty".to_string(),
            });
        }

        let capture_bytes = (FILE_WRITE_MAX_BYTES + 1).to_string();
        let read = self
            .sandbox
            .exec(&["head", "-c", &capture_bytes, "--", path], FS_TIMEOUT)
            .await
            .map_err(|e| exec_err("edit_file", e))?;
        let read = require_ok("edit_file", read)?;
        if read.stdout.len() > FILE_WRITE_MAX_BYTES {
            return Err(ToolError::InvalidArgs {
                tool: "edit_file".to_string(),
                reason: format!(
                    "'{path}' exceeds the {FILE_WRITE_MAX_BYTES}-byte edit limit and was not changed. Use a repository-native formatter/generator or replace it deliberately with write_file."
                ),
            });
        }
        if !read.stdout.contains(old) {
            return Err(ToolError::ExecutionFailed {
                tool: "edit_file".to_string(),
                reason: "the 'old' text was not found in the file".to_string(),
            });
        }
        let count = read.stdout.matches(old).count();
        // Replace exactly one occurrence unless the caller explicitly asked for
        // all of them. A silent replace-all is how a model that passes a common
        // fragment (`}`) rewrites every match in the file and corrupts it — the
        // failure is invisible until something downstream refuses to parse.
        // Making ambiguity an error forces the caller to supply unique context.
        let replace_all = args
            .get("all")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);
        if count > 1 && !replace_all {
            return Err(ToolError::InvalidArgs {
                tool: "edit_file".to_string(),
                reason: format!(
                    "the 'old' text appears {count} times in '{path}'; it must match \
                     exactly once. Include surrounding lines to make it unique, or \
                     pass \"all\": true to replace every occurrence."
                ),
            });
        }
        let replacements = if replace_all { count } else { 1 };
        let removed_bytes =
            old.len()
                .checked_mul(replacements)
                .ok_or_else(|| ToolError::InvalidArgs {
                    tool: "edit_file".to_string(),
                    reason: "the requested edit is too large to calculate safely".to_string(),
                })?;
        let added_bytes =
            new.len()
                .checked_mul(replacements)
                .ok_or_else(|| ToolError::InvalidArgs {
                    tool: "edit_file".to_string(),
                    reason: "the requested edit is too large to calculate safely".to_string(),
                })?;
        let updated_bytes = read
            .stdout
            .len()
            .checked_sub(removed_bytes)
            .and_then(|bytes| bytes.checked_add(added_bytes))
            .ok_or_else(|| ToolError::InvalidArgs {
                tool: "edit_file".to_string(),
                reason: "the requested edit is too large to calculate safely".to_string(),
            })?;
        if updated_bytes > FILE_WRITE_MAX_BYTES {
            return Err(ToolError::InvalidArgs {
                tool: "edit_file".to_string(),
                reason: format!(
                    "the edit would produce {updated_bytes} bytes; the file-tool limit is {FILE_WRITE_MAX_BYTES} bytes. Narrow the replacement or use a repository-native generator."
                ),
            });
        }
        let updated = if replace_all {
            read.stdout.replace(old, new)
        } else {
            read.stdout.replacen(old, new, 1)
        };
        let r = self
            .sandbox
            .exec_stdin(
                &["sh", "-c", "cat > \"$1\"", "sh", path],
                &updated,
                FS_TIMEOUT,
            )
            .await
            .map_err(|e| exec_err("edit_file", e))?;
        require_ok("edit_file", r)?;
        Ok(
            serde_json::json!({ "ok": true, "path": path, "replacements": replacements, "bytes": updated_bytes }),
        )
    }
}

// ── list_dir ────────────────────────────────────────────────────────────

pub struct ListDirTool {
    sandbox: Arc<dyn Sandbox>,
}

#[async_trait::async_trait]
impl BuiltinTool for ListDirTool {
    fn description(&self) -> &str {
        "List a directory in the session, returning up to 64 KiB and reporting truncation"
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Directory path; empty or . is the root (the default; maximum 4 KiB)" }
            }
        })
    }
    async fn execute(&self, args: serde_json::Value) -> Result<serde_json::Value, ToolError> {
        let path = optional_bounded_str_arg(&args, "path", ".", "list_dir", PATH_ARG_MAX_BYTES)?;
        // Models often pass an empty path for the root, which `ls` rejects.
        let path = if path.trim().is_empty() { "." } else { path };
        let path = confine(self.sandbox.root(), path, "list_dir")?;
        let r = exec_bounded_stdout(
            self.sandbox.as_ref(),
            &["ls", "-la", "--", path],
            FS_TIMEOUT,
            "list_dir",
            TOOL_TEXT_OUTPUT_MAX_BYTES,
        )
        .await?;
        let r = require_ok("list_dir", r)?;
        let (listing, truncated, captured_bytes) =
            bounded_text_fields(r.stdout, TOOL_TEXT_OUTPUT_MAX_BYTES);
        let returned_bytes = listing.len();
        Ok(serde_json::json!({
            "listing": listing,
            "truncated": truncated,
            "returned_bytes": returned_bytes,
            "captured_bytes": captured_bytes,
            "output_limit_bytes": TOOL_TEXT_OUTPUT_MAX_BYTES,
        }))
    }
}

// ── grep ────────────────────────────────────────────────────────────────

pub struct GrepTool {
    sandbox: Arc<dyn Sandbox>,
}

#[async_trait::async_trait]
impl BuiltinTool for GrepTool {
    fn description(&self) -> &str {
        "Search file contents recursively with line numbers, returning up to 64 KiB of whole matching lines; a cut result says how many matching lines and bytes it left out and how to narrow the search"
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "pattern": { "type": "string", "description": "Text or extended regex to search for (maximum 16 KiB)" },
                "path": { "type": "string", "description": "Directory or file to search (default: ., maximum 4 KiB)" }
            },
            "required": ["pattern"]
        })
    }
    async fn execute(&self, args: serde_json::Value) -> Result<serde_json::Value, ToolError> {
        let pattern = bounded_str_arg(&args, "pattern", "grep", SEARCH_ARG_MAX_BYTES)?;
        let path = optional_bounded_str_arg(&args, "path", ".", "grep", PATH_ARG_MAX_BYTES)?;
        let path = confine(self.sandbox.root(), path, "grep")?;
        let args = grep_args(pattern, path);
        let r = exec_bounded_stdout(
            self.sandbox.as_ref(),
            &args,
            FS_TIMEOUT,
            "grep",
            TOOL_TEXT_OUTPUT_MAX_BYTES,
        )
        .await?;
        // grep exits 1 when there are simply no matches — that is not an error.
        if r.exit_code > 1 {
            return Err(require_ok("grep", r).unwrap_err());
        }
        let (mut matches, truncated, captured_bytes) =
            bounded_text_fields(r.stdout, TOOL_TEXT_OUTPUT_MAX_BYTES);
        if !truncated {
            let returned_bytes = matches.len();
            return Ok(serde_json::json!({
                "matches": matches,
                "truncated": false,
                "returned_bytes": returned_bytes,
                "captured_bytes": captured_bytes,
                "output_limit_bytes": TOOL_TEXT_OUTPUT_MAX_BYTES,
            }));
        }
        // Show whole lines only, then count what the cut left out.
        if let Some(end) = matches.rfind('\n') {
            matches.truncate(end + 1);
        }
        let returned_bytes = matches.len();
        let shown = matches.matches('\n').count();
        let totals = count_output(self.sandbox.as_ref(), &args).await;
        let mut result = serde_json::json!({
            "matches": matches,
            "truncated": true,
            "returned_bytes": returned_bytes,
            "captured_bytes": captured_bytes,
            "output_limit_bytes": TOOL_TEXT_OUTPUT_MAX_BYTES,
            "returned_matches": shown,
        });
        let left_out = match totals {
            Some((lines, bytes)) => {
                let lines = lines.max(shown);
                let bytes = bytes.max(returned_bytes);
                result["total_matches"] = lines.into();
                result["total_bytes"] = bytes.into();
                result["omitted_matches"] = (lines - shown).into();
                result["omitted_bytes"] = (bytes - returned_bytes).into();
                format!(
                    "{shown} of {lines} matching lines are shown; {} lines ({} bytes) are left out",
                    lines - shown,
                    bytes - returned_bytes
                )
            }
            None => format!("{shown} matching lines are shown; the rest could not be counted"),
        };
        result["message"] = format!(
            "The matches were cut at 64 KiB: {left_out}. To see the rest, narrow the search: \
             give a path (a directory or one file) or a more specific pattern."
        )
        .into();
        Ok(result)
    }
}

/// The lines and bytes `argv` prints in all, counted by `wc` in the sandbox
/// without bringing the output back; `None` when they cannot be counted.
async fn count_output(sandbox: &dyn Sandbox, argv: &[&str]) -> Option<(usize, usize)> {
    let mut counted = vec!["sh", "-c", "\"$@\" | wc -lc", "sh"];
    counted.extend_from_slice(argv);
    let result = sandbox.exec(&counted, FS_TIMEOUT).await.ok()?;
    if result.exit_code != 0 {
        return None;
    }
    let mut numbers = result
        .stdout
        .split_whitespace()
        .map(|word| word.parse::<usize>().ok());
    Some((numbers.next()??, numbers.next()??))
}

// ── glob ────────────────────────────────────────────────────────────────

/// Longest glob pattern. Real path patterns are short; the bound keeps the
/// host-side match (pattern length times path length) cheap for every path.
const GLOB_PATTERN_MAX_BYTES: usize = 1024;
/// Most candidate-path bytes the sandbox lists before the host filters them.
/// Well inside the repository port's stream bound.
const GLOB_CANDIDATE_MAX_BYTES: usize = 512 * 1024;
/// Directories a glob does not descend into unless its pattern names one of
/// them: version-control internals, dependency trees and build output. The
/// audit's host listing of a repository that is not a Git work tree skips
/// the same directories.
pub const GLOB_SKIPPED_DIRECTORIES: &[&str] = &[
    ".git",
    ".hg",
    ".svn",
    "node_modules",
    "target",
    ".venv",
    "venv",
    "__pycache__",
    ".tox",
    ".mypy_cache",
    ".pytest_cache",
    ".next",
    ".gradle",
];

/// How one glob runs: the normalized pattern the host matches, the fixed
/// `find` listing that supplies candidates, and the directories it skips.
#[derive(Debug, PartialEq, Eq)]
struct GlobPlan {
    pattern: String,
    /// `./*.js` or `/project/*.js`: a single name the caller anchored at the
    /// root, which the matcher alone would look for at any depth.
    root_only: bool,
    argv: Vec<String>,
    skipped: Vec<&'static str>,
}

fn glob_invalid(reason: impl Into<String>) -> ToolError {
    ToolError::InvalidArgs {
        tool: "glob".to_string(),
        reason: reason.into(),
    }
}

/// Normalize a model's pattern to a root-relative one and plan the listing.
/// The pattern is matched on the host with the same gitignore-flavoured rules
/// as write scopes (`axocoatl_session::path_scope::pattern_matches`); `find`
/// only lists files, starting below the pattern's literal directory prefix and
/// filtered by its final name where `find -name` means the same thing. Every
/// model-derived value is one argv element, never shell text.
fn glob_plan(pattern: &str, root: &Path) -> Result<GlobPlan, ToolError> {
    let mut pattern = pattern.trim();
    let root_text = root.to_string_lossy();
    let root_text = root_text.trim_end_matches('/');
    if pattern.starts_with('/') {
        match pattern.strip_prefix(root_text) {
            Some(rest) if !root_text.is_empty() && (rest.is_empty() || rest.starts_with('/')) => {
                pattern = rest
            }
            _ => {
                return Err(glob_invalid(format!(
                    "pattern '{pattern}' is outside the project; glob patterns are relative to \
                     the project root, e.g. 'src/**/*.rs'"
                )))
            }
        }
    }
    let explicit_root = pattern.starts_with("./") || pattern.starts_with('/');
    let directory = pattern.ends_with('/');
    let segments: Vec<&str> = pattern
        .split('/')
        .filter(|segment| !segment.is_empty() && *segment != ".")
        .collect();
    if segments.is_empty() {
        return Err(glob_invalid(
            "pattern names no files; use '**' for every file, '*.rs' for a file name at any \
             depth, or 'src/**/*.rs' for a path",
        ));
    }
    if segments.contains(&"..") {
        return Err(glob_invalid(format!(
            "pattern '{pattern}' escapes the project root; glob patterns stay inside it"
        )));
    }
    let mut pattern = segments.join("/");
    if directory {
        pattern.push('/');
    }

    // What the pattern means segment by segment: a directory pattern names
    // everything under it, and a pattern without `/` is a name at any depth.
    let mut effective: Vec<&str> = segments.clone();
    if directory {
        effective.push("**");
    }
    let anchored = directory || segments.len() > 1;
    let root_only = explicit_root && !anchored;
    let wildcard = |segment: &str| segment.contains(['*', '?']);
    let prefix: Vec<&str> = if anchored {
        effective[..effective.len() - 1]
            .iter()
            .take_while(|segment| !wildcard(segment))
            .copied()
            .collect()
    } else {
        Vec::new()
    };
    let start = if prefix.is_empty() {
        ".".to_string()
    } else {
        format!("./{}", prefix.join("/"))
    };
    // `find -name` also knows `[...]` classes and `\` escapes, which this
    // matcher treats literally, so it only pre-filters names without them.
    let name = effective
        .last()
        .copied()
        .filter(|name| !name.chars().all(|c| c == '*'))
        .filter(|name| !name.contains(['[', ']', '\\']));
    let skipped: Vec<&'static str> = GLOB_SKIPPED_DIRECTORIES
        .iter()
        .copied()
        .filter(|skipped| !effective.contains(skipped))
        .collect();

    let mut argv = vec!["find".to_string(), start];
    if !skipped.is_empty() {
        argv.extend(["-type", "d", "("].map(String::from));
        for (index, directory) in skipped.iter().enumerate() {
            if index > 0 {
                argv.push("-o".to_string());
            }
            argv.push("-name".to_string());
            argv.push((*directory).to_string());
        }
        argv.extend([")", "-prune", "-o"].map(String::from));
    }
    argv.extend(["-type", "f"].map(String::from));
    if let Some(name) = name {
        argv.push("-name".to_string());
        argv.push(name.to_string());
    }
    argv.push("-print".to_string());
    Ok(GlobPlan {
        pattern,
        root_only,
        argv,
        skipped,
    })
}

/// Keep the listed paths that match, sorted and without `./`, within the
/// output cap. Returns the kept paths and the bytes all matches would need.
fn glob_matches(listing: &str, plan: &GlobPlan) -> (Vec<String>, usize) {
    let pattern = plan.pattern.as_str();
    let mut matches: Vec<&str> = listing
        .lines()
        .map(|line| line.strip_prefix("./").unwrap_or(line))
        .filter(|path| !path.is_empty() && *path != ".")
        .filter(|path| !plan.root_only || !path.contains('/'))
        .filter(|path| axocoatl_session::path_scope::pattern_matches(pattern, path))
        .collect();
    matches.sort_unstable();
    matches.dedup();
    let needed = matches.iter().map(|path| path.len() + 1).sum();
    let mut used = 0;
    let kept = matches
        .into_iter()
        .take_while(|path| {
            used += path.len() + 1;
            used <= TOOL_TEXT_OUTPUT_MAX_BYTES
        })
        .map(str::to_string)
        .collect();
    (kept, needed)
}

pub struct GlobTool {
    sandbox: Arc<dyn Sandbox>,
}

#[async_trait::async_trait]
impl BuiltinTool for GlobTool {
    fn description(&self) -> &str {
        "Find files by path pattern relative to the project root. `*` and `?` match within one \
         path segment and `**` spans directories. A pattern without `/` (e.g. `*.rs`) matches \
         file names at any depth; a pattern with `/` (e.g. `src/**/*.rs`, `lib/*.js`) matches \
         the whole path from the root. Skips .git, node_modules, target and similar \
         directories unless the pattern names them. Returns sorted paths within a 64 KiB cap."
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "pattern": { "type": "string", "description": "Path pattern, e.g. '*.rs', '**/*.test.js' or 'lib/*.js' (maximum 1 KiB)" }
            },
            "required": ["pattern"]
        })
    }
    async fn execute(&self, args: serde_json::Value) -> Result<serde_json::Value, ToolError> {
        let pattern = bounded_str_arg(&args, "pattern", "glob", GLOB_PATTERN_MAX_BYTES)?;
        let plan = glob_plan(pattern, self.sandbox.root())?;
        let argv: Vec<&str> = plan.argv.iter().map(String::as_str).collect();
        let r = exec_bounded_stdout(
            self.sandbox.as_ref(),
            &argv,
            FS_TIMEOUT,
            "glob",
            GLOB_CANDIDATE_MAX_BYTES,
        )
        .await?;
        // `find` also fails when the pattern's directory does not exist, or
        // when a subdirectory is unreadable after listing the rest.
        let missing_start = r.exit_code != 0
            && r.stdout.is_empty()
            && plan.argv[1] != "."
            && r.stderr.contains("No such file or directory");
        if r.exit_code != 0 && r.stdout.is_empty() && !missing_start {
            return Err(require_ok("glob", r).unwrap_err());
        }
        let listing = truncate_utf8(r.stdout, GLOB_CANDIDATE_MAX_BYTES);
        let mut candidates = listing.text;
        if listing.truncated {
            // A byte cap may end inside a path. Never invent a partial match.
            match candidates.rfind('\n') {
                Some(end) => candidates.truncate(end + 1),
                None => candidates.clear(),
            }
        }
        let (files, needed) = glob_matches(&candidates, &plan);
        let count = files.len();
        let returned_bytes: usize = files.iter().map(|path| path.len() + 1).sum();
        let output_truncated = returned_bytes < needed;
        let mut notes = Vec::new();
        if count == 0 {
            notes.push(format!(
                "no files match '{}'. A pattern without '/' matches file names at any depth; \
                 one with '/' is matched from the project root, and '**/' spans directories.",
                plan.pattern
            ));
            if !plan.skipped.is_empty() {
                notes.push(format!(
                    "Skipped directories unless named in the pattern: {}.",
                    plan.skipped.join(", ")
                ));
            }
        }
        if listing.truncated {
            notes.push(format!(
                "The project listing passed {GLOB_CANDIDATE_MAX_BYTES} bytes, so some files \
                 were not checked; start the pattern with a directory to narrow it."
            ));
        }
        if output_truncated {
            notes.push(format!(
                "Only the first {count} matches fit the {TOOL_TEXT_OUTPUT_MAX_BYTES}-byte result."
            ));
        }
        if r.exit_code != 0 && !missing_start {
            notes.push("Some directories could not be read.".to_string());
        }
        let mut output = serde_json::json!({
            "files": files,
            "count": count,
            "truncated": listing.truncated || output_truncated,
            "captured_bytes": needed,
            "output_limit_bytes": TOOL_TEXT_OUTPUT_MAX_BYTES,
        });
        if !notes.is_empty() {
            output["message"] = serde_json::Value::String(notes.join(" "));
        }
        Ok(output)
    }
}

// ── bash ────────────────────────────────────────────────────────────────

pub struct BashTool {
    sandbox: Arc<dyn Sandbox>,
}

#[async_trait::async_trait]
impl BuiltinTool for BashTool {
    fn concurrency_policy(&self) -> axocoatl_llm::ConcurrencyPolicy {
        axocoatl_llm::ConcurrencyPolicy::Exclusive
    }

    fn description(&self) -> &str {
        "Run a shell command inside the session sandbox. Stdout and stderr each return up to 32 KiB and report truncation."
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "command": { "type": "string", "description": "Shell command to run (maximum 64 KiB)" }
            },
            "required": ["command"]
        })
    }
    async fn execute(&self, args: serde_json::Value) -> Result<serde_json::Value, ToolError> {
        let command = bounded_str_arg(&args, "command", "bash", COMMAND_ARG_MAX_BYTES)?;
        // Run at the sandbox root, not the container's default cwd — these
        // differ for an attached (variant) sandbox, where the root is the
        // worktree. A no-op for the primary session (root == default cwd).
        let root = self.sandbox.root().to_string_lossy();
        // Both values are positional parameters. In particular, a Workspace
        // whose name contains a quote cannot alter the wrapper shell program.
        let r = self
            .sandbox
            .exec(
                &[
                    "sh",
                    "-c",
                    "cd \"$1\" && exec sh -c \"$2\" sh",
                    "sh",
                    root.as_ref(),
                    command,
                ],
                SHELL_TIMEOUT,
            )
            .await
            .map_err(|e| exec_err("bash", e))?;
        let (stdout, stdout_truncated, stdout_captured_bytes) =
            bounded_text_fields(r.stdout, SHELL_STREAM_OUTPUT_MAX_BYTES);
        let (stderr, stderr_truncated, stderr_captured_bytes) =
            bounded_text_fields(r.stderr, SHELL_STREAM_OUTPUT_MAX_BYTES);
        Ok(serde_json::json!({
            "stdout": stdout,
            "stderr": stderr,
            "exit_code": r.exit_code,
            "stdout_truncated": stdout_truncated,
            "stderr_truncated": stderr_truncated,
            "stdout_captured_bytes": stdout_captured_bytes,
            "stderr_captured_bytes": stderr_captured_bytes,
            "stream_output_limit_bytes": SHELL_STREAM_OUTPUT_MAX_BYTES,
        }))
    }
}

// ── bash_background ─────────────────────────────────────────────────────

/// `bash_background` already runs its command in the background (see
/// `SessionSandbox::spawn_background`), so a trailing `&` double-backgrounds it:
/// the wrapper shell forks the process, then exits and SIGHUPs it. For a dev
/// server that means it dies on startup and leaves its port stuck (`Errno 98`
/// on the next bind). Models reach for `&` by reflex, so strip a single trailing
/// `&`. `&&` (logical-and) and a mid-command `&` are left untouched. Returns the
/// cleaned command and whether anything was stripped.
fn strip_trailing_ampersand(command: &str) -> (String, bool) {
    let trimmed = command.trim_end();
    if trimmed.ends_with('&') && !trimmed.ends_with("&&") {
        (trimmed[..trimmed.len() - 1].trim_end().to_string(), true)
    } else {
        (trimmed.to_string(), false)
    }
}

pub struct BashBackgroundTool {
    sandbox: Arc<dyn Sandbox>,
}

#[async_trait::async_trait]
impl BuiltinTool for BashBackgroundTool {
    fn concurrency_policy(&self) -> axocoatl_llm::ConcurrencyPolicy {
        axocoatl_llm::ConcurrencyPolicy::Exclusive
    }

    fn description(&self) -> &str {
        "Start a long-running command in the background inside the session \
         container (a dev server, a build/test watch). Returns a task id \
         immediately — the command keeps running; check it in Background tasks."
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "command": { "type": "string", "description": "Shell command to run in the background (maximum 64 KiB)" }
            },
            "required": ["command"]
        })
    }
    async fn execute(&self, args: serde_json::Value) -> Result<serde_json::Value, ToolError> {
        let raw = bounded_str_arg(&args, "command", "bash_background", COMMAND_ARG_MAX_BYTES)?;
        let (command, stripped) = strip_trailing_ampersand(raw);
        if command.is_empty() {
            return Err(ToolError::InvalidArgs {
                tool: "bash_background".to_string(),
                reason: "field 'command' must contain a command".to_string(),
            });
        }
        // Root at the sandbox dir (the worktree, for a variant sandbox).
        let root = self.sandbox.root().to_string_lossy();
        let scoped = format!("cd {} && {command}", shell_quote(root.as_ref()));
        let task_id = self.sandbox.spawn_background(&scoped);
        let mut out = serde_json::json!({ "task_id": task_id, "started": true });
        if stripped {
            out["note"] = serde_json::Value::String(
                "Dropped a trailing '&' — bash_background already backgrounds the \
                 command and keeps it alive."
                    .to_string(),
            );
        }
        Ok(out)
    }
}

// ── spawn_terminal ──────────────────────────────────────────────────────
//
// Unlike `bash_background`, this opens a PTY-backed terminal that surfaces
// in the dashboard's Terminals pane.  The user can watch it live, scroll
// back through its scrollback buffer, type into it, and kill it from the
// UI.  Use for anything the human should observe: long-running scripts,
// dev servers, demos, watch loops.

pub struct SpawnTerminalTool {
    sandbox: Arc<dyn Sandbox>,
}

#[async_trait::async_trait]
impl BuiltinTool for SpawnTerminalTool {
    fn concurrency_policy(&self) -> axocoatl_llm::ConcurrencyPolicy {
        axocoatl_llm::ConcurrencyPolicy::Exclusive
    }

    fn description(&self) -> &str {
        "Open a new terminal in the user's Terminals pane and run a command \
         in it.  Use when the user should be able to watch live output \
         (scripts, dev servers, demos).\n\n\
         CONTRACT: when this returns successfully with a `terminal_id`, the \
         terminal is ALREADY ALIVE in the user's pane and the command is \
         running.  There is nothing else to do to make it visible — the \
         user can already see it.\n\n\
         Do NOT call spawn_terminal a second time for the same purpose. \
         If you need to confirm what's running, call `list_terminals` \
         instead.  If you need to see output from a terminal you spawned, \
         call `read_terminal` with the id you already received.  Calling \
         spawn_terminal again will start a SECOND independent process — \
         which is almost never what you want."
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "command": { "type": "string", "description": "Shell command to run in the new terminal (maximum 64 KiB)" },
                "rows":    { "type": "integer", "description": "Terminal rows (default 24)", "minimum": 4, "maximum": 500 },
                "cols":    { "type": "integer", "description": "Terminal cols (default 80)", "minimum": 20, "maximum": 1000 }
            },
            "required": ["command"]
        })
    }
    async fn execute(&self, args: serde_json::Value) -> Result<serde_json::Value, ToolError> {
        let command = bounded_str_arg(&args, "command", "spawn_terminal", COMMAND_ARG_MAX_BYTES)?;
        let rows = terminal_dimension(&args, "rows", 24, 4, 500)?;
        let cols = terminal_dimension(&args, "cols", 80, 20, 1000)?;
        let pty = self.sandbox.spawn_pty(command, rows, cols).map_err(|e| {
            ToolError::ExecutionFailed {
                tool: "spawn_terminal".into(),
                reason: bounded_reason(e),
            }
        })?;
        let command_preview =
            truncate_utf8(pty.command.clone(), TERMINAL_COMMAND_PREVIEW_MAX_BYTES);
        Ok(serde_json::json!({
            "terminal_id": pty.id,
            "command": command_preview.text,
            "command_truncated": command_preview.truncated,
            "rows": rows,
            "cols": cols,
        }))
    }
}

// ── list_terminals ──────────────────────────────────────────────────────
//
// Without this the agent can't see what it already spawned, leading to a
// re-spawn loop.  Returns every terminal currently in the session's
// pane — id, command, alive flag — so the agent can verify state before
// acting.

pub struct ListTerminalsTool {
    sandbox: Arc<dyn Sandbox>,
}

#[async_trait::async_trait]
impl BuiltinTool for ListTerminalsTool {
    fn description(&self) -> &str {
        "List every terminal currently open in the user's Terminals pane.  \
         Returns an array of objects with `terminal_id`, `command`, and \
         `alive` (up to 128 entries, with explicit truncation metadata).\n\n\
         Use this BEFORE calling spawn_terminal if you're not sure whether \
         a terminal for the same command already exists.  Also use this to \
         recover terminal ids after a turn break (the ids you got from \
         spawn_terminal earlier are still valid as long as the entry \
         appears in this list)."
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {}
        })
    }
    async fn execute(&self, _args: serde_json::Value) -> Result<serde_json::Value, ToolError> {
        let all = self.sandbox.list_terminals();
        let total_count = all.len();
        let entries: Vec<_> = all
            .into_iter()
            .take(TERMINAL_LIST_MAX_ENTRIES)
            .map(|(id, command, alive)| {
                let command = truncate_utf8(command, TERMINAL_COMMAND_PREVIEW_MAX_BYTES);
                serde_json::json!({
                    "terminal_id": id,
                    "command": command.text,
                    "command_truncated": command.truncated,
                    "alive": alive,
                })
            })
            .collect();
        let count = entries.len();
        Ok(serde_json::json!({
            "terminals": entries,
            "count": count,
            "total_count": total_count,
            "truncated": total_count > count,
            "entry_limit": TERMINAL_LIST_MAX_ENTRIES,
        }))
    }
}

// ── read_terminal ───────────────────────────────────────────────────────
//
// Returns the current scrollback (up to 64 KiB) so the agent can check on
// what its spawned terminals have done since it last looked.

pub struct ReadTerminalTool {
    sandbox: Arc<dyn Sandbox>,
}

#[async_trait::async_trait]
impl BuiltinTool for ReadTerminalTool {
    fn description(&self) -> &str {
        "Read the recent output of a terminal previously created with \
         spawn_terminal.  Returns the current scrollback buffer (up to \
         ~64 KiB) plus whether the terminal is still alive."
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "terminal_id": { "type": "string", "description": "ID returned by spawn_terminal" },
                "tail_lines":  { "type": "integer", "description": "If set, return only the last N lines (1-10000). Default: full bounded buffer.", "minimum": 1, "maximum": 10000 }
            },
            "required": ["terminal_id"]
        })
    }
    async fn execute(&self, args: serde_json::Value) -> Result<serde_json::Value, ToolError> {
        let id = bounded_str_arg(&args, "terminal_id", "read_terminal", TERMINAL_ID_MAX_BYTES)?;
        let Some(pty) = self.sandbox.get_terminal(id) else {
            return Err(ToolError::ExecutionFailed {
                tool: "read_terminal".into(),
                reason: format!("no terminal with id '{id}' (killed or never existed)"),
            });
        };
        let bytes = pty.snapshot();
        let text = String::from_utf8_lossy(&bytes).into_owned();
        let tail_lines = optional_tail_lines(&args)?;
        let output = match tail_lines {
            Some(n) => {
                let lines: Vec<&str> = text.lines().collect();
                let start = lines.len().saturating_sub(n);
                lines[start..].join("\n")
            }
            _ => text,
        };
        let output = truncate_utf8(output, TOOL_TEXT_OUTPUT_MAX_BYTES);
        Ok(serde_json::json!({
            "terminal_id": id,
            "alive": pty.is_alive(),
            "output": output.text,
            "truncated": output.truncated,
            "captured_bytes": output.original_bytes,
            "output_limit_bytes": TOOL_TEXT_OUTPUT_MAX_BYTES,
        }))
    }
}

// ── kill_terminal ───────────────────────────────────────────────────────

pub struct KillTerminalTool {
    sandbox: Arc<dyn Sandbox>,
}

#[async_trait::async_trait]
impl BuiltinTool for KillTerminalTool {
    fn concurrency_policy(&self) -> axocoatl_llm::ConcurrencyPolicy {
        axocoatl_llm::ConcurrencyPolicy::Exclusive
    }

    fn description(&self) -> &str {
        "Stop a terminal previously created with spawn_terminal and drop \
         it from the Terminals pane.  Idempotent — returns ok=false if \
         the id is unknown."
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "terminal_id": { "type": "string", "description": "ID returned by spawn_terminal" }
            },
            "required": ["terminal_id"]
        })
    }
    async fn execute(&self, args: serde_json::Value) -> Result<serde_json::Value, ToolError> {
        let id = bounded_str_arg(&args, "terminal_id", "kill_terminal", TERMINAL_ID_MAX_BYTES)?;
        let killed = self.sandbox.kill_terminal(id);
        Ok(serde_json::json!({ "terminal_id": id, "ok": killed }))
    }
}

#[cfg(test)]
mod tests {
    use super::{
        bounded_reason, confine, grep_args, lexical_normalize, optional_tail_lines, shell_quote,
        strip_trailing_ampersand, terminal_dimension, truncate_utf8, BashBackgroundTool, BashTool,
        BuiltinTool, EditFileTool, GlobTool, GrepTool, KillTerminalTool, ListDirTool,
        ListTerminalsTool, ReadFileTool, SpawnTerminalTool, WriteFileTool, COMMAND_ARG_MAX_BYTES,
        FILE_WRITE_MAX_BYTES, SHELL_STREAM_OUTPUT_MAX_BYTES, TERMINAL_COMMAND_PREVIEW_MAX_BYTES,
        TERMINAL_LIST_MAX_ENTRIES, TOOL_ERROR_MAX_BYTES, TOOL_TEXT_OUTPUT_MAX_BYTES,
    };
    use axocoatl_isolation::pty::PtyTerminal;
    use axocoatl_isolation::session_sandbox::{BgTask, ExecResult, Sandbox};
    use axocoatl_isolation::IsolationError;
    use serde_json::json;
    use std::collections::VecDeque;
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    type StdinCalls = Arc<Mutex<Vec<(Vec<String>, usize)>>>;

    #[derive(Clone)]
    struct StubSandbox {
        root: PathBuf,
        results: Arc<Mutex<VecDeque<ExecResult>>>,
        exec_calls: Arc<Mutex<Vec<Vec<String>>>>,
        stdin_calls: StdinCalls,
        background_calls: Arc<Mutex<Vec<String>>>,
        terminals: Arc<Mutex<Vec<(String, String, bool)>>>,
    }

    impl StubSandbox {
        fn new(root: impl Into<PathBuf>, results: Vec<ExecResult>) -> Self {
            Self {
                root: root.into(),
                results: Arc::new(Mutex::new(results.into())),
                exec_calls: Arc::new(Mutex::new(Vec::new())),
                stdin_calls: Arc::new(Mutex::new(Vec::new())),
                background_calls: Arc::new(Mutex::new(Vec::new())),
                terminals: Arc::new(Mutex::new(Vec::new())),
            }
        }

        fn next_result(&self) -> Result<ExecResult, IsolationError> {
            self.results
                .lock()
                .expect("results lock")
                .pop_front()
                .ok_or_else(|| {
                    IsolationError::OciContainerFailed(
                        "stub received an unexpected sandbox command".to_string(),
                    )
                })
        }
    }

    #[test]
    fn production_read_tools_are_safe_and_mutators_are_exclusive() {
        use axocoatl_llm::ConcurrencyPolicy;

        let sandbox: Arc<dyn Sandbox> =
            Arc::new(StubSandbox::new(std::env::temp_dir(), Vec::new()));
        assert_eq!(
            ReadFileTool::new(sandbox.clone(), super::READ_FILE_WINDOW_BYTES).concurrency_policy(),
            ConcurrencyPolicy::Safe
        );
        assert_eq!(
            GlobTool {
                sandbox: sandbox.clone()
            }
            .concurrency_policy(),
            ConcurrencyPolicy::Safe
        );
        for policy in [
            WriteFileTool {
                sandbox: sandbox.clone(),
            }
            .concurrency_policy(),
            EditFileTool {
                sandbox: sandbox.clone(),
            }
            .concurrency_policy(),
            BashTool {
                sandbox: sandbox.clone(),
            }
            .concurrency_policy(),
            BashBackgroundTool {
                sandbox: sandbox.clone(),
            }
            .concurrency_policy(),
            SpawnTerminalTool {
                sandbox: sandbox.clone(),
            }
            .concurrency_policy(),
            KillTerminalTool { sandbox }.concurrency_policy(),
        ] {
            assert_eq!(policy, ConcurrencyPolicy::Exclusive);
        }
    }

    #[async_trait::async_trait]
    impl Sandbox for StubSandbox {
        fn root(&self) -> &Path {
            &self.root
        }

        async fn exec(
            &self,
            argv: &[&str],
            _timeout: Duration,
        ) -> Result<ExecResult, IsolationError> {
            self.exec_calls
                .lock()
                .expect("exec calls lock")
                .push(argv.iter().map(|arg| (*arg).to_string()).collect());
            let mut result = self.next_result()?;
            if argv.get(2) == Some(&super::BOUNDED_STDOUT_SCRIPT) {
                let command_status = result.exit_code;
                result.stderr.push_str(&format!(
                    "{}{}\n",
                    super::BOUNDED_STATUS_MARKER,
                    command_status
                ));
                result.exit_code = 0;
            }
            Ok(result)
        }

        async fn exec_stdin(
            &self,
            argv: &[&str],
            stdin: &str,
            _timeout: Duration,
        ) -> Result<ExecResult, IsolationError> {
            self.stdin_calls.lock().expect("stdin calls lock").push((
                argv.iter().map(|arg| (*arg).to_string()).collect(),
                stdin.len(),
            ));
            self.next_result()
        }

        fn spawn_background(&self, command: &str) -> String {
            self.background_calls
                .lock()
                .expect("background calls lock")
                .push(command.to_string());
            "task-stub".to_string()
        }

        fn spawn_pty(
            &self,
            _command: &str,
            _rows: u16,
            _cols: u16,
        ) -> Result<Arc<PtyTerminal>, String> {
            Err("PTY creation is not used by these tests".to_string())
        }

        fn get_terminal(&self, _id: &str) -> Option<Arc<PtyTerminal>> {
            None
        }

        fn kill_terminal(&self, _id: &str) -> bool {
            false
        }

        fn list_terminals(&self) -> Vec<(String, String, bool)> {
            self.terminals.lock().expect("terminals lock").clone()
        }

        fn list_tasks(&self) -> Vec<BgTask> {
            Vec::new()
        }

        fn with_root(&self, root: &Path) -> Arc<dyn Sandbox> {
            let mut sandbox = self.clone();
            sandbox.root = root.to_path_buf();
            Arc::new(sandbox)
        }

        async fn stop(&self) {}
    }

    fn result(stdout: impl Into<String>, stderr: impl Into<String>, exit_code: i32) -> ExecResult {
        ExecResult {
            stdout: stdout.into(),
            stderr: stderr.into(),
            exit_code,
        }
    }

    #[test]
    fn lexical_normalize_collapses_dot_segments() {
        assert_eq!(
            lexical_normalize(Path::new("/proj/./src/../lib/x.rs")),
            PathBuf::from("/proj/lib/x.rs")
        );
    }

    #[test]
    fn confine_allows_paths_inside_root() {
        let root = Path::new("/home/u/proj");
        // Relative paths resolve against the root.
        assert!(confine(root, "src/main.rs", "read_file").is_ok());
        assert!(confine(root, ".", "list_dir").is_ok());
        assert!(confine(root, "a/b/../c.txt", "read_file").is_ok());
        // An absolute path that is genuinely inside the root is fine.
        assert!(confine(root, "/home/u/proj/src/main.rs", "read_file").is_ok());
    }

    #[test]
    fn confine_rejects_escapes() {
        let root = Path::new("/home/u/proj");
        // Absolute escape.
        assert!(confine(root, "/etc/passwd", "read_file").is_err());
        // Parent-dir traversal out of the root.
        assert!(confine(root, "../other/secret", "read_file").is_err());
        assert!(confine(root, "../../../../etc/shadow", "read_file").is_err());
        // Traversal that dips out then back in still escapes lexically.
        assert!(confine(root, "src/../../proj-evil/x", "write_file").is_err());
        // A sibling directory sharing a prefix must not be treated as inside.
        assert!(confine(root, "/home/u/proj-evil/x", "read_file").is_err());
    }

    #[test]
    fn confine_returns_the_original_path() {
        let root = Path::new("/home/u/proj");
        assert_eq!(
            confine(root, "src/main.rs", "read_file").unwrap(),
            "src/main.rs"
        );
    }

    #[test]
    fn grep_uses_extended_regular_expressions() {
        assert_eq!(
            grep_args("accumulator|FIXED_STEP", "src/main.ts"),
            [
                "grep",
                "-Ern",
                "-e",
                "accumulator|FIXED_STEP",
                "--",
                "src/main.ts"
            ]
        );
    }

    #[test]
    fn utf8_truncation_never_splits_a_scalar() {
        let bounded = truncate_utf8("abc🦀xyz".to_string(), 5);
        assert_eq!(bounded.text, "abc");
        assert!(bounded.truncated);
        assert_eq!(bounded.original_bytes, 10);
    }

    #[test]
    fn bounded_errors_are_utf8_safe_and_marked() {
        let bounded = bounded_reason("🦀".repeat(TOOL_ERROR_MAX_BYTES));
        assert!(bounded.is_char_boundary(bounded.len()));
        assert!(bounded.contains("error detail truncated"));
        assert!(bounded.len() < TOOL_ERROR_MAX_BYTES + 128);
    }

    #[test]
    fn quoted_workspace_paths_cannot_change_the_shell_wrapper() {
        assert_eq!(
            shell_quote("/tmp/Erick's repo"),
            "'/tmp/Erick'\"'\"'s repo'"
        );
    }

    #[test]
    fn bounded_stdout_script_drains_output_and_preserves_status() {
        let output = Command::new("sh")
            .args([
                "-c",
                super::BOUNDED_STDOUT_SCRIPT,
                "sh",
                "6",
                "sh",
                "-c",
                "printf abcdefghijkl; exit 7",
            ])
            .output()
            .expect("run bounded stdout wrapper");
        assert_eq!(output.stdout, b"abcdef");
        assert!(String::from_utf8_lossy(&output.stderr)
            .ends_with("\n__AXOCOATL_TOOL_EXIT_8F431C2D__:7\n"));
    }

    #[test]
    fn terminal_numeric_arguments_do_not_wrap() {
        assert!(terminal_dimension(&json!({"rows": 65_536}), "rows", 24, 4, 500).is_err());
        assert!(terminal_dimension(&json!({"rows": -1}), "rows", 24, 4, 500).is_err());
        assert_eq!(
            terminal_dimension(&json!({"rows": 40}), "rows", 24, 4, 500).unwrap(),
            40
        );
        assert!(optional_tail_lines(&json!({"tail_lines": u64::MAX})).is_err());
        assert!(optional_tail_lines(&json!({"tail_lines": 0})).is_err());
    }

    #[test]
    fn null_is_not_given_for_optional_numbers() {
        assert_eq!(
            terminal_dimension(&json!({"rows": null}), "rows", 24, 4, 500).unwrap(),
            24
        );
        assert_eq!(
            optional_tail_lines(&json!({"tail_lines": null})).unwrap(),
            None
        );
    }

    #[tokio::test]
    async fn null_path_is_the_repository_root_for_list_dir_and_grep() {
        for is_grep in [false, true] {
            let sandbox = Arc::new(StubSandbox::new("/workspace", vec![result("lib\n", "", 0)]));
            if is_grep {
                GrepTool {
                    sandbox: sandbox.clone(),
                }
                .execute(json!({"pattern": "needle", "path": null}))
                .await
                .unwrap();
            } else {
                ListDirTool {
                    sandbox: sandbox.clone(),
                }
                .execute(json!({"path": null}))
                .await
                .unwrap();
            }
            let calls = sandbox.exec_calls.lock().unwrap();
            assert_eq!(
                calls[0].last().map(String::as_str),
                Some("."),
                "grep {is_grep}: {calls:?}"
            );
        }
    }

    /// What `read_file`'s script prints for `bytes`: `od`'s hex dump, its
    /// status after the end marker, and the reading command's status on
    /// standard error.
    fn dumped(bytes: &[u8]) -> ExecResult {
        let mut stdout = String::new();
        for line in bytes.chunks(16) {
            for byte in line {
                stdout.push_str(&format!(" {byte:02x}"));
            }
            stdout.push('\n');
        }
        stdout.push_str(&format!("{}0\n", super::READ_END_MARKER));
        result(stdout, format!("{}0\n", super::BOUNDED_STATUS_MARKER), 0)
    }

    /// The window travels as a hex dump of at most one byte past it; a
    /// window that would end inside a character ends before it, and
    /// `next_offset` counts the file's bytes.
    #[tokio::test]
    async fn read_file_bounds_output_and_requests_only_one_extra_byte() {
        let euro = "€".repeat(TOOL_TEXT_OUTPUT_MAX_BYTES / 3 + 1);
        let sandbox = Arc::new(StubSandbox::new(
            "/workspace",
            vec![dumped(&euro.as_bytes()[..TOOL_TEXT_OUTPUT_MAX_BYTES + 1])],
        ));
        let tool = ReadFileTool::new(sandbox.clone(), super::READ_FILE_WINDOW_BYTES);

        let output = tool.execute(json!({"path": "src/lib.rs"})).await.unwrap();
        let content = output["content"].as_str().unwrap();
        assert!(output["truncated"].as_bool().unwrap());
        // 65,536 bytes end one byte into a three-byte character: the window
        // ends before it.
        assert_eq!(content, "€".repeat(TOOL_TEXT_OUTPUT_MAX_BYTES / 3));
        assert_eq!(output["returned_bytes"], TOOL_TEXT_OUTPUT_MAX_BYTES - 1);
        assert_eq!(output["next_offset"], TOOL_TEXT_OUTPUT_MAX_BYTES - 1);
        assert_eq!(output["captured_bytes"], TOOL_TEXT_OUTPUT_MAX_BYTES + 1);
        assert!(output.get("invalid_utf8").is_none());

        let calls = sandbox.exec_calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0][..3], ["sh", "-c", super::READ_WINDOW_SCRIPT]);
        assert_eq!(
            calls[0][4..],
            [
                "src/lib.rs".to_owned(),
                String::new(),
                (TOOL_TEXT_OUTPUT_MAX_BYTES + 1).to_string()
            ]
        );
    }

    /// A dump that does not finish, or whose `od` is missing, is a failed
    /// read, never an empty file.
    #[tokio::test]
    async fn read_file_refuses_a_dump_it_cannot_trust() {
        let missing_od = result(
            format!("{}127\n", super::READ_END_MARKER),
            format!("sh: od: not found\n{}0\n", super::BOUNDED_STATUS_MARKER),
            0,
        );
        let unfinished = result(
            " 61 62\n",
            format!("{}0\n", super::BOUNDED_STATUS_MARKER),
            0,
        );
        let not_hex = result(
            format!(" 6g\n{}0\n", super::READ_END_MARKER),
            format!("{}0\n", super::BOUNDED_STATUS_MARKER),
            0,
        );
        let missing_file = result(
            format!("{}0\n", super::READ_END_MARKER),
            format!(
                "tail: cannot open 'x' for reading: No such file or directory\n{}1\n",
                super::BOUNDED_STATUS_MARKER
            ),
            0,
        );
        let sandbox = Arc::new(StubSandbox::new(
            "/workspace",
            vec![missing_od, unfinished, not_hex, missing_file],
        ));
        let tool = ReadFileTool::new(sandbox, super::READ_FILE_WINDOW_BYTES);
        let mut reasons = Vec::new();
        for offset in [0, 0, 0, 5] {
            match tool
                .execute(json!({"path": "x", "offset": offset}))
                .await
                .unwrap_err()
            {
                super::ToolError::ExecutionFailed { reason, .. } => reasons.push(reason),
                other => panic!("{other:?}"),
            }
        }
        assert!(reasons[0].contains("no `od`"), "{reasons:?}");
        assert!(reasons[1].contains("did not finish"), "{reasons:?}");
        assert!(reasons[2].contains("'6g'"), "{reasons:?}");
        assert!(reasons[3].contains("cannot open 'x'"), "{reasons:?}");
    }

    #[test]
    fn complete_utf8_prefix_ends_before_a_cut_character_only() {
        use super::complete_utf8_prefix as prefix;
        assert_eq!(prefix(b""), 0);
        assert_eq!(prefix(b"ab"), 2);
        assert_eq!(prefix("a€".as_bytes()), 4);
        assert_eq!(prefix(&"a€".as_bytes()[..3]), 1);
        assert_eq!(prefix(&"a€".as_bytes()[..2]), 1);
        assert_eq!(prefix(&"a🦀".as_bytes()[..4]), 1);
        assert_eq!(prefix("a🦀".as_bytes()), 5);
        // Bytes that are not UTF-8 are not a cut character.
        assert_eq!(prefix(b"a\xff"), 2);
        assert_eq!(prefix(b"a\x80\x80"), 3);
        // A window shorter than one character keeps its bytes.
        assert_eq!(prefix(&"€".as_bytes()[..2]), 2);
    }

    #[test]
    fn the_default_window_is_a_quarter_of_the_context_at_one_token_per_byte() {
        use super::{read_file_window, READ_FILE_WINDOW_BYTES};
        assert_eq!(read_file_window(0), READ_FILE_WINDOW_BYTES);
        assert_eq!(read_file_window(32_768), 8 * 1024);
        assert_eq!(read_file_window(65_536), 16 * 1024);
        assert_eq!(read_file_window(262_144), READ_FILE_WINDOW_BYTES);
        assert_eq!(read_file_window(1_000_000), READ_FILE_WINDOW_BYTES);
        assert_eq!(read_file_window(2_048), 512);
        assert_eq!(read_file_window(1_000), 512);
    }

    #[tokio::test]
    async fn oversized_paths_and_writes_are_rejected_before_sandbox_io() {
        let sandbox = Arc::new(StubSandbox::new("/workspace", vec![]));
        let read = ReadFileTool::new(sandbox.clone(), super::READ_FILE_WINDOW_BYTES);
        let write = WriteFileTool {
            sandbox: sandbox.clone(),
        };

        let path_error = read
            .execute(json!({"path": "p".repeat(super::PATH_ARG_MAX_BYTES + 1)}))
            .await
            .unwrap_err();
        assert!(matches!(path_error, super::ToolError::InvalidArgs { .. }));

        let content_error = write
            .execute(json!({
                "path": "generated.bin",
                "content": "x".repeat(FILE_WRITE_MAX_BYTES + 1),
            }))
            .await
            .unwrap_err();
        assert!(matches!(
            content_error,
            super::ToolError::InvalidArgs { .. }
        ));
        assert!(sandbox.exec_calls.lock().unwrap().is_empty());
        assert!(sandbox.stdin_calls.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn edit_rejects_empty_needles_and_expansions_before_writing() {
        let empty_sandbox = Arc::new(StubSandbox::new("/workspace", vec![]));
        let empty_tool = EditFileTool {
            sandbox: empty_sandbox.clone(),
        };
        assert!(empty_tool
            .execute(json!({"path": "x", "old": "", "new": "value"}))
            .await
            .is_err());
        assert!(empty_sandbox.exec_calls.lock().unwrap().is_empty());

        let sandbox = Arc::new(StubSandbox::new(
            "/workspace",
            vec![result("aaaaaaaaa", "", 0)],
        ));
        let tool = EditFileTool {
            sandbox: sandbox.clone(),
        };
        let error = tool
            .execute(json!({
                "path": "x",
                "old": "a",
                "new": "z".repeat(1024 * 1024),
                "all": true,
            }))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("would produce"));
        assert!(sandbox.stdin_calls.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn list_and_grep_json_are_bounded_and_marked() {
        for is_grep in [false, true] {
            let sandbox = Arc::new(StubSandbox::new(
                "/workspace",
                vec![result("🦀".repeat(TOOL_TEXT_OUTPUT_MAX_BYTES), "", 0)],
            ));
            let output = if is_grep {
                GrepTool {
                    sandbox: sandbox.clone(),
                }
                .execute(json!({"pattern": "needle"}))
                .await
                .unwrap()
            } else {
                ListDirTool {
                    sandbox: sandbox.clone(),
                }
                .execute(json!({}))
                .await
                .unwrap()
            };
            let key = if is_grep { "matches" } else { "listing" };
            assert!(output["truncated"].as_bool().unwrap());
            assert!(output[key].as_str().unwrap().len() <= TOOL_TEXT_OUTPUT_MAX_BYTES);
        }
    }

    /// A grep cut at 64 KiB shows whole lines and says what it left out and
    /// how to narrow the search, so a model can go on.
    #[tokio::test]
    async fn a_cut_grep_says_what_it_left_out_and_how_to_narrow() {
        let line = |index: usize| format!("logs/app.log:{index}: needle in a long line of text\n");
        let all: String = (1..=5000).map(line).collect();
        let total_bytes = all.len();
        let sandbox = Arc::new(StubSandbox::new(
            "/workspace",
            vec![
                result(all.clone(), "", 0),
                result(format!("   5000 {total_bytes}\n"), "", 0),
            ],
        ));
        let output = GrepTool {
            sandbox: sandbox.clone(),
        }
        .execute(json!({"pattern": "needle", "path": "logs"}))
        .await
        .unwrap();
        let matches = output["matches"].as_str().unwrap();
        assert!(output["truncated"].as_bool().unwrap());
        assert!(matches.len() <= TOOL_TEXT_OUTPUT_MAX_BYTES);
        assert!(matches.ends_with('\n') && all.starts_with(matches));
        let shown = matches.lines().count();
        assert_eq!(output["returned_matches"], shown);
        assert_eq!(output["total_matches"], 5000);
        assert_eq!(output["omitted_matches"], 5000 - shown);
        assert_eq!(output["total_bytes"], total_bytes);
        assert_eq!(output["omitted_bytes"], total_bytes - matches.len());
        assert_eq!(
            output["message"],
            format!(
                "The matches were cut at 64 KiB: {shown} of 5000 matching lines are shown; {} \
                 lines ({} bytes) are left out. To see the rest, narrow the search: give a path \
                 (a directory or one file) or a more specific pattern.",
                5000 - shown,
                total_bytes - matches.len()
            )
        );
        // The count runs the same grep in the sandbox and brings back two numbers.
        let calls = sandbox.exec_calls.lock().unwrap().clone();
        assert_eq!(calls.len(), 2);
        assert_eq!(
            calls[1],
            [
                "sh",
                "-c",
                "\"$@\" | wc -lc",
                "sh",
                "grep",
                "-Ern",
                "-e",
                "needle",
                "--",
                "logs"
            ]
        );

        // A result within the limit is unchanged and runs nothing more.
        let sandbox = Arc::new(StubSandbox::new(
            "/workspace",
            vec![result("a.rs:1:needle\n", "", 0)],
        ));
        let output = GrepTool {
            sandbox: sandbox.clone(),
        }
        .execute(json!({"pattern": "needle"}))
        .await
        .unwrap();
        assert_eq!(output["truncated"], false);
        assert!(output.get("message").is_none());
        assert_eq!(sandbox.exec_calls.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn list_dir_treats_an_empty_path_as_the_repository_root() {
        for path in ["", " ", "."] {
            let sandbox = Arc::new(StubSandbox::new(
                "/workspace",
                vec![result("lib\ntest\n", "", 0)],
            ));
            let output = ListDirTool {
                sandbox: sandbox.clone(),
            }
            .execute(json!({ "path": path }))
            .await
            .unwrap();
            assert_eq!(output["listing"], "lib\ntest\n");
            let calls = sandbox.exec_calls.lock().unwrap();
            assert_eq!(
                calls[0].last().map(String::as_str),
                Some("."),
                "{path:?}: {calls:?}"
            );
        }
    }

    #[tokio::test]
    async fn bounded_wrapper_preserves_command_failure_and_removes_its_sentinel() {
        let sandbox = Arc::new(StubSandbox::new(
            "/workspace",
            vec![result("", "invalid regular expression", 2)],
        ));
        let error = GrepTool { sandbox }
            .execute(json!({"pattern": "["}))
            .await
            .unwrap_err();
        let rendered = error.to_string();
        assert!(rendered.contains("invalid regular expression"));
        assert!(!rendered.contains("AXOCOATL_TOOL_EXIT"));
    }

    #[tokio::test]
    async fn glob_never_returns_a_partial_path_at_the_byte_cap() {
        // The candidate listing is cut inside a path: that path is dropped.
        let output_text = format!(
            "./complete.rs\n./{}.rs",
            "x".repeat(super::GLOB_CANDIDATE_MAX_BYTES)
        );
        let sandbox = Arc::new(StubSandbox::new(
            "/workspace",
            vec![result(output_text, "", 0)],
        ));
        let output = GlobTool { sandbox }
            .execute(json!({"pattern": "*.rs"}))
            .await
            .unwrap();
        assert!(output["truncated"].as_bool().unwrap());
        assert_eq!(output["files"], json!(["complete.rs"]));
        assert_eq!(output["count"], 1);
        assert!(output["message"].as_str().unwrap().contains("narrow it"));

        // More matches than the result holds: whole paths only, marked.
        let listing: String = (0..8_000)
            .map(|index| format!("./src/file-{index:05}.rs\n"))
            .collect();
        let sandbox = Arc::new(StubSandbox::new("/workspace", vec![result(listing, "", 0)]));
        let output = GlobTool { sandbox }
            .execute(json!({"pattern": "src/*.rs"}))
            .await
            .unwrap();
        assert!(output["truncated"].as_bool().unwrap());
        let files = output["files"].as_array().unwrap();
        assert!(files.len() < 8_000);
        assert_eq!(files[0], "src/file-00000.rs");
        assert!(files
            .iter()
            .all(|file| file.as_str().unwrap().ends_with(".rs")));
        let returned: usize = files
            .iter()
            .map(|file| file.as_str().unwrap().len() + 1)
            .sum();
        assert!(returned <= TOOL_TEXT_OUTPUT_MAX_BYTES);
    }

    #[test]
    fn glob_plans_a_fixed_listing_below_the_pattern_s_literal_directory() {
        let root = Path::new("/workspace/repo");
        let plan = |pattern: &str| super::glob_plan(pattern, root).unwrap();

        let any_depth = plan("**/*.test.js");
        assert_eq!(any_depth.pattern, "**/*.test.js");
        assert_eq!(any_depth.argv[..2], ["find", "."]);
        assert!(any_depth.argv.ends_with(&[
            "-type".into(),
            "f".into(),
            "-name".into(),
            "*.test.js".into(),
            "-print".into()
        ]));
        assert!(any_depth.skipped.contains(&"node_modules"));
        assert!(any_depth.skipped.contains(&".git"));

        assert_eq!(plan("lib/*.js").argv[1], "./lib");
        assert_eq!(plan("./lib/deep/**/*.js").argv[1], "./lib/deep");
        assert_eq!(plan("./lib/deep/**/*.js").pattern, "lib/deep/**/*.js");
        assert_eq!(plan("*.js").argv[1], ".");
        assert!(!plan("*.js").root_only);
        // `./` or the project path anchors a bare name at the root.
        assert!(plan("./*.js").root_only);
        assert!(plan("/workspace/repo/*.js").root_only);
        assert!(!plan("./lib/*.js").root_only);
        // A directory names everything under it, with no name filter.
        let directory = plan("lib/");
        assert_eq!(directory.argv[1], "./lib");
        assert!(directory
            .argv
            .windows(2)
            .filter(|pair| pair[0] == "-name")
            .all(|pair| super::GLOB_SKIPPED_DIRECTORIES.contains(&pair[1].as_str())));
        // Naming a skipped directory searches it.
        let named = plan("node_modules/pkg/*.js");
        assert_eq!(named.argv[1], "./node_modules/pkg");
        assert!(!named.skipped.contains(&"node_modules"));
        // An absolute path inside the project is made relative.
        assert_eq!(plan("/workspace/repo/lib/*.js").pattern, "lib/*.js");
        // Classes `find -name` would interpret are matched only by the host.
        assert!(!plan("a[1].js")
            .argv
            .windows(2)
            .any(|pair| pair[0] == "-name" && pair[1] == "a[1].js"));
        // Option-looking values stay single arguments after `-name` or `./`.
        let option = plan("-delete");
        assert!(option
            .argv
            .windows(2)
            .any(|pair| pair[0] == "-name" && pair[1] == "-delete"));
        assert_eq!(plan("-rf/*.js").argv[1], "./-rf");

        for bad in ["", "  ", "/", "./", "../x/*.js", "lib/../../x", "/etc/*"] {
            assert!(
                matches!(
                    super::glob_plan(bad, root),
                    Err(super::ToolError::InvalidArgs { .. })
                ),
                "{bad:?}"
            );
        }
    }

    /// Runs argv on this machine inside a directory, as the sandbox runs it
    /// inside the container: real `find`, real exit status.
    struct HostDirSandbox {
        root: PathBuf,
    }

    #[async_trait::async_trait]
    impl Sandbox for HostDirSandbox {
        fn root(&self) -> &Path {
            &self.root
        }
        async fn exec(
            &self,
            argv: &[&str],
            _timeout: Duration,
        ) -> Result<ExecResult, IsolationError> {
            let output = Command::new(argv[0])
                .args(&argv[1..])
                .current_dir(&self.root)
                .output()
                .map_err(IsolationError::Io)?;
            Ok(ExecResult {
                stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
                stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
                exit_code: output.status.code().unwrap_or(-1),
            })
        }
        async fn exec_stdin(
            &self,
            _argv: &[&str],
            _stdin: &str,
            _timeout: Duration,
        ) -> Result<ExecResult, IsolationError> {
            unreachable!("glob never writes")
        }
        fn spawn_background(&self, _command: &str) -> String {
            unreachable!("glob never backgrounds")
        }
        fn spawn_pty(
            &self,
            _command: &str,
            _rows: u16,
            _cols: u16,
        ) -> Result<Arc<PtyTerminal>, String> {
            Err("unused".to_string())
        }
        fn get_terminal(&self, _id: &str) -> Option<Arc<PtyTerminal>> {
            None
        }
        fn kill_terminal(&self, _id: &str) -> bool {
            false
        }
        fn list_terminals(&self) -> Vec<(String, String, bool)> {
            Vec::new()
        }
        fn list_tasks(&self) -> Vec<BgTask> {
            Vec::new()
        }
        fn with_root(&self, root: &Path) -> Arc<dyn Sandbox> {
            Arc::new(Self {
                root: root.to_path_buf(),
            })
        }
        async fn stop(&self) {}
    }

    /// A file longer than one read is read to its end across calls, each at
    /// the previous result's `next_offset`, in windows of up to 64 KiB or of
    /// `limit` bytes; a read at the start keeps its shape, and an offset or
    /// limit that is not a whole number of bytes in range is refused.
    #[tokio::test]
    async fn read_file_reads_a_long_file_to_its_end_from_offsets() {
        let root = tempfile_dir("axocoatl-read-offset");
        let window = super::READ_FILE_WINDOW_BYTES;
        let text: String = (0..window * 2 + 100)
            .map(|index| char::from(b'a' + (index % 26) as u8))
            .collect();
        std::fs::write(root.join("long.txt"), &text).unwrap();
        std::fs::write(root.join("short.txt"), "one line\n").unwrap();
        let tool = ReadFileTool::new(
            Arc::new(HostDirSandbox { root: root.clone() }),
            super::READ_FILE_WINDOW_BYTES,
        );

        let first = tool.execute(json!({"path": "long.txt"})).await.unwrap();
        assert_eq!(first["content"].as_str().unwrap(), &text[..window]);
        assert_eq!(first["truncated"], true);
        assert_eq!(first["next_offset"], window as u64);
        assert!(first.get("offset").is_none());
        let second = tool
            .execute(json!({"path": "long.txt", "offset": first["next_offset"]}))
            .await
            .unwrap();
        assert_eq!(
            second["content"].as_str().unwrap(),
            &text[window..window * 2]
        );
        assert_eq!(second["offset"], window as u64);
        assert_eq!(second["next_offset"], (window * 2) as u64);
        // A string of digits is read as the number the model meant.
        let last = tool
            .execute(json!({"path": "long.txt", "offset": (window * 2).to_string()}))
            .await
            .unwrap();
        assert_eq!(last["content"].as_str().unwrap(), &text[window * 2..]);
        assert_eq!(last["truncated"], false);
        assert!(last.get("next_offset").is_none());
        let past = tool
            .execute(json!({"path": "short.txt", "offset": 4096}))
            .await
            .unwrap();
        assert_eq!(past["content"], "");
        assert_eq!(past["truncated"], false);
        let whole = tool.execute(json!({"path": "short.txt"})).await.unwrap();
        assert_eq!(
            whole,
            json!({"content": "one line\n", "truncated": false, "returned_bytes": 9,
                "captured_bytes": 9, "output_limit_bytes": window})
        );
        // A limit reads a smaller window, and the next one starts after it.
        let piece = tool
            .execute(json!({"path": "long.txt", "offset": 10, "limit": 100}))
            .await
            .unwrap();
        assert_eq!(piece["content"].as_str().unwrap(), &text[10..110]);
        assert_eq!(piece["truncated"], true);
        assert_eq!(piece["next_offset"], 110);
        assert_eq!(piece["output_limit_bytes"], 100);
        let start = tool
            .execute(json!({"path": "long.txt", "limit": "4096"}))
            .await
            .unwrap();
        assert_eq!(start["content"].as_str().unwrap(), &text[..4096]);
        assert_eq!(start["next_offset"], 4096);
        for offset in [json!(-1), json!(1.5), json!("ten"), json!([1])] {
            assert!(matches!(
                tool.execute(json!({"path": "long.txt", "offset": offset}))
                    .await
                    .unwrap_err(),
                super::ToolError::InvalidArgs { .. }
            ));
        }
        for limit in [json!(0), json!(window + 1), json!(-5), json!("all")] {
            assert!(matches!(
                tool.execute(json!({"path": "long.txt", "limit": limit}))
                    .await
                    .unwrap_err(),
                super::ToolError::InvalidArgs { .. }
            ));
        }
        assert!(tool
            .execute(json!({"path": "missing.txt", "offset": 10}))
            .await
            .is_err());
        std::fs::remove_dir_all(root).unwrap();
    }

    /// The window of a model with a 32,768-token context: a read without
    /// `limit` returns 8 KiB, a larger `limit` no more, and the description
    /// and schema say so; the next read starts where the window ended.
    #[tokio::test]
    async fn read_file_windows_fit_the_model_context() {
        let root = tempfile_dir("axocoatl-read-window");
        let text: String = (0..20_000)
            .map(|index| char::from(b'a' + (index % 26) as u8))
            .collect();
        std::fs::write(root.join("long.txt"), &text).unwrap();
        let window = super::read_file_window(32_768);
        let tool = ReadFileTool::new(Arc::new(HostDirSandbox { root: root.clone() }), window);
        assert_eq!(tool.window(), 8 * 1024);
        assert!(tool.description().starts_with("Read up to 8 KiB of a file"));
        assert_eq!(
            tool.parameters_schema()["properties"]["limit"]["maximum"],
            8 * 1024
        );

        let first = tool.execute(json!({"path": "long.txt"})).await.unwrap();
        assert_eq!(first["content"].as_str().unwrap(), &text[..window]);
        assert_eq!(first["next_offset"], window as u64);
        assert_eq!(first["output_limit_bytes"], window);
        let asked_more = tool
            .execute(json!({"path": "long.txt", "offset": first["next_offset"], "limit": 65_536}))
            .await
            .unwrap();
        assert_eq!(
            asked_more["content"].as_str().unwrap(),
            &text[window..window * 2]
        );
        assert_eq!(asked_more["next_offset"], (window * 2) as u64);
        let last = tool
            .execute(json!({"path": "long.txt", "offset": asked_more["next_offset"]}))
            .await
            .unwrap();
        assert_eq!(last["content"].as_str().unwrap(), &text[window * 2..]);
        assert_eq!(last["truncated"], false);
        std::fs::remove_dir_all(root).unwrap();
    }

    /// A file that is not UTF-8, read in windows that cut its characters,
    /// loses no byte: each read starts at the previous `next_offset`, the
    /// windows' `returned_bytes` add up to the file, a character a window
    /// would cut starts the next one, and only `content` is decoded (with
    /// `invalid_utf8` when it held bytes that are not UTF-8).
    #[tokio::test]
    async fn read_file_never_loses_bytes_of_a_file_that_is_not_utf8() {
        let root = tempfile_dir("axocoatl-read-bytes");
        let mut bytes = Vec::new();
        for index in 0..400u32 {
            match index % 6 {
                0 => bytes.extend_from_slice("é".as_bytes()),
                1 => bytes.extend_from_slice("€".as_bytes()),
                2 => bytes.extend_from_slice("🦀".as_bytes()),
                3 => bytes.extend_from_slice(&[0xff, 0xfe]),
                4 => bytes.extend_from_slice(&[0x80, b'x', 0xf0, 0x9f]),
                _ => bytes.extend_from_slice(b"line\n"),
            }
        }
        std::fs::write(root.join("mixed.bin"), &bytes).unwrap();
        let tool = ReadFileTool::new(
            Arc::new(HostDirSandbox { root: root.clone() }),
            super::READ_FILE_WINDOW_BYTES,
        );
        let mut offset = 0u64;
        let mut content = String::new();
        let mut invalid = 0;
        loop {
            let window = tool
                .execute(json!({"path": "mixed.bin", "offset": offset, "limit": 7}))
                .await
                .unwrap();
            let returned = window["returned_bytes"].as_u64().unwrap();
            assert!((1..=7).contains(&returned), "{window}");
            content.push_str(window["content"].as_str().unwrap());
            invalid += usize::from(window["invalid_utf8"] == true);
            offset += returned;
            if window["truncated"] == false {
                assert!(window.get("next_offset").is_none());
                break;
            }
            assert_eq!(window["next_offset"], offset, "{window}");
        }
        assert_eq!(offset, bytes.len() as u64);
        assert_eq!(content, String::from_utf8_lossy(&bytes));
        assert!(invalid > 0);
        // The whole file in one read decodes the same way.
        let whole = tool.execute(json!({"path": "mixed.bin"})).await.unwrap();
        assert_eq!(whole["returned_bytes"], bytes.len());
        assert_eq!(whole["content"].as_str().unwrap(), content);
        assert_eq!(whole["invalid_utf8"], true);
        std::fs::remove_dir_all(root).unwrap();
    }

    fn tempfile_dir(prefix: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "{prefix}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    /// 1.0 ran `find . -name PATTERN`, so every pattern with a `/` matched
    /// nothing: `**/*.test.js`, `lib/*.js` and `**/manifest*.js` all returned
    /// no files in the 1.1.0 eval.
    #[tokio::test]
    async fn glob_matches_paths_with_directories_in_a_real_tree() {
        let root = std::env::temp_dir().join(format!(
            "axocoatl-glob-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        for file in [
            "lib/a.js",
            "lib/b.test.js",
            "lib/deep/c.js",
            "lib/deep/d.test.js",
            "test/e.test.js",
            "x.js",
            "manifest.js",
            "src/manifest-builder.js",
            "src/notes.md",
            "node_modules/pkg/index.js",
            "node_modules/pkg/f.test.js",
            ".git/hooks/h.js",
        ] {
            let path = root.join(file);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, "x").unwrap();
        }
        let tool = GlobTool {
            sandbox: Arc::new(HostDirSandbox { root: root.clone() }),
        };
        let glob = |pattern: &'static str| {
            let tool = &tool;
            async move {
                tool.execute(json!({ "pattern": pattern }))
                    .await
                    .unwrap_or_else(|error| panic!("{pattern}: {error}"))
            }
        };

        let cases: [(&str, &[&str]); 8] = [
            (
                "**/*.test.js",
                &["lib/b.test.js", "lib/deep/d.test.js", "test/e.test.js"],
            ),
            ("lib/*.js", &["lib/a.js", "lib/b.test.js"]),
            (
                "*.js",
                &[
                    "lib/a.js",
                    "lib/b.test.js",
                    "lib/deep/c.js",
                    "lib/deep/d.test.js",
                    "manifest.js",
                    "src/manifest-builder.js",
                    "test/e.test.js",
                    "x.js",
                ],
            ),
            (
                "**/manifest*.js",
                &["manifest.js", "src/manifest-builder.js"],
            ),
            ("lib/**/*.test.js", &["lib/b.test.js", "lib/deep/d.test.js"]),
            ("src/", &["src/manifest-builder.js", "src/notes.md"]),
            ("node_modules/**/*.test.js", &["node_modules/pkg/f.test.js"]),
            ("./*.js", &["manifest.js", "x.js"]),
        ];
        let mut outputs = Vec::new();
        for (pattern, _) in &cases {
            outputs.push(glob(pattern).await);
        }
        let missing_directory = glob("nothing/*.js").await;
        let no_name = glob("*.py").await;
        std::fs::remove_dir_all(&root).unwrap();

        for ((pattern, expected), output) in cases.iter().zip(&outputs) {
            assert_eq!(output["files"], json!(expected), "{pattern}");
            assert_eq!(output["count"], expected.len(), "{pattern}");
            assert_eq!(output["truncated"], false, "{pattern}");
            assert!(output.get("message").is_none(), "{pattern}: {output}");
        }
        for output in [missing_directory, no_name] {
            assert_eq!(output["count"], 0);
            assert_eq!(output["files"], json!([]));
            let message = output["message"].as_str().unwrap();
            assert!(message.starts_with("no files match '"), "{message}");
            assert!(message.contains("node_modules"), "{message}");
        }
    }

    #[tokio::test]
    async fn bash_bounds_both_streams_and_passes_workspace_positionally() {
        let sandbox = Arc::new(StubSandbox::new(
            "/tmp/Erick's repo",
            vec![result(
                "o".repeat(SHELL_STREAM_OUTPUT_MAX_BYTES + 1),
                "e".repeat(SHELL_STREAM_OUTPUT_MAX_BYTES + 1),
                7,
            )],
        ));
        let output = BashTool {
            sandbox: sandbox.clone(),
        }
        .execute(json!({"command": "printf done"}))
        .await
        .unwrap();

        assert_eq!(output["exit_code"], 7);
        assert!(output["stdout_truncated"].as_bool().unwrap());
        assert!(output["stderr_truncated"].as_bool().unwrap());
        assert_eq!(
            output["stdout"].as_str().unwrap().len(),
            SHELL_STREAM_OUTPUT_MAX_BYTES
        );
        assert_eq!(
            output["stderr"].as_str().unwrap().len(),
            SHELL_STREAM_OUTPUT_MAX_BYTES
        );
        let calls = sandbox.exec_calls.lock().unwrap();
        assert_eq!(calls[0][4], "/tmp/Erick's repo");
        assert_eq!(calls[0][5], "printf done");
    }

    #[tokio::test]
    async fn shell_command_arguments_are_bounded_before_execution() {
        let sandbox = Arc::new(StubSandbox::new("/workspace", vec![]));
        let error = BashTool {
            sandbox: sandbox.clone(),
        }
        .execute(json!({"command": "x".repeat(COMMAND_ARG_MAX_BYTES + 1)}))
        .await
        .unwrap_err();
        assert!(matches!(error, super::ToolError::InvalidArgs { .. }));
        assert!(sandbox.exec_calls.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn terminal_inventory_bounds_entries_and_command_previews() {
        let sandbox = Arc::new(StubSandbox::new("/workspace", vec![]));
        *sandbox.terminals.lock().unwrap() = (0..(TERMINAL_LIST_MAX_ENTRIES + 5))
            .map(|index| {
                (
                    format!("terminal-{index}"),
                    "🦀".repeat(TERMINAL_COMMAND_PREVIEW_MAX_BYTES),
                    true,
                )
            })
            .collect();
        let output = ListTerminalsTool { sandbox }
            .execute(json!({}))
            .await
            .unwrap();
        assert_eq!(
            output["terminals"].as_array().unwrap().len(),
            TERMINAL_LIST_MAX_ENTRIES
        );
        assert!(output["truncated"].as_bool().unwrap());
        assert_eq!(
            output["total_count"],
            (TERMINAL_LIST_MAX_ENTRIES + 5) as u64
        );
        assert!(
            output["terminals"][0]["command"].as_str().unwrap().len()
                <= TERMINAL_COMMAND_PREVIEW_MAX_BYTES
        );
    }

    #[test]
    fn strip_trailing_ampersand_drops_redundant_background() {
        // The reflexive `&` an agent adds — bash_background already backgrounds.
        assert_eq!(
            strip_trailing_ampersand("python3 -m http.server 8000 &"),
            ("python3 -m http.server 8000".to_string(), true)
        );
        // Trailing whitespace after the `&`.
        assert_eq!(
            strip_trailing_ampersand("npm run dev &   "),
            ("npm run dev".to_string(), true)
        );
        // No trailing `&` — left as-is.
        assert_eq!(
            strip_trailing_ampersand("npm run dev"),
            ("npm run dev".to_string(), false)
        );
        // `&&` (logical-and) must not be touched.
        assert_eq!(
            strip_trailing_ampersand("make && ./serve"),
            ("make && ./serve".to_string(), false)
        );
        // A mid-command `&` (job control) is left alone.
        assert_eq!(
            strip_trailing_ampersand("a & b"),
            ("a & b".to_string(), false)
        );
    }
}
