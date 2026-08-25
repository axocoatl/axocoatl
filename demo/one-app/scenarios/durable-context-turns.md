# Durable context and Turns

This film proves that attachments, active execution, Stop ownership, and
History belong to the Session's durable Turn model.

## Claim

Axocoatl records accepted Turn context, survives browser reload during a live
Turn, restores the exact active Turn and Stop control, preserves an honest
cancelled result with partial output and immutable context evidence, and starts
the next Turn without leaking the cancelled actor's output or tool work.

## Do not claim

- Stop is not rollback. A tool already dispatched may finish at its safe
  boundary, and filesystem or external side effects remain.
- Removing Session context does not securely erase the immutable blob.
- Session attachments are not passed into isolated Ways.
- This scenario proves a browser reload, not recovery of an arbitrary provider
  call after a daemon crash.

## Start or reset

Close prior demo Sessions, stop the daemon, and prepare a fresh root:

```bash
./demo/one-app/prepare.sh --scenario harbor-catalog
AXOCOATL_DEMO_ROOT=/private/tmp/axocoatl-one-app-showcase-harbor-catalog \
  ./demo/one-app/start.sh
```

Create a **Single agent** Session using **Minimal Coder** for
`/private/tmp/axocoatl-one-app-showcase-harbor-catalog/workspace`. Keep
**Explore several ways** off for this entire scenario.

## Browser actions

### Once context

1. Choose the paperclip and attach
   `/private/tmp/axocoatl-one-app-showcase-harbor-catalog/workspace/README.md`.
   Leave this chip set to **Once**.
2. Attach `AXOCOATL.md` from the same workspace and change that chip to
   **Session**. Send:

   ```text
   Using only the attached README.md and AXOCOATL.md, reply exactly: CONTEXT ACCEPTED. Do not call tools. /no_think
   ```

3. After the Turn is accepted, confirm that the README.md **Once** chip is
   consumed, the AXOCOATL.md **Session** chip remains selected, and both
   historical attachment chips remain clickable on that Turn.

### Session context

4. Confirm that AXOCOATL.md remains selected for the next Turn. Do not remove
   it until after the clean-next-Turn evidence has been captured; its continued
   selection is part of the accepted film.

### Reload and exact Stop

5. Send this long no-tool Turn:

   ```text
   Using the retained AXOCOATL.md Session context, write a detailed 30-point repository handoff checklist. Do not call tools. /no_think
   ```

6. Once streaming has begun, reload the browser.
7. Confirm that partial output reappears and **Stop** belongs to the same active
   Turn. Choose **Stop** once.
8. Wait for the safe cancellation boundary and open **History**. The Turn must
   appear as cancelled with `Stopped by you` and any honest partial output.
9. Close History and send this exact follow-up:

    ```text
    Reply with exactly: CLEAN NEXT TURN. Do not call tools. /no_think
    ```

10. The new Turn must complete with exactly `CLEAN NEXT TURN.`, no stale audit
    continuation, and no tool card inherited from the cancelled Turn.

## Visible proof

- README.md **Once** context disappears from future composition only after
  acceptance.
- AXOCOATL.md **Session** context remains selected on later Turns.
- Both historical attachments remain linked to the Turn that used them.
- Reload restores the in-flight Turn and exact **Stop** action.
- History shows a durable cancelled lifecycle instead of silently dropping the
  partial result.
- The next accepted Turn begins cleanly after the cancelled actor is removed.

## Durable evidence

```bash
export AXO_DEMO_URL='http://127.0.0.1:18080'
export AXO_DEMO_ROOT='/private/tmp/axocoatl-one-app-showcase-harbor-catalog'
curl -sS "$AXO_DEMO_URL/api/sessions"
```

Copy the Session id, then:

```bash
export AXO_SESSION_ID='ses-paste-the-id-here'
curl -sS "$AXO_DEMO_URL/api/sessions/$AXO_SESSION_ID/attachments"
curl -sS "$AXO_DEMO_URL/api/sessions/$AXO_SESSION_ID/turns"
grep -F "$AXO_SESSION_ID" \
  "$AXO_DEMO_ROOT/data/session-history/turns.v1.jsonl"
sed -n '1,220p' \
  "$AXO_DEMO_ROOT/data/session-history/session-attachments.v1.json"
```

The Turn ledger is the lifecycle authority. The attachment relation store
records name, scope, consumption, and historical ownership; immutable bytes
live separately in the content-addressed file store. The Turn list must show
the cancelled audit followed by a distinct completed `CLEAN NEXT TURN.` Turn
with no tool records.

## Recording beats

1. Attach README.md as **Once** and AXOCOATL.md as **Session**, send, and show
   the accepted context relation.
2. Start the long no-tool handoff checklist and reload while output is visibly
   partial.
3. Show that the AXOCOATL.md Session chip remains selected.
4. Show the restored **Stop**, press it, and wait for `Stopped by you`.
5. Open History and end on the cancelled Turn plus retained context evidence.
6. Send the exact clean-next-Turn probe and show `CLEAN NEXT TURN.` with no
   tools.

Target 40–55 seconds after editing. The reload and Stop transition should play
at normal speed because they carry the claim; compress earlier attachment
holds before dropping the clean-next-Turn proof.

## Cleanup

1. Wait until Stop has reached its terminal cancelled state and save the Turn
   and attachment API evidence.
2. Close the Session from **All sessions**, then stop `start.sh` with Ctrl-C.
3. Reset with `./demo/one-app/prepare.sh --scenario harbor-catalog`. Do not
   manually remove attachment blobs or edit the Turn ledger; the fresh marked
   root is the reset boundary.

## Known constraints

- One normal Turn may be active per Session.
- A **Once** attachment is consumed only after the exact Turn begin is durable.
- Declared images are limited to 10 MiB, other documents to 25 MiB, and
  extracted/OCR context is bounded to 256 KiB.
- Rewind is logical transcript history. It does not undo repository or external
  side effects and is outside this film's scope.
