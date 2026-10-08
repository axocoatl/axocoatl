# Axocoatl Architecture

A practical overview of how Axocoatl's one workbench runs and coordinates agents.

## The big picture

```
            ┌─────────────────────────── axocoatl daemon ───────────────────────────┐
 App / CLI  │  ProviderRegistry   AgentRegistry   EventFeed      McpToolRegistry     │
 HTTP / WS ─┼─▶ (per-agent LLMs)  (ractor actors) (skills/events)  (MCP tools)         │
    / IPC   │        │                 │                │                            │
            │        └──────── DefaultAgentBehavior ─────┘                            │
            │       turn ledger → session mem → budget → LLM → tools → checkpoint     │
            └────────────────────────────────────────────────────────────────────────┘
```

The **daemon** (`axocoatl-daemon`) bootstraps everything: providers, agents
(spawned as `ractor` actors), the event feed, MCP connections, and the
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

`AutomationStore` is the canonical persisted configuration for manual, scheduled, event-
and Skill-triggered automations. Legacy workflow/schedule/proactive YAML seeds the store
only when its canonical file does not exist; it is not a parallel live registry. One dispatcher
reconciles store changes and event-feed notifications for both `dev` and `serve`, prevents
overlapping automatic runs of the same automation, and records last outcome/count/error in
compatibility views. It clones an owned
execution context before provider/tool work, so neither the store nor the daemon state lock is
held across a run.

## Session turn ownership and control

New data roots use the native Session execution controller. Native repository tools and
checks use the local Podman process supervisor. E2B's compatibility executor does not
implement this owned process-supervision interface. Existing legacy roots retain
compatibility execution; startup does not silently convert them. The explicit offline
`axocoatl session upgrade --confirm` command holds the normal data-root leases, reconciles
old execution, and prepares and completes source-bound conversion without starting actors
or providers. Unknown historical roles retain raw checkpoint archives and accounting with
empty future model context. Ordinary Send, dependent
Agents, Coordinator children, and native Ways use canonical admission and the shared authority
boundary. A Session team's required checks are conditions of each admitted turn graph:
after the turn's required Agents are accepted, the host runs each command between two
repository captures of that exact candidate, charged to the grant of the first required
Agent whose own profile may use `bash` (a lead's helper profiles never count), and
records one readiness review. The turn completes only when every check passes and the
captured tree did not change; a failure leaves it needing attention. The review records
why it failed in words, and it also fails when an accepted activation was accepted after
the Before capture's intent, so work admitted during a pass cannot complete unchecked.
The required reviewer's activation is the exception: it is read-only, starts only after
readiness passed, and its verdict binds only to the tree its own captures saw, so
accepting it leaves readiness current.
A Continue that selects any condition of the group, or restarts any Agent, reruns the
whole group: readiness needs every command on the one tree its captures saw. A pass
starts only when the paying grant can pay for all of it; otherwise the readiness review
records, in words, why the checks could not run, and a check-only Continue that could
not be paid is refused.

A Session team's required review adds one optional reviewer node, `required-review`,
with its own grant from the same Apply and a fresh conversation per turn, and one review
condition, `required-review:verdict`, over the required Agents. The one turn driver
starts each round itself after the required Agents are accepted and the check readiness
passed: a reviewer activation whose input is the request, a bounded prompt with each
required Agent's accepted answer and the change between the turn's first Before capture
and the candidate tree, and the round. It records the verdict only from a reviewer
accepted in the current epoch whose input names that exact prompt, and only an
`APPROVE` whose own captures, when it has them, saw the same tree passes. The verdict is
the answer's `VERDICT:` line, wherever it is; none, or lines that disagree, is
unreadable. An unreadable answer about the tree it was shown records nothing while a
round remains and the host has not yet asked again: the next round revises the reviewer
in the same epoch with the same prompt, a host note and its previous answer as revision
context. `CHANGES`
with rounds left pauses the epoch and continues in a new one whose plan revises the
single required sink with the findings, as a person's Revise does, and reruns every
condition; the preview of those events must apply first, or the turn needs attention
with the reason.

`turn_contract` decodes and folds a bounded schema-2 logical-turn contract separately from
the live schema-1 ledger. Immutable manifests bind definitions, conversations, starting
savepoints, exact accepted parents, repository references, budgets, and grant revisions.
An initial graph declares distinct team slots and conversations, acyclic dependencies,
required nodes, and scoped check/review conditions. Continuation covers every declared node,
including work that never started, and atomically prepares selected generations as Unstarted.
Accepted revisions explicitly supersede every affected materialized descendant; rebasing must
select current accepted parents. Completion requires current accepted required nodes, fresh
passing condition observations, and settled effects. References still require physical
evidence resolution and executor validation; the fold does not dispatch work.

`execution_store::SessionExecutionStore` persists this v2 history under a retained upgraded
format guard and a separate Session writer lock. Its journal owns at most one unfinished
turn, verifies linked predecessors against closed history, and returns the original receipt
for exact command repeats. Reopening durably interrupts a Running epoch before exposing the
projection. Admission reserves command and byte room within the turn's own bounds (4,096
commands and 8 MiB of retained events) for bounded activation/tool settlement, condition
observations, interruption, and closure. Those bounds can reject new work in a turn; nothing
bounds how many turns or records a Session keeps, and retained history is never evicted. Opaque durable snapshots bind downstream storage to
the canonical journal, Workspace, and Session; an ordinary in-memory fold cannot mint them.
Retained user request references commit atomically with Begin. An immutable legacy history
frontier can be sealed before the first v2 turn, whose projection retains the legacy predecessor.

`execution_legacy` captures only the existing v1 journal beneath that same held data root.
Its opaque snapshot records the source inodes, format ownership, byte length, and digest;
it never creates a missing journal or repairs a partial tail. Capture bounds the shared source
and the selected Session separately. Retention and the first canonical seal revalidate the
source. A changed source requires a fresh immutable candidate; an acknowledged seal cannot be
replaced. An exact seal retry returns the original receipt even if the old source later changes.
The host must retire in-process legacy writers before conversion. Source checks do not prove
the absence of checkpoint-only or private actor state.

`segment_log` holds the records of every per-Session journal that grows with the Session's
work, beside a small head file per journal: the canonical turn journal, execution content,
the invocation audit, activation state, Session team revisions and Ways decisions (whose
record and patch bodies are files named by their SHA-256). Records are appended to an active
segment as one synced JSON line each. The network record uses the same layout with its own
writer, which acknowledges a complete write and syncs every 200 ms so that egress decisions
do not wait on a sync. When it reaches its record or byte bound the segment
is sealed: its bytes and a seal line with the record count and SHA-256 digest are published
under `segments/`, and a new active segment starts whose header names that digest, so the
chain of segments cannot be reordered, removed or changed unnoticed. Opening verifies every
seal and link, removes a torn last line (a write that never finished, so never
acknowledged), and completes a seal a crash interrupted. A reader without the writer lock
reads the same files and never writes.

Memory holds the active segment, a few decoded sealed segments, a per-segment summary, and
for stores looked up by key a small key filter per sealed segment; older records are read
back by key or range on demand. Per-turn and per-record bounds stay as they were. The head
file carries a `segments` marker that an older daemon's strict parser refuses, so a migrated
Session cannot be opened by a release that would misread it. A single-file journal written
before segmentation is validated with its old bounds and converted on first open: its records
are appended to a fresh log and the head replaces the file last, so a crash during conversion
leaves the old file, and the next open converts it again. The canonical journal is the first
of a Session's journals to open, so before converting it `segment_backup` copies the whole
Session directory to `backups/before-segments/<key>/session` under the data root and writes
`backup.json` last; a complete copy is never replaced, a partial one is taken again, and a
copy that cannot be made stops the conversion.

