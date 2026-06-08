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

use axocoatl_isolation::session_sandbox::{ExecResult, SessionSandbox};

use crate::builtin::BuiltinTool;
use crate::error::ToolError;
use crate::executor::ToolExecutor;

/// Timeout for quick filesystem operations.
const FS_TIMEOUT: Duration = Duration::from_secs(30);
/// Timeout for shell commands (builds, test runs, …).
const SHELL_TIMEOUT: Duration = Duration::from_secs(180);

fn exec_err(tool: &str, e: axocoatl_isolation::IsolationError) -> ToolError {
    ToolError::ExecutionFailed {
        tool: tool.to_string(),
        reason: e.to_string(),
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
                r.stderr.trim().to_string()
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
pub fn register_session_tools(executor: &mut ToolExecutor, sandbox: Arc<SessionSandbox>) {
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
    sandbox: Arc<SessionSandbox>,
}

#[async_trait::async_trait]
impl BuiltinTool for ReadFileTool {
    fn description(&self) -> &str {
        "Read the contents of a file in the session directory"
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "File path to read" }
            },
            "required": ["path"]
        })
    }
    async fn execute(&self, args: serde_json::Value) -> Result<serde_json::Value, ToolError> {
        let path = str_arg(&args, "path", "read_file")?;
        let path = confine(self.sandbox.root(), path, "read_file")?;
        let r = self
            .sandbox
            .exec(&["cat", path], FS_TIMEOUT)
            .await
            .map_err(|e| exec_err("read_file", e))?;
        let r = require_ok("read_file", r)?;
        Ok(serde_json::json!({ "content": r.stdout }))
    }
}

// ── write_file ──────────────────────────────────────────────────────────

pub struct WriteFileTool {
    sandbox: Arc<SessionSandbox>,
}

#[async_trait::async_trait]
impl BuiltinTool for WriteFileTool {
    fn description(&self) -> &str {
        "Write (creating or overwriting) a file in the session directory"
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "File path to write" },
                "content": { "type": "string", "description": "Full file content" }
            },
            "required": ["path", "content"]
        })
    }
    async fn execute(&self, args: serde_json::Value) -> Result<serde_json::Value, ToolError> {
        let path = str_arg(&args, "path", "write_file")?;
        let path = confine(self.sandbox.root(), path, "write_file")?;
        let content = str_arg(&args, "content", "write_file")?;
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
    sandbox: Arc<SessionSandbox>,
}

#[async_trait::async_trait]
impl BuiltinTool for EditFileTool {
    fn description(&self) -> &str {
        "Replace an exact substring in a file with new text"
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "File path to edit" },
                "old": { "type": "string", "description": "Exact text to replace" },
                "new": { "type": "string", "description": "Replacement text" }
            },
            "required": ["path", "old", "new"]
        })
    }
    async fn execute(&self, args: serde_json::Value) -> Result<serde_json::Value, ToolError> {
        let path = str_arg(&args, "path", "edit_file")?;
        let path = confine(self.sandbox.root(), path, "edit_file")?;
        let old = str_arg(&args, "old", "edit_file")?;
        let new = str_arg(&args, "new", "edit_file")?;

        let read = self
            .sandbox
            .exec(&["cat", path], FS_TIMEOUT)
            .await
            .map_err(|e| exec_err("edit_file", e))?;
        let read = require_ok("edit_file", read)?;
        if !read.stdout.contains(old) {
            return Err(ToolError::ExecutionFailed {
                tool: "edit_file".to_string(),
                reason: "the 'old' text was not found in the file".to_string(),
            });
        }
        let count = read.stdout.matches(old).count();
        let updated = read.stdout.replace(old, new);
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
        Ok(serde_json::json!({ "ok": true, "path": path, "replacements": count }))
    }
}

// ── list_dir ────────────────────────────────────────────────────────────

pub struct ListDirTool {
    sandbox: Arc<SessionSandbox>,
}

#[async_trait::async_trait]
impl BuiltinTool for ListDirTool {
    fn description(&self) -> &str {
        "List the contents of a directory in the session"
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Directory path (default: .)" }
            }
        })
    }
    async fn execute(&self, args: serde_json::Value) -> Result<serde_json::Value, ToolError> {
        let path = args.get("path").and_then(|v| v.as_str()).unwrap_or(".");
        let path = confine(self.sandbox.root(), path, "list_dir")?;
        let r = self
            .sandbox
            .exec(&["ls", "-la", path], FS_TIMEOUT)
            .await
            .map_err(|e| exec_err("list_dir", e))?;
        let r = require_ok("list_dir", r)?;
        Ok(serde_json::json!({ "listing": r.stdout }))
    }
}

