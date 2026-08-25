# Product-film shot contracts

[`portfolio.json`](portfolio.json) is the authority for the 12-film release
portfolio, placement order, duration bounds, poster beat, scenarios, fixtures,
media paths, and evidence requirements. This document defines the visible edit
contract for each manifest beat. A film is accepted only when its manifest
status is `ready` and strict verification passes.

Every source image must be a real reachable state from the linked scenario.
Repeating a screenshot for a readable hold is allowed. A fake cursor, invented
progress, synthetic tool output, or evidence borrowed from another Session,
attempt, run, checkout, or binary is not.

## Capture, stage, and delivery contract

- Capture one exact 1280×720 JPEG for every beat as
  `shot-<beat>.jpg`. Use browser zoom 100% and device scale factor 1.
- Keep the browser theme and Session identity stable within a film unless the
  scenario explicitly calls for a switch, reload, or restart.
- Put a `timeline.json` beside the shots. Beats must appear once, in portfolio
  order, and each `hold_frames` value is a positive count at 8 fps:

  ```json
  {
    "schema_version": 1,
    "film": "session-workbench",
    "input_fps": 8,
    "shots": [
      { "beat": "visible-defect", "source": "shot-visible-defect.jpg", "hold_frames": 112 },
      { "beat": "agent-route", "source": "shot-agent-route.jpg", "hold_frames": 112 },
      { "beat": "verified-result", "source": "shot-verified-result.jpg", "hold_frames": 112 }
    ]
  }
  ```

- Materialize holds by repeating the exact JPEG. Compress idle model time,
  never a lifecycle transition, failure, approval, reload, restart, or operator
  choice.
- Use the current release candidate binary for the whole take. Capture evidence
  identifiers from the same root before resetting anything.
- Deliver H.264/yuv420p 1280×720 at 24 fps, no audio, with MP4 fast-start, and
  an MJPEG 1280×720 poster. The encoded duration must remain inside the
  portfolio bounds.
- Choose the poster from the manifest's named poster beat. The encoder copies
  that staged JPEG byte-for-byte so provenance can verify the exact keyframe.
  Never use a blank
  Preview, open menu, transient toast, approval modal for a non-approval film,
  or raw payload wall as a poster.
- Durable API/filesystem evidence is required in addition to visible keyframes.

The repeatable manual-capture path from the repository root is:

```bash
node demo/one-app/films/record-capture.mjs \
  session-workbench \
  /private/tmp/axocoatl-v1-film-capture/session-workbench \
  --captured-at 2026-08-20T19:30:00.000Z

node demo/one-app/films/stage-film.mjs \
  session-workbench \
  demo/one-app/films/source/session-workbench/timeline.json \
  /private/tmp/axocoatl-v1-film-stage/session-workbench \
  --replace

./demo/one-app/films/encode-film.sh \
  session-workbench \
  /private/tmp/axocoatl-v1-film-stage/session-workbench \
  sites/marketing/assets/films \
  "$(node -p 'require("./demo/one-app/films/source/session-workbench/stage.json").poster_frame')" \
  --replace

node demo/one-app/films/write-provenance.mjs \
  session-workbench \
  --binary "$PWD/target/release/axocoatl" \
  --frames /private/tmp/axocoatl-v1-film-stage/session-workbench \
  --evidence demo/one-app/films/source/session-workbench/evidence.json \
  --replace
```

The writer emits provenance schema version 2. Its `source.content_sha256` is a
canonical digest of every Git-visible path, entry type, normalized executable
state, and current bytes, so the digest stays the same when an identical checkout
moves between dirty and committed Git representations. Capture sources, staged
frames, provenance records, and shipped film media remain excluded;
`source.patch_excludes` records that exact four-part contract. Portable
verification validates the recorded digest field without binding it to the
current checkout. Source-bound and unflagged release verification also compare
it with the current checkout.

`evidence.json` uses schema version 1, the matching `film`, one ordered
`passed` check per portfolio beat with a concrete `detail`, and a non-empty
`identities` object containing the durable Session, Turn, attempt-set, run, or
checkout identifiers appropriate to that scenario. Run
`node demo/one-app/films/verify-film-set.mjs` only as the final release gate.

