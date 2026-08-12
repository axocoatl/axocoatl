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
- [ ] Browser at `http://localhost:8765` shows `-$20.00 · Invariant broken`.
- [ ] Collapse the Terminal dock so it cannot cover the Activity composer.

## Core 20-minute path

### 1. One workbench — 2 minutes

- [ ] Show the workspace and session in the rail.
- [ ] Point out Activity, right-side Attempts, bottom Terminal, Files, Browser, and Git.
- [ ] Open **All sessions** briefly, then return to this session.

### 2. Diagnose — 3 minutes

- [ ] Send prompt 1 exactly.
- [ ] Show `npm run check` red and the matching negative total in Browser.
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
- [ ] Browser now shows `$0.00 · Ready`.
- [ ] Git → **Last turn** shows only the uncommitted `lib/orders.js` change.
- [ ] Do not commit unless committing is explicitly part of this showing.

### 6. Recovery — 1 minute

- [ ] Reload `/`; confirm the same session and attempt decision record.
- [ ] If Compare says “Reading the attempts…”, press **Review outcomes** once.

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

## Fast fallbacks

- One passing way: skip Judge and keep the checked survivor.
- Two passing ways plus Judge ranking error: preserve the ways, reselect `Evidence Judge`,
  retry once, then decide from Checks/diff/Outcome/Route.
- No passing ways: show the failure and choose **Finish without keeping**.
- Explore control cannot be clicked: collapse the Terminal dock.
- Blank preview after daemon restart: rerun `npm run demo`, then refresh Browser.
- Missing model: stop; verify `ollama list`; do not silently change models.

## Shutdown

- [ ] Keep or Finish without keeping every attempt set.
- [ ] Resume or cancel every waiting Automation Interrupt.
- [ ] Close the demo session from the rail.
- [ ] Stop the foreground daemon with Ctrl+C.
- [ ] Leave Ollama and Podman alone unless this demo started those exact processes.