// ── grep ────────────────────────────────────────────────────────────────

pub struct GrepTool {
    sandbox: Arc<SessionSandbox>,
}

#[async_trait::async_trait]
impl BuiltinTool for GrepTool {
    fn description(&self) -> &str {
        "Search file contents for a pattern (recursive, with line numbers)"
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "pattern": { "type": "string", "description": "Text or regex to search for" },
                "path": { "type": "string", "description": "Directory or file to search (default: .)" }
            },
            "required": ["pattern"]
        })
    }
    async fn execute(&self, args: serde_json::Value) -> Result<serde_json::Value, ToolError> {
        let pattern = str_arg(&args, "pattern", "grep")?;
        let path = args.get("path").and_then(|v| v.as_str()).unwrap_or(".");
        let path = confine(self.sandbox.root(), path, "grep")?;
        let r = self
            .sandbox
            .exec(&["grep", "-rn", "-e", pattern, path], FS_TIMEOUT)
            .await
            .map_err(|e| exec_err("grep", e))?;
        // grep exits 1 when there are simply no matches — that is not an error.
        if r.exit_code > 1 {
            return Err(require_ok("grep", r).unwrap_err());
        }
        Ok(serde_json::json!({ "matches": r.stdout }))
    }
}

// ── glob ────────────────────────────────────────────────────────────────

pub struct GlobTool {
    sandbox: Arc<SessionSandbox>,
}

#[async_trait::async_trait]
impl BuiltinTool for GlobTool {
    fn description(&self) -> &str {
        "Find files whose name matches a glob pattern (e.g. *.rs)"
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "pattern": { "type": "string", "description": "Filename glob, e.g. '*.rs'" }
            },
            "required": ["pattern"]
        })
    }
    async fn execute(&self, args: serde_json::Value) -> Result<serde_json::Value, ToolError> {
        let pattern = str_arg(&args, "pattern", "glob")?;
        // pattern is $1 — passed to `find`, never interpolated into the script.
        let r = self
            .sandbox
            .exec(
                &["sh", "-c", "find . -name \"$1\" -type f", "sh", pattern],
                FS_TIMEOUT,
            )
            .await
            .map_err(|e| exec_err("glob", e))?;
        let r = require_ok("glob", r)?;
        let files: Vec<&str> = r.stdout.lines().filter(|l| !l.is_empty()).collect();
        Ok(serde_json::json!({ "files": files, "count": files.len() }))
    }
}

// ── bash ────────────────────────────────────────────────────────────────

pub struct BashTool {
    sandbox: Arc<SessionSandbox>,
}

#[async_trait::async_trait]
impl BuiltinTool for BashTool {
    fn description(&self) -> &str {
        "Run a shell command inside the session's sandboxed container"
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "command": { "type": "string", "description": "Shell command to run" }
            },
            "required": ["command"]
        })
    }
    async fn execute(&self, args: serde_json::Value) -> Result<serde_json::Value, ToolError> {
        let command = str_arg(&args, "command", "bash")?;
        // Run at the sandbox root, not the container's default cwd — these
        // differ for an attached (variant) sandbox, where the root is the
        // worktree. A no-op for the primary session (root == default cwd).
        let root = self.sandbox.root().to_string_lossy();
        let scoped = format!("cd '{root}' && {command}");
        // The container is the boundary, so an arbitrary command is safe to run.
        let r = self
            .sandbox
            .exec(&["sh", "-c", &scoped], SHELL_TIMEOUT)
            .await
            .map_err(|e| exec_err("bash", e))?;
        Ok(serde_json::json!({
            "stdout": r.stdout,
            "stderr": r.stderr,
            "exit_code": r.exit_code,
        }))
    }
}

// ── bash_background ─────────────────────────────────────────────────────

pub struct BashBackgroundTool {
    sandbox: Arc<SessionSandbox>,
}