`execution_namespace` provisions typed component roots under the canonical Session writer.
Each component and its descendants retain the format guard, Session lock, and component lock.
Content, activation state, invocation audit, authority, and command stores have owned openers
that bind their files to the exact canonical journal. A matching Session name alone is insufficient.

`execution_content` retains bounded request text/context, typed activation input evidence,
partial/final output with explicit usage completeness, and exact tool arguments. Tool admission
reserves result space; oversized returns retain their observed success/failure, full digest and
length, and an explicit bounded prefix. The prefix cannot establish replayable result bytes.
Read projections preserve logical state, accepted output references, and unavailable evidence.
Activation admission also reserves bounded partial and terminal output slots. A truncated
final body remains evidence and cannot become an accepted complete answer.

`invocation_audit` retains durable intent and authoritative outcome evidence independently of
logical closure. Late outcomes do not reopen a turn or replace accepted output. Protected
argument references are distinct from display previews; host code must verify and resolve
the actual post-hook bytes. Opaque intent receipts prove persistence, never replay safety.
The journal keeps every invocation for the life of the Session in segments and evicts
nothing; only unresolved invocations are held in memory. A tool call still needs room in
its turn: an intent and a settlement in the canonical turn, and claims in the turn's
control authority (at most 4,096). The host reports that room (less one After capture per
running activation that observes its repository) as the Agent's `tool_calls` allowance,
declines an Agent's call it has no room for before anything is written, and records a
capture it cannot admit as unavailable. A full turn therefore ends each Agent's tool loop
with a final answer instead of fencing the controller, and the next turn starts with fresh
room.

`control_authority` persists grant policies and their prior revisions, exact generation
registration, revocation, Stop, cumulative budget reservations, and dispatch claims. A fresh
live scope and opaque generation lease bind preparation to an unresolved intent in its exact
audit. Stop, revocation, and the final durable dispatch claim share one mutex; external
execution occurs after its release. Reopening closes old generation gates and preserves
charges. Settlement matches the exact audit identity and immutable intent, can occur after
dispatch closure, and does not refund a tool claim's conservative reservation. Provider-specific
claims share that same gate and budget. They bind the exact request digest/length and
executor-enforced spend/response limits, reserve terminal metadata, and retain observed usage
or an unknown subtotal independently of accepted conversation state. A terminal response with
complete usage settles the call's reservation to that usage (and its cost to a known observed
cost): the call records what stays charged and the rest returns to the grant. An interrupted
call, an incomplete measurement, an unknown cost, or a claim with no recorded outcome keeps
the whole reservation. Reload re-derives every grant's usage from these records exactly;
calls settled by earlier builds carry no settlement and stay fully charged. Older tool-only
registrations cannot establish provider accounting coverage. Historical inspection requires
an existing matching authority with closed gates and establishes durability without
rewriting its records.
Authenticated Team & budget Apply retains the full edit and exact selected-template
provenance. The native host resolves captured definitions, provider profiles, and repository
identity before dispatch. Coordinator child allocations reserve from the parent's aggregate
allowance; child authority is distinct rather than a copy of the parent's grant. The parent
holds a child's full limits while any child activation runs. Once none runs (completion, Stop,
or the reopen after a crash), the parent is charged only the child's own usage, so the unused
part returns; a later activation of that child must reserve its full limits again and is
refused when the parent can no longer hold them. Both follow from the stored activation state,
so recovery cannot return a reservation twice. Children reserved by earlier builds stay
reserved in full.

Native provider preparation supports reviewed local Ollama profiles and bounded
OpenRouter-credit profiles. OpenRouter retains one exact static model and qualified
endpoint variant, its text/tool capabilities, context/output limits, the catalog's
reasoning contract for a reasoning model, and two decimal price ceilings: the highest
input rate (prompt, cache read, cache write, every tier) and the highest output rate
(completion, internal reasoning, every tier), plus any per-request fee. Priced features a
native request cannot start (web search, image and audio) are retained by name. Those
metadata are revalidated before each request; the wire request pins the endpoint (never
a service-tier endpoint), disables fallbacks, the web plugin and context compression,
sends the Agent's resolved `reasoning` setting, and supplies `max_tokens` and prompt,
completion and request price limits. Each call reserves a bound of its own request:
the body's bytes plus a template allowance and any replayed reasoning tokens for the
prompt, and the output limit plus the effort's reasoning allowance for the response, at
those rates. Admission refuses an output and effort whose sum passes the endpoint's
output limit or the per-call response byte bound, rather than clamping it. Streaming retains terminal measured tokens (reasoning as
output) and billed cost independently of accepted output and settles the reservation to
them; usage beyond the reservation is a contract breach that keeps the spend; incomplete
responses keep the reservation and unknown usage. Reasoning blocks of a tool-calling
response ride on its first tool call and are sent back unmodified with the tool results.
The normal API key stays in daemon configuration, never retained profile evidence.
`providers.openrouter_billing: credits` explicitly declares an account without
connected BYOK keys. This is a supported configuration requirement, not detection or
prevention of external account changes. BYOK execution is not supported; an unexpected
BYOK response is refused and cannot establish complete credit accounting.

`control_command` separates typed requested parameters from trusted source attribution and
persists Requested, Accepted, Applied, and final receipts. It reserves the bounded remaining
lifecycle before acknowledging a request. Agent attribution is minted from a current grant
and activation lease; human attribution is a host authentication boundary, not a JSON tag.
Source evidence does not authorize the operation. The internal daemon controller joins human
Stop, Retry, Revise, Continue and Finish receipts to exact canonical transitions;
delegated controls require an actual live Agent source, an approved operation, and exact
parent/descendant scope. Human graph edits and Agent child admission share graph validation;
Agent requests cannot manufacture human attribution. Accepted remains
pending until the corresponding safe boundary or new generation is durable. A repeated command
returns its original receipt, and reconstruction repairs evidence without replaying execution.

Memory's `activation_state` binds immutable input manifests and checkpoint bytes to exact
activation and conversation identities. It takes durable Session snapshots and promotes only
current accepted generations from immutable closed history. A durable promotion manifest
precedes conversation pointer changes; recovery completes that decision before restore.
Newer failed/superseded candidates cannot replace selected accepted state. An owned reservation
binds one bounded immutable candidate to the exact current input before provider work. Unrelated
writes cannot consume its metadata/record capacity; late diagnostic staging cannot accept it.
Normal v1 checkpoint behavior remains unchanged, and Tier 2–4 speculative writes remain disabled.
Its original strict plain-history importer remains available. An additional versioned ordinary
autonomous-Agent policy shares the live v1 restart projection in `legacy_conversation`: inline
code/DOM context, complete native tool-call groups, terminal status filtering, and bounded
checkpoint construction. It records omitted/superseded/cancelled rows, truncation, tool replay
policy, and all incurred usage separately from retained model context. A supported projection
that filters every row has an explicit committed empty checkpoint with provenance. Unsupported
coordinated/private state, mixed Agents, attempts, unresolved attachment blobs, and unknown
metadata fail closed. Import requires the current canonical seal and no v2 events. The host
still has to establish the historical Agent's role and account for state absent from the ledger;
this does not enable automatic actor migration or restore.

