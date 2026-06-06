# Changelog

All notable changes to Axocoatl are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added
- **A flexible cockpit layout with mode presets.** The session cockpit's
  hardwired three-pane grid is now an N-surface layout engine: a registry of
  surfaces (Files, Activity, Browser, Agent graph, Terminal) the engine tiles,
  resizes, focuses, collapses, and reorders generically. A **preset switcher**
  in the cockpit bar (and ⌘⌥-number) swaps the arrangement:
  - **Classic** — Files | Activity | Browser (the previous default, unchanged).
  - **Review** — Activity as a sidebar with the file/diff surface filling the
    rest, opened on Source Control, so a Monaco diff renders full-width and
    side-by-side instead of collapsing in the slim files pane.
  - **Build** — Activity + a large embedded browser/live-preview, terminal
    drawer opened.
  - **Debug** — Activity + Terminal + the live **agent graph** (the Studio
    lattice scoped to the session), tiled side by side — watch an agent's
    reasoning graph and its shell at once. The terminal is promoted from the
    overlay drawer into a real grid surface for this view (the live xterm is
    moved, not recreated). The active preset, sizes, and order persist.
- **Unified, polished conversation UI across the Chat tab and the Sessions
  Activity pane.** The two surfaces now share one rendering layer:
  - Messages render with **markdown-it** (tables, nested/task lists,
    blockquotes, highlighted code) instead of the old hand-rolled renderer.
  - One **tool-call card** with a verb header ("▸ Bash: …", "◆ Read …",
    "◍ Search the web: …"), a collapsible result, and web-search citations —
    identical in both tabs.
  - A shared **"thinking…" indicator** from the moment a turn is sent until
    the first token, tool call, or reasoning chunk.
  - Agent **reasoning** now renders in the Sessions pane (a collapsible block,
    matching Chat), and session messages use the same prose styling as Chat.
  - **Per-message actions on Chat turns** — Copy, Rewind (user turns), and
    Retry + Fork (assistant turns) — all branch via `POST /api/chat/{id}/fork`,
    leaving the parent chat intact.
- **Persisted session transcripts with Retry and Rewind.** A directory
  session's conversation now survives reopening the cockpit — it rehydrates
  from the session agent's checkpoint via the new
  `GET /api/sessions/{id}/messages` (user/agent turns + tool cards). Each turn
  carries actions: **Copy**, **Rewind** (drop this turn onward and re-ask), and
  **Retry** (regenerate the reply), backed by a new
  `POST /api/sessions/{id}/rewind` that truncates the checkpoint and resumes the
  next turn from the truncated state.
- **Git-native sessions: a live Source Control pane.** A directory session is
  now (auto-)a git repo — `git init` + a baseline commit on first use if the
  folder isn't already one (existing repos used as-is). git runs inside the
  session sandbox, on the bind-mounted folder. A VS Code-style **Source
  Control** tab in the cockpit's files pane shows the agent's working-tree
  changes live (branch + changed files with A/M/D/U badges + a count badge),
  opens each change as a **Monaco diff** (HEAD vs working), and supports
  **commit**, per-file **discard**, and **branch switching** from a dropdown.
  An open diff **stays live** — it re-fetches as the agent keeps editing and
  clears itself once the file is committed or reverted — and binary or
  oversized (>512 KB) files report a sentinel instead of dumping bytes into the
  editor. New routes under `/api/sessions/{id}/git`: `status`, `diff`,
  `branches`, `commit`, `discard`, `checkout`. This is the substrate for
  parallel branch "Variants" (next).

### Fixed
- **A lingering session sandbox container no longer breaks new sessions.** A
  container left running by a prior daemon run (a crash, a kill, or a fresh
  data dir) keeps holding its published host ports, so the next session that
  publishes overlapping ports fails to start its rootless port-forwarding proxy
  ("proxy already running") and hard-fails — e.g. the auto-started terminal
  errors on open. The daemon now reaps orphaned `axo-ses-*` containers on
  startup, and treats "proxy already running" as a recoverable port conflict
  (the session opens without that port's forwarding rather than failing).
- **Multi-turn tool-calling round-trip now works on every provider.** Agents
  could be handed tools, but the conversation could not continue after a tool
  ran: the agent loop never recorded the assistant's tool-call turn before the
  tool results, and the results carried no `tool_call_id`, so every follow-up
  request was malformed and rejected by the provider APIs. The full loop —
  model emits a tool call → the tool runs → its result is fed back → the model
  continues — now works on Ollama, OpenAI, OpenRouter, Anthropic, Gemini, and
  Mistral, in both the chat path and resumable sessions. Verified end-to-end
  against each provider's live API.
  - `ToolCall` moved into `axocoatl-core` (re-exported from `axocoatl-llm`) so
    the universal message model can reference it. `ChatMessage` and the
    persisted `StoredMessage` now carry an assistant turn's `tool_calls` and a
    tool result's `name` + `tool_call_id`; new fields are `#[serde(default)]`
    for backward compatibility.
  - The agent loop appends the assistant tool-call turn before dispatching and
    tags each result with its originating call, so the replayed conversation is
    well-formed for every provider's native format (OpenAI `tool_calls` +
    `role: tool`, Anthropic `tool_use`/`tool_result` blocks, Gemini
    `functionCall`/`functionResponse`).
  - Streaming tool-call deltas accumulate by provider `index`. OpenAI, Mistral,
    OpenRouter, and Ollama send the call id only on the first SSE chunk and key
    later argument fragments by index, so tool arguments split across many
    chunks now assemble correctly instead of fragmenting into bogus calls.
  - Gemini and Mistral now send tool definitions and parse tool calls; their
    `capabilities()` report `tool_calling: true`.