```json
{
  "schema_version": 1,
  "film": "session-workbench",
  "checks": [
    { "id": "visible-defect", "status": "passed", "detail": "Captured the matching red Preview and repository check." },
    { "id": "agent-route", "status": "passed", "detail": "The canonical Turn changed only lib/orders.js." },
    { "id": "verified-result", "status": "passed", "detail": "Six checks and the corrected Preview passed before reset." }
  ],
  "identities": {
    "session_id": "ses-…",
    "turn_id": "turn-…",
    "source_head": "40-character Git object id"
  }
}
```

## `session-workbench`

- **Placement:** Home 1; Showcase 1.
- **Scenario:** [`single-agent.md`](../scenarios/single-agent.md), Northstar Storefront.
- **Target:** 35–50 seconds. **Poster beat:** `verified-result`.

| Beat | Required visible evidence | Edit rule |
| --- | --- | --- |
| `visible-defect` | Preview reads `-$20.00 · Invariant broken`; `npm run check` has exactly the intended failure. | Establish both customer and repository symptoms before the repair. |
| `agent-route` | One Agent reads, edits, and checks; only `lib/orders.js` changes. | Tool activity must come from this completed canonical Turn. |
| `verified-result` | Six checks pass and Preview reads `$0.00 · Ready`. | Hold long enough to read the product and repository proof; this is the poster. |
| `git-ownership` | Source Control shows only uncommitted `lib/orders.js` and its meaningful production hunk. | Do not stage or commit merely for the film. |

## `workspace-sessions-turns`

- **Placement:** Concepts 1; Why 1; Showcase 2.
- **Scenario:** [`workspace-sessions.md`](../scenarios/workspace-sessions.md), Harbor Catalog with the deterministic local `harbor-ways-fixture` provider.
- **Target:** 25–35 seconds. **Poster beat:** `restored-turn`.

| Beat | Required visible evidence | Edit rule |
| --- | --- | --- |
| `workspace` | `Harbor Catalog` and its authorized path are readable. | Do not present a path label alone as the Workspace identity. |
| `two-sessions` | Two differently named Sessions appear under the same Workspace. | Keep their Agent identities and transcript separation legible. |
| `accepted-turn` | One read-only request, tool evidence, and terminal answer complete. | A draft or streaming-only state does not prove a Turn. |
| `restored-turn` | Switching away, returning, and reloading restores the exact Turn. | Use the restored transcript as the poster; avoid menus and toasts. |

## `durable-turn`

- **Placement:** Home 2; Concepts 2; Showcase 3.
- **Scenario:** [`durable-context-turns.md`](../scenarios/durable-context-turns.md), Harbor Catalog.
- **Target:** 40–55 seconds. **Poster beat:** `cancelled-history`.

| Beat | Required visible evidence | Edit rule |
| --- | --- | --- |
| `context` | Accepted Once and Session context retain their historical attachment relation. | Consumption must follow acceptance; do not imply secure erasure. |
| `reload` | Reload reconnects to the exact active Turn with honest partial output and Stop. | Play the reload at normal speed. |
| `cancelled-history` | Stop settles as `Stopped by you` and History retains partial evidence. | Stop is cancellation, not rollback. |
| `clean-next-turn` | A new Turn replies exactly `CLEAN NEXT TURN.` without stale tools or output. | This beat proves actor cleanup; it is not optional acceptance polish. |

## `sandbox-terminal-preview`

- **Placement:** Concepts 3; Showcase 4.
- **Scenario:** [`sandbox-preview.md`](../scenarios/sandbox-preview.md), Northstar Storefront.
- **Target:** 25–35 seconds. **Poster beat:** `preview`.

