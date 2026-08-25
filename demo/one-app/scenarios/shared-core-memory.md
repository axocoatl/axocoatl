# Shared core memory across Sessions

This runtime-depth film demonstrates one explicit, inspectable Tier-3 memory
contract. It should be recorded only after the launch-set films are stable.

## Claim

Two configured Agents that opt into the same shared `team` core-memory block
see one persisted value across different Sessions and a daemon restart. One
Agent writes through `core_memory_set`; another receives the saved block in its
system context and recalls it without reading the first Session transcript.

## Do not claim

- This is shared core memory, not semantic search or automatic long-term recall.
- Different Agents do not share every memory by default. Only labels configured
  with `shared: true` use the shared registry.
- Shared memory is not a privacy boundary between those Agents.
- Ways do not receive writable shared core-memory blocks.

## Start or reset

This proof requires a fresh data directory so the shared block starts empty:

```bash
./demo/one-app/prepare.sh --scenario signal-desk
AXOCOATL_DEMO_ROOT=/private/tmp/axocoatl-one-app-showcase-signal-desk \
  ./demo/one-app/start.sh
```

Create a **Single agent** Session using **Minimal Coder** for the Signal Desk
workspace. Rename it `Remember release language`.

## Browser actions

### Write from the first Agent

1. Send this exact prompt:

   ```text
   Call core_memory_set with block "team" and value exactly "AXO-DEMO-JADE-731: release summaries use Root cause / Change / Proof". Do not merely promise to remember it. Confirm only after the tool succeeds.
   ```

2. Show the `core_memory_set` tool start and successful result on the Turn.
3. Confirm the file evidence below before restarting.

### Restart without resetting data

4. Stop the daemon with Ctrl-C. Do not rerun `prepare.sh` and do not close the
   first Session.
5. Restart the same root:

   ```bash
   AXOCOATL_DEMO_ROOT=/private/tmp/axocoatl-one-app-showcase-signal-desk \
     ./demo/one-app/start.sh
   ```

6. Reload the browser and show `Remember release language` still present.

### Recall from a different Agent and Session

7. From **All sessions**, create a second **Single agent** Session in the same
   workspace using **Invariant Defender**. Rename it `Recall release language`.
8. Send:

   ```text
   Do not read files, search history, or call tools. Quote the current shared team core-memory value exactly, then state in one sentence why it is available to you.
   ```

9. The answer must include the exact `AXO-DEMO-JADE-731` value. Switch back to
   the first Session briefly to show that the two transcripts remain separate.

## Visible proof

- The first Turn visibly calls `core_memory_set`, not just conversationally
  asserting that it remembered something.
- The first Session survives daemon restart.
- A different configured Agent in a different Session quotes the exact nonce
  without reading files or the first transcript.
- The two Session histories remain distinct even though the `team` block is
  intentionally shared.

## Durable evidence

Before and after restart:

```bash
export AXO_DEMO_URL='http://127.0.0.1:18080'
export AXO_DEMO_ROOT='/private/tmp/axocoatl-one-app-showcase-signal-desk'
sed -n '1,160p' \
  "$AXO_DEMO_ROOT/data/memory/core/shared/team.json"
curl -sS "$AXO_DEMO_URL/api/sessions"
grep -F 'AXO-DEMO-JADE-731' \
  "$AXO_DEMO_ROOT/data/session-history/turns.v1.jsonl"
```

Copy each Session id and inspect both Turn lists independently:

```bash
export AXO_WRITER_SESSION_ID='ses-paste-writer-id-here'
export AXO_READER_SESSION_ID='ses-paste-reader-id-here'
curl -sS "$AXO_DEMO_URL/api/sessions/$AXO_WRITER_SESSION_ID/turns"
curl -sS "$AXO_DEMO_URL/api/sessions/$AXO_READER_SESSION_ID/turns"
```

The shared block file is the Tier-3 persistence proof. The separate Turn lists
show that cross-Agent recall did not come from a shared Session transcript.

## Recording beats

1. Create the writer Session and show the explicit `core_memory_set` call.
2. Cut to the persisted `team.json` value, then show daemon stop/start.
3. Reopen the first Session, create the Defender Session, and ask for the nonce.
4. End on the exact recalled value with both separate Session names visible in
   **All sessions**.

Target 35–50 seconds after editing. Keep the unique nonce readable in the tool
call, persisted file, and second Agent answer.

## Cleanup

1. Capture `team.json` and both Session Turn lists before resetting.
2. Close both Sessions from **All sessions**, then stop `start.sh` with Ctrl-C.
3. Reset with `./demo/one-app/prepare.sh --scenario signal-desk`. This rotates
   the entire marked data root; do not edit or truncate `team.json` in place
   because that would no longer prove the product path.

## Known constraints

- The first registration of a shared label defines the block metadata for the
  process; all configured Agents referring to that label share its value.
- Core memory is intentionally small and editable. `core_memory_set` overwrites
  the whole block, so this scenario must use a fresh demo data directory.
- Tier-2 daily logs and Tier-4 semantic memory have different behavior and are
  not proven here.
- If the first model does not emit the required tool call, the scenario has not
  passed. Reset and rehearse; do not seed the file by hand for the film.