`execution_ownership` acquires the supported legacy external-file, in-root-file, and directory
locks before durably preparing and atomically exchanging a versioned directory with the old
mandatory lock pathname. The original lock inode is retained. Unsupported atomic exchange,
ambiguous manifests, and incomplete shapes fail closed. Reopening validates both sides and
repairs durability before returning a non-Clone format-ownership guard. Existing storage
ancestors must already be durably provisioned. This proves format ownership, not settlement of
orphaned containers or external work. Only the canonical execution store can provision its
internal Session directories through this guard. `DataRootFormatOwnership` acquires and detects
the root format while holding the external lease; consuming legacy-to-upgraded handoff retains
the original lock descriptors throughout. Live bootstrap now uses this format-aware acquisition.
It selects the matching native or legacy startup path before runtime reconciliation. Native
startup retains canonical ownership and interrupts an orphaned running epoch; restarting the
daemon does not authorize replay of uncertain effects. Existing roots are not automatically
converted.

The actor's optional `ToolExecutionBoundary` awaits admission before each backend invocation,
including behavior-owned memory tools, and awaits raw outcome persistence before post-hooks.
Boundary failure is terminal across controlled Coordinator children; already started calls are
joined before return, while unresolved outcomes remain unknown. Children require distinct host
provisioning rather than inherited authority.
Unsupported custom behaviors refuse the optional boundary.

Native `DefaultAgentBehavior` derives its tool-round ceiling from the reviewed
grant's invocation allowance, lowered by the Agent template's `max_tool_rounds` when it
sets one, and held to 1–1,024 rounds (`axocoatl_core::MAX_TOOL_ROUNDS`). Every round
spends at least one invocation, so the grant normally runs out before the ceiling.
Compatibility construction retains the default ten-round ceiling. This is a finite loop
guard, not a reservation or permission: provider and tool calls still pass their
individual durable admission boundaries. An activation that reaches the ceiling while the
model still asks for tools fails with `AgentError::ToolRoundLimit`; its pending calls do
not run, and the failure is classed `round_limit` with Continue as the next step.

The daemon's explicit `session_dispatch` adapter joins the owned stores for that tool boundary.
It verifies exact current activation, physical input, profile, and grant; reserves protected
content; persists canonical intent then audit intent; and claims authority before execution.
Stop closes authority before cooperative cancellation. Late outcomes survive closed turns in
the audit, and reopening reconciles known evidence without automatically replaying effects.

A native lead's `delegate` call retains a replay policy that names the helper node and the
command that admits it. If the raw return is lost, reopening reads it back from the command
journal and the helper's canonical outcome: an accepted helper yields its answer, and a
refused, failed, or stopped one yields a tool error. The evidence is labeled reconciliation.
No helper or provider call is repeated. Reopening first ends an admitting command left
requested or accepted without a canonical node, then reads the return again, so one reopen
records that no helper ran. A repeat of a call whose command admitted no helper is a new
admission attempt with its own command and node ids; the first attempt keeps the original
ids. Ordinary shell/tool effects remain `ManualOnly`.

`delegate` is a concurrency-safe tool, so a lead's `delegate` calls in one response run
their helpers at the same time. The controller admits them one after another: each reads
the turn and graph revisions, checks the lead's follow-up reserve, reserves the helper's
limits, and applies its graph revision under the same controller lock, so each admission
builds on the one before it and identical calls reattach to one helper.

Its one-shot autonomous actor port additionally reserves the actual candidate checkpoint and
terminal output before any provider dispatch. The optional `ActivationCheckpointPort` restores
only the captured savepoint, excludes legacy latest-file lookup, keeps durable memory read-only,
and stages the native actor's complete candidate once. Periodic checkpoints stay in memory;
Stop during final staging cannot escape as successful completion. The host replaces estimated
actor usage with recorded provider observations across the conversation's exact generations
and earlier closed turns, plus its immutable legacy baseline once. Missing accounting coverage
refuses execution. Failed candidates remain diagnostic state.

The provider wrapper covers both streaming and nonstreaming calls, including compaction, within
the existing actor loop. Every call requires a backend-enforced whole-request bound; approximate
token counts and output-token request parameters cannot mint this capability. Response payloads,
including reasoning, tools and native metadata, are bounded. Completion is withheld until
accounting settles. Dropped calls retain incomplete observed usage and their conservative reserved
charge. An enforced zero provider API charge remains known zero even when token usage is
incomplete; a positive reservation does not establish the actual monetary cost. Nonstreaming
`AccountedChatOutcome` and streaming `UsageObservation` preserve reported subtotals independently
of response decoding and explicitly distinguish complete measurements from lower bounds.
Complete output and the
actual candidate must both persist, and the exact generation must still be current and unstopped,
before canonical acceptance. Completion then leaves its dispatch gate closed.

`NativeOllamaProvider` supplies an explicit bounded local capability alongside the existing
compatible adapter. Its profile validates the configured local server's audited Ollama 0.20.6
version, reported cloud-disabled mode, and local GGUF completion model before inference.
Native `/api/chat` requests set finite context/prediction limits and disable implicit history
truncation and shifting. Plain calls reserve context plus prediction capacity; JSON calls reserve
two full passes because the server may perform a separate thinking pass. The final structured
response reports only the last pass, so its token measurement remains incomplete. Neither this
profile nor its zero provider API charge describes hardware/electricity cost or establishes a
wall-clock GPU execution ceiling. A plain call's terminal counts settle its reservation to
them. Cancellation closes the request; absent terminal evidence, usage remains incomplete
and the reservation remains charged.

The autonomous port projects retained text evidence into the actual user message appended by
the native actor, so provider input and successful checkpoints share the same user content. Rich
inputs carry versioned, bounded text with ordered guidance references, captured code/browser
selections, retained attachment text and provenance, exact accepted direct-parent output
selections, and explicitly superseded revision context. It does not load another node's
conversation or reopen a captured path/URL. Missing or conflicting text evidence, unsupported
binary inputs, and aggregate input overflow refuse preparation rather than silently omitting
context. Physical starting and parent checkpoints still require validation before execution.
Provider errors preserve the last complete conversation prefix in the diagnostic checkpoint,
with incurred usage retained separately; failed input remains in its immutable manifest.

The internal autonomous driver schedules child tasks from the canonical graph while the same
controller continues accepting exact controls. Independent branches can finish when a sibling
fails; dependent work uses only current accepted parent outputs. Explicit revision supersedes
affected descendants, and the driver prepares their next generation from the retained input and
new accepted parents. A person's Continue that restarts failed work selects never-started
work that depends on it, directly or through other such work, as awaiting its dependencies
when every other parent is accepted, so the driver starts it once the restarted work is
accepted; work behind a parent left blocked stays blocked in that epoch. A lost driver
interrupts its epoch and drains owned tasks for late evidence;
reconstruction requires explicit continuation. An activation stopped before binding receives a
durable never-dispatched record, not an inference from missing accounting. Prepared generations
that never started are identified from canonical history even after supersession. Definitive
grant or budget refusal is local to the activation; uncertain persistence fences the controller.

