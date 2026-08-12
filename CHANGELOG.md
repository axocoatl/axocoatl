# Changelog

All notable changes to Axocoatl are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added
- **One browser workbench at `/`.** A persistent workspace/session rail and chat
  spine now host Files, Activity, Attempts, Terminal, Browser, Agent graph, git,
  and Settings as modules instead of peer product destinations.
- **Heterogeneous attempts in a session.** A turn can select a different agent and
  model for each attempt, show live state across sessions, compare Outcome and
  Route, run a stored repository check command, and keep a result in the session
  checkout. Parallel attempts currently require a single-agent session on local
  Podman; E2B and multi-agent modes remain available for normal session execution.
- **Native embedded UI modules.** The browser app now uses buildless custom elements
  under `axocoatl-server/static/ui/`, served from `/ui/*` as native ES modules.
- **Pluggable session isolation.** Rootless Podman remains the local default; an
  E2B-compatible remote sandbox is an opt-in configured backend for normal sessions.
- **Opt-in provider rate-limit fallback.** A configured `provider:model` backup is
  tried once when the primary rate-limits before streaming any tokens.
- **Per-agent sampling config.** Default-agent executions accept `temperature`,
  `top_p`, `max_tokens`, and `response_format` in YAML. Provider support varies,
  and the coordinator path does not currently carry these settings.
- **Per-request overrides on agent execute.** `POST /api/agents/{id}/execute`
  accepts optional `system_override` and `model_override` for a single call.
- **Stateless per-request execution.** An isolated one-shot mode that runs a
  request without persisting to the agent's session, memory, or checkpoints —
  useful for evaluation.
- **Outbound event webhooks.** An opt-in dispatcher can POST signed
  (HMAC-SHA256) events from the lattice feed to configured endpoints, with
  bounded retries and secret redaction. A default install makes no outbound
  webhook requests.
- **One live automation runtime.** `AutomationStore` now drives manual,
  interval, lattice-event, and Skill triggers in both `dev` and `serve`. CRUD,
  enable/disable, cadence, and trigger edits reconcile without a restart;
  legacy workflow/schedule/proactive YAML seeds a missing store file once instead of
  registering a second set of runners.
- **Automation creation in Settings.** The Automation explorer can create a
  canonical manual, interval, event, or Skill-triggered record with a valid
  Input → Agent starter DAG, then open it directly in the graph editor.

### Fixed
- **Automation editor validity and dialog access.** Map nodes now require an
  Agent, Tool, or Subgraph body; Subgraph nodes require a known Automation.
  Invalid references block save and run with inline guidance. Settings dialogs
  have accessible names, contain keyboard focus, and restore prior focus when
  they close.
- **Durable, deterministic Automation outcomes.** Bootstrap marks orphaned
  persisted `running` records as `failed` with a retained restart reason, and
  failed-node checkpoint diagnostics survive reload. Completed output now joins
  all executed runtime sinks in declaration order, including terminal Tool,
  Map, and Subgraph results.
- **Restart-safe Automation approvals.** Top-level runs parked at an Interrupt
  are rebuilt from their durable checkpoint after daemon restart, reappear in
  the rail, and continue after operator input without replaying completed nodes.
  New runs persist an immutable Automation/input snapshot, while run status and
  Interrupt checkpoints transition through one atomic file replacement. The
  Runs drawer now bypasses stale browser cache and polls open history in place.
- **Deterministic attempt judging.** Judge prompts now require every surviving
  attempt exactly once with unique ranks `1..N`, using the lower attempt index
  as the deterministic tie-break when outcomes are otherwise equivalent.
- **One live Automation runtime.** The persisted Automation store now drives manual,
  scheduled, lattice-event, and Skill-triggered execution in both `dev` and `serve`.
  Store edits take effect without restart, legacy YAML is first-boot seed data only,
  compatibility workflow/schedule/proactive endpoints project canonical records, and
  provider execution no longer holds the daemon or store lock. Automatic runs are
  single-flight, cool down at dispatch and completion, and retain last-run/count/error
  observations without letting one failure stop trigger dispatch.
