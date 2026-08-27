# Axocoatl Architecture

A practical overview of how Axocoatl's one workbench runs and coordinates agents.

## The big picture

```
            ┌─────────────────────────── axocoatl daemon ───────────────────────────┐
 App / CLI  │  ProviderRegistry   AgentRegistry   EventLattice   McpToolRegistry     │
 HTTP / WS ─┼─▶ (per-agent LLMs)  (ractor actors) (skills/events)  (MCP tools)         │
    / IPC   │        │                 │                │                            │
            │        └──────── DefaultAgentBehavior ─────┘                            │
            │       turn ledger → session mem → budget → LLM → tools → checkpoint     │
            └────────────────────────────────────────────────────────────────────────┘
```

The **daemon** (`axocoatl-daemon`) bootstraps everything: providers, agents
(spawned as `ractor` actors), the event lattice, MCP connections, and the
canonical Automation trigger runtime. Both `axocoatl dev` and `axocoatl serve`
expose the Unix-socket IPC server and HTTP/browser app from the same daemon
state; `serve` is also what the installed background service runs.

## Product surface

The installed CLI resolves one user configuration and durable data root independent of
the current working directory. On macOS these live under
`~/Library/Application Support/Axocoatl/`; Linux and WSL use XDG configuration and data
directories. `axocoatl onboard` configures that user-level product and creates no
repository folder. An explicit `--config` retains project-local operator mode and uses
data beside that configuration unless `AXOCOATL_DATA_DIR` overrides it.

The browser app at `/` is the operational face of the runtime and the only supported
interactive browser route. A Workspace is a durable, user-named identity for one authorized
project directory; a Session belongs to one Workspace and owns persistent work and chat
anchored to that directory. The Session conversation owns the main canvas;
Files/editor/Source Control, Preview, attempt comparison, and Agent graph open as focused
tools, Ways is a contextual inspector, and Terminal remains in its bottom dock. Agents,
Skills, MCP servers, and Automations are configured through Settings.

`WorkspaceStore` persists one JSON record per canonical path under
`{data_dir}/workspaces/`. Workspace identity and display name survive when the Workspace has
no open Session. `Session.workspace_id` records the durable owner while `working_dir` remains
the execution and compatibility path authority. On startup, Sessions written before
Workspace ownership existed are grouped by canonical `working_dir`, a Workspace is created
for each distinct path, and the Session files are linked idempotently. The path-based Session
creation API remains compatible by finding or creating that Workspace; the browser uses the
Workspace-scoped creation API so a New Session action cannot change folders implicitly.

A request can execute as one session turn or as several isolated attempts with different
agents and models. Attempts are checked and compared, one result is kept, and the resulting
changes return to the session checkout for git review. See [PRODUCT.md](PRODUCT.md) for the
interaction and terminology contract.

Normal directory-session execution now has a canonical Session turn ledger and Session-owned
attachment relations. The runtime still contains separately evolved lightweight Chat,
Automation, and attempt execution paths. Their retained compatibility APIs do not make those
state models identical or restore peer browser destinations. Changes at this seam must verify
run identity, transcript ownership, reconnect, cancellation, persistence, and cleanup end to
end.

`AutomationStore` is the canonical persisted configuration for manual, scheduled, lattice-
event, and Skill-triggered automations. Legacy workflow/schedule/proactive YAML seeds the store
only when its canonical file does not exist; it is not a parallel live registry. One dispatcher
reconciles store changes and lattice notifications for both `dev` and `serve`, prevents
overlapping automatic runs of the same automation, and records last outcome/count/error in
compatibility views. It clones an owned
execution context before provider/tool work, so neither the store nor the daemon state lock is
held across a run.

## Session turn ownership and control

`SessionTurnStore` is the canonical user-visible transcript for normal Session execution. Its
versioned JSONL ledger records an idempotent begin event before execution, bounded output and
execution facts, per-agent output, and one terminal transition. Materialized lifecycle is
`running`, `completed`, `failed`, `cancelled`, or `interrupted`; bootstrap reconciles an
orphaned running turn to interrupted instead of presenting it as still live. Older
single-agent actor checkpoints are imported when a Session without any canonical turns,
including rewound turns, is first read. Axocoatl decodes the exact 0.1.x Bincode layouts and
the temporary unframed launch-candidate Postcard layout under a strict size limit, validates
the checkpoint identity and version, and searches older versions when a newer cache is
corrupt. The canonical markerless byte languages overlap, so an exact dual-valid file resolves
to the shipped 0.1.x Bincode interpretation; unframed Postcard is selected only when the legacy
reader does not match. The complete recovered transcript becomes one fsynced ledger event, so a crash leaves
either every imported turn or an ignored partial tail. Only after that canonical write does the
daemon add a higher-version, enveloped Postcard cache; cache promotion failure cannot roll back
History. A segment without a completed assistant response is retained as interrupted. The
legacy `/messages` projection remains for compatibility.

The browser supplies the durable `turn_id` and idempotency key. One Session may own one active
normal turn. `session-stop` must match the active Session and turn id, so a stale browser cannot
stop newer work. Cancellation is cooperative: a provider stream can be dropped as soon as the
control fires, but a tool already dispatched is awaited to its safe completion boundary. The
ledger records honest partial output and usage as cancelled; it does not imply rollback of a
filesystem, MCP, or other external effect.