Closure reserves and completes selective conversation promotion, including empty turns. A
consuming successor handoff requires predecessor handles to be released, validates committed
savepoint bytes and preserves canonical format ownership. Normal Finish waits through dependency
scheduling and requires declared completion conditions; it cannot turn an absent QA result into
a pass. Human-only `ForcePartial` retains the confirmed accepted-output selection, exact
running activations to stop, never-started work, and missing completion conditions. It uses the
same closing owner, waits for safe settlement, and promotes only selected accepted sinks into
successor conversation state; partial output and incurred usage remain audit evidence. Agents
cannot authorize this override, and isolated Ways retain their existing Keep/no-Keep flow.

Live bootstrap uses this port for approved native autonomous execution, with repository identity
and approved checks bound to the same authority boundary. The native factory also runs approved
Coordinator children through the existing controller and distinct bounded grants. Ordinary
compatible/hosted adapters that lack the enforced-bound capability are refused by this port.
The existing tool-only host adapter remains separate and does not establish provider
accounting coverage.

`session_history` is a versioned read facade. Legacy projections preserve ledger order, literal
search, visibility, and transcript behavior. Upgraded projections require the canonical seal,
read only its retained legacy frontier, and append typed v2 entries in Begin order. Lifecycle,
unknown usage, and unavailable evidence remain explicit; compatibility projections refuse v2
entries instead of flattening them into v1 rows. Existing daemon History, search, export, and
message reads use the format-aware facade. Browser projections retain exact activation
identity and expose controls only through current host capabilities. Pending owned stores can
resolve exact command retries before provider or repository reacquisition.

The existing Session composer and Agent inspector expose native Guide, exact generation
controls, reviewed current-turn Add/Replace, and explicit continuation. Team edits configure
future turns separately. Environment review, close/reopen, and deletion retain the canonical
Session owner through settlement.

### Legacy Session execution

`SessionTurnStore` is the canonical user-visible transcript for legacy Session execution. Its
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

The single-agent rewind projection spans two durable stores but is not one atomic database
transaction. The daemon prepares a new checkpoint, commits the append-only ledger boundary, and
removes the prepared checkpoint if the ledger append returns an error. A `SIGKILL` can interrupt
between those writes; the next bootstrap treats the ledger as authoritative and deterministically
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
backend. Native attempts require explicit per-attempt grant limits, expiry, output bounds,
and configured decision-history retention limits. Each attempt has a canonical activation and
its own repository owner while retaining the existing Ways runner and Keep transaction. E2B, coordinator Sessions, and other multi-agent modes remain available for normal
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

For native Sessions, `WaysDecisionStore` retains a separate bounded decision record and
protected patch bodies under the canonical Session namespace. Keep and no-Keep reserve and
retain the decision before runtime cleanup. The record includes all candidate Outcomes,
Routes, review diffs, Checks, usage, Judge evidence, human choice, and cleanup receipts, with
explicit unavailable/truncated fields. Retention has caller-configured limits, no silent
eviction, and blocks cleanup on an unsatisfied storage promise. History reads/export/context
capture do not reopen candidate runtimes. The existing graph surface renders one decision
with expandable set/index-keyed candidate subgraphs from the live attempt owner or retained
record. It has no Keep endpoint of its own; unresolved navigation uses existing Compare,
while closed records are read-only. Explicit deletion keeps a tombstone and preserves
patch pins still referenced by another decision. Legacy decisions without this record remain
unavailable rather than being reconstructed from current repository state.

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
unresolved Attempt set. Native decision records retain validated shared Plan/model-preflight
usage once, separately from candidate usage; legacy planning accounting retains its existing
checkpoint-backed Agent total.

## Loadouts and headless runs

A loadout (`axocoatl.loadout/1`, parsed and validated in `axocoatl-config::loadout`) is a
versioned YAML file that declares one run: Agents with roles (`writer`, `explorer`,
`planner`, `worker`, `integrator`), models or model parameters and runtimes (`native`,
`claude-code`, `codex`), required checks (`argv`, `shell`, `detected` or `e2e`, each with
a timeout of at most 30 minutes, 3 by default), an optional required review (1 to 3
rounds, read-only tools, adjudication on by default), `egress` and `routes`, budgets with a
wall clock, the prompt (`{task}` required), an environment (image or recipes, and the one
setup command a run approves) and the `qa` or `audit` settings of those kinds. Unknown
fields are refused, a file is at most 64 KiB, and the kind's shape is validated (`fix`:
one writer, a review, at least one check; `qa`: one explorer whose only write scope is its
reproduction directory; `audit`: one planner, one worker template and one integrator, all
read-only). A reviewer on a writer's model is a `same_model_reviewer` warning, never a
refusal.

Built-in loadouts (`fix`, `qa`, `audit`) are compiled in with `include_str!`. User
loadouts are read on each listing and each admission from `loadouts/` beside the
configuration file the daemon started with: at most 128 regular files of at most 64 KiB,
no symbolic links, a reused built-in id listed with its error. Each loadout's digest is the
SHA-256 of its bytes. The lattice in Settings displays a loadout's graph read-only; it has
no execution path for loadouts.

**Admission.** `POST /api/runs` (from `axocoatl run`) resolves parameters (unknown names
refused, missing required ones a usage error), canonicalizes the repository and finds or
creates its Workspace, creates a native Session with `Session.loadout =
SessionLoadoutBinding {run_id, loadout, network, workload}`, prepares its environment with
exactly the loadout's setup or `--setup` approved, and writes the run manifest with the
repository HEAD and the paths that were dirty. It is idempotent on `request_id`.

**Per-Session sandbox.** Every place the daemon reads the network mode or the workload plan
for a Session goes through `session_sandbox_policy`: an unbound Session gets the global
values unchanged; a bound one gets `egress` (or `none`), `WorkloadPlan::Hardened {required:
true}` and the daemon's egress lists plus the loadout's `egress`, `routes` and the routes
its external Agents and e2e checks need. A host that cannot provide that (rootful Podman,
no egress, E2B) refuses the run with an infrastructure error; nothing falls back.

**Driving.** The server spawns the run's driver with a `RunHost` over the daemon, so each
kind's driver (`FixDriver`, `QaDriver`, `AuditDriver`, `SingleTurnDriver` for `custom`) is
testable with a fake host. `team_plan::team_edit` turns the resolved loadout into an
ordinary `SessionTeamEdit`: one slot per Agent with `reset_history`, limits from the
budgets (`cost_microunits = cost_usd × 10⁶`) and an expiry at the wall clock, inline Agent
definitions retained as the slot's definition evidence, `required_checks` with
index-aligned `check_options` (name, timeout, report), the required review synthesized as
a read-only Worker, and dependencies. It goes through the existing preview and apply path,
so every 1.2 grant, write-scope and readiness rule applies. The driver sends the turn,
waits for a terminal state or the deadline (then stops it and records the budget), and
observes nodes, generations, check views and review proofs. An audit runs its turns in
one Session: plan; parallel read-only area workers with fresh contexts, each told the
files the host listed and assigned to its area; up to two follow-ups naming the files a
worker did not read, judged from the `read_file` calls its Session recorded; integrate.

**Outcome.** `RunOutcome` (`axocoatl.run-outcome/1`) adds to a turn's view: check results
with parsed reports, the review rounds with findings split by id, adjudications, findings
with their reproduction classification, not-covered entries with a failure class
(`provider_refusal`, `provider_failure`, `provider_rejected`, `budget`, `blocked`,
`not_reached`, `runtime_limit`, `stopped`, `other`), notes, warnings, usage with retry counts, a
network summary and Keep. `RunOutcome::decide` sets the verdict and exit code with the
precedence error (5) > interrupted (6) > checks failed (1) > needs attention (2) > pass
(0); anything not covered, a check that did not run on the final result, an unanswered
finding, an unpassed review, an exhausted budget or a finding the loadout fails on is
attention. Nothing is a pass by default.