- **Attempt-set ownership, isolation, and cleanup.** Parallel attempts now create a
  hidden snapshot of tracked changes and non-ignored untracked files, then give every
  attempt an independent no-origin Git clone in a dedicated Podman container. Durable
  set identity namespaces actors, clones, containers, and artifacts; persisted lane
  state, natural-language outputs, and review evidence survive reloads; stale actions
  conflict; and cleanup stops containers and joins actors before removing exact derived
  paths. Attempt actors receive one request-local snapshot of the canonical session
  history, cannot write shared core memory, and do not receive Skills, MCP tools, or
  configured web search while those effects lack set-scoped rollback.
- **Resumable Keep and honest attempt cost.** Keep now requires a completed,
  non-empty attempt with a passing Check, records `applying`, `applied`, and
  `transcript_recorded` phases, and resumes the same selected attempt after an apply,
  transcript, or cleanup failure. It leaves the delta uncommitted for git review.
  Ollama is known-zero; incomplete usage and unconfigured remote prices remain
  explicitly unknown instead of appearing as a complete `$0.00` total.
- **Lightweight chat transcript isolation.** Separate chats and forks now run from
  their own stored history instead of the configured agent's live or
  checkpoint-restored Tier-1 transcript.
- **One persistent daemon surface.** `serve` now starts the same IPC service as
  `dev`, so the installed background service supports session-oriented CLI
  commands as well as the browser/API. The default socket is stable per user,
  protected by owner-only permissions, and startup will not unlink a live daemon
  or a non-socket path. Service definitions run from the config directory so
  relative runtime data stays attached to that project.
- **Source-build instructions** now target `axocoatl-cli`, the package that produces
  the binary and embeds the browser app. A root-only build compiles the placeholder
  workspace package and can otherwise leave stale UI bytes.
- **Coordinator decomposition parsing** now tolerates the surrounding prose that
  reasoning models emit around the JSON subtask list.
- **macOS workspace build** for `axocoatl-isolation`.

### Removed
- The standalone `/app` and page-level `/variants` product shells. Their workflows
  now live in the session-centered app at `/`, the only interactive browser route.
- The Studio destination, directoryless lightweight Chat destination, and
  cross-chat Files browser destination from the one-app navigation. Their
  underlying lattice, chat, FileStore, REST, and WebSocket compatibility
  surfaces remain available to integrations; they are not hidden product pages.

## [0.1.4] — 2026-06-13

### Added
- **Coordinator run view in the dashboard.** A coordinator's Layer-2 work —
  decomposing a goal, auctioning each subtask to a worker by capability and
  budget, running them in parallel, then synthesizing — is now a live drill-in
  view: goal → the auction (the winning worker and the runner-up bids per
  subtask) → each worker's status and output → the final synthesis. Workers are
  driven by the coordinator (they are not lattice nodes), so this is the surface
  that shows the team. A `CoordinatorReporter` trait keeps the actor crate
  decoupled from the daemon's stream types, and the run id is threaded through
  `AgentInput.context`.
- **Prebuilt `aarch64-unknown-linux-gnu` binaries.** The workspace now uses
  rustls instead of native-tls/openssl, so it cross-compiles to ARM Linux; that
  target is built and published alongside the other release binaries, with a CI
  job guarding the cross-build.

### Fixed
- **Ollama tool calls emitted as text are now recovered.** Some local models
  (e.g. qwen3-coder) return tool calls as `<function=…>` text in the message
  content instead of structured `tool_calls`. Axocoatl now parses that fallback
  form, so those models can drive tools — write files, run commands — instead of
  silently doing nothing.

## [0.1.3] — 2026-06-11

### Fixed
- **Coordinator workers now run on a configured model instead of `gpt-4o`.**
  Spawned workers inherited `AgentConfig::default()`'s `gpt-4o`, so on a
  local-only (Ollama) provider every worker returned `404 model 'gpt-4o' not
  found` and the coordinator could never synthesize. `WorkerConfig` now carries a
  model: declared workers use their own configured model, and ad-hoc workers
  (spawned when no pooled worker bids) inherit the coordinator's.
