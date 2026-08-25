# Single-agent workbench

This film proves that the ordinary Axocoatl path is a complete coding
workbench, not a reduced mode before multi-agent features.

## Claim

One configured Agent can inspect a real repository, run its checks, make a
focused change inside the Session sandbox, and leave the result available in
Files, Preview, Terminal, Source Control, and the durable conversation.

## Do not claim

- The Agent does not commit, push, deploy, or release unless the operator asks.
- A green model-written summary is not proof; the repository check, diff, and
  visible Preview are the proof.
- This film does not demonstrate Ways or a multi-agent handoff.

## Start or reset

After closing prior demo Sessions and stopping their daemon:

```bash
./demo/one-app/prepare.sh
./demo/one-app/start.sh
```

Open `http://127.0.0.1:18080`, create a **Single agent** Session for
`/private/tmp/axocoatl-one-app-showcase/workspace`, and select
**Minimal Coder**. Keep the detected
`localhost/axocoatl-one-app-demo:latest` image and exposed port `8765`.

## Browser actions

1. Open Terminal and run:

   ```bash
   npm run demo
   ```

2. Open Preview at `http://localhost:8765`. Confirm the conference cable pack
   reads `-$20.00` and `Invariant broken`.
3. Return to Conversation and send:

   ```text
   Inspect this storefront repository, run its documented check, and repair the customer-visible fixed-discount defect. A fixed discount greater than the subtotal must floor the payable amount at $0. Preserve cent rounding, percentage discounts, ordinary fixed discounts, and the no-discount case. Do not change tests or commit. Run npm run check and report Root cause / Change / Proof.
   ```

4. Let the Turn reach a terminal result. Show the live tool cards while it runs.
5. Open Files and inspect `lib/orders.js` and `lib/orders.test.js`.
6. Open Source Control and inspect the actual diff. It should touch production
   code, not weaken the tests.
7. Open a fresh Terminal and run:

   ```bash
   npm run check
   ```

8. Refresh Preview. The same product card should now read `$0.00` and `Ready`.
9. Return to Conversation to show the Agent's Root cause / Change / Proof
   answer, then end in Source Control on the uncommitted `lib/orders.js` hunk.

## Visible proof

- File reads, shell commands, and the edit stream in one Session Turn.
- The initial Preview and red check expose the same defect.
- The diff is focused in `lib/orders.js` and the six checks pass afterward.
- Preview reflects the changed checkout, while Source Control keeps the change
  uncommitted and reviewable.

## Durable evidence

```bash
export AXO_DEMO_URL='http://127.0.0.1:18080'
export AXO_DEMO_ROOT='/private/tmp/axocoatl-one-app-showcase'
git -C "$AXO_DEMO_ROOT/workspace" status --short
git -C "$AXO_DEMO_ROOT/workspace" diff -- lib/orders.js
npm --prefix "$AXO_DEMO_ROOT/workspace" run check
curl -sS "$AXO_DEMO_URL/api/sessions"
```

Copy the Session id, then inspect its canonical Turns and Git projection:

```bash
export AXO_SESSION_ID='ses-paste-the-id-here'
curl -sS "$AXO_DEMO_URL/api/sessions/$AXO_SESSION_ID/turns"
curl -sS "$AXO_DEMO_URL/api/sessions/$AXO_SESSION_ID/git/status"
curl -sS \
  "$AXO_DEMO_URL/api/sessions/$AXO_SESSION_ID/git/diff?path=lib%2Forders.js"
grep -F "$AXO_SESSION_ID" \
  "$AXO_DEMO_ROOT/data/session-history/turns.v1.jsonl"
```

## Recording beats

1. Open on the broken Preview, then reveal the matching red check.
2. Send the task and show a compact sequence of real repository tools.
3. Move from completed answer to Files and the focused diff.
4. Show the green check.
5. Refresh Preview to show `$0.00 · Ready`, then end in Source Control on the
   uncommitted production hunk.

Target 35–50 seconds after editing. Do not speed through the before/after
states so quickly that the invariant is unreadable.

## Cleanup

1. Capture the final Git diff, green check, and Turn evidence before cleanup.
2. Stop `npm run demo` in its Terminal, then close the Session from
   **All sessions**.
3. Stop `start.sh` with Ctrl-C.
4. Reset with `./demo/one-app/prepare.sh`; it preserves the previous marked
   root as a timestamped backup.

## Known constraints

- Local model timing and the exact prose patch explanation vary. Rehearse until
  the actual patch and six-check result are correct; never substitute a staged
  screenshot for a failed run.
- Cancellation is cooperative at tool boundaries. This scenario lets the Turn
  finish; exact Stop is covered by
  [Durable context and Turns](durable-context-turns.md).
- The container has a read-write bind mount of this disposable workspace. The
  sandbox boundary is shown separately in
  [Sandbox, Terminal, and Preview](sandbox-preview.md).