#[async_trait::async_trait]
impl BuiltinTool for BashBackgroundTool {
    fn description(&self) -> &str {
        "Start a long-running command in the background inside the session \
         container (a dev server, a build/test watch). Returns a task id \
         immediately — the command keeps running; check it in Background tasks."
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "command": { "type": "string", "description": "Shell command to run in the background" }
            },
            "required": ["command"]
        })
    }
    async fn execute(&self, args: serde_json::Value) -> Result<serde_json::Value, ToolError> {
        let command = str_arg(&args, "command", "bash_background")?;
        // Root at the sandbox dir (the worktree, for a variant sandbox).
        let root = self.sandbox.root().to_string_lossy();
        let task_id = self
            .sandbox
            .spawn_background(&format!("cd '{root}' && {command}"));
        Ok(serde_json::json!({ "task_id": task_id, "started": true }))
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
    sandbox: Arc<SessionSandbox>,
}

#[async_trait::async_trait]
impl BuiltinTool for SpawnTerminalTool {
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
                "command": { "type": "string", "description": "Shell command to run in the new terminal" },
                "rows":    { "type": "integer", "description": "Terminal rows (default 24)", "minimum": 4 },
                "cols":    { "type": "integer", "description": "Terminal cols (default 80)", "minimum": 20 }
            },
            "required": ["command"]
        })
    }
    async fn execute(&self, args: serde_json::Value) -> Result<serde_json::Value, ToolError> {
        let command = str_arg(&args, "command", "spawn_terminal")?;
        let rows = args.get("rows").and_then(|v| v.as_u64()).unwrap_or(24) as u16;
        let cols = args.get("cols").and_then(|v| v.as_u64()).unwrap_or(80) as u16;
        let pty = self.sandbox.spawn_pty(command, rows, cols).map_err(|e| {
            ToolError::ExecutionFailed {
                tool: "spawn_terminal".into(),
                reason: e,
            }
        })?;
        Ok(serde_json::json!({
            "terminal_id": pty.id,
            "command": pty.command,
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
    sandbox: Arc<SessionSandbox>,
}

#[async_trait::async_trait]
impl BuiltinTool for ListTerminalsTool {
    fn description(&self) -> &str {
        "List every terminal currently open in the user's Terminals pane.  \
         Returns an array of objects with `terminal_id`, `command`, and \
         `alive`.\n\n\
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
        let entries: Vec<_> = self
            .sandbox
            .list_terminals()
            .into_iter()
            .map(|(id, command, alive)| {
                serde_json::json!({
                    "terminal_id": id,
                    "command": command,
                    "alive": alive,
                })
            })
            .collect();
        Ok(serde_json::json!({ "terminals": entries, "count": entries.len() }))
    }
}

// ── read_terminal ───────────────────────────────────────────────────────
//
// Returns the current scrollback (up to 64 KiB) so the agent can check on
// what its spawned terminals have done since it last looked.

pub struct ReadTerminalTool {
    sandbox: Arc<SessionSandbox>,
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
                "tail_lines":  { "type": "integer", "description": "If set, return only the last N lines.  Default: full buffer." }
            },
            "required": ["terminal_id"]
        })
    }
    async fn execute(&self, args: serde_json::Value) -> Result<serde_json::Value, ToolError> {
        let id = str_arg(&args, "terminal_id", "read_terminal")?;
        let Some(pty) = self.sandbox.get_terminal(id) else {
            return Err(ToolError::ExecutionFailed {
                tool: "read_terminal".into(),
                reason: format!("no terminal with id '{id}' (killed or never existed)"),
            });
        };
        let bytes = pty.snapshot();
        let text = String::from_utf8_lossy(&bytes).into_owned();
        let output = match args.get("tail_lines").and_then(|v| v.as_u64()) {
            Some(n) if n > 0 => {
                let n = n as usize;
                let lines: Vec<&str> = text.lines().collect();
                let start = lines.len().saturating_sub(n);
                lines[start..].join("\n")
            }
            _ => text,
        };
        Ok(serde_json::json!({
            "terminal_id": id,
            "alive": pty.is_alive(),
            "output": output,
        }))
    }
}

// ── kill_terminal ───────────────────────────────────────────────────────

pub struct KillTerminalTool {
    sandbox: Arc<SessionSandbox>,
}

#[async_trait::async_trait]
impl BuiltinTool for KillTerminalTool {
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
        let id = str_arg(&args, "terminal_id", "kill_terminal")?;
        let killed = self.sandbox.kill_terminal(id);
        Ok(serde_json::json!({ "terminal_id": id, "ok": killed }))
    }
}

#[cfg(test)]
mod tests {
    use super::{confine, lexical_normalize};
    use std::path::{Path, PathBuf};

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
}
