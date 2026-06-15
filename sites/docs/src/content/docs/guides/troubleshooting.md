---
title: Troubleshooting
description: "Known issues and how to diagnose them."
---

# Troubleshooting

Run `axocoatl doctor` first — it diagnoses most of the issues below
automatically with fix hints.

## Install

**`axocoatl: command not found` after install.sh**
The binary went to `~/.local/bin`, which isn't on your PATH. Add:
```sh
export PATH="$HOME/.local/bin:$PATH"
```

**`cargo install axocoatl-cli` fails to compile**
Needs Rust 1.82+. Update with `rustup update stable`. Or use the prebuilt
binary: `curl -fsSL https://raw.githubusercontent.com/axocoatl/axocoatl/main/scripts/install.sh | sh`.

## Ollama

**`Ollama not reachable at http://localhost:11434`**
Start it: `ollama serve &`. Verify: `curl http://localhost:11434/api/tags`.

**`Model 'llama3.2' not pulled`**
`ollama pull llama3.2`. Confirm with `ollama list`.

## Config

**`Config invalid` / parse errors**
`axocoatl validate axocoatl.yaml` prints the exact field and a suggestion.
Common causes: missing `name` on an agent, `per_call > per_execution`,
duplicate agent IDs, unresolved `${ENV_VAR}` (set it or add to `.env`).

**API provider key warnings**
Set the key in `.env` (copied from `.env.example`) or directly in the config.
`${OPENAI_API_KEY}`-style placeholders are interpolated from the environment.

## Runtime

**`Token budget exceeded: used N, budget M`**
Working as designed — the budget is enforced before the LLM call. With
`overflow_policy: abort` the agent stops; switch to `warn` to continue past
the budget, or raise `per_execution`. Note: memory context + tool schemas
count toward the input budget.

**`actor is likely terminated` after a budget abort**
Expected: `abort` policy terminates the agent. Restart it
(`axocoatl agents restart <id>`) or use `warn`.

**Workflow agents run in parallel instead of cascading**
Ensure downstream agents declare `depends_on: [<upstream>]` and the workflow
sets a correct `entry_point`. Entry agents must have `depends_on: []`.

**Workflow times out (300s)**
A slow/unreachable provider, or an agent never completing. Check the daemon
logs (`axocoatl dev` prints them) and `axocoatl agents status`.

## Daemon / IPC

**A command says "requires a running daemon"**
Only `agents restart` strictly needs one. Start `axocoatl dev` (IPC + HTTP) or
`axocoatl serve` (HTTP only) in another terminal. Other commands fall back to
an in-process daemon automatically.

**`axocoatl chat` connects in-process instead of via IPC**
No daemon is running, or the socket is stale. Start `axocoatl dev`; it removes
stale sockets on startup.

## MCP

**`mcp servers` is empty**
Add an `mcp_servers:` section to the config. `stdio` servers need `command`;
`streamable_http` servers need `url`. A failing server logs a warning at
bootstrap but never aborts the daemon — check the logs.

## Still stuck?

Run with verbose logs: `RUST_LOG=debug axocoatl dev`. File an issue at
https://github.com/axocoatl/axocoatl/issues with the output of
`axocoatl doctor`.
