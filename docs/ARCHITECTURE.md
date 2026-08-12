# Axocoatl Architecture

A practical overview of how Axocoatl's one workbench runs and coordinates agents.

## The big picture

```
            ┌─────────────────────────── axocoatl daemon ───────────────────────────┐
 App / CLI  │  ProviderRegistry   AgentRegistry   EventLattice   McpToolRegistry     │
 HTTP / WS ─┼─▶ (per-agent LLMs)  (ractor actors) (skills/events)  (MCP tools)         │
    / IPC   │        │                 │                │                            │
            │        └──────── DefaultAgentBehavior ─────┘                            │
            │            session mem → budget → LLM → tools → checkpoint              │
            └────────────────────────────────────────────────────────────────────────┘
```

The **daemon** (`axocoatl-daemon`) bootstraps everything: providers, agents
(spawned as `ractor` actors), the event lattice, MCP connections, and the
canonical Automation trigger runtime. Both `axocoatl dev` and `axocoatl serve`
expose the Unix-socket IPC server and HTTP/browser app from the same daemon
state; `serve` is also what the installed background service runs.

## Product surface

The browser app at `/` is the operational face of the runtime and the only supported
interactive browser route. A workspace is an authorized project directory; a session is
persistent work and chat anchored to that directory. The session chat stays in the main
area while files, editor, browser, activity, attempt comparison, git, terminal, and agent
graph open around it. Agents, Skills, MCP servers, and Automations are configured through
Settings.

A request can execute as one session turn or as several isolated attempts with different
agents and models. Attempts are checked and compared, one result is kept, and the resulting
changes return to the session checkout for git review. See [PRODUCT.md](PRODUCT.md) for the
interaction and terminology contract.

The current runtime still contains separately evolved chat, directory-session,
Automation, and attempt execution paths. A visually unified route does not make
those state models identical. Changes at this seam must verify run identity,
transcript ownership, reconnect, cancellation, persistence, and cleanup end to
end.

`AutomationStore` is the canonical persisted configuration for manual, scheduled, lattice-
event, and Skill-triggered automations. Legacy workflow/schedule/proactive YAML seeds the store
only when its canonical file does not exist; it is not a parallel live registry. One dispatcher
reconciles store changes and lattice notifications for both `dev` and `serve`, prevents
overlapping automatic runs of the same automation, and records last outcome/count/error in
compatibility views. It clones an owned
execution context before provider/tool work, so neither the store nor the daemon state lock is
held across a run.

## Attempt ownership and lifecycle

A session may own one unresolved `AttemptSet` at a time. The set records its UUID,
original task, effective instruction, snapshot commit/tree, resolved agent/provider/model for
every lane, and creation time. Starting another set or sending another session turn conflicts
until the current set is kept or discarded. This keeps one decision loop attached to one turn
in the chat spine.

Parallel attempts currently require a single-agent session on the local Podman backend. E2B
and multi-agent modes remain available for normal session turns, but the daemon rejects an
attempt-set start there until it can give every attempt the same filesystem, transcript, and
cleanup guarantees.

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
clones through its repository tools. Every lane receives the same snapshot of the canonical
single-agent session transcript through `SuppliedHistory`. The normal streamed tool loop
remains available, but the request-local Tier-1 transcript is never checkpointed back into the
canonical actor. It receives no writable shared core-memory blocks; any core and daily-log
state belongs to the set-scoped actor rather than the canonical session actor. Skills, MCP
tools, and configured web search are also withheld because those external effects do not yet
have set-scoped rollback semantics; repository file, shell, and terminal tools remain
available inside the attempt container.

The set manifest, per-lane lifecycle, output, usage, and Route records are written to disk.
Checks verdicts and Judgment are persisted with the same set. Queued or running lanes found
without a live process after restart are reported as `interrupted`; completed, failed,
cancelled, and interrupted are terminal. Checks require every lane to be terminal and run
against each clone. Judge requires prior Checks, derives its candidates from passing,
non-empty changes, and validates the returned ranks and winner against those survivors.

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
that price is known. Ollama is explicitly known-zero. An unconfigured remote price contributes
only to a known subtotal and leaves `actual_cost_known` false; it is not presented as free.
Counterfactual cost has a separate `baseline_cost_known` flag, and `all_local` is derived from
the persisted provider identities rather than a zero-dollar total. These figures cover attempt
execution; optional Plan first and Judge provider calls are reported as outside that total.

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

Run history persists node checkpoints, including the diagnostic text for a failed node.
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

Each agent is a `ractor` actor running `DefaultAgentBehavior`. On an actor-owned
conversation turn:

