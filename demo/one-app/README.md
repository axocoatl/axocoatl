# Axocoatl one-app demonstration

One folder. One durable session. Several independently checked ways. One deliberate
Git decision.

This kit creates a disposable Northstar Supply storefront with one visible regression:
an over-large fixed discount produces a negative payable total. The same defect appears
as a red repository check and as `-$20.00` in Preview. The demo uses the
one-app loop to diagnose it, explore two independently produced ways, verify them, keep
one, and review the resulting Git change.

## What this demonstrates

- folder-authorized Workspaces and durable Sessions;
- a normal single-agent Conversation with streamed repository tools;
- a Conversation spine with focused Files/editor/Source Control and Preview, a bottom
  Terminal, and a contextual Ways inspector;
- Plan first, model probe, two isolated Attempts, Cost, Outcome, Route, diff, Checks,
  Judge, Keep, and Finish without keeping;
- Agent, Skill, event, and Automation controls in Settings;
- an optional seven-node human-in-the-loop Automation;
- reload and reconnect without losing the session decision record;
- a post-core drill for **Once**/**Session** attachment context, exact Stop,
  durable History search/export, and logical rewind.

The core presenter path takes about 20 minutes after the machine is warm. The gated
Automation and Skill appendix takes another 15–25 minutes. The optional memory prompt and
remaining Session export/rewind steps are untimed. A separate 13 August follow-up record below
defines which Session-history/context claims have been live-verified.

Use [PRESENTER-CHECKLIST.md](PRESENTER-CHECKLIST.md) at the podium. This README is the
full rehearsal and recovery guide; [PROMPTS.md](PROMPTS.md) is the copy-ready script.

### Rehearsal baseline

The 11 August 2026 warm-machine rehearsal proved the complete coding loop with two
surviving ways:

- `Minimal Coder` settled in 58.8 seconds and `Invariant Defender` in 1 minute 32
  seconds. Budget two minutes per way on stage; model time naturally varies.
- each way changed only `lib/orders.js` and passed all six repository checks;
- Judge assigned unique ranks `#1` and `#2`, explained its deterministic tie-break for
  equivalent patches, and preserved both checked ways;
- **Keep this one** succeeded on the first click, left one visible uncommitted change in
  the primary workspace, and the primary check became green.

The Automation path has a separate verification ledger below. Do not infer that an
Automation behavior is stage-ready merely because the core coding path is green.

### Session capability follow-up baseline

The 13 August 2026 rebuilt-app pass verified this narrower Session path:

- upload of `AXOCOATL.md` as **Once**, followed by consumption only after acceptance;
- an answer that actually used the attachment content;
- a clickable attachment chip on the historical turn after consumption;
- History rendering four durable turns and the test query returning exactly one result;
- page reload during generation reattaching to the active turn with partial output and
  restoring **Stop**;
- pressing **Stop** and reopening History with the terminal result `Stopped by you` persisted.

The 20 August 1.0 acceptance pass completed the remaining product seams: **Session**
context survived an accepted turn and was removed only for future turns; Stop waited for
an already-started side effect, persisted the cancelled Turn, and allowed a clean next
Turn; History restored completed, cancelled, and failed tool evidence; scoped and
all-Session search, Markdown/JSON export, and logical rewind all passed in the rebuilt app.

## Prerequisites

- macOS or Linux with Podman running;
- Ollama listening on `127.0.0.1:11434`;
- the local model `qwen3:8b`;
- host Node.js and npm (`prepare.sh` runs the fixture check on the host);
- Rust and Cargo for the Axocoatl build;
- ports `18080` and `8765` free.

The first preparation may pull `node:22-alpine` and install a few Alpine packages into
the local demo image. Attempts need no package install or network access after that image
exists.

### Five-minute preflight

Run this before the audience arrives, not while presenting:

First resolve and close every old session while its daemon is running, then stop every
Axocoatl daemon. Confirm that no `axo-ses-*` container remains, including one from an
earlier demo. `prepare.sh` moves the old demo root, so a surviving container would no
longer have a matching session record for the next start.

```bash
podman system check --quick
ollama list
./demo/one-app/prepare.sh
./demo/one-app/start.sh
```

Confirm that `qwen3:8b` appears in `ollama list`, the launcher prints
`http://127.0.0.1:18080`, and `http://127.0.0.1:18080/health/ready` responds. In a second
terminal, run `./demo/one-app/seed-automation.sh` after the daemon is healthy.

Then complete one private warm-up through the first diagnosis. This catches model,
Podman, port, cable, and browser-proxy problems while there is still time to reset. If
the audience should see a pristine transcript, close the smoke-test session and daemon,
then rerun `prepare.sh`, `start.sh`, and `seed-automation.sh`; the second build is warm.

### Runtime safety

Every daemon-created Session and Way container carries a stable runtime-authority label
derived from its canonical data directory. Startup reconciliation lists and removes only
containers bearing that exact authority, so a second data root cannot reap another
daemon's work. The disposable demo scripts remain deliberately conservative: they refuse
surviving demo containers and validate them against this demo's isolated Session records
so every rehearsal starts at a clean boundary. They never prune, reset, or remove an
unrelated container for you.

### Demo root

The scripts choose a platform-specific disposable root automatically. In the steps below,
`DEMO_ROOT` means:

| Platform | `DEMO_ROOT` |
| --- | --- |
| macOS | `/private/tmp/axocoatl-one-app-showcase` |
| Linux | `/tmp/axocoatl-one-app-showcase` |

An `AXOCOATL_DEMO_ROOT` override is accepted only for a direct
`axocoatl-one-app-showcase*` root below `/private/tmp` or `/tmp`. When choosing the folder
in the UI, use the platform's actual path; the folder picker does not expand
`$DEMO_ROOT` as shell syntax.

## Prepare the disposable workspace

From the Axocoatl repository:

```bash
./demo/one-app/prepare.sh
```

This command:

1. starts Podman when necessary;
2. builds `localhost/axocoatl-one-app-demo:latest`;
3. moves an older marked demo root to a timestamped backup;
4. creates `$DEMO_ROOT/workspace`;
5. initializes and commits the red seed repository;
6. proves that the intended discount test—and only that contract—starts red.

It never resets the Axocoatl repository or `axocoatl-testbed`.

## Start Axocoatl

In one terminal:

```bash
./demo/one-app/start.sh
```

The launcher builds the real embedded app, validates the demo config, uses isolated data
and IPC paths below `$DEMO_ROOT`, and starts Axocoatl at:

```text
http://127.0.0.1:18080
```

For a 1.0 recording, build the exact locked release candidate first and force
the launcher to validate and run that binary:

```bash
cargo build --release --locked -p axocoatl-cli
AXOCOATL_DEMO_BIN="$PWD/target/release/axocoatl" \
  ./demo/one-app/start.sh
```

`AXOCOATL_DEMO_BIN` must be an absolute executable path. The launcher does not
silently rebuild or substitute the product binary when that override is set;
it prints the selected path, `--version`, and SHA-256 for capture provenance.
It still builds the deterministic debug `mcp-bridge` fixture referenced by the
demo configuration.

In another terminal, seed the advanced Automation:

```bash
./demo/one-app/seed-automation.sh
```

## 1.0 product films

[`DEMO-CATALOG.md`](DEMO-CATALOG.md) defines the narrative portfolio and
[`films/portfolio.json`](films/portfolio.json) is the machine-readable
authority for its 12 slugs, complete Showcase order, placements, scenarios,
beats, duration bounds, poster, and evidence. All 12 retain exact source-frame,
capture-record, durable-evidence, staged-frame, and shipped-media hashes. A history
audit restored every provenance JSON byte-for-byte from its earliest commit after
later source and binary fields had been rewritten without recapture. Those restored
fields are the first committed declarations. The capture binary bytes themselves
were not preserved, so its declared path, version, and hash are not an independently
authenticated binary artifact.

Use [`films/SHOT-MANIFEST.md`](films/SHOT-MANIFEST.md) for the exact manual
capture, timeline/holds, staging, encoding, and provenance workflow. Manual
1280×720 keyframes may be captured into
`/private/tmp/axocoatl-v1-film-capture/<slug>/` as `shot-<beat>.jpg`; no
standalone browser-launch script is required.

Verify the contracts in progressively stronger layers:

```bash
node demo/one-app/films/verify-film-set.mjs --manifest-only
node demo/one-app/films/verify-film-set.mjs --allow-needs-recording
node demo/one-app/films/verify-film-set.mjs --portable
node demo/one-app/films/verify-film-set.mjs --source-bound
node demo/one-app/films/verify-film-set.mjs
```

The first command validates all 12 scenario and shot-contract references. The
second is a work-in-progress gate that warns for each explicit
`needs_recording` entry while strictly checking any entry marked `ready`. The
portable gate checks recorded evidence and media without requiring the capture
machine's binary. The source-bound gate additionally binds every film to the
current checkout's canonical source-content digest. The unflagged local release
gate requires a local binary whose version and hash match the first-committed
declaration; that is a local equality check, not proof that the original binary
bytes were preserved. Every film must still be `ready` and meet the exact media,
duration, source-frame, durable-evidence, and shipped-media contract.

Those exact-source gates do not silently accept patch releases. When a patch has
an intentionally bounded source delta that does not change the filmed product
surface, a separate reviewed compatibility record can be checked with
`--release-compatibility <attestation> --release-root <frozen-checkout>`. That
mode reruns the portable gate in the frozen checkout and compares its historical
source/binary-only provenance rewrite with the byte-for-byte restored files in the
control checkout. For `v1.0.1`, it proves the annotated tag/commit/tree, all 55
changed paths from the first provenance commit (43 non-recording paths plus 12
audited provenance rewrites), both 153-artifact aggregate digests, the complete
runtime-changed path set, six reviewed runtime build/test fixes, and the remaining
protected filmed-product content. It means the recordings remain evidence for
their stated visible claims; it does not claim they were captured with the
`v1.0.1` binary.

## Create the presenter Session

1. Open `http://127.0.0.1:18080`.
2. From the Workspace switcher, choose **Open workspace…**.
3. Authorize `$DEMO_ROOT/workspace` using the expanded platform path from the table
   above and name it `Northstar Storefront`.
4. With `Northstar Storefront` selected, choose **New session** and name the Session
   `Storefront implementation`.
5. Use **Single agent** with `Minimal Coder`.
6. Keep the detected `npm run check` command.
7. Keep the repository-provided `localhost/axocoatl-one-app-demo:latest` image and
   exposed port `8765`.

Open Terminal and run:

```bash
npm run demo
```

Open Preview at `http://localhost:8765`. The conference cable pack should visibly show
`-$20.00` and `Invariant broken` before the fix.

Collapse the Terminal dock after the server starts so the Conversation and composer remain
prominent. The server keeps running when the dock is collapsed; reopen it only when showing
command output.

## The 20-minute presenter path

| Segment | Budget | Audience proof |
| --- | ---: | --- |
| Surface and visible defect | 2 min | one conversation with contextual Files, Ways, Terminal, Preview, and Source Control |
| Single-agent diagnosis | 3 min | red check, customer-visible defect, clean Git tree |
| Plan and two ways | 6 min | two isolated attempts settle independently; budget 2 min of model time each |
| Outcome, Route, Checks, Judge | 5 min | repository evidence and a ranked recommendation |
| Keep and primary verification | 3 min | one deliberate uncommitted change, six green checks, `$0.00 · Ready` |
| Reload and close | 1 min | the same session and decision record remain |

### 1. Establish the work surface

Show that the Session—not a feature dashboard—is the center:

- the workspace and session live in the left rail;
- Conversation remains the main surface;
- Ways opens its contextual inspector only while planning or watching several ways;
- Terminal stays in the bottom dock;
- Files, editor, Preview, Source Control, and comparison focus over the conversation when opened.

Open **All sessions** briefly, then return to the prepared Session.

### 2. Diagnose normally

Send prompt 1 from [PROMPTS.md](PROMPTS.md). Open the failing test and
`applyDiscount` from Files. Rerun `npm run check` in Terminal. Source Control should
still be clean. Collapse the Terminal again before the next step.

### 3. Explore two ways

With the Terminal collapsed, choose **Explore several ways** to open its configuration
before putting prompt 2 in the composer. Configure:

- `Minimal Coder` looks for the smallest contract-preserving fix;
- `Invariant Defender` reasons from boundary invariants;
- both use the same local `qwen3:8b`, so the comparison is honestly Agent diversity,
  not a provider-cost claim.

Enable **Plan first** with `Acceptance Planner`. Only after Explore mode and both ways
are visibly configured, paste prompt 2 into the composer. Review the proposed acceptance
plan, run **Check models**, and start. Pasting or sending prompt 2 before Explore mode is
active dispatches a normal primary-session turn and can change the main workspace.

The live rehearsal settled the two ways in 58.8 seconds and 1 minute 32 seconds. Narrate
the visible roster, tools, files, and independent progress while waiting; do not promise
those exact times.

While they run, show the roster, rail state, in-thread attempt summary, local known-zero
cost, Files, and the Conversation. The conversation remains the permanent spine.

### 4. Compare evidence

When both ways settle:

1. Compare **Outcome**.
2. Inspect changed paths and the actual diff.
3. Toggle **Only differences**.
4. Compare **Route** to see tools, files, and commands.
5. Reveal branches only as the Git implementation detail.
6. Run **Checks** with `npm run check`.
7. If two non-empty ways pass, use `Evidence Judge`.
8. Choose **Keep this one** yourself.

Judge advises. Checks and the operator decide.

The rehearsed Judge result ranked both equivalent one-line patches without discarding
either one. It used the lower attempt index as the required deterministic tie-break and
explained that choice. A Judge error does not erase the two ways; use the recovery below.

### 5. Prove the kept result

- Refresh Preview: the negative payable total becomes `$0.00`.
- Run `npm run check` in the primary Terminal.
- Send prompt 3 so the primary Agent explains the actual working-tree result.
- Open Source Control → **Last turn**.
- Inspect the relevant file or hunk. Stage only if staging is part of the audience story.
- Optionally commit in this disposable repository:

```text
fix: clamp over-large fixed discounts
```

Keep does not commit, merge, push, or open a pull request.

### 6. Close the loop

- Reload `/` and show the same Session and attempt decision record.
- Optionally open Settings → Agents for a quick view of the configured Agent models.
- Return to the coding Session and end on the kept change and green proof.

The one-minute close stops here. Skills, event-triggered work, and the seven-node HITL
Automation are a separate 15–25 minute appendix, not part of the core 20-minute claim.

## Session context and History drill — core path verified, remaining gates open

This drill exercises the Session-native replacements for the useful parts of the retired
peer Chat and cross-chat Files surfaces. The attachment, History/search, reload, and Stop path
is recorded in the 13 August follow-up baseline above. Recheck the exact presentation build;
steps 3, the all-Session/export portion of step 5, and step 6 remain live-demo gates.

1. Make sure **Explore several ways** is off. Composer attachments currently apply to
   normal Session turns, not isolated Attempts.
2. Choose the paperclip and attach the expanded
   `$DEMO_ROOT/workspace/AXOCOATL.md` path. Leave its chip at **Once**, then send prompt
   3A from [PROMPTS.md](PROMPTS.md). After the server accepts the turn, the one-turn chip
   should disappear and the transcript should retain its context name.
3. Attach the file again and change the chip from **Once** to **Session**. Send a short
   follow-up from prompt 3A and show that the chip remains selected. Then remove it and show
   that it stops future inclusion while the attachment link on the historical turn still opens.
4. Send prompt 3B. Once the response is streaming—or after a read-only tool begins—press
   **Stop**. The button should show that it is stopping after a safe boundary and the
   durable turn should settle as cancelled. Do not describe an already-started tool as
   rolled back.
5. Open **History**. Search this Session for `reporting format`, then switch to **All
   sessions** to show the same durable search scope. Export Markdown or JSON.
6. Create one harmless no-edit turn, use **Rewind** at the preceding turn, and show that
   the later turn leaves the normal transcript. Explain that this is append-only logical
   history; it does not undo repository or external side effects and is not secure erasure.

Inline **Retry** is intentionally unavailable for the attachment proof. Resending only the
visible prompt after a **Once** attachment was consumed would be a different request; compose
a new turn and reattach the file instead.

Uploads are bounded: declared images are accepted up to 10 MiB and other documents up to
25 MiB; extracted/OCR context is capped at 256 KiB. Removing used context deactivates future
inclusion while retaining its historical relation and blob pin. Original content-addressed
blobs are retained rather than garbage-collected without safe cross-owner reference counts.

## Optional Automation and Settings appendix — 15–25 minutes

Run this appendix only when every presentation gate in the rehearsal ledger below is
green on the exact build being shown.

1. Open Settings → Agents and show each Agent's model and status.
2. Fire **Demo health check** under Skills.
3. Open Automations → **Demo event response** → **Runs** and show the Skill-triggered
   run. The history request bypasses the browser cache, so reopening **Runs** must show
   the current daemon record rather than a stale empty response.
4. Run **Spec review · multi-perspective with HITL** with the seeded input. When it
   pauses, use the waiting-Interrupt utility in the session rail and resume with prompt
   4.
5. Return to the coding Session without changing products.

### Automation rehearsal ledger

These statuses are deliberately narrower than “the Automation feature works.” Update
them only after exercising the actual embedded page on the presentation build.

| Behavior | 11 August 2026 rehearsal status |
| --- | --- |
| Reopening **Runs** bypasses stale browser history | **Verified** after a rebuild and daemon restart |
| An open **Runs** panel polls about every 1.5 seconds and updates in place | **Verified**: a new run appeared as `running` and changed to `completed` without closing the panel |
| The session-rail `⏸ waiting` utility opens the Interrupt panel | **Verified** after a daemon restart with recovered Interrupt evidence |
| Run and Interrupt checkpoint survive a daemon restart | **Verified** |
| The parked Interrupt can be discovered and resumed after that restart | **Verified**: the legacy run appended `InterruptResumed` and Planner `NodeCompleted`, reached `completed`, and did not duplicate upstream checkpoints |

All appendix gates are green on the rebuilt embedded page. Restart recovery is scoped to
a top-level Automation parked at an Interrupt. A nested Subgraph Interrupt remains
process-local, and Axocoatl does not claim to resume an arbitrary provider or tool call
that was in flight when the process stopped.

## Reload and restart behavior

A browser-page reload is safe after the attempts settle: the rehearsal restored the
session, its current attempt set, and the decision record. After a reload:

- click **Review outcomes** if Attempts review remains at “Reading the attempts…”;
- reselect `Evidence Judge` before judging, because the selector can return to its
  default Agent;
- verify both checked ways are still listed before pressing Judge or Keep.

A daemon restart is a stronger test. The settled attempt set survived the rehearsal,
but terminal processes and the preview should not be assumed to survive. Reopen Terminal,
run `npm run demo` again if necessary, and refresh Preview. The rebuilt rehearsal also
restored a top-level parked Interrupt to the rail and resumed it without replaying its
completed upstream nodes. Keep the nested-Subgraph and arbitrary in-flight-call limits
above when narrating that proof.

## Troubleshooting the rehearsed path

### Explore is visible but cannot be clicked

Collapse the bottom Terminal dock so the Conversation composer and **Explore several ways**
control remain prominent.

### Both ways pass but Judge reports an invalid ranking

The Judge contract requires a unique permutation of `1..N`; even equivalent solutions
cannot tie. The prompt contract now says to rank equivalent candidates by lower attempt
index, and the retry succeeded with explicit tie-break reasoning.

If this recurs, do not discard or restart the attempt set. Confirm that both ways still
show green Checks, select `Evidence Judge`, and press Judge again. If the second judgment
fails, skip Judge and make the operator decision from Checks, diff, Outcome, and Route.
The two checked ways are still valid evidence.

### Attempts review keeps reading after a reconnect

Press **Review outcomes** once to rehydrate the comparison from the durable attempt set.
Confirm both attempt cards and their green Checks before continuing.

### Preview is blank after a daemon restart

Open the restored primary Terminal and rerun `npm run demo`, then refresh
`http://localhost:8765`. This affects the disposable preview process, not the persisted
session or attempt evidence.

### Runs shows no new Automation run

Leave **Runs** open for about 1.5 seconds; the verified panel adds a new `running` record
and updates it to `completed` in place. If it still looks stale, close and reopen
**Runs**. The fresh request uses `no-store` and reads the daemon's current history.

## Honest fallback paths

- One passing way: skip Judge; Checks already selected the survivor.
- No passing ways: show the failure and use **Finish without keeping**.
- No changed files: the way is not keepable, by design.
- A slow way: allow it to finish or use the visible cleanup action; do not silently kill
  the daemon.
- Model unavailable: stop and run `ollama list`; do not switch models mid-presentation
  without checking their tool-call behavior first.
- Judge unavailable with two passing ways: skip it and choose from Checks, diff, Outcome,
  and Route. Judge is advisory, not the source of truth.

## Claims this kit does not make

- Settings can run, edit, pause, move, and delete seeded Automations, but the current UI
  does not create a net-new Automation. The seed script uses the API.
- The MCP panel uses a real local stdio connection to the deterministic
  `mcp-bridge` weather fixture; its values are canned and make no network
  forecast request.
- Firing a Skill publishes its declared event. The configured OnEvent Automation is what
  turns that event into Agent work.
- Attempt tools intentionally omit Skills, MCP, and web search because those external
  effects cannot be rolled back with a Git candidate.
- Composer attachments are for normal Session turns; Attempts do not currently receive
  them.
- Stop is cooperative. It targets the exact turn but waits for an already-started
  side-effecting tool to reach a safe boundary; it is not rollback.
- History rewind logically supersedes later turns. It does not erase blobs, reverse Git or
  tool effects, or rewrite every older checkpoint; it currently requires a single-agent
  Session so the daemon can reconstruct the next actor checkpoint. A `SIGKILL` between the
  ledger and checkpoint writes converges from the ledger on the next startup, not instantly.
- Canonical tool Route evidence is bounded. JSON keeps structured values up to 16 KiB;
  larger values become 8 KiB audit previews, and Markdown shows at most 2 KiB per Route event.
- Local-first does not mean no network. Ollama, image pulls, bridged preview ports, MCP,
  integrations, and optional remote sandboxes can use it.
- Keep is not release automation. The real checkout and Git decision remain visible.
- Restart-safe Automation recovery covers a top-level parked Interrupt. It does not cover
  nested Subgraph Interrupts or arbitrary provider/tool calls stopped in flight.

## Shutdown and reset

Before stopping the foreground daemon:

1. resolve every Attempt set with Keep or Finish without keeping;
2. resume or cancel every waiting Automation Interrupt;
3. close the demo Session from the rail so its exact container is removed;
4. press Ctrl+C in the Axocoatl terminal;
5. leave Ollama and the Podman machine running unless this demo started those exact
   processes.

For a fresh rehearsal, rerun `prepare.sh`. It preserves the old marked demo root as a
timestamped backup instead of deleting it.
