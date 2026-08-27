# Changelog

All notable changes to Axocoatl are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Fixed
- **User-level first run.** `axocoatl onboard` now writes one owner-only user
  configuration and platform data directory instead of creating an unrelated project
  folder. Plain config-aware commands resolve that configuration consistently from any
  working directory. Hosted credentials use a masked prompt and a private `0600`
  configuration file. Project-local YAML remains available only through an explicit
  path; repositories become Workspaces through **Open workspace…** in the app. The
  combined `onboard --install-daemon` flow refuses shell-only credential placeholders
  that the generated service cannot inherit.

## [1.0.0] — 2026-08-25

### Changed
- **Stable product release.** Axocoatl's local-first workbench, durable Session runtime,
  normal and multi-Way coding loops, review/recovery controls, Skills, Automations, MCP
  approvals, and release artifacts make up the 1.0 product release.
- **Hardened dependency floor.** Source builds now require Rust 1.88. The PDF,
  spreadsheet, HTTP/2, MCP, terminal, OpenAI, concurrency, and random-number dependency
  lines are updated past current RustSec findings; CI runs a daily deny-by-default RustSec
  audit and rejects yanked lock entries. New Agent checkpoints use a versioned Postcard
  envelope. A narrowly isolated, size-limited Bincode 2.0.1 reader remains only to recover
  0.1.x checkpoint transcripts; the temporary unframed Postcard shape written during 1.0
  launch development is also recognized. Because the two markerless encodings can overlap,
  an exact dual-valid file uses the shipped 0.1.x Bincode interpretation; raw Postcard is
  selected only when the legacy reader does not match. Migration commits the complete transcript to
  canonical Session History as one crash-safe event before writing a higher-version Postcard
  cache. Corrupt or oversized newest caches fall back to an older valid version instead of
  hiding recoverable history.
- **Bounded checkpoint reconstruction.** Canonical Session History remains complete while
  restart projection keeps the newest whole turn segments within an 8 MiB message budget and
  the checkpoint's 64 MiB encoded envelope. Exact final-size validation prevents a very long
  Session from blocking daemon startup or its next turn, and a projected tail never begins
  with an orphan tool result.

### Added
- **Reviewed Session environments.** Session creation now persists a visible runtime/setup
  lifecycle before repository tools can run. Detected commands such as `npm ci` remain exact,
  unapproved proposals; Files, Source Control, Preview, Terminal, normal turns, and Ways
  operations that start work or inspect a live checkout stay gated until the environment is
  durably Ready, while durable evidence and Keep/Discard recovery remain reachable. File
  operations use that same sandbox boundary. Local Podman accepts a fixed curated-image set
  without arbitrary-image trust, verifies or provisions its required repository commands,
  masks a root Node project's host dependencies with a Linux-local volume, and fails with
  manual guidance instead of silently installing Podman or creating its VM. E2B uses one
  daemon-global template and rejects a per-Session OCI image rather than substituting it. Its
  exact runtime identity and remote root remain durable: Close and graceful shutdown pause a
  Ready runtime, Reopen/restart reconnect, and failed or interrupted preparation retains checked
  teardown. Only Delete Session or Change/Rebuild runtime is a deliberate destructive transition
  from Ready. A durable per-generation creation token reconciles ambiguous provider
  responses; if provider access cannot prove the result, **Review setup** exposes an exact-token
  manual-cleanup confirmation rather than creating a replacement or dropping ownership.
- **Durable named Workspaces.** Authorized project directories now have persistent Workspace
  identities independent of their Sessions. The rail scopes Sessions to the selected Workspace,
  **Open workspace…** is separate from **New session**, and legacy path-owned Sessions are
  migrated without changing their ids, transcripts, or execution directories.
- **One browser workbench at `/`.** A persistent workspace/session rail and chat
  spine now hosts the Conversation canvas, Files and Source Control, Preview,
  contextual Ways, focused attempt review, the Terminal dock, Agent graph, and
  Settings instead of splitting them into peer product destinations.
- **Heterogeneous attempts in a session.** A turn can select a different agent and
  model for each attempt, show live state across sessions, compare Outcome and
  Route, run a stored repository check command, and keep a result in the session
  checkout. Parallel attempts currently require an autonomous single-Agent Session on local
  Podman; E2B and multi-agent modes remain available for normal session execution.