1. Append input to **session memory** (Tier 1).
2. **Compact context** automatically when the session approaches the model's
   window — old turns are summarized (raw archived to the Tier-2 daily log, so
   nothing is lost) instead of being dropped.
3. Build the request, injecting the agent's **core-memory blocks** (Tier 3) and
   the top-k **semantic recall** (Tier 4) for the turn.
4. **Token budget** pre-flight check (`abort` / `warn`) — the spend cap.
5. Call the agent's **provider** (Ollama, OpenAI, Anthropic, …).
6. Run any **tool calls** (built-in or MCP) with hooks, up to 10 iterations.
7. **Checkpoint** the session to disk for crash recovery. Checkpointing is separate
   from the four memory tiers.

The agent curates its core-memory blocks (Tier 3) during the conversation; the
lossless raw is always preserved in Tiers 2 and 4.

## Token budgets

Per-agent `token_budget` with `per_call`, `per_execution`, and an
`overflow_policy`:

- `abort` — refuse the over-budget call and return a budget error (the default)
- `warn` — log and continue past the budget

Budgets are checked **before** the LLM call, so an over-budget request never
costs tokens. The `overflow_policy` is purely the **spend cap** — context
compaction toward the model window is automatic and independent of it.
(`summarize` is accepted as a deprecated alias for `warn`.)

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
   a first-class agent — its own tools, checkpoints, core + semantic
   memory, and hooks — with a run-scoped actor name so repeated runs never
   collide.
4. **Synthesize** the workers' outputs back into one answer to the original
   goal, accounting for any subtasks that failed.

The pass is **resumable**: the plan and each completed subtask are checkpointed
(`OrchestrationState`), so a crash mid-run resumes where it left off instead of
re-doing finished work. Workers are always torn down after a pass — on success
and on every error path — so no actor or task leaks, and a fully failed worker
set surfaces an error rather than a hollow result. The underlying primitives
(`axocoatl-coordination`: lattice, HTN, auction) are independently tested.

## Memory tiers

| Tier | What | Persistence |
|---|---|---|
| 1 — Session | conversation transcript | in-memory |
| 2 — Daily log | append-only activity by date | disk (JSONL) |
| 3 — Core memory | agent-edited curated blocks | disk (JSON; per-agent + shared) |
| 4 — Semantic | neural vector recall | disk (embeddings) |

Checkpoint snapshots are stored separately and pruned to the latest three.

**Transcript ownership.** The actor's session memory owns Tier-1 history for a
normal agent conversation. Lightweight chats instead treat `ChatStore` as the
authority for Tier-1 history and execute each turn in `SuppliedHistory` mode from
that chat's stored transcript. This mode retains the full streaming and tool loop,
but it does not read, write, or checkpoint the configured actor's Tier-1 session.
The configured agent's core and semantic memory remain shared across its chats by
design, so this separation protects verbatim transcripts; it is not a strict
privacy boundary.

Tier 4 runs a pure-Rust neural embedding model (`all-MiniLM-L6-v2`, 384-dim) on
Candle — the ~90 MB model is downloaded once, with a feature-hash fallback when
it's unavailable. No external service, no network at inference time.

**Recall is hybrid.** Each turn the top-k Tier-4 hits are injected passively (the
baseline), and the agent can also *pull* on demand with two tools: `recall_search`
(semantic search over Tier 4) and `recall_timeframe` (read the Tier-2 daily log
for a date or range). A standing capability hint — plus a post-compaction note
pointing at the summary — tells the agent what's recallable, so the tools get
used instead of sitting idle. Passive injection, `top_k`, and the relevance
`min_score` are per-agent (`memory.recall` in config); passive can be turned off
to go fully agent-driven.

**Core memory is agent-managed.** Tier 3 is a small set of named, editable blocks
(`persona`, `human`, `project`, …) rendered into the system prompt every turn. The
agent curates them itself via `core_memory_append` / `core_memory_replace` /
`core_memory_set` as it learns durable facts (the MemGPT/Letta model — replacing
the old session-end fact extraction). Blocks are per-agent by default; a block
marked `shared` forms cross-agent team memory. This is the **curated top** of the
hierarchy — small and lossy by design, safe to rewrite because the lossless raw
stays in Tier 2 (daily log) and Tier 4 (semantic). Configure the block set per
agent under `memory.core`.