- **Tool calling on the OpenAI and Anthropic providers.** Both built the
  outbound chat request without attaching the tool definitions, so models on
  these providers never received the available tools and could not make tool
  calls — only the Ollama provider sent tools. OpenAI now attaches converted
  tools via a shared `build_chat_request` used by both `chat` and `chat_stream`;
  Anthropic attaches `tools` in `build_request_body`. Adds regression tests
  asserting the tool definitions reach the request.
- **Gemini and Mistral providers were non-functional for agents.** The agent
  runtime always streams (`stream_chat` → `provider.chat_stream`, no fallback),
  but both providers' `chat_stream` returned "Streaming not yet implemented", so
  any agent on `provider: gemini` or `provider: mistral` failed on its first
  turn. Implemented real token-by-token SSE streaming for both — Gemini via
  `streamGenerateContent?alt=sse`, Mistral via `stream: true` — matching the
  Anthropic provider's `reqwest_eventsource` pattern, with unit-tested chunk
  parsers.
- **Gemini targeted an endpoint that cannot do function calling.** The provider
  used the `v1` endpoint, which serves the current models but rejects the
  `tools` field outright (`Unknown name "tools"`) and has no `systemInstruction`
  field — so it can never make a tool call. Moved to `v1beta`, which serves the
  current models (e.g. `gemini-2.5-flash`) *and* supports both `tools` and
  `systemInstruction`; restored native `systemInstruction` instead of folding
  the system prompt into the first user turn. Verified end-to-end against the
  live Gemini API.
- **A corrupt or outdated checkpoint no longer prevents an agent from starting.**
  Checkpoint load now discards an undecodable snapshot (corruption, or a schema
  change across an Axocoatl upgrade) with a warning and starts fresh, instead of
  failing agent startup with a fatal deserialization error. A checkpoint is a
  regenerable cache, never a source of truth.

## [0.1.0] — 2026-04-24

First public release. The framework is functional end-to-end with a real LLM
(local via Ollama, or any configured provider).

### Added
- **Stigmergic multi-agent coordination**: EventLattice with pheromone-signal
  activation, HTN symbolic planning, and auction-based agent selection wired
  into the daemon. Agents in a workflow self-activate via a `depends_on` DAG —
  no central orchestrator.
- **Workflow execution**: `axocoatl workflow list|run`, `POST /api/workflows/{id}/execute`,
  and IPC support. Entry agents activate directly; downstream agents cascade
  via `TaskCompleted` events.
- **Full command surface** — previously stubbed commands now functional:
  `tokens report`, `agents status`, `agents restart`, `mcp servers`, `mcp tools`.
- **MCP integration**: daemon connects to configured MCP servers at bootstrap
  (stdio + streamable-http transports).
- **Developer experience**: `axocoatl onboard` interactive setup wizard and
  `axocoatl doctor` environment health check.
- **Distribution**: one-line install script and prebuilt binaries for Linux,
  macOS, and Windows; published to crates.io.
- Root `README.md`, `CHANGELOG.md`, `.gitignore`, user-facing
  `docs/ARCHITECTURE.md` and `docs/TROUBLESHOOTING.md`.

### Changed
- Workspace and all crates renamed from **Nexus** to **Axocoatl**.
- Version bumped from `0.0.1` (name-reservation placeholder) to `0.1.0`
  (first real release).
- Examples are now part of the workspace build and each has a README.

### Fixed
- Workflow coordination bug where the initial `UserInput` event spuriously
  activated downstream agents in parallel instead of cascading after their
  dependencies completed.
- `LICENSE` copyright attribution corrected to "Axocoatl Contributors".
- Zero compiler warnings across the workspace.

[0.1.0]: https://github.com/axocoatl/axocoatl/releases/tag/v0.1.0