- **Native embedded UI modules.** The browser app now uses buildless custom elements
  under `axocoatl-server/static/ui/`, served from `/ui/*` as native ES modules.
- **Canonical durable Session turns.** A Session-owned append-only ledger now records
  the accepted request, structured context references, per-agent outputs, and running,
  completed, failed, cancelled, or interrupted lifecycle. Stable turn and idempotency
  identities make reconnect and retries explicit; older single-agent checkpoint
  transcripts are migrated exactly once when canonical history is first read. A legacy turn
  without a completed assistant response is retained as interrupted, including any readable
  partial assistant output, rather than being presented as complete or silently dropped.
- **Session history controls.** The one app rehydrates canonical turns, searches within
  one Session or across Sessions with case-insensitive literal text matching, exports
  Markdown or JSON, and rewinds visible history at a durable turn boundary. Rewind is a
  logical append-only ledger operation; the current daemon limits it to an autonomous
  single-Agent Session so it can reconstruct that actor's checkpoint. The retained raw-message-count
  request remains a compatibility form.
- **Session-owned attachment context.** The composer accepts drag/drop or file-picker
  uploads as immutable, content-addressed context with **Once** and **Session** scope,
  preview/download, explicit extraction status, and bounded ingestion. Images are capped
  at 10 MiB, other documents at 25 MiB, and cached extracted/OCR text at 256 KiB. This
  context is wired to normal Session turns; isolated Attempts do not currently receive it.
  Accepted one-turn references are re-consumed from the canonical ledger after restart, and
  removing used Session context deactivates future inclusion while retaining the historical
  relation and blob pin so prior turns still open their exact bytes.
- **Durable bounded tool evidence.** Canonical turns record tool start/result events before
  they are broadcast. JSON export keeps the bounded structured events; Markdown renders a
  shorter Route preview and labels truncation. Oversized values remain bounded audit evidence,
  not provider-replay history. Complete provider response groups retain original call order,
  original provider arguments separately from hook-transformed execution arguments, assistant
  content, and bounded native replay metadata. Restart reconstruction is atomic per group: a
  malformed, incomplete, or truncated member stays visible as Route evidence but is omitted
  from the model-facing checkpoint rather than being replayed inaccurately.
- **Context-faithful Retry guard.** A historical turn with attachment or structured composer
  context cannot be reproduced from visible text alone, so inline Retry is unavailable for
  that turn; reattach the context in a new request instead.
- **Exact cooperative Stop for Session turns.** The browser supplies a durable turn id
  and Stop must match that active Session and turn. Provider streaming can be dropped
  immediately; an already-started side-effecting tool runs to a safe boundary before the
  turn becomes cancelled, preserving honest partial output and usage.
- **Pluggable session isolation.** Rootless Podman remains the local default; an
  E2B Cloud remote sandbox is an opt-in configured backend for normal sessions.
  The 1.0 backend targets E2B Cloud, not third-party E2B API implementations.
- **Opt-in provider rate-limit fallback.** A configured `provider:model` backup is
  tried once when the primary rate-limits before streaming any tokens. Plain-text-only
  histories remain per-call; once a response starts a tool exchange, the exact selected
  slot/provider/model stays pinned across later turns and restart while that native transaction
  remains in history. Missing, conflicting, or stale route markers fail closed rather than
  replaying provider-native ids or signatures to another API. A known smaller fallback rejects
  an already oversized request locally; unknown custom-model constraints remain
  endpoint-validated. This applies to global and normal Session Agents, including each
  declared Coordinator Worker. Ways remain primary-only until effective-route cost evidence
  can identify and price a fallback honestly.
- **Per-agent sampling config.** Autonomous, Coordinator, and declared Worker executions accept
  `temperature`, `top_p`, `max_tokens`, and `response_format` in YAML. Provider support varies.
- **Per-request overrides on agent execute.** `POST /api/agents/{id}/execute`
  accepts optional `system_override` and `model_override` for a single call.