`SessionAttachmentStore` owns the Session-local relation—display name, scope, extraction
snapshot, and consumption state—while `FileStore` owns immutable SHA-256-addressed bytes and a
bounded extraction cache. A **Once** relation becomes consumed only after the exact turn begin
is durable; startup idempotently replays every accepted one-turn upload reference—including a
superseded turn—and can rebuild a missing relation from immutable Begin metadata. A **Session**
relation stays selected for later turns. Any relation already named by a canonical turn is a
durable blob pin. Removing it deactivates future selection while preserving its historical
relation and content route. Declared image uploads are limited
to 10 MiB, other documents to 25 MiB. Extracted and OCR representations are each bounded to
256 KiB, and OCR has a 30-second process timeout. Only images and PDFs may render inline; other
content downloads with `nosniff`.

Removing an unused relation or deleting a Session does not currently garbage-collect its
underlying content-addressed blob. That retention is deliberate until reference-safe garbage
collection exists across Session, Chat, and global FileStore ownership; deleting shared bytes
speculatively would be worse than retaining them. Normal Session turns receive selected
attachments. The isolated Attempts path currently passes no attachment context.

Tool start/result events are canonical execution records for both single- and multi-agent
turns and are fsynced before live broadcast. Structured arguments and results are bounded at
16 KiB; a larger value becomes `{truncated, original_bytes, preview}` with an 8 KiB audit
preview. Each value also has a separate ledger-truncation flag so a legitimate tool payload
whose own schema contains `"truncated": true` remains replayable. JSON export carries these
bounded events. Markdown Route output applies a further 2 KiB rendered preview and marks
truncation.

The start event retains both the arguments actually executed after hooks and the original
provider arguments used for provider-native replay. It also retains bounded response-group,
call-order, assistant-content, and native provider metadata (for example, Anthropic content
blocks or Gemini thought signatures). Restart projection accepts only a complete group with
unique call identities, one correlated result per call, untruncated names/ids/values, and exact
nonempty provider metadata. Any malformed or oversized member omits that whole group from the
model-facing checkpoint while leaving its bounded Route evidence visible in canonical History.

A provider response may contain at most 128 actionable tool calls. Incremental streams reject
the 129th distinct call before growing the actor accumulator, and recovered text candidates are
bounded before hooks or dispatch. Text-to-tool recovery is an Ollama compatibility path selected
from the effective provider route and accepts only names offered in that request; other providers
leave response text non-actionable because their native ids, signatures, and block structure
cannot be fabricated safely. Concurrent dispatch preserves each original call identity and order
even when one spawned task panics.

Session history, literal search, and Markdown/JSON export are projections of the canonical
ledger. Rewind appends a logical boundary that supersedes later turns in normal list, search,
and transcript views. It is blocked by a running turn or unresolved Attempt set and currently
requires a single-agent Session: the daemon writes a new actor checkpoint reconstructed from
the retained canonical transcript. The retained raw-message-count request is a compatibility
way to choose the same kind of boundary. Neither form rolls back tools, filesystem or external
effects, supports multi-agent checkpoint reconstruction, or provides secure deletion. Explicit
Session deletion is separate: it durably removes the Session owner first, then idempotently
rewrites that Session's turn events out of the ledger and removes its attachment relations. A
Session-store unlink failure keeps the owner and history visible; a retry after owner removal
finishes any interrupted cleanup. Retained blobs, prior checkpoint files, and other memory tiers
follow their own retention policies.

The rewind projection spans two durable stores but is not one atomic database transaction. The
daemon prepares a new checkpoint, commits the append-only ledger boundary, and removes the
prepared checkpoint if the ledger append returns an error. A `SIGKILL` can interrupt between
those writes; the next bootstrap treats the ledger as authoritative and deterministically
repairs the checkpoint before serving. Startup and every single-agent actor respawn also
reconcile terminal canonical turns into the checkpoint cache, including hidden code/DOM context
and complete bounded tool pairs. The canonical ledger remains complete; the recovery cache
selects only the newest whole turn segments that fit its bounded message and envelope limits.
It never begins with an orphan tool result, and a final exact encoding check prevents an
oversized projection from bricking startup or the next turn. Consistency is restored on restart
rather than at the instant of an uncatchable process death.

Retry has a similar fidelity boundary. A canonical turn can retain immutable attachment
context, while a consumed **Once** relation and structured code/DOM references are not generally
reselectable from request text alone. The shell disables inline Retry for a context-bearing
turn and directs the person to reattach context in a new request; it does not silently turn an
attachment-dependent retry into a text-only request. Rewind-to-edit can prefill the historical
text, but it warns that context must be attached again.

## Attempt ownership and lifecycle

A session may own one unresolved `AttemptSet` at a time. The set records its UUID,
original task, effective instruction, snapshot commit/tree, resolved agent/provider/model for
every lane, and creation time. Starting another set or sending another session turn conflicts
until the current set is kept or discarded. This keeps one decision loop attached to one turn
in the chat spine.

Parallel attempts currently require a single autonomous-Agent Session on the local Podman
backend. E2B, coordinator Sessions, and other multi-agent modes remain available for normal
Session turns, but the daemon rejects an Attempt-set start there until nested-worker route,
cost, memory, transcript, and cleanup evidence can be represented honestly.

The base is a hidden commit built with an alternate Git index. It captures tracked changes and
non-ignored untracked files—including the current staged and unstaged content—without changing
the real index, branch, or working tree. A repository before its first commit is seeded from an
empty tree. A hidden ref protects the snapshot for the set's lifetime.

