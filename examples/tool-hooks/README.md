# Tool hooks — pre/post execution policy and audit logging

Every tool call an Axocoatl agent makes passes through a `HookRegistry` before
it reaches the tool, and the result passes back through it afterward. A hook can
**allow** a call, **deny** it with a reason, or **transform** its arguments (or
its result). This is how the runtime enforces policy without the tools
themselves knowing anything about policy: tools do work, hooks decide what is
permitted.

This example wires two custom hooks plus the built-in `LoggingHook` onto a real
agent and runs a deny → recover cascade with no API keys.

```
cargo run
```

## What it shows

A `write_file` tool scoped to a temp workspace, an agent that tries to escape
that workspace, and a hook that stops it:

```
turn 1   LLM asks: write_file("../../etc/passwd", …)
           └─ Pre hooks: audit logs it, workspace_jail DENIES it
                → agent receives {"error": "path escapes workspace"}
turn 2   LLM sees the denial, retries: write_file("notes/summary.md", …)
           └─ Pre hooks: allowed → tool runs → Post hook audits the result
turn 3   LLM sees the success and writes its closing message
```

The deny is **not** special-cased in the example. It flows through the real
`DefaultAgentBehavior` tool loop, which records the deny reason as a tool result
in the session and makes a follow-up LLM call — so the model genuinely recovers
from the policy block, the same way it would in production.

Sample output:

```
1 of 2 attempts were blocked by the deny policy; the agent recovered and succeeded on the next turn.
Audit trail: 3 events (2 pre, 1 post). The 1 pre-without-post gap is the blocked call.
Filesystem check: …/notes/summary.md exists inside workspace: true
```

## The three actions

A `ToolHook` returns a `HookAction` from its `execute`:

| Action      | Phase     | Effect                                                          |
|-------------|-----------|-----------------------------------------------------------------|
| `Allow`     | Pre, Post | Let the call proceed / pass the result through unchanged        |
| `Deny`      | Pre only  | Stop the call; the reason becomes the tool result for the agent |
| `Transform` | Pre, Post | Rewrite the arguments (Pre) or the result (Post)                |

In a Pre chain, the first `Deny` wins and short-circuits the rest. A `Transform`
feeds its new value to the next hook, so hooks compose. Post hooks cannot deny
(the call already ran) — a `Deny` returned in Post is ignored.

## Global vs per-tool scope

Two registration paths, two policy scopes — both used here:

| Hook             | Registration        | Scope                        |
|------------------|---------------------|------------------------------|
| `logging`        | `register_global`   | every tool, Pre + Post       |
| `audit`          | `register_global`   | every tool, Pre + Post       |
| `workspace_jail` | `register_for_tool` | `write_file` only, Pre       |

Audit everything; police only the one dangerous tool. The `audit` hook writes
one JSON line per event to stdout and to a shared in-memory log, which the
example reads back at the end to show the Pre/Post counts.

## How this maps to enterprise patterns

The same three actions cover the policy controls a deployment usually needs:

- **Tool allowlists / denylists.** A global Pre hook that returns `Deny` for any
  tool not on a sanctioned list. The built-in `DenyListHook`
  (`crates/axocoatl-tools/src/hooks.rs`) does exactly this; the
  `workspace_jail` hook here is the same shape but inspects an argument
  (`path`) rather than just the tool name.
- **Path / capability sandboxing.** The `workspace_jail` hook is a sandbox
  boundary: it denies any `write_file` whose path escapes the agent's
  workspace, blocking traversal (`../../`) and absolute paths before the tool
  ever runs.
- **PII redaction.** A Pre `Transform` hook that rewrites arguments — strip
  emails, card numbers, or secrets out of the JSON before the tool (or an
  outbound MCP call) ever sees them. Because Transform feeds the next hook, you
  can redact and then still allow/deny on the cleaned value.
- **Result scrubbing.** A Post `Transform` hook that redacts a tool's *output*
  before it re-enters the model's context — useful when a tool returns more
  than the agent should retain.
- **Argument-size / rate limits.** The built-in `ArgSizeLimitHook`
  (`crates/axocoatl-tools/src/hooks.rs`) denies calls whose serialized
  arguments exceed a byte cap — a cheap guard against prompt-stuffing a tool.
- **Audit trail for compliance.** A global Pre+Post hook that records every
  call and result. The `audit` hook here is that trail; in production you would
  ship the JSON to your log pipeline instead of stdout.

The production runtime uses this exact mechanism: `McpApprovalHook`
(`crates/axocoatl-daemon/src/mcp_approval_hook.rs`) is a global Pre hook that
parks every MCP tool call on a user-approval gate and returns `Deny` when the
user (or a recorded permission) rejects it.

## Where this lives in the real runtime

- The `ToolHook` trait, the `HookPhase` / `HookAction` / `HookContext` types,
  and the built-in `LoggingHook` / `DenyListHook` / `ArgSizeLimitHook`:
  [`crates/axocoatl-tools/src/hooks.rs`](../../crates/axocoatl-tools/src/hooks.rs)
- The registry that runs the Pre/Post chains (`run_pre_hooks` /
  `run_post_hooks`, global vs per-tool, timeout enforcement):
  [`crates/axocoatl-tools/src/hook_registry.rs`](../../crates/axocoatl-tools/src/hook_registry.rs)
- Where hooks attach to an agent and the tool loop that calls them
  (`with_hook_registry`, the Pre-deny → tool-result → recover path):
  [`crates/axocoatl-actor/src/default_behavior.rs`](../../crates/axocoatl-actor/src/default_behavior.rs)
- The production approval hook this models:
  [`crates/axocoatl-daemon/src/mcp_approval_hook.rs`](../../crates/axocoatl-daemon/src/mcp_approval_hook.rs)
- Architecture overview: [`docs/ARCHITECTURE.md`](../../docs/ARCHITECTURE.md)