| Beat | Required visible evidence | Edit rule |
| --- | --- | --- |
| `session` | The Ready Session and authorized Workspace are visible. | Do not start on an unexplained shell close-up. |
| `sandbox` | Terminal shows container hostname, exact workspace path, runtime version, and image OS. | Keep output readable and prove the current runtime-authority label separately. |
| `check` | `npm run check` reaches its honest result in the Session Terminal. | Never splice output from a host shell or another checkout. |
| `preview` | The same Session serves the current checkout through its published port. | Reject a blank Preview or static marketing mock. |

## `multi-agent-handoff`

- **Placement:** Showcase 5.
- **Scenario:** [`multi-agent.md`](../scenarios/multi-agent.md), Harbor Catalog with a design-only prompt.
- **Target:** 25–35 seconds. **Poster beat:** `dependency-edge`.

| Beat | Required visible evidence | Edit rule |
| --- | --- | --- |
| `dependency-edge` | Exactly `architect → reviewer` is visible before execution. | Do not confuse this graph with the event lattice. |
| `architect` | Architect is active first and returns one non-empty sentence. | Do not cut the sequence to imply concurrency. |
| `reviewer` | Reviewer activates second, receives the upstream sentence, and returns a decision. | A blank scheduled output is a failed take. |
| `durable-outputs` | Reload restores two separate labeled outputs on one completed Turn. | Make no repository-edit or Ways claim. |

## `several-ways`

- **Placement:** Showcase 6.
- **Scenario:** [`several-ways.md`](../scenarios/several-ways.md), Harbor Catalog with the deterministic local `harbor-ways-fixture` provider.
- **Target:** 55–75 seconds. **Poster beat:** `comparison`.

| Beat | Required visible evidence | Edit rule |
| --- | --- | --- |
| `configuration` | Two Agents, one exact task, Plan first, and the common snapshot are identifiable. | Ways must be configured before sending. |
| `independent-routes` | Each Way has its own output, tools, clone, and container identity. | Preserve blocked or failed states honestly. |
| `checks` | Both non-empty Ways pass the same six checks against protected checked snapshots. | One survivor is not sufficient for this accepted film. |
| `comparison` | Outcome, diff, usage, known cost, and Route are visible. | Reveal branch plumbing only as supporting Git detail. |
| `judge` | Judge assigns unique ranks while both candidates remain intact. | Judge advises; it does not choose or apply. |
| `keep` | The operator keeps one result and temporary attempt resources clean up. | Keep must not be described as merge or commit. |

## `git-last-turn`

- **Placement:** Showcase 7; Why 2.
- **Scenario:** [`git-last-turn.md`](../scenarios/git-last-turn.md), Northstar Storefront.
- **Target:** 20–30 seconds. **Poster beat:** `last-turn`.

| Beat | Required visible evidence | Edit rule |
| --- | --- | --- |
| `kept-decision` | History shows the completed canonical Turn produced by the kept checked Way; durable evidence names its Keep receipt. | Establish the accepted result without pretending the cleared attempt set still exists. |
| `uncommitted` | Source Control opens immediately with the kept change unstaged. | Do not send another Agent Turn first. |
| `last-turn` | The exact kept path and meaningful hunk are readable. | Use actual diff evidence, not an attempt summary. |
| `restart` | Reload or daemon restart rehydrates the same Last turn scope. | Do not use a reset or new Keep between states. |
| `ownership` | Staging remains optional and HEAD stays at `demo-seed`. | Never commit merely to make the film look finished. |

## `settings-runtime`

- **Placement:** Concepts 4; Showcase 8.
- **Scenario:** [`settings-runtime.md`](../scenarios/settings-runtime.md), Signal Desk.
- **Target:** 20–30 seconds. **Poster beat:** `agents`.

| Beat | Required visible evidence | Edit rule |
| --- | --- | --- |
| `session` | Begin in a normal selected Session. | Conversation remains the product surface behind Settings. |
| `agents` | Critical Reviewer identity shows provider, model, and role. | Keep the Agent drawer at its top; do not present Agents as a peer route. |
| `agent-dependency` | The same Critical Reviewer drawer shows the checked `architect` dependency. | Scroll only the Agent drawer; do not toggle the configured dependency. |
| `skills` | Release candidate ready declares the `ReleaseCandidateReady` event. | Show the configured Skill and do not fire it again. |
| `mcp` | `weather` uses stdio and exposes `mcp__weather__get_weather`. | Show the discovered tool without changing Permissions. |
| `automations` | A completed Automation run exposes its recorded Result while the same Session remains selected behind Settings. | Hold on the Result; do not present Settings as a separate product surface. |