Each attempt receives an independent `--no-hardlinks` Git clone of that snapshot, checked out
on a set-scoped branch. The setup-only branch and `origin` remote are removed from each clone,
so it neither shares the primary repository's Git directory nor retains a route back to it.
Set and session digests namespace the clone, branch, artifacts, actor, and container.

Each clone is the sole workspace mount in a fresh rootless Podman container. The attempt actor
therefore cannot reach the primary workspace, sibling attempts, or the metadata beside the
clones through its repository tools. Every lane receives the same provider-safe projection of
the canonical single-agent Session context through `SuppliedHistory`: prior User and plain
Assistant text remain ordered, while historical System messages and complete provider-native
tool-transaction groups are omitted atomically. The current task is supplied separately and the
full canonical turn record remains in History, with bounded tool evidence. The normal streamed tool loop
remains available, but the request-local Tier-1 transcript is never checkpointed back into the
canonical actor. It receives no writable shared core-memory blocks; any core and daily-log
and semantic state belongs to the set-scoped actor rather than the canonical Session actor;
all of that scoped memory is removed during Attempt cleanup. Ways call each configured primary
provider/model directly. They do not use provider fallback yet because retained lane cost and
identity must describe the route that actually ran. Skills, MCP
tools, and configured web search are also withheld because those external effects do not yet
have set-scoped rollback semantics; repository file, shell, and terminal tools remain
available inside the attempt container.

The set manifest, per-lane lifecycle, output, usage, and Route records are written to disk.
Checks verdicts and Judgment are persisted with the same set. Queued or running lanes found
without a live process after restart are reported as `interrupted`; completed, failed,
cancelled, and interrupted are terminal. Checks require every lane to be terminal and run
against each clone. A completed Checks run protects the exact checked candidate as Git objects;
Compare status and per-file diffs, Judge, and Keep all read that same identity without restarting
a lane or rerunning its approved setup. Before Checks, live changed paths remain available only
while the daemon still owns the lane runtime; after a restart they stay explicitly unavailable
until Checks can create protected evidence. Judge requires prior Checks, derives its candidates
from passing, non-empty changes, and validates the returned ranks and winner against those
survivors.

**Keep** requires a completed attempt with a passing Check and a non-empty change. It first
stops every attempt container and joins every lane task, then persists a resumable transaction:
`applying` records the selected lane, `applied` means its binary delta is present in the primary
working tree, and `transcript_recorded` means the transcript phase is complete and cleanup may
finish. That phase durably appends the original task and chosen answer exactly once to the
canonical single-agent transcript. A retry must select the same attempt. Keep does not merge or
commit, so the changes remain available for normal git review.

Cleanup removes only identities derived from the validated session, set, and attempt index.
If transcript recording or cleanup fails after apply, the set remains unresolved and retrying
the same Keep resumes from its durable phase. Discard is available before Keep begins: it stops
the actors and containers, joins tasks, removes the set's clones and protected refs, and clears
its artifacts and current pointer. Once Keep reaches `applying`, Discard is rejected so it
cannot erase the evidence needed to finish or diagnose the transaction.

Per-attempt usage records carry the model, provider, token counts, duration, price, and whether
that price is known. Ollama at a configured loopback endpoint has a known-zero model API
charge. A non-loopback Ollama endpoint follows the remote-provider rule: a configured model
price is used, while a missing price contributes only to a known subtotal and leaves
`actual_cost_known` false rather than being presented as free. Counterfactual cost has a
separate `baseline_cost_known` flag, and `all_local` requires both persisted Ollama provider
identities and a loopback-configured endpoint rather than merely a zero-dollar total. These
figures cover attempt execution. Plan first and Judge run through the selected autonomous
Agent—including its provider, model, system prompt, sampling limits, fallback, and per-run
budget—while applying only a call-local JSON response constraint (native where the provider
supports it). For these short schema-bound Ollama control calls, the same call-local override
requests `reasoning_effort: "none"`; ordinary Session, Automation, and tool turns omit that
override and preserve the model's default reasoning behavior. Meanwhile,
model preflight targets the selected provider/model directly. All three control operations report
usage separately from each Way. A failed, timed-out, or invalid Plan/Judge response carries its
known subtotal and completeness in the error response; timeout first requests cooperative
cancellation and waits for a bounded safe boundary. Successful Judge usage persists with the
unresolved Attempt set. Plan and model-preflight detail is request-local; the planning Agent's
cumulative usage remains in its checkpoint-backed Agent total.

## Automations

`AutomationStore` (`{data_dir}/automations.json`) is the single runtime source for
manual, scheduled, lattice-event, and Skill-triggered DAGs. When that canonical file
does not exist, legacy `workflows:`, `schedules:`, and `proactive:` YAML seeds it once.
An existing file remains authoritative even when the user has deleted every record;
later YAML changes do not replace or resurrect Automations.

One trigger runtime is started by both `axocoatl dev` and `axocoatl serve`. A single
timer reconciles every `Schedule` record against the live store; one lattice
subscriber matches `OnEvent` by canonical event type and `OnSkill` by exact
`produced_by = skill:<id>`. It checks the current record again immediately before
execution. Create, update, enable, cadence/event/Skill changes, and delete therefore
affect subsequent dispatch without per-Automation tasks or stale runners.

