# Presenter prompts

These prompts are written for the disposable Northstar Supply workspace created by
`prepare.sh`. Copy them as written during the first rehearsal; shorten them only after
the complete path is reliable on the presentation machine.

The exact coding path below was rehearsed with two green ways, a successful unique Judge
ranking, and a first-click Keep. The warm local ways took 58.8 seconds and 1 minute 32
seconds; budget two minutes each without promising that timing. Check the Automation
verification ledger in [README.md](README.md) before including prompts 4 or 5 on stage.

## 1. Diagnose with one agent

Use normal single-agent chat first.

```text
Inspect this storefront repository and run its documented check command. Identify the single failing customer-visible behavior in no more than three bullets. Do not change any files yet.
```

Expected proof:

- Files and shell tools stream into Activity.
- `npm run check` exposes the fixed-discount regression.
- Git remains clean.

Presenter action: collapse the Terminal dock after showing the red output. An expanded
bottom dock can cover the Activity composer and **Explore several ways** control even
though the preview server continues running.

## 2. Explore several ways

Collapse the Terminal dock, then turn on **Explore several ways** before pasting the task
into the Activity composer. Configure:

- Way 1: `Minimal Coder` / `qwen3:8b`
- Way 2: `Invariant Defender` / `qwen3:8b`
- Plan first: `Acceptance Planner`

Only after Explore mode and both ways are visibly configured, paste this task:

```text
Implement the fix we just diagnosed. A fixed discount greater than the subtotal must floor the payable amount at $0. Preserve cent rounding, percentage discounts, ordinary fixed discounts, and the no-discount case. Do not change tests. Run npm run check and report Root cause / Change / Proof.
```

Review the proposed plan, run **Check models**, then start both ways. Do not send this
task before Explore mode is active: doing so creates a normal primary-session turn and
can change the main workspace. While the ways run, show the independently updating
attempt cards, tool activity, and local known-zero cost.

After both ways settle, compare Outcome, changed paths, diff, and Route. Run Checks with
`npm run check`. Only when both non-empty ways pass, select `Evidence Judge` and Judge.
The rehearsed equivalent patches received unique ranks `#1` and `#2`; the judgment
explained the lower-index deterministic tie-break. The operator still chooses which way
to keep.

## 3. Verify the kept result

After Checks, Judge, and Keep:

```text
Review the current working tree, confirm exactly what changed, rerun npm run check in the primary session, and do not commit. Report Root cause / Change / Proof.
```

Then refresh the storefront preview. The conference cable pack should change from
`-$20.00 · Invariant broken` to `$0.00 · Ready`.

Expected proof:

- `lib/orders.js` is the only changed path in the primary workspace.
- all six checks pass in the primary session.
- Git shows the kept change as uncommitted; Keep did not commit or push.

## 4. Human-in-the-loop Automation

Open **Settings → Automations → Spec review · multi-perspective with HITL** and run
the seeded default input:

```text
A real-time chat app that stores every message in plaintext and has no rate limiting.
```

The blocking path should pause for operator guidance. A waiting utility should appear in
the session rail as `⏸ 1 waiting`; choose it to open the Interrupt panel, then resume
with:

```text
Reject the plaintext design. Require encryption at rest, tenant isolation, per-user and per-IP rate limiting, abuse monitoring, retention controls, and a threat model before implementation.
```

Expected proof:

- Text input feeds an Agent.
- Map runs security, performance, and UX reviews.
- Conditional detects `BLOCKING`.
- Interrupt waits for operator input.
- Resume feeds the Planner.
- Runs preserves the step record. Reopening **Runs** bypasses stale browser cache.

The rebuilt rehearsal verified a controlled daemon restart at this exact top-level parked
Interrupt: the rail rediscovered it, Resume continued to the Planner, the run completed,
and upstream checkpoints stayed single. This does not extend to a nested Subgraph
Interrupt or an arbitrary provider/tool call stopped in flight.

## 5. Skill and event Automation

Open **Settings → Skills → Demo health check** and choose **Fire**. The Skill publishes
`DemoRequested`; the seeded `Demo event response` Automation consumes the event.

Open **Automations → Demo event response → Runs** to show the event-triggered record. A
verified open Runs panel adds the new run as `running` after about 1.5 seconds and changes
it to `completed` without being closed. Reopening also bypasses stale browser cache.

## 6. Optional, unverified memory probe

This was not exercised in the timed rehearsal and is not part of the demo's proof. Use
it only after the core demo is green and only when a separate memory check is useful:

```text
Use your core-memory tool to remember this durable preference: When presenting work to Erick, summarize as Root cause / Change / Proof. Confirm only after saving it.
```

In a later session using the same Agent:

```text
What reporting format does Erick prefer? Answer in that format.
```

Core and semantic memory are shared by Agent identity; do not describe this as private
per-chat memory.

## Recovery and presenter fallbacks

If one attempt is still exploring after the other finishes:

```text
Finish the scoped fix now. Run npm run check and report the result without adding unrelated changes.
```

Use that only through an explicit attempt-steering surface if one is visible. Do not send
it as a new primary-session turn while the attempt set is unresolved. Waiting for the
scoped way or using its visible interrupt/cleanup action is the safe presenter choice.

If exactly one non-empty attempt passes, skip Judge. Checks already made the decision.

If both non-empty attempts pass but Judge reports that ranks must be unique and cover
`1..N`:

1. Leave the attempt set in place; the Judge error does not erase either way.
2. Confirm both Checks are still green.
3. Select `Evidence Judge` again, especially after a page reload, and retry Judge once.
4. If it fails again, skip Judge and choose from Checks, diff, Outcome, and Route.

If a page reload leaves Compare at “Reading the attempts…”, press **Review outcomes**
once. If the Browser preview is blank after a daemon restart, rerun `npm run demo` in the
restored primary Terminal and refresh `http://localhost:8765`.

If no attempt passes, use **Finish without keeping**, explain what the failed evidence
proved, and stop the coding segment. Reset with `prepare.sh` only after the audience
segment. Do not hide the failed state; this kit does not provision a known-good backup
workspace.