- **Stateless per-request execution.** An isolated one-shot mode that runs a
  request without persisting to the agent's session, memory, or checkpoints —
  useful for evaluation. It performs one provider inference and advertises no
  tools; work that requires a tool loop belongs on a normal stateful execution.
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
- **Reliable Ollama Plan and Judge control calls.** Schema-bearing Plan and Judge calls now
  apply a call-local JSON response constraint (native where the provider supports it) and ask
  Ollama for `reasoning_effort: "none"`, without changing the selected autonomous Agent's
  ordinary Session, Skill, Automation, or tool-turn reasoning behavior. The Ollama adapter
  keeps any unexpected reasoning in the reasoning channel and rejects a reasoning-only
  terminal response instead of accepting a blank answer. Malformed scope JSON now stops after the first
  measured call, and a parsed plan must contain a concrete non-test implementation step and
  acceptance evidence before Ways can use it.
- **Complete provider-usage accounting.** Agent activations now retain input, output, and
  reasoning usage on success, cooperative cancellation, and measured failure. A dispatched
  call without terminal usage makes completeness sticky across later calls and checkpoint
  restart, so Settings, Session History, Ways controls, Automation checkpoints, HTTP,
  WebSocket, IPC, and CLI surfaces show a known subtotal instead of an exact-looking zero.
  Plan first, model checks, and Judge remain separate from each Way's execution economics;
  failed, timed-out, and invalid Plan/Judge responses still report their control-call usage.
- **Safe legacy Session rewind boundaries.** The retained raw-message-count endpoint now
  resolves exact boundaries from the canonical transcript rather than assuming every Turn is
  two messages. Failed or cancelled Turns with no Assistant output are retained correctly,
  and a count that would split a Turn fails closed.
- **Vendored browser dependency security and attribution.** The embedded Monaco 0.56 AMD
  graph is built with DOMPurify 3.4.13 and markdown-it 14.3.0, gated against known advisories,
  and packaged with Monaco's upstream notices plus the exact sanitizer licenses and
  attribution required by the shipped JavaScript.
- **Provider-native tool-loop continuity.** Provider-safe request-local tool aliases are
  deterministic, bounded, collision-checked, and decoded before hooks or dispatch. Parallel
  calls and results retain original provider order. Anthropic content blocks and thinking
  signatures, Gemini thought signatures, and original provider arguments survive live
  follow-ups and durable restart projection without substituting transformed execution data.
  Malformed arguments, incomplete streams, invalid terminal sequences, and partial cancelled
  calls fail closed instead of executing or being recorded as a completed response.
- **Fail-closed tool recovery and parallel dispatch.** Response-text tool recovery is limited
  to the effective Ollama route and to names the request actually offered; other providers keep
  response text as text instead of fabricating provider-native replay state. Recovered and
  structured responses reject a 129th tool call before hooks or dispatch, candidate parsing is
  bounded, and a panicking parallel task retains its originating call identity and position so
  another call's success cannot be attributed to it.
- **Bounded Coordinator provider calls.** Decomposition, unresolved HTN frontiers, and
  synthesis reserve output headroom against the exact configured model window before
  dispatch. When older context must be reduced, the Coordinator omits only completed
  User/plain-Assistant text at User boundaries while preserving System messages, the current
  request suffix, attachments, and canonical Session History. This request-local projection
  is distinct from the summarization pipeline used by stateful autonomous Agents.
- **Bounded provider transport failures.** Provider adapters reject redirects, cap error bodies
  and stream events, apply request and total-stream deadlines, and avoid reflecting secrets in
  surfaced errors. OpenAI-compatible, Anthropic, Gemini, Mistral, and Ollama response parsing is
  covered at malformed, parallel, and provider-native replay boundaries.
- **Control-plane storage and upgrade safety.** On supported Unix hosts, the daemon now
  retains one opened data-root capability and performs descendant state I/O relative to it,
  rejecting symlink traversal and multiply linked managed files. Bootstrap acquires an
  external per-root lease, the 0.1-compatible in-root lease, and the opened directory inode
  lock before runtime reconciliation or mutable state reads. It removes only inspected
  non-current Podman containers whose immutable identity and bind mounts prove they expose the
  data or lease root, then verifies those roots still name the opened directories. Local Session and
  Attempt sandboxes mask either protected root when it is below the Workspace and reject a
  Workspace at or beneath one. Current checkpoints, Agent memory, and Automation runs use
  versioned portable namespaces; bounded exact-name legacy recovery validates embedded
  identity where available, writes only the current location, and preserves the legacy source.