Event-triggered runs are single-flight. A cooldown begins at dispatch and is extended
at completion, bounding a loop even when an Automation fires a Skill that publishes
the event it consumes. Failures are recorded without terminating the shared dispatcher.
Compatibility schedule/proactive tables are rebuildable observation caches for last
run, count, outcome, and error; they never drive execution.

Provider and tool calls use an owned `AutomationExecutionContext`. The daemon,
Automation store, and observation locks are released before execution; only internally
synchronized run dependencies are cloned into the context. Manual API, compatibility
API, WebSocket, IPC, and CLI execution use the same boundary.

Run history persists node checkpoints, including per-activation Agent outputs, completed and
failed subjects, cumulative input/output/reasoning usage, a sticky completeness flag, and the
diagnostic text for a failed node. Agent usage is accumulated for top-level, Map, and nested
Subgraph activations. A failed provider call contributes its returned usage; a dispatched call
without terminal usage makes the subtotal incomplete instead of appearing free. Structural
failure after earlier Agent work carries the same measured subtotal to the live error boundary.
On bootstrap, a persisted `running` record with no executor in the new process is changed
durably to `failed` with an explicit restart reason; Axocoatl does not leave it looking
active or imply that arbitrary in-flight work resumed. A completed run's `final_content`
is the output of every executed runtime sink—an executed node with no activated edge to
another executed node—joined in Automation declaration order. This is deterministic for
disconnected or branched DAGs and includes terminal Tool, Map, and Subgraph results rather
than selecting only the last Agent.

A top-level `Interrupt` parks through one atomic run-store transition: the persisted
status becomes `interrupted` in the same file replacement that appends the
`interrupt_parked` checkpoint. Bootstrap scans those checkpoints and reconstructs the
pending operator prompts. Resume restores saved outputs and active edges, completes the
Interrupt, and continues without replaying completed nodes. New runs retain an immutable
Automation snapshot and submitted TextInput values; older run files can use the current
Automation after validating that the parked node is still an Interrupt.

This recovery boundary is the operator pause, not arbitrary in-flight Automation work.
A crash during a later provider or tool node does not reconstruct that call, and an
Interrupt inside a nested Subgraph remains process-local because nested execution does
not yet own an independent durable parent-continuation record.

## Agents