**Record.** `{data root}/loadout-runs/{run_id}/` holds `manifest.json` (written once, with
the loadout's exact text), `events.jsonl` (append-only, synced, at most 100,000 events of
at most 256 KiB) and `outcome.json` (written once), through `SecureDir`. Runs are never
evicted and outlive their Session. A daemon restart during a run marks it failed; it is not
resumed. The run's Session loads again after the restart: its `Custom` mode lists no Agent
(the run applies the loadout's team), which startup validation accepts with its `loadout`
binding. `render_junit` writes the JUnit view (not covered is always a failure, fails on
clean build is skipped); `render_refused_run_junit` writes the one `axocoatl run --junit`
leaves for a run that was never admitted. The record bundle (`axocoatl.record-bundle/1`) streams the
manifest, loadout, Outcome, Session, team (with each applied slot's `reset_history`, tools
and definition), every turn's control-plane projection, the versioned History export,
every network-record event and every run event as JSON Lines, ending with the line count
and the SHA-256 of every preceding byte; `verify_bundle` checks order, count and digest.
Its header carries the run's `finished_at_ms`, so every download of a finished run is the
same bytes while its record and Session do not change. Routes never record credential
values, so the bundle holds none.

**Fix.** Every required review, in a loadout run or not, asks the reviewer to number its
findings (`F1`, `F2`, …) and asks the lead to answer every finding in an `ADJUDICATIONS`
block when the host sends findings back. In a `fix` run (with `review.adjudicate`, the
default), `review_adjudication::adjudicate` pairs each round sent back with the writer
generation that answered it; a finding without an answer, or answered without a reason, is
`missing`.

**QA.** The explorer reports `FINDINGS` and `COVERAGE` blocks. For each finding the host
runs its reproduction with `browser_check` against the target and, when configured, the
reference URL, and `qa_repro::classify` decides `confirmed` (fails on target, passes on
reference), `fails_on_clean_build`, `reproduced` (no reference), `not_reproduced`,
`repro_error` or `missing`. Areas reported `not_reached` or `blocked`, every area left when
the explorer failed, or the whole app without a coverage report are not covered.

**External agents.** For a writer whose retained definition has an external runtime, the
native activation path hands off to `run_external_activation`: the activation is admitted,
granted and captured like a native writer, and the program runs through the in-sandbox
supervisor as the hardened writer user under `--harden`, with the activation's timeout and
stdout bounded to 16 MiB. Its JSON output becomes the activation's evidence and answer.
Model traffic goes through routes from `external_agent::routes_for` with credentials from
`credentials` or the secret store (`{data root}/secrets/<name>`, `0600`, written from stdin
by `axocoatl secret set`); the container holds placeholders and trusts the Session CA. The
activation reserves its grant's limits up front, route requests count against its
invocations, and the program's own usage report settles it: Claude Code reports its cost;
Codex reports tokens only, and its cost is computed from them at the configuration's
`pricing` entry or the pinned list price of its model (`external_agent::models`; the run's
`usage.cost_computed` says so). A Codex model with no price keeps its cost unknown: its
activation reserves the cost left divided by the activations left, that stays charged, and
its run's `usage.cost_known` is false; there is no per-call reservation, and the
program's internal tool calls are evidence, not admitted calls.

**e2e.** An `e2e` check expands to a wrapper that forces `E2E_TELEMETRY_DISABLED=1`, sets
the model and the route-backed key placeholder, runs `e2e run|explore --reporter json`
with the report written to `/tmp/axocoatl-check-reports/<name>/report.json`, prints the
report's SHA-256 as its last line and exits with e2e's status. Its check definition
carries `egress: true`, so under `network: egress` that check's process alone gets its
own egress credential until it settles; other required checks have no network. Admission
checks an OpenRouter model's tool-call and image-input capabilities in OpenRouter's
public catalog, a request the host makes outside the Session's network record. The Session container mounts `.e2e/cache` read-only. After the
final turn, `collect_reports` reads each report (at most 4 MiB) and attaches it only when
its digest matches the marker of the final recorded check run.

**Keep as PR.** For a passing run, host `git` commits exactly the run's attributed paths
through a temporary `GIT_INDEX_FILE` seeded from HEAD (`update-index`, `write-tree`,
`commit-tree -p HEAD`, `update-ref` of a new branch), refusing paths that were dirty before the run, paths
whose status changed after the run ended (by inode change time) and an existing branch;
HEAD, the index, the current branch and the working tree are unchanged.
Opt-in, it pushes without `--force` to a new remote branch that is not the default branch
and opens a pull request with `gh`, whose body carries checks, review, adjudications,
findings, not-covered entries, warnings and the run id.

**Runtime policy.** Native provider calls that fail with 429, 5xx, a timeout or a reset
connection before the provider sent anything are retried once (after `Retry-After`, at most 30 s, or 2 s) on the same pinned
model, as a new call with its own reservation; 400 to 403, refusals and safety stops are
not retried. A failed helper, slot or area keeps a classifiable failure so the Outcome
lists it as not covered. `check_options[i].timeout_ms` (1 s to 30 min, 180 s by default)
reaches the admitted check definition and the condition permission's bound; 1.2 graphs
still verify. `sandbox.egress.host_ollama` is an opt-in route from Session containers to a
loopback Ollama port through `ollama.host.axocoatl.internal`, ended with the Session CA
and recorded like any route.

## Automations

`AutomationStore` (`{data_dir}/automations.json`) is the single runtime source for
manual, scheduled, event- and Skill-triggered DAGs. When that canonical file
does not exist, legacy `workflows:`, `schedules:`, and `proactive:` YAML seeds it once.
An existing file remains authoritative even when the user has deleted every record;
later YAML changes do not replace or resurrect Automations.

One trigger runtime is started by both `axocoatl dev` and `axocoatl serve`. A single
timer reconciles every `Schedule` record against the live store; one event-feed
subscriber matches `OnEvent` by canonical event name and `OnSkill` by exact
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
4. **Actor token-budget guard** (`abort` / `warn`) reserves locally estimated
   input plus the bounded completion before every provider call. `abort` stops a
   call that cannot fit and surfaces any provider-reported overrun immediately;
   provider tokenization/reporting can differ, so this is not an absolute remote
   billing guarantee. Native Session execution additionally requires the canonical
   grant reservation and enforced provider bounds described below.
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

Native Session execution reserves each provider call against its exact current grant
before dispatch. Admission requires an enforced whole-request token and provider-charge
bound from the selected adapter; an estimated prompt size or requested output limit is
insufficient. Unknown usage keeps its conservative reservation charged. The native local
Ollama profile described above supplies this capability; compatible adapters without it
cannot enter this execution path. These provider API bounds do not cover electricity,
hardware, or arbitrary external effects.

The existing per-Agent `token_budget` API remains a separate actor guard, with `per_call`,
`per_execution`, and an `overflow_policy`. It is the compatibility execution path's token
guard and can further constrain a native call; it cannot weaken a native grant:

- `abort` — refuse the over-budget call and return a budget error (the default)
- `warn` — log and continue past this actor guard; native authority still applies

Before each provider call, Axocoatl makes a local reservation from the estimated
input plus the explicit or resolved bounded completion, including the reasoning a
provider allows on top of the output (`LlmProvider::response_tokens`). With `abort`, a
call is not dispatched when that reservation cannot fit either limit. Provider-reported
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

## Reliability for local models

Native activations use the mechanisms below for short context windows, streams that end
early and provider failures. They describe what the host does, not a measured quality
claim about any model.

- **Invocation reserve.** An activation that can run commands in a repository keeps a
  reserve of invocations for the host's observations: its After capture and, on the grant
  that pays for required checks, two passes of one shared Before capture, each check and
  one shared After capture (the turn's pass and one Continue), less the check runs the
  turn already paid for and never less than one pass. A tool call must also leave room
  for the provider call that reads it and one more, so a model whose next tool round is
  declined can still answer. Calls of one response are counted together before any
  pre-hook runs, and a call that no longer fits at admission is declined as a tool error
  rather than failing the activation. Before a lead admits a helper, what the lead has
  left after the helper's reservation must still cover reading the helper's answer and
  the lead's own reserve, so a paying lead cannot delegate its check allowance away.
  Helpers requested together are checked one after another, each after the reservations
  of the ones before it.
  Apply and turn admission refuse a paying Agent whose invocation limit is smaller than
  that allowance plus its own two captures and one answer.
- **Project instructions.** Every native activation with a repository, whether a lead, a
  helper or the required reviewer, is given the `AXOCOATL.md` at the root of its checkout
  (at most 64 KiB) in its system prompt, as the compatibility path does. The host reads it
  from the Way's clone or the Session's Workspace after the activation's Before capture;
  when that capture lists the file, only the exact bytes it recorded are used, so the
  instructions never differ from the retained capture. The system prompt is rebuilt for
  each request and is not part of the checkpoint.
- **Bounded context.** A request fits when it and its answer allowance fit the model's
  window less a 1/32 margin, counted as the provider counts once a call has shown its
  tokenizer counts more than the local count. Tool output, and long string arguments of
  the model's own earlier calls (such as a whole-file write), are replaced with a
  placeholder in later requests only as far as needed to keep the conversation at 60% of
  what fits (and at most 32,768 tokens whole), oldest first, never beyond the latest three
  to five tool rounds, and moving in steps of three so the request prefix stays stable.
  If a request would still not fit, only the latest round stays whole and shorter output
  is elided too; as a last resort the Agent's oldest tool rounds (each call with its
  results) are left out of the request with a one-line note, enough to be back at that
  60%. They stay out for the rest of the activation, so later requests keep fitting with
  the same prefix until new work overflows it. A person's messages and the Agent's
  answers to earlier turns always stay, and Session History keeps the full content.
  `workspace_knowledge` results are never masked; a helper's `delegate` answer stays
  whole until a request would not fit. When the Agent's own conversation (its checkpoint)
  grows past 85% of the window, the next turn first shrinks the earlier turns only as far
  as needed, least useful first: long tool output (`workspace_knowledge` results too) and
  arguments are elided, then tool rounds are left out (each request and final answer
  stays, with a note of how many rounds went), and only then do the oldest turns go,
  never the most recent completed one.
- **Answering at the end of a budget.** When what is left of the Agent's token guard (once
  it has spent some) or of its grant (tokens, invocations less the host's reserve, or
  spending) cannot pay for another tool round and an answer, its next request goes
  without tools and asks for the final answer. A limit that still stops it is named in
  plain words in the failure.