**Sleep-time consolidation.** A background loop (`consolidation.rs`, mirroring
supervision) periodically asks **idle** agents to consolidate. Each agent runs an
LLM "memory manager" pass — `on_consolidate`, triggered by an
`AgentMessage::Consolidate` and once more on graceful stop — that reviews recent
Tier-4 activity and **promotes durable facts into the right core block**, merging
duplicates and tightening wording within the char limits. It is **promotion-only**:
it reads Tier 4 and never evicts it. The agent itself decides whether it has been
idle long enough (the pass runs only past `idle_threshold_secs`), so a pass never
fires between a user's two messages. Tune under `consolidation` (`enabled`,
`idle_threshold_secs`, `interval_secs`).

## Protocols

- **MCP** — the daemon connects to configured `mcp_servers` (stdio or
  streamable-http) at bootstrap and exposes their tools to agents. Axocoatl is
  also an MCP **server**: `axocoatl mcp serve` runs over stdio and exposes each
  agent as an `agent_<id>` tool.
- **A2A** — agent-to-agent interop for cross-framework workflows, reachable over
  `GET /.well-known/agent.json` and `POST /a2a/tasks`.

Runnable examples: [`mcp-bridge`](../examples/mcp-bridge) (consume an MCP tool
over stdio, expose agents as an MCP server) and [`a2a-server`](../examples/a2a-server)
(publish an agent card and call it from a client, in-process).

## Security model

On the default Podman backend, a session runs the agent's tools inside a
**rootless, daemonless Podman container**, not directly on the host. The threat
model is deliberately narrow, and stated plainly so you know what it does and
doesn't cover.

**What the sandbox contains — the blast radius of a mistaken or misbehaving
agent:**

- **Filesystem.** Only the session's working directory is bind-mounted into the
  container (`{dir}:{dir}:rw`). Nothing else of the host is visible — not your
  home directory, SSH keys, or sibling projects. A destructive command
  (`rm -rf`, a bad `git reset`) can only reach that one directory.
- **Privileges.** The container runs with `--security-opt=no-new-privileges` and
  drops the escape/recon capabilities (`SYS_ADMIN`, `SYS_PTRACE`, `NET_ADMIN`,
  `NET_RAW`, `DAC_READ_SEARCH`, …), so a setuid binary can't escalate and the
  classic namespace/mount escape levers are gone.
- **Network.** The default is bridged networking so installs and development
  servers work. Set `sandbox.network: none` for an untrusted workspace that must
  have no outbound connection; this also disables network-dependent setup and
  tools in that session.
- **Resources.** Memory, CPU, and PID caps (2 GB / 2 CPUs / 512 pids) bound a
  runaway loop or fork bomb, where the host's cgroup delegation allows it.

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
`sandbox.backend`. The tools never know which backend they run on.

- **`podman` (default).** The local, rootless container described above. Tool
  execution stays on the machine, but its default bridged network permits
  outbound traffic; use `sandbox.network: none` when that traffic must be blocked.
- **`e2b`.** A remote, E2B-compatible microVM — E2B cloud **or a self-hosted
  CubeSandbox on your own cluster**. Use it when you want a normal session's tool
  execution to run off-box in throwaway, clean compute. It is opt-in; the default stays
  local. Parallel attempts currently reject this configured backend and require a
  single-agent session on local Podman so every attempt can receive an independent clone,
  container, and canonical transcript snapshot.

  A **git-repo** session is reproduced *git-natively*: the microVM `git clone`s
  the repo from a clean, committed branch over https. The git token
  (`sandbox.e2b.git_token`, e.g. `${GITHUB_TOKEN}`) is injected as a sandbox
  secret and read by an in-VM credential helper at fill-time — it is never
  written into the repo's git config, the remote URL, or a command line. The
  agent commits and pushes a branch; you review it through the normal git/PR
  flow. A scratch session (no repo) just gets a fresh remote workspace.

  Honest trade: with the remote backend, the repo (a committed ref) and a scoped
  token intentionally travel to the remote sandbox *you* chose. That is the cost
  of remote execution; it is opt-in, and the default (`podman`) keeps everything
  local. See [`examples/configs/e2b-backend.yaml`](../examples/configs/e2b-backend.yaml).

Report security issues per [SECURITY.md](../SECURITY.md).

## Crate map

`axocoatl-core` (types) · `axocoatl-token` (budgets) · `axocoatl-llm*`
(providers) · `axocoatl-config` · `axocoatl-actor` (runtime) ·
`axocoatl-memory` · `axocoatl-coordination` (lattice/HTN/auction) ·
`axocoatl-graph` · `axocoatl-mcp` · `axocoatl-a2a` · `axocoatl-tools` ·
`axocoatl-isolation` (Podman sandbox) · `axocoatl-daemon` · `axocoatl-server` ·
`axocoatl-cli`.
