# Axocoatl product model

This document is the contributor-facing source of truth for what Axocoatl is as a product.
Implementation remains the final authority for what ships. `BRAND.md` defines how the
product speaks and looks; `ARCHITECTURE.md` explains the runtime beneath it.

## One product

Axocoatl is a local-first coding workbench backed by a durable multi-agent runtime. It is
not a collection of dashboards for runtime subsystems. The runtime, actor model, memory,
lattice, isolation, tools, MCP, and automation machinery are the engine. The browser app
at `/` is how a person uses that engine.

A folder-anchored session is the unit of work. Its conversation is the permanent spine.
Files/editor/Source Control, Preview, comparison, and agent graph open as focused tools;
Ways is a contextual inspector and Terminal remains in its bottom dock.

Installation and onboarding configure one Axocoatl product for the current OS user.
Onboarding never manufactures a project, repository, Workspace, or Session directory.
The app starts with no authorized Workspace until the person chooses **Open workspace…**.
The normal configuration and durable data live in platform user directories; a
project-local YAML configuration is an explicit operator override and is never selected
merely because it exists in the current working directory.

## The primary loop

1. Start Axocoatl and open the local app.
2. Resume the last session or open a project directory as a named Workspace, then start a
   Session inside it.
3. Add any files needed as **Once** or **Session** context, then ask for one solution; or
   turn on **Explore several ways** and choose the agent and model for each attempt.
4. Watch attempts execute in isolated working copies. While the decision remains unresolved,
   running, blocked, failed, cancelled, and complete states remain visible even when switching
   sessions.
5. Compare **Outcome** and **Route**, inspect the actual diff and cost, run the repository's
   **Checks**, and use **Judge** when useful.
6. **Keep this one**. The selected changes return to the session checkout and the other
   attempts are cleaned up.
7. Review **Last turn** through Git, stage or discard deliberately, and commit.

Ways evidence has the lifetime of the unresolved decision. During that decision, candidate
Routes, diffs, failures, Checks, cost, and optional Judge remain available after reload. Once
Keep finishes, durable Session History retains the selected task, output, and turn attribution;
cleanup removes the attempt set and its candidate review evidence. Finishing without keeping
removes the set without adding a selected result to History.

**Last turn** is an attribution filter over the repository's current Git status and diff. It
shows current changes on paths attributed to the latest durable turn, not a frozen per-turn
snapshot or a permanent copy of the kept candidate diff.

At any point, **Stop** addresses the exact active turn. **History** keeps completed,
failed, cancelled, and interrupted work reachable for search, export, or an explicit
rewind. These are Session actions, not alternate places to work.

This loop is the product differentiator. A single answer remains the simple path; parallel
attempts add confidence when the task merits them.

## Information architecture

- **Left rail:** the selected named Workspace and only the Sessions it owns. The Workspace
  switcher lists every authorized Workspace, including one with no open Session; its canonical
  path is secondary identity rather than its display name. A Session row may show live attempt
  state. The rail is not a list of features. Below 720 px it becomes an off-canvas sheet so the
  conversation keeps the full working width.
- **Main area:** the active Session conversation is the resting canvas. Transcript and
  composer share a readable centered measure; supporting tools do not shrink it into a pane.
- **Contextual right inspector:** planning, live roster, and cost for several ways. It opens
  only when Explore is being configured, watched, or explicitly requested, and overlays at
  compact widths instead of reserving empty space.
- **Bottom dock:** terminal.
- **Focused review:** Files/editor/Source Control, Preview, comparison, and agent graph replace
  the center stage temporarily with a visible return to Conversation. Wide-screen side-by-side
  pinning remains an explicit secondary action under More; compact widths always show one
  usable center surface.
- **Settings:** Agents, Skills, MCP servers, and Automations.

The app should resume the last active Session in the last selected Workspace. **Open
workspace…** authorizes and names a folder without manufacturing a Session. **New session**
is always scoped to the selected Workspace and never changes directories. A global **All
sessions** surface may switch both Workspace and Session, but must name that cross-Workspace
transition explicitly. The Finder-style folder browser remains a deliberate secondary action
for opening or locating a Workspace.

