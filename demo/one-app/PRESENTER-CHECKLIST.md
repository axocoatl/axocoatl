# Axocoatl one-app presenter checklist

Use this page at the podium. The full recovery detail is in
[README.md](README.md); all copy-ready text is in [PROMPTS.md](PROMPTS.md).

## Before the room opens

- [ ] No Axocoatl daemon is running, including an earlier demo run, and no
  `axo-ses-*` container exists at all.
- [ ] `podman system check --quick` is clean.
- [ ] `ollama list` includes `qwen3:8b`.
- [ ] Ports `18080` and `8765` are free.
- [ ] `DEMO_ROOT` is `/private/tmp/axocoatl-one-app-showcase` on macOS or
  `/tmp/axocoatl-one-app-showcase` on Linux.
- [ ] Run `./demo/one-app/prepare.sh` from the Axocoatl repository.
- [ ] In terminal A, run `./demo/one-app/start.sh` and leave it foregrounded.
- [ ] In terminal B, run `./demo/one-app/seed-automation.sh` after readiness.
- [ ] Open `http://127.0.0.1:18080` and create the prepared session:
  - [ ] authorize `$DEMO_ROOT/workspace` using its expanded platform path;
  - [ ] choose **Single agent** with `Minimal Coder`;
  - [ ] keep the detected `npm run check` command;
  - [ ] keep image `localhost/axocoatl-one-app-demo:latest`;
  - [ ] keep exposed port `8765`.
- [ ] Start `npm run demo` in the session Terminal.
- [ ] Preview at `http://localhost:8765` shows `-$20.00 · Invariant broken`.
- [ ] Collapse the Terminal dock so the conversation and composer stay prominent.

## 1.0 recording gate

Use this gate for every product film; the podium path above may still use the
debug binary for rehearsal.

- [ ] Build once with `cargo build --release --locked -p axocoatl-cli`.
- [ ] Start with absolute
  `AXOCOATL_DEMO_BIN=$PWD/target/release/axocoatl`; record the printed binary
  path, version, and SHA-256.
- [ ] Run `node demo/one-app/films/verify-film-set.mjs --manifest-only` before
  capturing; it must report all 12 scenarios and shot contracts.
- [ ] Capture each manifest beat from the same release binary at 1280×720,
  zoom 100%, device scale factor 1, with stable theme and Session identity.
- [ ] Save exact durable Session/Turn, attempt-set, run, checkout, or MCP
  evidence identities before resetting a scenario root.
- [ ] Ingest `shot-<beat>.jpg` and `timeline.json`, stage holds, encode, and
  write provenance using [`films/SHOT-MANIFEST.md`](films/SHOT-MANIFEST.md).
- [ ] Inspect the encoded MP4, poster, light/dark use as applicable, narrow-page
  placement, and reduced-motion fallback on the built marketing site.
- [ ] Mark a portfolio entry `ready` only after its scenario, media, duration,
  poster beat, evidence, and provenance all pass.
- [ ] Run `node demo/one-app/films/verify-film-set.mjs --source-bound`; all 12
  source digests must match the checkout used for a new capture and release.
- [ ] For every new capture and ordinary future release, run unflagged
  `node demo/one-app/films/verify-film-set.mjs` before launch; all 12 must pass
  release-strict against the preserved capture binary and source checkout.
- [ ] Confirm all 12 provenance JSONs are byte-for-byte restored from their first
  commit. The declared binary bytes are not preserved, so do not describe its
  path/version/hash as independently authenticated capture-binary evidence.
- [ ] For the `v1.0.1` incident only, substitute the reviewed compatibility
  attestation for that ordinary unflagged launch gate. Run it
  against a clean frozen Git checkout with `--release-compatibility <attestation>
  --release-root <checkout>`. Require the reported split of 55 exact paths: 43
  non-recording changes plus 12 audited source/binary-only provenance rewrites.
- [ ] Treat compatibility as evidence only for the visible recorded film claims.
  The films were not captured with the `v1.0.1` binary. For every future capture,
  retain the strict ordinary source-bound contract and never rewrite provenance.

## Core 20-minute path

### 1. One workbench — 2 minutes

- [ ] Show the workspace and session in the rail.
- [ ] Point out the Conversation canvas, focused Files and Preview tools, **More**,
  the contextual Ways inspector, bottom Terminal, and Source Control.
