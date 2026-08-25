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
        None => Ok(default),
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
    let Some(value) = args.get(key) else {
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
    let Some(value) = args.get("tail_lines") else {
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
    let mut result = sandbox
        .exec(&borrowed, timeout)
        .await
        .map_err(|error| exec_err(tool, error))?;

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
/// each tool bound to `sandbox`.
pub fn register_session_tools(executor: &mut ToolExecutor, sandbox: Arc<dyn Sandbox>) {
    executor.register_builtin(
        "read_file",
        Arc::new(ReadFileTool {
            sandbox: sandbox.clone(),
        }),
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
}

#[async_trait::async_trait]
impl BuiltinTool for ReadFileTool {
    fn description(&self) -> &str {
        "Read up to 64 KiB from the start of a file in the session directory. The result reports when more content was truncated."
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "File path to read (maximum 4 KiB)" }
            },
            "required": ["path"]
        })
    }
    async fn execute(&self, args: serde_json::Value) -> Result<serde_json::Value, ToolError> {
        let path = bounded_str_arg(&args, "path", "read_file", PATH_ARG_MAX_BYTES)?;
        let path = confine(self.sandbox.root(), path, "read_file")?;
        // Bound the command's stdout before it reaches the sandbox transport.
        // One extra byte lets the JSON result state truncation honestly.
        let capture_bytes = (TOOL_TEXT_OUTPUT_MAX_BYTES + 1).to_string();
        let r = self
            .sandbox
            .exec(&["head", "-c", &capture_bytes, "--", path], FS_TIMEOUT)
            .await
            .map_err(|e| exec_err("read_file", e))?;
        let r = require_ok("read_file", r)?;
        let (content, truncated, captured_bytes) =
            bounded_text_fields(r.stdout, TOOL_TEXT_OUTPUT_MAX_BYTES);
        let returned_bytes = content.len();
        Ok(serde_json::json!({
            "content": content,
            "truncated": truncated,
            "returned_bytes": returned_bytes,
            "captured_bytes": captured_bytes,
            "output_limit_bytes": TOOL_TEXT_OUTPUT_MAX_BYTES,
        }))
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
                "path": { "type": "string", "description": "Directory path (default: ., maximum 4 KiB)" }
            }
        })
    }
    async fn execute(&self, args: serde_json::Value) -> Result<serde_json::Value, ToolError> {
        let path = optional_bounded_str_arg(&args, "path", ".", "list_dir", PATH_ARG_MAX_BYTES)?;
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
        "Search file contents recursively with line numbers, returning up to 64 KiB and reporting truncation"
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
        let (matches, truncated, captured_bytes) =
            bounded_text_fields(r.stdout, TOOL_TEXT_OUTPUT_MAX_BYTES);
        let returned_bytes = matches.len();
        Ok(serde_json::json!({
            "matches": matches,
            "truncated": truncated,
            "returned_bytes": returned_bytes,
            "captured_bytes": captured_bytes,
            "output_limit_bytes": TOOL_TEXT_OUTPUT_MAX_BYTES,
        }))
    }
}

// ── glob ────────────────────────────────────────────────────────────────

pub struct GlobTool {
    sandbox: Arc<dyn Sandbox>,
}

#[async_trait::async_trait]
impl BuiltinTool for GlobTool {
    fn description(&self) -> &str {
        "Find files whose name matches a glob pattern (e.g. *.rs), returning complete paths within a 64 KiB result cap"
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "pattern": { "type": "string", "description": "Filename glob, e.g. '*.rs' (maximum 16 KiB)" }
            },
            "required": ["pattern"]
        })
    }
    async fn execute(&self, args: serde_json::Value) -> Result<serde_json::Value, ToolError> {
        let pattern = bounded_str_arg(&args, "pattern", "glob", SEARCH_ARG_MAX_BYTES)?;
        // Pattern is a positional argument to `find`, never shell text.
        let r = exec_bounded_stdout(
            self.sandbox.as_ref(),
            &["find", ".", "-name", pattern, "-type", "f"],
            FS_TIMEOUT,
            "glob",
            TOOL_TEXT_OUTPUT_MAX_BYTES,
        )
        .await?;
        let r = require_ok("glob", r)?;
        let bounded = truncate_utf8(r.stdout, TOOL_TEXT_OUTPUT_MAX_BYTES);
        let mut output = bounded.text;
        if bounded.truncated {
            // A byte cap may end inside a path. Never invent a partial match.
            match output.rfind('\n') {
                Some(end) => output.truncate(end + 1),
                None => output.clear(),
            }
        }
        let files: Vec<&str> = output.lines().filter(|line| !line.is_empty()).collect();
        let count = files.len();
        Ok(serde_json::json!({
            "files": files,
            "count": count,
            "truncated": bounded.truncated,
            "captured_bytes": bounded.original_bytes,
            "output_limit_bytes": TOOL_TEXT_OUTPUT_MAX_BYTES,
        }))
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
            ReadFileTool {
                sandbox: sandbox.clone()
            }
            .concurrency_policy(),
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

    #[tokio::test]
    async fn read_file_bounds_output_and_requests_only_one_extra_byte() {
        let sandbox = Arc::new(StubSandbox::new(
            "/workspace",
            vec![result("🦀".repeat(TOOL_TEXT_OUTPUT_MAX_BYTES), "", 0)],
        ));
        let tool = ReadFileTool {
            sandbox: sandbox.clone(),
        };

        let output = tool.execute(json!({"path": "src/lib.rs"})).await.unwrap();
        let content = output["content"].as_str().unwrap();
        assert!(output["truncated"].as_bool().unwrap());
        assert!(content.len() <= TOOL_TEXT_OUTPUT_MAX_BYTES);
        assert!(content.is_char_boundary(content.len()));

        let calls = sandbox.exec_calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0][0], "head");
        assert_eq!(calls[0][2], (TOOL_TEXT_OUTPUT_MAX_BYTES + 1).to_string());
        assert_eq!(calls[0].last().unwrap(), "src/lib.rs");
    }

    #[tokio::test]
    async fn oversized_paths_and_writes_are_rejected_before_sandbox_io() {
        let sandbox = Arc::new(StubSandbox::new("/workspace", vec![]));
        let read = ReadFileTool {
            sandbox: sandbox.clone(),
        };
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
        let output_text = format!(
            "./complete.rs\n./{}",
            "x".repeat(TOOL_TEXT_OUTPUT_MAX_BYTES)
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
        assert_eq!(output["files"], json!(["./complete.rs"]));
        assert_eq!(output["count"], 1);
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