- **`bash_background` no longer kills the dev server it's asked to start.** A
  trailing `&` double-backgrounds the command (the tool already backgrounds it),
  so the wrapper shell exits and SIGHUPs the process — a dev server dies on
  startup and leaves its port stuck (`Errno 98` on the next bind). The tool now
  strips a single trailing `&` (leaving `&&` and a mid-command `&` untouched).
- **Demo config (`axocoatl.yaml`): the `coder` agent now uses `qwen3:8b`.**
  `qwen2.5-coder:14b` does not support tool-calling through Ollama — it returns
  tool calls as text content rather than structured calls, so a coder session
  never executed them and could not write files or run commands. `qwen3:8b` emits
  structured `tool_calls`; `/no_think` suppresses its reasoning blocks for clean
  session output.

## [0.1.2] — 2026-06-11

### Added
- **Agent-managed core memory (MemGPT/Letta-style blocks).** Tier 3 is now a set
  of named, character-limited, agent-editable blocks (default: `persona`,
  `human`, `project`) rendered into the system prompt every turn. The agent
  curates them mid-conversation via three tools — `core_memory_append`,
  `core_memory_replace`, `core_memory_set` — and an edit is visible on the very
  next request (same turn). Blocks are per-agent by default; a block marked
  `shared` is backed by a process-wide registry so multiple agents see each
  other's edits (team memory). Configure per agent under `memory.core`. This is
  the curated top of the hierarchy — the lossless raw stays in Tiers 2 and 4.
- **Background "sleep-time" memory consolidation.** A daemon loop periodically
  asks **idle** agents to run an LLM memory-manager pass (`on_consolidate`, also
  run once on graceful stop) that promotes durable facts from recent Tier-4
  activity into the right core-memory block and tidies them — promotion-only,
  never evicting Tier 4. The agent self-gates on idle time so a pass never fires
  mid-conversation. Tunable under `consolidation` (`enabled`,
  `idle_threshold_secs`, `interval_secs`).