- **Answering when a tool loop stops making progress.** When the Agent's last three tool
  rounds only printed text (`bash` commands made of `echo`, `printf`, `true`, `:`, `cd`
  or `pwd`, with no redirection, pipe or substitution), or its last four rounds repeated
  the same calls and got the same results, its next request goes without tools and asks
  for the final answer, the same way. A different round in between (an edit before the
  same test runs again, another file read) starts the count again, polling a terminal
  never counts, and new guidance from a person resets it.
- **One retry for a broken stream.** A provider stream that ends early (for Ollama also
  one ended by an error record, such as an unparseable tool call) is retried once. Its
  estimated input and the output it had already streamed are charged to the same grant
  before the retry, and the retry is checked against what is left.
- **Answering when the turn can record no more tool calls.** The Session keeps every
  record; when the turn has no room for another tool call, the Agent's next request goes
  without tools and asks for the final answer, the same way.
- **Failure classes.** A failed activation states its failure class (provider stream,
  budget, tool-round limit, context limit, write scope, capture, admission) and a
  suggested next step.

## Multi-agent sessions and the event feed

Native Sessions retain their approved whole-team revision and immutable definitions before
Begin. Their common controller activates exact dependencies, records every generation, and
admits Coordinator-created Worker instances from explicitly approved reusable templates. Each
instance has a distinct conversation and authority allocation. The versioned
`GET /api/sessions/{id}/turns/{turn_id}/control-plane` projection binds reads of that graph
to the exact Session and turn. It distinguishes unknown, missing, unavailable, and unrecorded
evidence; current Agent settings do not substitute for an absent historical definition.

A Session on a 1.0-format data root retains either a selected legacy `workflows:` ID for
`Lattice` mode or selected Agent IDs for `Custom` mode and resolves that selection against
current Agent configuration at each all-team request. Legacy execution runs one Agent per
turn: a single-Agent Session, a request targeted at one Agent, a one-Agent team, and a
Coordinator-led team (which runs only its Coordinator) execute directly. A request that
resolves to two or more Agents is refused before durable Begin; the error tells the operator
to stop Axocoatl, make a cold backup, and run `axocoatl session upgrade --confirm`, which
converts 1.0 Sessions, including multi-Agent Sessions, to native Sessions. Removing or
renaming a referenced Agent or team does not quarantine the Session or hide its History; a
genuinely new turn is rejected before durable Begin and tells the operator to restore the
reference or create a new Session with an available selection. Retained legacy History still
loads and renders each Agent's recorded output. The control-plane projection also reads
legacy turns, and legacy activation identities confer no per-Agent command authority.

Every legacy Lattice or Custom Session turn, whether targeted, one-Agent, or Coordinator-led,
uses a two-phase checkpoint cache boundary. A SingleAgent
Session also uses this boundary when its selected Agent is a Coordinator; ordinary autonomous
SingleAgent Sessions retain their existing canonical-ledger checkpoint repair. A Completed
ordinary turn may keep its live actor for conversation continuity. After Failed, Cancelled, or
Interrupted, both an exact retry and the next new turn must first prove any retained actor stopped;
the replacement is then rebuilt from canonical History rather than reusing ahead-of-ledger state.
Once canonical
Begin is fsynced, the daemon creates a turn manifest and gives every transaction-owned actor a scoped
store. Autonomous Agents, the Coordinator, and its declared Workers can read their own
latest staged generation during that turn, but unscoped readers see only the prior committed
checkpoint. After the canonical terminal transition is durable, `Completed` promotes the latest
checkpoint for each participating identity. `Failed`, `Cancelled`, and `Interrupted` instead
publish an accounting-only successor: the prior committed transcript remains, private behavior
or orchestration state is cleared, and the newest staged cumulative usage and completeness are
retained. Only then are the scoped actors stopped and live turn ownership released.