- **Recoverable Checks and Keep.** Attempt cleanup now stops Podman containers
  without inheriting Podman's default grace period, allows enough time for VM/client
  overhead, and preserves the last-known comparison evidence on transient refresh
  failures. Keep validates its durable journal through Git-owned scratch storage, so
  an applied result can resume and finish even when `.axo-variants/` is ignored by the
  repository. An exact cross-tab Keep or Discard settlement now re-reads durable Results,
  canonical History, and current Git state before restoring the primary runtime, so a
  completed decision cannot leave another tab showing a stale comparison.
- **Conversation-first responsive workbench.** The Session transcript and composer now own a
  centered primary canvas instead of competing with restored dashboard panes. Files, Preview,
  attempt review, and Agent graph open as focused surfaces with an explicit return; wide-screen
  pinning remains under More. The Ways inspector reserves width only while explicitly open and
  becomes an overlay at compact sizes, while the Session rail becomes off-canvas below 720 px.
  Existing Files/editor/Source Control, Terminal, History, context, graph, Attempts, comparison,
  Checks, Judge, Keep, and Settings behavior remains reachable.
- **Live-safe History hydration.** A delayed canonical History response now merges and repaints
  the longer same-turn live projection, including per-Agent text, reasoning, and correlated tool
  evidence, instead of briefly replacing already-visible output with an older durable prefix.
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
  state, natural-language outputs, and review evidence survive reloads while the set is
  unresolved; stale actions conflict; and cleanup stops containers and joins actors before
  removing exact derived paths. Attempt actors receive the same request-local, provider-safe
  projection of prior Session context: User and plain Assistant text remain ordered while
  historical System and provider-native tool-transaction groups are omitted from model-facing
  Way history. History retains the full canonical turn record, with bounded tool evidence.
  Attempt actors cannot write shared
  core memory and do not receive Skills, MCP tools, or configured web search while those effects
  lack set-scoped rollback.
- **Resumable Keep and honest attempt cost.** Keep now requires a completed,
  non-empty attempt with a passing Check, records `applying`, `applied`, and
  `transcript_recorded` phases, and resumes the same selected attempt after an apply,
  transcript, or cleanup failure. It leaves the delta uncommitted for Git review. After
  cleanup, durable Session History retains the selected task, output, and turn attribution;
  candidate Routes, diffs, Checks, Judge ranking, and cost evidence end with the attempt set.
  Ollama at a configured loopback endpoint has a known-zero model API charge;
  incomplete usage and unconfigured remote prices—including non-loopback Ollama—remain
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
- **Release-package hygiene.** Every publishable crate archive now retains the
  `Apache-2.0` SPDX expression and includes the repository's license text. Raw
  host-specific resource measurements stay local; the resource guide documents
  the reproducible benchmark and validation commands without publishing a dirty
  machine-specific result as a product claim. The server crate carries exact
  package-local mirrors of its embedded Lattice and brand assets, rejects source
  drift and unexpected files, enforces a reviewed archive-size ceiling, and is
  compiled from its extracted `.crate` before the release can publish.
- **Source-build instructions** now target `axocoatl-cli`, the package that produces
  the binary and embeds the browser app. A root-only build compiles the placeholder
  workspace package and can otherwise leave stale UI bytes.
- **Coordinator decomposition parsing** now tolerates the surrounding prose that
  reasoning models emit around the JSON subtask list.
- **macOS workspace build** for `axocoatl-isolation`.

### Removed
- Unwired Wasmtime, Firecracker, and youki isolation prototypes and their feature flags.
  They were never selectable workbench backends; 1.0 ships only the product's reviewed
  rootless Podman path and configured E2B Cloud remote path.
- The standalone `/app` and page-level `/variants` product shells. Their workflows
  now live in the session-centered app at `/`, the only interactive browser route.