- **Agent-driven memory recall (MemGPT/Letta-style).** Retrieval is now hybrid:
  the top-k semantic hits are still injected passively each turn, and the agent
  can also pull on demand with two new tools — `recall_search` (semantic search
  over Tier-4 memory) and `recall_timeframe` (read the Tier-2 daily log for a date
  or range). The recall tools are agent-scoped (owned by the behavior, since they
  reach a *specific* agent's per-agent stores), advertised to the model, and
  dispatched in the existing tool loop alongside executor tools. A standing
  capability hint plus a post-compaction note tell the agent what's recallable so
  the tools get used. Recall is tunable per agent via `memory.recall`
  (`passive_inject`, `top_k`, `min_score`), inherited by coordinator workers.
- **Coordinator role — hierarchical task decomposition with worker agents.** An
  agent with `role: coordinator` decomposes a goal into subtasks, assigns each to
  the best-fit worker by **auction** (tool-capability match + remaining token
  budget), runs the workers **in parallel**, and synthesizes their outputs into a
  single answer. Decomposition is HTN-symbolic when methods are configured (an
  `HtnPlanner` expands compound tasks; an `LlmFrontierResolver` fills only the
  frontiers the methods don't cover) and LLM-driven otherwise. Workers are
  first-class agents — their own tools, checkpoints, core + semantic memory,
  and hooks — with run-scoped actor names, and they are torn down after every
  pass (on success and on every error path) so nothing leaks. The pass is
  **resumable**: the plan and each finished subtask are checkpointed, so a crash
  mid-run resumes where it left off instead of re-decomposing; a fully failed
  worker set surfaces an error rather than a hollow result. Per-agent activation
  thresholds are configurable, and coordinator/worker role invariants are
  validated at config load.
- **Automatic context compaction with real LLM summarization.** As a session
  grows toward the model's context window, old turns are now **summarized** (via
  the agent's own provider) and the raw transcript is archived to the Tier-2
  daily log — instead of being silently snipped away. Compaction is always on and
  runs before each request, so long conversations keep their early context
  instead of forgetting it. The 5-stage `CompressionPipeline`'s LLM stages
  (microcompact, autocompact) are now wired to a concrete `LlmSummarizer`, whose
  own summarization tokens count against the agent's budget.
- **OpenAI-compatible servers + per-agent model.** The `openai` provider now
  honors a configurable `base_url`, so it targets any OpenAI-compatible endpoint
  (LM Studio, MLX/oMLX, vLLM, and others), not just `api.openai.com`. Each agent's
  configured `model` is sent as a per-request override, so a shared provider uses
  the agent's model, including in the summarizer and the consolidation pass. Stdio
  MCP servers now receive their configured env vars (e.g. an API key), and four
  catalog entries were repointed from nonexistent npm packages to their `uvx` /
  PyPI equivalents. (Initial PR by first-time contributor Andris Gauračs.)

### Changed
- **Tier 3 is no longer a shared key-value fact store.** The old daemon-global
  `LongTermMemory` (one `long_term.bin` for all agents, written by a session-end
  LLM extraction in `on_stop`) is **retired**, replaced by per-agent core-memory
  blocks. Any existing `{data_dir}/memory/long_term.bin` is obsolete and may be
  deleted; no migration is performed.
- **`overflow_policy` is now strictly a spend cap: `abort` (default) or `warn`.**
  Context management is automatic and independent of the budget, so the old
  `summarize` policy is no longer a distinct behavior — it is accepted as a
  deprecated alias for `warn`. The default is now `abort` (a configured budget is
  enforced) rather than the previous continue-on-overflow default.

### Removed
- Dead `ContextCompressor` (superseded by the wired `CompressionPipeline`).

## [0.1.1] — 2026-06-08

### Added
- **Variants — run one prompt several ways, right in the conversation.** Fan a
  turn out into N parallel attempts (the ⑂ control in the composer, configurable
  from 1 up to 100) and keep the one you like. Each attempt is a real agent
  working in isolation — its own `git worktree` + branch (`axo/variant-{i}`)
  inside the session's container, separate from the others and from your working
  tree. The attempts appear as live **option-pills** at the head of the
  assistant's turn: flip between them as they stream, glance at each one's
  changed-files summary, and **keep** one (reply to it, or a single Keep) — which
  silently merges its branch into your working tree and dissolves the rest. A
  heavy fan-out degrades gracefully: a failed attempt settles on its own, and a
  failed worktree set rolls back cleanly rather than leaving debris. The agent's
  `bash` tools run rooted at each attempt's worktree, so a variant's shell edits
  stay on its own branch. New routes under `/api/sessions/{id}/variants` (start,
  status, adopt, discard); `SessionSandbox::attach` reuses one container across
  worktrees.
- **A conversation-forward cockpit you configure, not a grid you're handed.**
  The session cockpit's hardwired three-pane layout is now an N-surface engine
  (Files, Activity, Browser, Terminal, Agent graph) that tiles, resizes,
  collapses, and reorders generically — but the resting state is calm: a freshly
  opened session is **just the conversation**. Surfaces show up when they're
  useful. The agent's edits land as a **change card** ("Changed N files", tap a
  file for an inline diff); a running dev server lands as a **preview card**
  ("Open" brings the browser in). You add the file tree, terminal, or agent
  graph yourself from a **Panes** menu when you want them, and the files pane's
  editor collapses to nothing when no file is open so it never sits there empty.
  The per-turn model/agent-target pickers and the Panes toggles are small
  on-theme web components (`ax-select`, `ax-toggle`) rather than stock browser
  controls. Layout, sizes, and order persist.
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

[0.1.1]: https://github.com/axocoatl/axocoatl/releases/tag/v0.1.1
[0.1.0]: https://github.com/axocoatl/axocoatl/releases/tag/v0.1.0