A Session does not become executable merely because its record exists. The selected runtime
image and any repository setup command pass through a visible environment review first. A
detected command such as `npm ci` is a proposal, never consent: the person must approve that
exact command or explicitly continue without it. Conversation Send, Files, Source Control,
Preview, Terminal, and Ways operations that start work or inspect a live checkout remain
gated until the durable environment state is **Ready**. Durable History, unresolved Attempt
evidence, and the exact Keep/Discard recovery path remain available so failure can be resolved.
Preparing and failed states stay visible with the exact command, bounded setup output, and a
path to review or rebuild. Session startup may start an existing Podman VM, but it must never
install host software or create a VM implicitly.

Under local Podman, the review accepts a fixed set of curated runtime references without the
arbitrary-image trust flag. Curated means accepted by policy, not guaranteed to contain every
project dependency: Axocoatl verifies its own repository command surface before Ready and fails
if the image cannot provide it. A root Node project masks host `node_modules` behind a
Session-owned Linux volume. Under E2B, the review shows the daemon-global template instead;
per-Session and devcontainer OCI images are rejected rather than silently replaced. A Ready
remote Session owns one durable provider identity and working root: Close pauses it, Reopen
and restart recovery reconnect to it, and a missing runtime fails visibly rather than silently
starting a fresh clone. Delete Session and Change/Rebuild runtime are the explicit destructive
transitions.

The daemon operator may configure the exact `devcontainer.json` post-create command to appear
approved by default. That policy is visible in the environment review and applies only to an
unreviewed Session; checking or unchecking the Session control is authoritative. It never
preapproves an independently detected command such as `npm ci` or an edited command.

There is one interactive page route: `/`. API routes can use internal names and remain
independently addressable; they do not become additional product shells.

## Session-owned conversation and context

The canonical user-visible conversation is a sequence of durable Session turns. A turn owns
the request text, stable identity, structured context references, per-agent output, lifecycle,
and error state. It is accepted before execution starts. Actor session memory and checkpoints
remain execution state and crash-recovery caches; they are not the product's only transcript
authority.

Files used as model context are attached from the composer. **Once** means the reference is
consumed after the next normal Session turn is durably accepted. **Session** keeps it selected
for later normal turns. Removing an attachment stops its future selection. If a turn already
used it, the inactive historical relation and blob pin remain so durable history can still open
the exact bytes. Uploads are bounded (10 MiB for declared
image types and 25 MiB for other documents), cached extracted or OCR text is capped at 256 KiB,
and the turn records an immutable context snapshot. Parallel Attempts do not currently receive
these attachments. Each receives the same task and isolated repository snapshot plus the same
provider-safe projection of prior Session context: prior User and plain Assistant text remain
ordered, while historical System and provider-native tool-transaction groups are omitted from
the Way's model-facing history. History retains the full canonical turn record, with bounded
tool evidence. Replaying an old
turn must preserve its attachment snapshot: the browser must not silently
resend only the request text when its attachment or structured composer context cannot be
reconstructed. Inline Retry is therefore unavailable for a context-bearing historical turn;
the person composes a new turn and reattaches the needed context. Rewind-to-edit restores the
request text but likewise does not reselect historical context automatically.

Stop is cooperative and exact. The request includes a durable turn id and a stop request must
match both the Session and active turn. Provider streaming may stop immediately. A tool that
has already begun—especially one with filesystem or external side effects—is allowed to finish
to a safe boundary; cancelled state does not claim that its effects were rolled back.

History can run case-insensitive literal text search over the current Session or all Sessions;
it is not semantic search. It exports one Session as Markdown or JSON. Rewind marks later
canonical turns superseded in the append-only ledger. It is a logical history operation, not
secure erasure. It currently requires a single-agent Session so the daemon can reconstruct that
actor's checkpoint from retained turns, and is blocked while a turn or unresolved Attempt set
owns the Session. It does not roll back tool, filesystem, or external effects. The ledger and
checkpoint are separate durable stores; checkpoint preparation and ledger commit use
compensation rather than one cross-store atomic write. A returned ledger error removes the
prepared checkpoint; if an uncatchable process death lands between the writes, bootstrap
converges the checkpoint from the authoritative ledger before serving again.