- [ ] Open **All sessions** briefly, then return to this session.

### 2. Diagnose — 3 minutes

- [ ] Send prompt 1 exactly.
- [ ] Show `npm run check` red and the matching negative total in Preview.
- [ ] Show Git still clean.
- [ ] Collapse Terminal again.

### 3. Explore — 6 minutes

- [ ] Confirm the Terminal dock is collapsed.
- [ ] Choose **Explore several ways** before pasting prompt 2.
- [ ] Way 1: `Minimal Coder`; Way 2: `Invariant Defender`.
- [ ] Enable **Plan first** with `Acceptance Planner`.
- [ ] Paste prompt 2 only after Explore mode and both ways are visibly configured.
- [ ] Review the plan and run **Check models**.
- [ ] Start both ways; budget two minutes per way.
- [ ] While waiting, narrate live tools, files, roster, and local known-zero cost.

### 4. Compare and decide — 5 minutes

- [ ] Compare Outcome, changed paths, diff, **Only differences**, and Route.
- [ ] Run Checks; require a non-empty change and green `npm run check`.
- [ ] Select `Evidence Judge`; after a reload, select it again.
- [ ] Judge only when two ways survive.
- [ ] Read the recommendation, then choose **Keep this one** yourself.

### 5. Prove Keep — 3 minutes

- [ ] Send prompt 3 exactly.
- [ ] Primary `npm run check` reports all six green.
- [ ] Preview now shows `$0.00 · Ready`.
- [ ] Git → **Last turn** shows only the uncommitted `lib/orders.js` change.
- [ ] Do not commit unless committing is explicitly part of this showing.

### 6. Recovery — 1 minute

- [ ] Reload `/`; confirm the same session and attempt decision record.
- [ ] If Attempts review says “Reading the attempts…”, press **Review outcomes** once.

## Automation segment gate

Include prompts 4 and 5 only after all four presentation checks are green on the exact
build being shown:

- [x] Reopened Runs history bypasses stale browser cache.
- [x] An already-open Runs panel visibly updates through live polling.
- [x] The rail's `⏸ waiting` utility visibly opens the Interrupt panel.
- [x] A parked Interrupt is rediscovered and resumable after a daemon restart.

These checks are green on the rebuilt embedded page. Restart recovery is limited to a
top-level parked Interrupt; do not claim recovery for a nested Subgraph Interrupt or an
arbitrary provider/tool call stopped in flight.

## Session capability verification record

The checked facts passed on the rebuilt app on 13 August 2026. Recheck them on the exact build
being shown; unchecked items remain presentation gates:

- [x] An `AXOCOATL.md` **Once** attachment is accepted, appears on the durable turn, and
  disappears from the composer only after acceptance.
- [x] The answer uses the attachment content and the historical context chip remains clickable.
- [ ] A **Session** attachment remains selected for a second normal turn; Remove stops future
  inclusion while the attachment link on the historical turn still opens.
- [x] Reload during generation reattaches with partial output and restores **Stop**; pressing it
  persists `Stopped by you` in History.
- [ ] An already-started side-effecting tool reaches its safe boundary before Stop settles.
- [x] **History** renders four durable turns and the Session query returns exactly one result.
- [ ] The same query works in all-Session scope.
- [ ] Markdown and JSON exports download.
- [ ] Rewind hides the later harmless turn without claiming tool or Git rollback.

Do not attach context while **Explore several ways** is enabled; Attempts do not currently
receive composer attachments. Run the rewind proof only in a single-agent Session.

## Fast fallbacks

- One passing way: skip Judge and keep the checked survivor.
- Two passing ways plus Judge ranking error: preserve the ways, reselect `Evidence Judge`,
  retry once, then decide from Checks/diff/Outcome/Route.
- No passing ways: show the failure and choose **Finish without keeping**.
- Explore control cannot be clicked: collapse the Terminal dock.
- Blank Preview after daemon restart: rerun `npm run demo`, then refresh Preview.
- Missing model: stop; verify `ollama list`; do not silently change models.

## Shutdown

- [ ] Keep or Finish without keeping every attempt set.
- [ ] Resume or cancel every waiting Automation Interrupt.
- [ ] Close the demo session from the rail.
- [ ] Stop the foreground daemon with Ctrl+C.
- [ ] Leave Ollama and Podman alone unless this demo started those exact processes.
