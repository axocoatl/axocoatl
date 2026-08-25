# Multi-agent design handoff and Agent graph

This film distinguishes fixed Agent collaboration inside one Session from
parallel competing Ways. The deterministic task is a short design review with
no tools or repository changes.

## Claim

A Custom multi-agent Session can connect configured Agents in a dependency
order, activate them sequentially, pass the earlier response to the downstream
Agent, and retain separate labeled outputs on one durable Turn.

## Do not claim

- This is not parallel execution or several Ways. The architect completes
  before the reviewer becomes active.
- This film does not prove a code change, repository repair, executable check,
  or design correctness. Neither Agent uses tools, files, or edits.
- The reviewer's `SHIP` or `BLOCK` applies only to the architect's one-sentence
  proposal. It is not a release gate for working software.
- The Agent graph is not the event lattice.
- This fixed handoff is not the dynamic Coordinator auction/decomposition path.

## Start or reset

After closing previous demo Sessions and stopping the daemon:

```bash
./demo/one-app/prepare.sh --scenario harbor-catalog
AXOCOATL_DEMO_ROOT=/private/tmp/axocoatl-one-app-showcase-harbor-catalog \
  ./demo/one-app/start.sh
```

Create a Session for
`/private/tmp/axocoatl-one-app-showcase-harbor-catalog/workspace` and choose
**Custom workflow**. Select exactly:

1. **Systems Architect** (`architect`)
2. **Critical Reviewer** (`reviewer`), dependent on `architect`

Confirm the creation summary shows `reviewer ← architect`, then create the
Session and rename it **Film · Handoff sequence**. Before recording, confirm the
Session header lists exactly those two Agents.

## Browser actions

1. Open **Agent graph** before sending the task. Confirm two idle nodes and the
   configured `architect → reviewer` dependency edge.
2. Return to Conversation. Keep the target at
   **send to · all · agent defaults** and send this exact prompt:

   ```text
   Design a safe in-memory catalog cache invalidation rule after add, update, and remove mutations. Follow only your configured role. The reviewer must end with a standalone line containing exactly SHIP or BLOCK. This is a design-only handoff; do not inspect the repository and do not use tools. /no_think
   ```

3. Reopen or focus **Agent graph** while the Turn runs.
4. Capture the architect becoming active first. Hold through its completed
   state, then capture the reviewer becoming active. Do not cut the film to
   imply overlap.
5. Return to Conversation after the Turn settles. Show the non-empty,
   separately labeled architect and reviewer outputs. The reviewer output must
   assess the architect's proposal and end with `SHIP` or `BLOCK`.
6. Reload, reopen **Film · Handoff sequence** from All Sessions, and show that both
   labeled outputs remain on the same completed Turn.

## Visible proof

- The `architect → reviewer` edge is visible before execution.
- Active state advances from architect to reviewer without simultaneous
  activation.
- The transcript contains a non-empty architect output and a separate
  non-empty reviewer output on one Turn.
- Reopening the Session preserves both labeled outputs.
- No tool call, file edit, or repository-result claim appears in the film.

## Durable evidence

```bash
export AXO_DEMO_URL='http://127.0.0.1:18080'
export AXO_DEMO_ROOT='/private/tmp/axocoatl-one-app-showcase-harbor-catalog'
curl -sS "$AXO_DEMO_URL/api/sessions"
```

Copy the Film · Handoff sequence Session id, then:

```bash
export AXO_SESSION_ID='ses-paste-the-id-here'
curl -sS "$AXO_DEMO_URL/api/sessions/$AXO_SESSION_ID/turns"
grep -F "$AXO_SESSION_ID" \
  "$AXO_DEMO_ROOT/data/session-history/turns.v1.jsonl"
git -C "$AXO_DEMO_ROOT/workspace" status --short
```

The Session record establishes the Custom multi-agent mode and configured
roster. The canonical Turn response and ledger establish the two non-empty
per-Agent outputs and completed lifecycle. An empty Git status confirms that
this design-only prompt did not become a repository-edit claim. A scheduled
Agent with a blank output must make the Turn fail; it must never be represented
as a successful empty handoff.

## Recording beats

1. Establish the **Film · Handoff sequence** Session and two-Agent roster.
2. Open Agent graph on the two idle nodes and dependency edge.
3. Send the exact one-sentence design-review prompt.
4. Show active state moving from architect to reviewer in order.
5. Reload and end on the two separately labeled, durable outputs in Conversation.

Target 25–35 seconds after editing. Preserve the transition between Agents and
enough of both sentences to make the handoff legible.

## Cleanup

1. Wait for the shared Turn to become completed, failed, or cancelled and
   capture its per-Agent evidence.
2. Close the Session from **All sessions**, then stop `start.sh` with Ctrl-C.
3. Reset with:

   ```bash
   ./demo/one-app/prepare.sh --scenario harbor-catalog
   ```

   Do not delete a running Session container to interrupt the handoff.

## Known constraints

- Multi-agent Session dependencies execute in topological order; this scenario
  intentionally proves sequential handoff rather than concurrency.
- **Explore several ways** is unavailable in a multi-agent Session.
- If either output is empty, the reviewer fails to address the architect's
  sentence, or the reviewer omits the required terminal word, keep that Turn as
  truthful rehearsal evidence and record a new clean take.
- The film proves configured orchestration and durable outputs. It does not
  independently validate the proposal's technical quality.