Turn history also records bounded tool start/result evidence before live broadcast. This keeps
Route evidence available after reconnect and in exports without allowing an unbounded tool
payload to become transcript state. Values above the canonical cap remain truncated audit
previews and are not replayed into later provider history.

The lightweight Chat, Chat attachment, and global FileStore APIs remain for compatibility
clients. They do not restore a directoryless Chat destination or a cross-chat Files browser.
The workbench behavior belongs to the Session chat spine.

A configured Agent is a template, not one memory identity shared by every Workspace. In a normal
Session, an autonomous Agent owns model-facing conversation, checkpoint, daily log, core memory,
and semantic memory under `{session}:{agent}` and retains them across actor restart. A Coordinator
owns scoped Tier-1 conversation plus its live orchestration checkpoint; each declared Worker owns
its own scoped Tier 1–4 identity beneath `{session}:{coordinator}:worker:{worker}`. Ad-hoc Workers
are run-scoped and ephemeral. Another Session using the same template starts with separate local
memory; only core blocks explicitly marked `shared: true` cross Agent or Session scopes. Attempt
memory is set-scoped and removed with the Attempt runtime. A terminal Completed, Cancelled,
Failed, or Interrupted Coordinator turn never auto-resumes private orchestration state; the next
turn decomposes fresh.

## Product language

| Product term | Meaning |
|---|---|
| Workspace | A durable, user-named identity for one authorized project directory; the path remains visible as secondary context. |
| Session | Persistent work and conversation owned by one Workspace and anchored to its directory. |
| Session environment | The reviewed runtime image and optional exact setup command that must become Ready before repository tools run. |
| Attempt / way | One candidate solution produced in parallel with others. |
| Outcome | What changed and whether the result passed checks. |
| Route | The observable path an attempt took: tool calls, files, commands, failures, and normalized trajectory. |
| Keep | Select one attempt and return its changes to the session checkout. |

Use internal words such as variant, lane, fan-out, worktree, branch, adopt, and discard in
code and APIs. On the product surface, prefer plain language. Git implementation details
must remain easy to reveal because they make the work inspectable and trustworthy.

## Runtime relationship

The one app does not replace Axocoatl's runtime strengths. It makes them legible:

- Actors provide durable agent execution and supervision.
- Memory and checkpoints let work survive beyond one request or process lifetime.
- The Session turn ledger preserves the canonical user-visible request, context, outcome, and
  lifecycle independently of the actor checkpoint cache.
- Session isolation bounds repository file, shell, and terminal tool execution to the chosen workspace.
- Heterogeneous providers let each attempt use a different local or remote model.
- Multi-agent session mode and the coordinator are explicit work paths; the
  event lattice carries typed notifications for Skills, triggers, webhooks, and
  retained API/WebSocket observers.
- MCP, Skills, and Automations extend what sessions and agents can do.

These are capabilities of one product. They should not compete as peer navigation
destinations.

## Definition of shipped

A feature is shipped in the app only when a person can discover it from the session,
complete it end to end, receive honest error and reconnect states, and return to the
session without changing products.

The following are necessary but not sufficient:

- a Rust type or store exists;
- a server endpoint exists;
- a custom element is defined or imported;
- a hidden legacy panel still contains the old controls;
- a unit test passes below the product seam.

Before deleting an older surface, inventory its capabilities and prove that each one is
reachable in `/`, or record an explicit product decision to remove it. Visual fusion without
runtime, state, and lifecycle fusion is not one product.

## Non-goals

- A generic consumer assistant or hosted account-first SaaS.
- A collection of framework primitives that requires users to assemble their own product.
- A second app for comparison, agents, automations, or the lattice.
- Hiding failures, git state, cost, or model choice to make the interface look simpler.
- Replacing the local-first default with a vendor-controlled runtime or data path.

Axocoatl can remain serious developer infrastructure and still have a first-class workbench.
The app is the operational face of the infrastructure, not a departure from it.