A pending commit validates the complete staged Agent namespace and identity set, admits checkpoint
candidates only when their filenames exactly match the canonical zero-padded name emitted by the
checkpoint store, and decodes the
selected checkpoint for each Agent before publishing the `Committing` manifest state. Malformed
staging therefore fails closed while the manifest remains `Pending` and retryable; no actor may
restore from that unresolved transaction.

Bootstrap first changes orphaned `running` turns to `interrupted`, then resolves every unfinished
checkpoint manifest from that exact ledger status before any Session actor can spawn. Commit and
abort phases are idempotent across process death. On first adoption, a per-Session fsynced marker
guards an exact-prefix migration. Older Lattice and Custom identities become accounting-only. A
legacy single-Agent Coordinator is different: bootstrap imports checkpoint-only conversation into
the canonical ledger first, clears unsafe pre-transaction behavior and Worker state, then rebuilds
that Coordinator's model-facing cache from canonical Completed turns with no orchestration state.
Cumulative accounting survives both paths. The marker is written only after sanitization and any
Coordinator rebuild are durable, so a crash safely repeats adoption and later transaction-committed
checkpoints are never reset on subsequent restarts. This transaction protects model-facing
continuity only. It does not claim to undo repository, tool, or external side effects.
The manifest currently transacts Tier-1 conversation/checkpoint state only. Transaction-scoped
Agents and Coordinator Workers may read existing Tier 2–4 memory and recall, but durable-memory
mutation is disabled for the whole turn: no semantic auto-store, daily-log archive write,
personal/shared core edit, or core consolidation is promoted, including after Completed. This
fail-closed limit remains until those stores gain generation-aware transactional deltas.