Each configured Agent is instantiated as a `ractor` actor. Autonomous Agents and
declared coordinator Workers run `DefaultAgentBehavior`; the separate Coordinator
pipeline is described under [Coordinator role](#coordinator-role). On a
`DefaultAgentBehavior` conversation turn:

1. Append input to **session memory** (Tier 1).
2. **Compact context** automatically when the session approaches the model's
   window. Older model-facing messages may be summarized; when a Tier-2 daily
   log is configured, Axocoatl first writes a bounded structured archive there.
   The canonical Session or Chat record remains the transcript authority.
3. Build the request, injecting the agent's **core-memory blocks** (Tier 3) and
   the top-k **semantic recall** (Tier 4) for the turn.
4. **Token budget** pre-flight (`abort` / `warn`) reserves locally estimated
   input plus the bounded completion before every provider call. `abort` stops a
   call that cannot fit and surfaces any provider-reported overrun immediately;
   provider tokenization/reporting can differ, so this is not an absolute remote
   billing guarantee.
5. Call the agent's **provider** (Ollama, OpenAI, Anthropic, …).
6. Run any **tool calls** (built-in or MCP) with hooks, up to 10 iterations.
7. **Checkpoint** the model-facing conversation and cumulative provider-usage subtotal—with a
   sticky completeness flag—to disk for actor restart recovery. Checkpointing is separate from
   the four memory tiers.

For normal workbench execution, this actor-owned state is the model-facing execution
conversation. The Session turn ledger separately owns the canonical user-visible request,
context snapshot, output, and lifecycle. Keeping those roles distinct allows multi-agent
outputs, cancelled turns, restart interruption, search, and export to remain legible without
pretending an actor checkpoint is an append-only product record.

An autonomous Agent or declared Worker curates its core-memory blocks (Tier 3) during the
conversation. Daily-log and semantic stores are derived recall aids, not lossless transcript
authorities. A Coordinator instead owns Tier-1 conversation plus live orchestration state; it
does not expose the Tier 2–4 memory loop in 1.0.

## Token budgets

Per-agent `token_budget` with `per_call`, `per_execution`, and an
`overflow_policy`:

- `abort` — refuse the over-budget call and return a budget error (the default)
- `warn` — log and continue past the budget

Before each provider call, Axocoatl makes a local reservation from the estimated
input plus the explicit or resolved bounded completion. With `abort`, a call is
not dispatched when that reservation cannot fit either limit. Provider-reported
usage, including provider-reported reasoning tokens, is recorded after a response; an overrun stops the turn immediately, but
those remote tokens may already have been incurred. Providers can tokenize
differently, misreport usage, or ignore an output limit, so this is a local token
guard rather than an absolute billing cap. Context compaction toward the model
window is automatic and independent of the guard. (`summarize` is accepted as a
deprecated alias for `warn`.)

Each activation also has a request-local measurement. Success, cooperative cancellation, and
provider failure carry their known usage to Session, Attempt, Automation, HTTP, IPC, CLI, and
Settings projections. If a dispatched call ends without a terminal response and without a Usage frame,
`token_usage_known` remains false across later calls and checkpoint restart; displayed numbers
are labeled as known subtotals rather than exact totals.

## Multi-agent sessions and event lattice

A session in `Lattice` mode uses the selected legacy `workflows:` record as an
agent-membership definition. The daemon spawns session-scoped actors in the
session's one sandbox and runs them in dependency order. The first agent receives
the instruction; later agents receive that instruction plus the outputs already
produced in the turn. `AgentActivated` and `TaskCompleted` frames are streamed
under the session id so the app can follow the run. This is a bounded session
execution path, not a background config-owned workflow runner.

`EventLattice` remains the typed event substrate. Skills publish into it; the
canonical Automation dispatcher matches `OnEvent` and `OnSkill`; configured
webhooks, the recent-events API, and WebSocket compatibility frames observe the
same feed. Agent pheromone
metadata and the reusable lattice primitives remain available to coordination
code and runnable examples, but the daemon does not consume activated agent ids
through a second workflow execution loop.

The remaining reads of legacy `workflows:` are intentional: Lattice-session
membership, coordinator worker/HTN selection, validation, and first-boot
Automation migration. Legacy `schedules:` and `proactive:` records are validation
and first-boot migration inputs only. None of these sections forms a parallel
manual, scheduled, or event-triggered runtime after `AutomationStore` exists.

## Coordinator role

Separately, an agent can take the **coordinator** role (`role: coordinator`)
for explicit hierarchical decomposition. Each coordination pass
(`CoordinatorBehavior`):

1. **Decompose** the goal into subtasks. With HTN methods configured, planning
   is symbolic — an `HtnPlanner` expands compound tasks via its methods and an
   `LlmFrontierResolver` resolves only the frontiers the methods don't cover.
   Without methods, the LLM decomposes the whole goal. Each subtask carries the
   tools it needs.
2. **Assign** each subtask to a worker by **auction** (`compute_bid` /
   `run_auction`) — best fit by tool-capability match and remaining token
   budget. If no pooled worker can cover a subtask's tools, an ad-hoc worker is
   spawned with exactly those tools, so a subtask is never forced onto an unfit
   worker.
3. **Delegate** the pending subtasks to workers **in parallel**. Each worker is
   a first-class agent with its own configured provider, model, tools, budget,
   sampling, hooks, and (for a normal actor-owned Session turn) scoped
   checkpoint/core/daily/semantic memory. Ad-hoc and supplied-history Workers
   remain run-scoped and ephemeral.
4. **Synthesize** the workers' outputs back into one answer to the original
   goal, accounting for any subtasks that failed.

Each Coordinator-owned provider call reserves output headroom against the exact
configured model window before dispatch. Under pressure it can omit only older,
completed User/plain-Assistant text at User boundaries. System messages, the
current request suffix, and attachments remain byte-for-byte protected, and the
canonical Session History is never rewritten. This is a request-local projection,
not the LLM summarization pipeline used by `DefaultAgentBehavior`.

The live pass checkpoints its plan and completed subtasks as internal
`OrchestrationState`. That state can protect the actor's live/internal recovery
boundary, but it does not cross a canonical terminal Session boundary: startup
marks an orphaned running turn Interrupted, and Completed, Cancelled, Failed, or
Interrupted projection clears private orchestration state. The next user turn
decomposes fresh. Workers are always torn down after a pass — on success and on
every error path — so no actor or task leaks, and a fully failed worker set
surfaces an error rather than a hollow result. The underlying primitives
(`axocoatl-coordination`: lattice, HTN, auction) are independently tested.

## Memory tiers

| Tier | What | Persistence |
|---|---|---|
| 1 — Session | live actor execution conversation | in-memory |
| 2 — Daily log | append-only activity by date | disk (JSONL) |
| 3 — Core memory | agent-edited curated blocks | disk (JSON; per-agent + shared) |
| 4 — Semantic | neural vector recall | disk (embeddings) |

Checkpoint snapshots are stored separately in a versioned Postcard envelope and pruned to the
latest three. A snapshot has a 64 MiB encoded envelope limit; canonical-ledger reconstruction
also caps projected messages at 8 MiB and keeps the newest complete turn segments that fit.
This bounds the cache without truncating canonical Session History. The private Bincode reader
exists only for the one-time 0.1.x transcript import; current checkpoint writes never use it.

**Transcript ownership.** For a normal Session, `SessionTurnStore` owns the canonical
user-visible turn history while actor session memory owns the model-facing Tier-1 execution
conversation and `CheckpointStore` caches it for recovery. Lightweight chats instead treat `ChatStore` as the
authority for Tier-1 history and execute each turn in `SuppliedHistory` mode from
that chat's stored transcript. This mode retains the full streaming and tool loop,
but it does not read, write, or checkpoint the configured actor's Tier-1 session.
The global compatibility actor's core and semantic memory remain shared across its lightweight
chats by design, so this separation protects verbatim transcripts; it is not a strict privacy
boundary. Normal workbench Sessions instantiate an autonomous configured Agent template under a
Session-scoped Tier 1–4 identity retained across actor restart. A Coordinator instead owns scoped
Tier-1 conversation plus orchestration checkpointing, while each declared Worker owns a scoped
Tier 1–4 identity below `{session}:{coordinator}:worker:{worker}`. Ad-hoc Workers are ephemeral.
A new Session using the same template starts with separate local memory; only core blocks
explicitly marked `shared: true` cross those scopes. Lightweight Chat and global FileStore routes
remain compatibility APIs, not browser destinations alongside the Session workbench.

Tier 4 runs a pure-Rust neural embedding model (`all-MiniLM-L6-v2`, 384-dim) on
Candle — the ~90 MB model is downloaded once, with a feature-hash fallback when
it's unavailable. No external service, no network at inference time.

**Recall is hybrid for autonomous Agents and declared Workers.** Each turn the top-k Tier-4
hits are injected passively (the baseline), and the Agent can also *pull* on demand with two
tools: `recall_search`
(semantic search over Tier 4) and `recall_timeframe` (read the Tier-2 daily log
for a date or range). A standing capability hint — plus a post-compaction note
pointing at the summary — tells the agent what's recallable, so the tools get
used instead of sitting idle. Passive injection, `top_k`, and the relevance
`min_score` are per-Agent (`memory.recall` in config); passive can be turned off
to go fully Agent-driven. The Coordinator provider loop owns Tier 1 and does not expose
Tier 2–4 memory/recall tools in 1.0; its declared Workers apply their own memory settings.

**Core memory is agent-managed.** Tier 3 is a small set of named, editable blocks
(`persona`, `human`, `project`, …) rendered into the system prompt every turn. The
agent curates them itself via `core_memory_append` / `core_memory_replace` /
`core_memory_set` as it learns durable facts (the MemGPT/Letta model — replacing
the old session-end fact extraction). Blocks are per-agent by default; a block
marked `shared` forms cross-agent team memory. This is the **curated top** of the
hierarchy — small and lossy by design. The canonical Session or Chat transcript remains
separate; Tier 2 (daily log) and Tier 4 (semantic) support recall without claiming exact raw
preservation. Configure the block set per agent under `memory.core`.

**Sleep-time consolidation.** When `consolidation.enabled` is true, a background
loop (`consolidation.rs`, mirroring supervision) periodically asks registered
**idle autonomous Agents** to consolidate. A supported autonomous behavior runs
an LLM "memory manager" pass — `on_consolidate`, triggered explicitly by an
`AgentMessage::Consolidate` — that reviews recent Tier-4 activity and **promotes
durable facts into the right core block**, merging duplicates and tightening wording
within the char limits. It is **promotion-only**: it reads Tier 4 and never evicts
it. The Agent itself decides whether it has been idle long enough (the pass runs
only past `idle_threshold_secs`), so a pass never fires between a user's two
messages. Declared Coordinator Workers are created inside a coordinator run rather
than polled by this registry loop, and Stop starts no provider or memory work. Tune
under `consolidation` (`enabled`, `idle_threshold_secs`, `interval_secs`).

## Protocols

- **MCP** — the daemon connects to configured `mcp_servers` (stdio or
  streamable-http) at bootstrap and exposes their tools to agents. Axocoatl is
  also an MCP **server**: `axocoatl mcp serve` runs over stdio and exposes each
  agent as an `agent_<id>` tool.
- **A2A** — agent-to-agent interop for cross-framework workflows, reachable over
  `GET /.well-known/agent.json` and `POST /a2a/tasks`.

MCP-qualified tool names remain the canonical executor, permission, evidence, and
transcript identity. Immediately before a provider request, Axocoatl maps names that
fall outside the common 64-byte ASCII function-name subset to deterministic reserved
aliases, applies the same map to replayed assistant calls and tool results, and reverses
streamed calls before hooks or dispatch. The request-local map is bijective and rejects
an alias collision rather than risking a call to the wrong tool.

Runnable examples: [`mcp-bridge`](../examples/mcp-bridge) (consume an MCP tool
over stdio, expose agents as an MCP server) and [`a2a-server`](../examples/a2a-server)
(publish an agent card and call it from a client, in-process).

## Security model

On the default Podman backend, a session runs the agent's repository file, shell,
and terminal tools inside a **rootless, daemonless Podman container**, not directly
on the host. The threat model is deliberately narrow, and stated plainly so you
know what it does and doesn't cover.

**What the sandbox contains — the blast radius of a mistaken or misbehaving
agent:**

- **Filesystem.** The session's working directory is the only host bind mount
  (`{dir}:{dir}:rw`). Nothing else of the host is visible — not your home
  directory, SSH keys, or sibling projects. A root Node project additionally
  receives a Podman-managed volume over `{dir}/node_modules`; it masks the host
  dependency tree rather than exposing another host path. If the canonical data root or
  external lease root is below the Workspace, an exact nested `tmpfs` masks that protected
  directory inside the container. A Workspace equal to or below either protected root is
  rejected; canonicalizing the Workspace before mounting prevents a symlink spelling from
  bypassing this test. A destructive command (`rm -rf`, a bad `git reset`) can still change
  the rest of the read-write Workspace and the container-owned dependency volume.
- **Privileges.** The container runs with `--security-opt=no-new-privileges` and
  drops the escape/recon capabilities (`SYS_ADMIN`, `SYS_PTRACE`, `NET_ADMIN`,
  `NET_RAW`, `DAC_READ_SEARCH`, …), so a setuid binary can't escalate and the
  classic namespace/mount escape levers are gone.
- **Network.** The default is bridged networking so installs and development
  servers work. Set `sandbox.network: none` when repository code and commands in
  the local container must have no outbound connection; this also disables
  network-dependent setup and commands in that container. It does not govern
  daemon-side model providers, MCP, web search, webhooks, remote sandboxes, the
  embedding-model download, or image-registry access. Configured Preview ports
  remain logical container-port identities; local Podman assigns each Session its
  own loopback host mapping, and the Session-aware proxy resolves that mapping
  without exposing arbitrary host services.
- **Resources.** Memory, CPU, and PID caps (2 GB / 2 CPUs / 512 pids) bound a
  runaway loop or fork bomb, where the host's cgroup delegation allows it.

**Environment readiness and consent.** A Session persists an environment generation and one
of `unprepared`, `awaiting_approval`, `preparing`, `ready`, or `failed`. Repository detection
may propose an image and setup command, but detection alone never grants execution consent.
`sandbox.allow_post_create_command` is an operator default for the exact devcontainer command
on an unreviewed Session; a reviewed per-Session choice overrides it, and detected `npm ci` is
outside that policy. The daemon
fsyncs `preparing` before starting the sandbox, runs only an exact approved command, then
fsyncs `ready` before publishing the sandbox to Files, Terminal, Preview, tools, or Ways. A
generation-bound guard owns the unpublished sandbox so cancellation, setup failure, or a
failed Ready write removes the container and its dependency volume before recording failure.
Each Attempt repeats the same approved setup inside its own isolated clone and volume.

The daemon's ordered WebSocket stream also owns the browser-side runtime boundary. Explicit
environment changes, Close/Delete, and a cold reconstruction that cannot use the live-sandbox
fast path publish `session-environment-changing` before destructive or state-changing awaits
and `session-environment-settled` on every exit. The active transition set is part of the
reconnect Snapshot. Every tab therefore suspends Files, Git, Preview, and task requests while
the daemon owns replacement, then re-reads the canonical Session rather than treating the
settled edge itself as a Ready claim.

Ways adds a Workspace-scoped owner because attempt setup quiesces every primary Session
runtime anchored to the repository, not only the Session that started the exploration. The
daemon publishes `workspace-attempt-changing` after acquiring the operation/start gates and
before teardown, retains the exact Workspace/Session/set identity while the durable current-set
pointer exists, and publishes `workspace-attempt-settled` only after that pointer is removed.
Reconnect snapshots merge the live pre-persistence owner with durable unresolved sets, including
after daemon restart. On an exact settlement, the owning Session tab re-reads durable Results,
canonical History, and current Git state before it restores the primary runtime; a delayed
settlement for an older set is ignored. A sibling Session tab therefore cannot keep issuing
runtime requests against primary state stopped or changed by another tab's Explore, Keep, or
Discard lifecycle, and a completed decision cannot leave its judged comparison visible.

On supported Unix hosts, bootstrap opens the configured data root once and retains that
directory capability for the process lifetime. Managed descendant traversal, reads, appends,
atomic replacements, and deletion stay relative to opened directory descriptors. Symlink
components and final symlinks fail closed; managed regular files with more than one hard link
are rejected. Atomic replacement uses an unpredictable same-directory create-new file, fsync,
and rename. The ambient path remains only for diagnostics, sandbox policy, and identity checks;
bootstrap and later runtime starts verify that it still resolves to the opened directory.

Exclusive ownership has three compatible layers acquired in fixed order: a per-canonical-root
lock in the owner-only external lease directory, the retained `.axocoatl-daemon.lock` used by
0.1.x, and a lock on the opened data-directory inode itself. This lets a live 0.1 daemon exclude
1.0 while a Workspace that could see the historical in-root file cannot admit a second 1.0
daemon by replacing it. The lease is held for a daemon or direct bootstrap lifetime and is
acquired before interrupted-runtime reconciliation, so a second CLI/MCP bootstrap cannot pause,
reconnect, or delete resources owned by the running daemon.

Before mutable Session or Workspace records are read, the upgrade preflight inspects
`axo-ses-*` Podman containers. It removes by immutable container id only a non-current container
whose inspected host bind overlaps the data root or external lease root, then verifies both
opened roots still have their original filesystem identity. Local cleanup derives Session
container names from validated Session filenames rather than trusting an embedded runtime id;
invalid records are quarantined only after that cleanup. A stopped Podman VM has no running
legacy process, and Axocoatl-managed containers have no restart policy; normal exact-name
startup removes a dormant predecessor before creating a current container.

Current agent-scoped persistence uses portable keys in explicit namespaces:
`checkpoints/v1/`, `memory/daily_log/v1/`, `memory/core/v1/`,
`memory/core/shared/v1/`, and `memory/semantic/v1/`. Current Automation runs live under
`automation/runs-v1/`; `runs/` is the 0.1 compatibility source. On Unix, a store may consult
only the exact bounded legacy component that its logical identity would have used. Checkpoint
and Automation run embedded identities and versions, plus semantic/core persisted shapes, are
validated before use; legacy paths are never a write target or deleted during promotion. New
writes go to the current namespace. This is a bounded 0.1 compatibility path, not a general
schema migration facility.

For a root Node project, local Podman masks the host's `node_modules` with a deterministic
Linux-local volume. The workspace itself remains a read-write bind mount, so an explicitly
approved command can still change repository files, including dependency directories in
nested projects. Podman startup never runs a host package manager or creates a VM: missing
host prerequisites fail with an explicit manual action. It may start an already-created,
stopped Podman VM.

Local image trust and runtime readiness are separate decisions. Alpine 3.20, Debian bookworm
slim, Ubuntu 24.04, Python 3.12 slim, Node 20 slim, and Rust bookworm are the exact curated
references accepted without `allow_untrusted_images`; common Docker Hub aliases canonicalize
to them. Startup probes the POSIX/Git command surface Axocoatl itself needs, attempts
distro-aware provisioning inside the container when commands are missing, and removes the
container if it still cannot satisfy the probe. With `sandbox.network: none`, the selected
local image must already contain those commands because provisioning cannot download them.

Files tree, read, and write operations resolve paths and perform I/O through the same Ready
sandbox handle as Git, agents, terminals, and Preview. They never substitute the host checkout
for an E2B clone or for container-local dependency volumes.

**What it does NOT solve — and we won't pretend otherwise:**

- **Prompt injection.** If the agent reads malicious instructions from a file, a
  web page, or tool output, the sandbox does not stop it from *acting* on them
  inside its workspace and its allowed network. Isolation bounds the blast
  radius; it is not a defense against an agent being talked into the wrong
  thing. Keep secrets out of the workspace and prefer `--network none` for
  untrusted inputs.
- **Host kernel / Podman bugs.** Container isolation is only as strong as the
  host kernel and Podman underneath it. A kernel-level container-escape CVE is
  outside our control.
- **What you explicitly grant.** Bridged networking, mounted credentials, or a
  permissive tool policy widen the surface — by your choice.

### Isolation backends (local-first by default; you choose the sandbox)

The sandbox is pluggable behind one trait, selected for this daemon configuration with
`sandbox.backend`. The repository tool layer is backend-agnostic.

- **`podman` (default).** The local, rootless container described above. Repository
  file, shell, and terminal tool execution stays on the machine, but the default
  bridged network permits container egress; use `sandbox.network: none` when that
  container traffic must be blocked. Daemon-side integrations use separate paths.
- **`e2b`.** A remote microVM backend targeting E2B Cloud for Axocoatl 1.0.
  Third-party E2B API implementations are not part of the 1.0 support scope. Use it when
  you want a normal session's repository tool execution to run off-box in throwaway,
  clean compute. It is opt-in; the default stays local. Parallel attempts currently reject
  this configured backend and require a single-agent session on local Podman so every
  attempt can receive an independent clone and container plus the same provider-safe projection
  of prior Session context.

  The backend and template are selected once in daemon configuration. E2B's create API does
  not accept a per-Session OCI image, so an explicit or devcontainer image is rejected before
  a VM is created instead of being replaced by the template. The template must already contain
  Axocoatl's required repository commands; startup verifies them and retains a failed
  environment record when they are absent rather than provisioning the remote template.

  A **git-repo** Session clones a clean, pushed branch over HTTPS. The git token
  (`sandbox.e2b.git_token`, e.g. `${GITHUB_TOKEN}`) is injected as a sandbox
  secret and read by an in-VM credential helper at fill-time — it is never
  written into the repo's Git config, remote URL, or a command line. Changes remain
  ordinary working-tree state in the remote sandbox. Axocoatl does not automatically
  commit or push; review, commit, and push deliberately through the Session's repository
  tools. A scratch Session (no repository) gets a fresh remote workspace.

  The exact remote sandbox ID, control-plane authority, data-plane domain, and
  working root are persisted before preparation completes. Once the environment
  is durably Ready, Close and graceful daemon shutdown pause that exact VM rather
  than deleting it. Reopen and process recovery reconnect to the same ID and root,
  preserving uncommitted or scratch work. A missing remote ID becomes an explicit
  failed environment; it never triggers a silent fresh clone. Delete Session and
  Change/Rebuild runtime perform the checked destructive teardown. Before a create
  request, Axocoatl also persists a unique Session-generation token and sends it as
  `axocoatl_creation_token` provider metadata. If the provider commits the VM but the
  response is lost, restart reconciliation discovers and deletes exact token matches.
  An ambiguous token with no provable provider result remains blocked; releasing it
  requires high-friction confirmation that every matching sandbox was deleted outside
  Axocoatl. Headless in-process CLI fallbacks still request and validate this
  reconciliation under the data-directory lease, but never reconnect Active Ready
  E2B Sessions; only the workbench daemon or an explicit Session action reconnects
  them. If the provider rejects a pause request, the Session retains Ready state,
  its exact identity, and an actionable error because Axocoatl cannot claim that VM
  is paused.

  Honest trade: with the remote backend, the repo (a committed ref) and a scoped
  token intentionally travel to the remote sandbox *you* chose. That is the cost
  of remote execution; it is opt-in. Podman keeps repository tool execution on the
  local machine, subject to its configured network policy. Providers and daemon-side
  integrations remain separate egress paths. See
  [`examples/configs/e2b-backend.yaml`](../examples/configs/e2b-backend.yaml).

Report security issues per [SECURITY.md](../SECURITY.md).

## Crate map

`axocoatl-core` (types) · `axocoatl-token` (budgets) · `axocoatl-llm*`
(providers) · `axocoatl-config` · `axocoatl-actor` (runtime) ·
`axocoatl-memory` · `axocoatl-coordination` (lattice/HTN/auction) ·
`axocoatl-graph` · `axocoatl-mcp` · `axocoatl-a2a` · `axocoatl-tools` ·
`axocoatl-isolation` (Podman sandbox) · `axocoatl-daemon` · `axocoatl-server` ·
`axocoatl-cli`.