## `event-lattice-automation`

- **Placement:** Concepts 5; Showcase 9.
- **Scenario:** [`event-lattice.md`](../scenarios/event-lattice.md), Signal Desk.
- **Target:** 30–45 seconds. **Poster beat:** `result`.

| Beat | Required visible evidence | Edit rule |
| --- | --- | --- |
| `skill` | `Release candidate ready` declares `ReleaseCandidateReady` and fires once. | Do not use a manual Automation Run button. |
| `event` | After firing, the Skills list retains `ReleaseCandidateReady` while the downstream waiting state appears. | Producer and payload belong to the captured API evidence; do not fabricate a raw-event UI. |
| `trigger` | Exactly one matching Automation run identifies the Skill trigger. | Keep the causal transition continuous. |
| `result` | After reload, completed run history shows the exact recorded `final_content` Result. | Do not end on a transient success toast or status badge alone. |

## `mcp-approval`

- **Placement:** Concepts 6; Showcase 10.
- **Scenario:** [`mcp-approval.md`](../scenarios/mcp-approval.md), Northstar Storefront with deterministic `mcp-bridge`.
- **Target:** 25–35 seconds. **Poster beat:** `pending-approval`.

| Beat | Required visible evidence | Edit rule |
| --- | --- | --- |
| `request` | Weather Agent discovers qualified `mcp__weather__get_weather`. | Keep fixture/local-service limits explicit. |
| `pending-approval` | Reload while pending restores the exact actionable approval beyond 30 seconds. | Hold long enough to read Agent, server, tool, and arguments. |
| `deny` | Deny once yields visible Turn failure and zero MCP dispatches. | Denial must precede any allowed request. |
| `allow` | A fresh request is allowed once and produces exactly one dispatch. | Show deterministic city-dependent output. |
| `durable-result` | Reload restores tool start, result, and final answer. | Allow once must not create a saved permission rule. |

## `shared-core-memory`

- **Placement:** Concepts 7; Showcase 11.
- **Scenario:** [`shared-core-memory.md`](../scenarios/shared-core-memory.md), Signal Desk.
- **Target:** 35–50 seconds. **Poster beat:** `recall`.

| Beat | Required visible evidence | Edit rule |
| --- | --- | --- |
| `write` | `core_memory_set` succeeds for the shared team block and exact nonce. | A conversational promise to remember is a failed take. |
| `restart` | Daemon restarts against the same data root. | Never run preparation or reset between states. |
| `separate-sessions` | Writer and reader Sessions and Agent identities are distinct. | Their transcripts must remain separate. |
| `recall` | The second Agent quotes the exact nonce without file, History, or tool call. | Claim shared core memory only, not semantic or universal recall. |

## `automation-hitl-recovery`

- **Placement:** Showcase 12.
- **Scenario:** [`automation-hitl.md`](../scenarios/automation-hitl.md), seeded Signal Desk Spec review Automation.
- **Target:** 45–65 seconds. **Poster beat:** `interrupt`.

| Beat | Required visible evidence | Edit rule |
| --- | --- | --- |
| `run` | Upstream nodes and three Map perspectives complete in the graph. | Keep real node order; compress provider idle time only. |
| `interrupt` | Blocking branch parks at the top-level Interrupt with upstream evidence. | The operator prompt is the poster, not an arbitrary mid-run pulse. |
| `restart` | Daemon restarts on the same root and rediscovers the pending Interrupt without replay. | This proves only the durable parked boundary. |
| `resume` | Operator guidance resumes the same saved run. | Completed upstream nodes must not run again. |
| `result` | Completed history retains terminal status and exact `final_content` Result after reload. | Do not substitute a status badge for the recorded output. |