`EventFeed` (`axocoatl_core::event_feed`) is the process-wide event feed. The
daemon publishes one kind of event on it: firing a Skill (the fire route or an
Agent's `skill_<id>` tool) publishes one `Custom` event per name in the Skill's
`emits` list. The canonical Automation dispatcher matches `OnEvent` and `OnSkill`;
configured webhooks, the recent-events API, and WebSocket compatibility frames
observe the same feed. It keeps no history and never starts Agents on its own.
A Skill's 1.0 `reacts_to`, `agents` and `prompt` keys still parse but are ignored
with a startup warning. The Session scheduler is deliberately turn-scoped and
predicate-based.

The per-Agent `activation_threshold` / `activation_decay` keys were removed in 1.1.0 with
the process-wide threshold counter; the daemon warns when a config still sets them.

The remaining reads of legacy `workflows:` are intentional: Lattice-session
membership, coordinator worker selection, validation, and first-boot
Automation migration. Legacy `schedules:` and `proactive:` records are validation
and first-boot migration inputs only. None of these sections forms a parallel
manual, scheduled, or event-triggered runtime after `AutomationStore` exists.

## Coordinator role

Separately, an agent can take the **coordinator** role (`role: coordinator`)
for explicit hierarchical decomposition in a legacy Session. A native Session team
runs a Coordinator template as a `DefaultAgentBehavior` lead instead: its approved
Worker templates are reachable only through the `delegate` tool. Each legacy
coordination pass (`CoordinatorBehavior`):

1. **Decompose** the goal into subtasks with the model. Each subtask carries the
   tools it needs. The symbolic HTN planner was removed in 1.1.0; the daemon
   warns when a workflow still sets `htn_methods_file` and ignores it.
2. **Assign** each subtask to the **first declared worker**, in declaration
   order, whose callable tools cover the subtask's required tools. If no pooled
   worker can cover a subtask's tools, an ad-hoc worker is spawned with exactly
   those tools, so a subtask is never forced onto an unfit worker.
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
surfaces an error rather than a hollow result.

## Workspace knowledge

`axocoatl-memory::knowledge` owns versioned Workspace notes separately from
actor conversation, execution recovery, and compatibility recall. Immutable
Markdown revision documents retain the note body, typed links, source hashes,
provenance, and acceptance. An atomically replaced manifest publishes revisions
and proposal receipts together. Expected revisions prevent a concurrent edit from
silently overwriting another author; reopening validates referenced documents.
The authorized Workspace owns the store, and its lifetime spans Sessions.

Native `workspace_knowledge` is offered, and a new call admitted, only for an Agent whose `tools` list it; claims recorded before that rule still reload. Its operations read/search notes, search indexed code,
expose a bounded observed code map, and stage proposals attributed to the exact
activation. Publication uses the
canonical accepted-state boundary or an explicit human decision. Isolated Ways
additionally require an exact retained Keep selection before automatic publication;
successful unselected candidates remain pending. Knowledge cannot
expand a grant, make repository writes, or mark a required check passed. The API
and Session inspector expose provenance, source links, backlinks, proposal states,
and explicit editing/export rather than trusting model confidence as evidence.

A proposed finding or pitfall cites sources as `must_change` (the default: the file that
must change to fix the problem) or `evidence` (a supporting file). Once a proposal has
passed every check, the host reads each cited file with one fixed, read-only digest
observation, admitted on the activation's grant only while the invocation reserve can
spare it, that hashes a path only when it resolves to exactly `<repository>/<path>` as a
readable regular file, never through a symbolic link or outside the repository. A digest
the model gives is kept only for a file the host could not read; the activation's starting
capture is the last fallback. An omitted `expected_revision` creates a note and never
replaces an existing one, and a proposal whose note could never be published (over the
document limit) is refused when proposed.

`knowledge_index` builds a bounded, rebuildable index from caller-supplied source
under a declared snapshot identity. Pinned Tree-sitter grammars parse Rust,
JavaScript, TypeScript/TSX, and Python definitions and import syntax. Unsupported,
partial, and timed-out parsing remain visible. Imports are not resolved call or
type relationships. The map and search support targeted source reads; they do not
replace a language server or certify complete repository coverage.

Before durable Begin, bounded lexical retrieval selects accepted notes from the
request text. The retained context includes their exact revisions, provenance, and
source applicability from bounded live Session reads. Its code map retains the
cached index identity and observation timestamp; it is not represented as a fresh
read of every indexed file.

Freshness compares a note's recorded source hashes with the requesting Session's
source context. It is not a global stale bit that would invalidate another Session
still using the original source. Retained revisions remain historical
evidence when newer source differs. Direct mutation of internal revision files is
unsupported; explicit edits cross the store's validated revision boundary.

## Memory tiers

This section describes legacy and compatibility actors. Native Session activations
use `SessionExecutionStore` for canonical history and `ActivationStateStore` for exact
input, candidate checkpoints, and accepted-generation promotion. Their actor construction
in `session_dispatch_run.rs` does not attach daily-log, core-memory, or semantic-memory
stores, so the Tier 2–4 recall and core-edit capabilities below are not available on that
path. Legacy Lattice, Custom, and Coordinator turns may read their attached stores but
cannot make Tier 2–4 writes or run consolidation during the turn.

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

**Transcript ownership.** For a legacy Session, `SessionTurnStore` owns the canonical
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
  classic namespace/mount escape levers are gone. A hardened `network: egress`
  container keeps `SYS_PTRACE` for its first process (and root's readiness commands),
  which reads `/proc` to name the program behind each proxied connection; the workload
  users run Agents' commands and helpers without capabilities. See
  [`sandbox.workload`](https://docs.axocoatl.ai/configure/sandboxes/#workload-users).
- **Network.** The default is bridged networking so installs and development
  servers work. Set `sandbox.network: none` for repositories you do not trust, or
  whenever repository code and commands in the local container must have no
  outbound connection; this also disables
  network-dependent setup and commands in that container. `sandbox.network: egress`
  gives the container only loopback and Axocoatl's egress proxy, which reaches only
  the hosts under `sandbox.egress` and records each connection; see
  [Network egress](https://docs.axocoatl.ai/configure/sandboxes/#network-egress) and
  [the security guide](https://docs.axocoatl.ai/operate/security/#control-network-egress).
  Only `bridge`, `none` and `egress` are accepted; any other value fails config
  validation, `axocoatl doctor` and daemon start rather than falling back to bridge.
  Every `podman run` passes
  `--http-proxy=false`, so the host's proxy variables (which can hold a proxy user
  name and password) are not copied into the container. It does not govern
  daemon-side model providers, MCP, `web_search` and `web_fetch` (which run in the
  daemon), webhooks, remote sandboxes, the embedding-model download, or
  image-registry access. The browser tools run in a container of their own that
  reaches the Session's exposed ports and, through the egress proxy, the hosts under
  `browser.allow`. Configured Preview ports
  remain logical container-port identities; local Podman assigns each Session its
  own loopback host mapping, and the Session-aware proxy resolves that mapping
  without exposing arbitrary host services. Under `egress` the Session container
  publishes nothing: a service forwarder serves each port as a socket and a separate
  Preview container publishes it.
- **Resources.** Memory, CPU, and PID caps (2 GB / 2 CPUs / 512 pids) bound a
  runaway loop or fork bomb, where the host's cgroup delegation allows it. With the
  default `require_resource_limits: false`, a host that cannot apply them starts the
  container without them and logs a warning; set it to `true` to refuse instead.

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
When the cancelled or shutdown-interrupted preparation was a local re-preparation of an
already Ready plan (for example Files or Terminal after a restart), the guard returns the
Session to Ready at the same generation with its recorded setup evidence once that removal
succeeds, so a paused turn bound to that generation can continue. A crash mid-preparation
still reloads as Failed; rebuilding the unchanged plan of a local Failed environment while a
paused turn waits prepares it again at the same generation instead of refusing the change.
E2B preparations never take either path.
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
Acquisition detects the root format under the external lease. Supported prepared conversions
retain legacy ownership; installed v2 roots retain their native guard before their matching
recovery path runs. New roots initialize native ownership. The conversion API can consume
held legacy ownership without a release/reacquire window; legacy roots are not silently
converted during normal startup.

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

The host executable embeds first-party Linux x86_64 and aarch64 process-supervisor
payloads. Normal local Session and attempt startup inspects the approved image, pins its
immutable identity, and prepares the matching executable in the protected data directory.
Its read-only container mount preserves the image's configured user. A no-dispatch handshake
verifies the actual program before repository setup. There is no separate user installation
or startup download; contributor rebuild instructions are in
[`EXEC_SUPERVISOR_BUILD.md`](EXEC_SUPERVISOR_BUILD.md).

The internal v2 repository-check port joins exact durable admission to that supervisor and
retains output, command status, and an opaque process-settlement receipt. Caller cancellation
does not abandon its result owner. A daemon registry retains canonical ownership through
retryable cleanup and joins actual agent/tool tasks before transferring the Workspace gate.
The borrowed daemon shutdown API retains its owner after a failed or cancelled wait;
pending Agent joins return to the retained registry so the same shutdown can be retried.
Registered Session cleanup keeps its controller and Workspace gate until the actual
lifecycle action succeeds. Standalone CLI commands retain the daemon through bounded
cleanup retries and exit unsuccessfully if shutdown remains incomplete.
This does not establish complete shared-writer coverage: live v2 ingress, Files/Git/PTY
integration remain unfinished.
Ordinary v1 execution keeps its existing dispatch paths.

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

### File ownership inside one checkout

The Agents of one native Session share one checkout. An Agent's `writes` list, or the
**May change** choice in Team & budget, is recorded in the profile of every activation it
runs, and enforcement reads it from that admitted record, never from live configuration.
A scope that cannot be read refuses the write or process it was checking. A helper cannot
be admitted with a wider scope than the lead that delegated to it, and `delegate` admits
only a read-only helper: one whose template has no tool that writes files or runs
commands, or whose scope is empty (`writes: []`), so its `bash` runs under the kernel
restriction below and `write_file` and `edit_file` are withheld.

- `write_file` and `edit_file` refuse paths outside the scope before any effect, refuse
  `..` and any path through a symbolic link, and tell the Agent to leave the file
  unchanged and describe the needed change in its answer.
- A read-only Agent (`writes: []`), including a required reviewer with `bash`, is not
  offered `write_file` or `edit_file`. Its own `bash` commands run under a kernel
  restriction (Landlock, applied by the in-sandbox execution supervisor between fork and
  exec): only `/tmp`, `/var/tmp` and `/dev` are writable (for a hardened container's
  helper user, whose view the supervisor builds, only a scratch directory of the
  command's own and `/dev/null`, `/dev/zero`, `/dev/tty` and `/dev/urandom`), never the
  repository, and every
  TCP bind and connect is refused on any address, loopback included (Landlock network
  rules with no allowed port). Landlock does not cover UDP, or `listen` on an unbound
  socket, which the kernel binds to an ephemeral port itself. The execution request names
  this as `WriteRestriction.deny_network`, a protocol 3 field omitted while false, so
  earlier requests keep their bytes and digest and an earlier supervisor rejects it
  rather than ignore it. `HOME` (and the XDG directories) point at a scratch directory
  under `/tmp`, created for that command and removed when it ends, so the Session's
  shared home stays unchanged. A supervisor that cannot apply all of it (Landlock below
  ABI 3, Linux 6.2, cannot refuse truncation; below ABI 4, Linux 6.7, cannot refuse TCP)
  refuses to launch that command, and the Agent is told to use its read-only file tools
  instead. The read-only file tools and the host's own repository captures run without
  the restriction; in a hardened container the file tools run through the helper's
  view, which refuses them every write and every socket.
- A writer's shell can still write outside its paths, so the activation's own Before and
  After repository captures decide: an equal tree digest means no change; otherwise
  complete manifests are compared exactly, or the retained patches against the same HEAD
  file by file. If any non-ignored file outside the scope changed, or the captures cannot
  establish the change set (an exhausted invocation allowance, a HEAD moved by a commit, a
  patch over 512 KiB, a submodule or nested checkout), the activation fails and its turn
  needs attention. The change is not reverted; it stays for the person to keep or undo.
  Ignored files are not judged. This is review evidence, not confinement.

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
  written into the repo's Git config, remote URL, or a command line. It is an
  environment variable of the VM, so every command in the VM can read it, including
  repository scripts and Agent commands. Changes remain
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

`axocoatl-core` (types, event feed) · `axocoatl-token` (budgets) · `axocoatl-llm*`
(providers) · `axocoatl-config` · `axocoatl-actor` (runtime) ·
`axocoatl-memory` · `axocoatl-graph` · `axocoatl-mcp` · `axocoatl-a2a` · `axocoatl-tools` ·
`axocoatl-isolation` (Podman and E2B sandboxes) · `axocoatl-exec` (in-sandbox
command supervisor; applies the Landlock write and TCP restriction) · `axocoatl-session`
(durable Workspace, Session and turn storage) · `axocoatl-daemon` ·
`axocoatl-server` · `axocoatl-service` (systemd / launchd) · `axocoatl-cli`.