- The Studio destination, directoryless lightweight Chat destination, and
  cross-chat Files browser destination from the one-app navigation. Their
  underlying lattice, chat, FileStore, REST, and WebSocket compatibility
  surfaces remain available to integrations; they are not hidden product pages.
  Session-native history and attachment context now cover the corresponding workbench
  needs without recreating either peer destination.

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
  structured `tool_calls`; its system prompt requests `/no_think` to reduce reasoning
  output on runtimes that honor that soft prompt switch.

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
  the curated top of the hierarchy. Canonical Session or Chat history remains
  the transcript authority; Tiers 2 and 4 are derived recall stores.
- **Background "sleep-time" memory consolidation.** When enabled, a daemon loop
  periodically asks registered **idle autonomous Agents** to run an LLM
  memory-manager pass (`on_consolidate`) that promotes durable facts from recent
  Tier-4 activity into the right core-memory block and tidies them — promotion-only,
  never evicting Tier 4. The Agent self-gates on idle time so a pass never fires
  mid-conversation. Declared Coordinator Workers are not polled by this loop, and
  stopping an Agent starts no provider or memory work. Tunable under
  `consolidation` (`enabled`, `idle_threshold_secs`, `interval_secs`).
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
  first-class agents — their own configured provider, model, tools, budgets,
  sampling, hooks, and Session-scoped memory/checkpoints — and they are torn down after every
  pass (on success and on every error path) so nothing leaks. The Coordinator owns Tier-1
  conversation and an internal orchestration checkpoint; its provider loop does not expose
  Tier 2–4 recall. A terminal Completed, Cancelled, Failed, or Interrupted Session turn clears
  private orchestration state, so a later turn decomposes fresh rather than auto-resuming
  finished subtasks. A fully failed
  worker set surfaces an error rather than a hollow result. Per-agent activation
  thresholds are configurable, and coordinator/worker role invariants are
  validated at config load.
- **Automatic context compaction with real LLM summarization.** As a session
  grows toward the model's context window, old turns are now **summarized** (via
  the agent's own provider). When Tier-2 daily log memory is configured, a bounded
  structured archive is written before compaction; canonical Session or Chat
  history remains the transcript authority. Compaction is always on and
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
- **`overflow_policy` now controls the local token guard: `abort` (default) or
  `warn`.** Context management is automatic and independent of the budget, so
  the old `summarize` policy is no longer a distinct behavior — it is accepted
  as a deprecated alias for `warn`. `abort` refuses locally over-budget calls
  and surfaces provider-reported overruns, but it is not an absolute provider
  billing cap.

### Removed
- Dead `ContextCompressor` (superseded by the wired `CompressionPipeline`).

## [0.1.1] — 2026-06-09

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

## [0.1.0] — 2026-06-03

First public release. The framework is functional end-to-end with a real LLM
(local via Ollama, or any configured provider).

### Added
- **Stigmergic multi-agent coordination**: EventLattice pheromone-signal
  activation wired into the daemon. Agents in a workflow self-activate via a
  `depends_on` DAG—no central orchestrator. HTN and auction types were library
  primitives in this release, not part of the live daemon execution path.
- **Workflow execution**: `axocoatl workflow list|run`, `POST /api/workflows/{id}/execute`,
  and IPC support. Entry agents activate directly; downstream agents cascade
  via `TaskCompleted` events.
- **Full command surface** — previously stubbed commands now functional:
  `tokens report`, `agents status`, `agents restart`, `mcp servers`, `mcp tools`.
- **MCP integration**: daemon connects to configured MCP servers at bootstrap
  (stdio + streamable-http transports).
- **Developer experience**: `axocoatl onboard` interactive setup wizard and
  `axocoatl doctor` environment health check.
- **Distribution**: one-line install script and prebuilt binaries for Linux
  x86_64 and macOS; published to crates.io.
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

[1.0.0]: https://github.com/axocoatl/axocoatl/releases/tag/v1.0.0
[0.1.4]: https://github.com/axocoatl/axocoatl/releases/tag/v0.1.4
[0.1.3]: https://github.com/axocoatl/axocoatl/releases/tag/v0.1.3
[0.1.2]: https://github.com/axocoatl/axocoatl/releases/tag/v0.1.2
[0.1.1]: https://github.com/axocoatl/axocoatl/releases/tag/v0.1.1
[0.1.0]: https://github.com/axocoatl/axocoatl/releases/tag/v0.1.0
