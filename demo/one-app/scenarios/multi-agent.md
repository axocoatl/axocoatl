# Lead with read-only helpers

This film shows the default native team: one Lead owns the change, and two
read-only helpers answer the questions it delegates. It distinguishes team
collaboration toward one result from parallel competing Ways.

## Claim

In a native Session, **Team and budget** approves the Lead, its maximum output,
its finite limits, the required check `npm run check`, and two read-only
helpers, **Scout** and **Reviewer**, each with limits reserved from the Lead's
budget. During one Turn the Lead delegates a question to Scout, edits the
repository, runs the check, and delegates a review of its diff to Reviewer. The
Turn's execution graph records each helper run with a `delegated_by` edge to the
Lead; only the Lead changes files; the host-run required check passes; and the
helper answers remain in History after reload.

## Do not claim

- This is not parallel execution or several Ways. A helper runs inside the
  Lead's activation while the Lead waits for its answer.
- Scout and Reviewer are read-only helpers (`writes: []`). They are not offered
  file-writing tools, and they do not change the repository.
- Reviewer's answer is advice to the Lead. This film does not use a required
  review, and it does not claim the review proves correctness.
- This is not a Coordinator decomposition or unlimited autonomous delegation:
  the Lead may add at most the approved number of helper runs.
- The local model's prose varies between takes. The accepted take is judged by
  the retained delegation edges, changed paths, and check result, not by wording.

## Start or reset

Follow [Native Session capture setup](../films/NATIVE-CAPTURE.md) with the
`harbor-catalog` fixture, its own fresh root, real local Ollama calls, and the
exact candidate binary at `http://127.0.0.1:18080`. Start the daemon with the
team configuration so Team and budget offers the helpers:

```bash
export AXOCOATL_DEMO_ROOT=/private/tmp/axocoatl-one-app-showcase-harbor-team
./demo/one-app/prepare.sh --scenario harbor-catalog
AXOCOATL_DEMO_CONFIG=axocoatl.team.yaml ./demo/one-app/start.sh
```

[`axocoatl.team.yaml`](../axocoatl.team.yaml) is the default team that
`axocoatl init` writes: **Lead** (may change files), **Scout** and **Reviewer**
(Workers with `writes: []`), each with a 4096-token maximum output. The other
films use `axocoatl.demo.yaml`, which has no Worker templates, so their Team and
budget does not offer helpers.

Open the prepared Workspace and create a **Single agent** Session named
`Film · Lead with helpers` with **Lead**, the detected image, and the detected
check. Verify `history_version: execution_v2` and Ready before opening Team and
budget.

## Browser actions

1. Open **Team and budget** and choose **Edit**. Select **Lead** and review its
   provider, model, `Maximum output tokens per request`, and **May change: Any
   file**. Enter finite activation, invocation, and token limits, zero local
   cost, and a future expiry.
2. Under **Helpers this Agent may delegate to**, select **Let this Agent
   delegate to helpers** and keep both **Use helper: Scout** and **Use helper:
   Reviewer**. Keep the offered graph bounds, and enter each helper's finite
   limits and maximum output within the Lead's budget.
3. Add the detected `npm run check` under **Required checks**. Do not add a
   required review. Choose **Preview changes**, inspect the result, and
   **Apply to this Session**. Hold on the reviewed team: Lead may change files,
   both helpers are read-only, their limits are visible, and the required check
   is listed.
4. Close Team and budget and send this exact prompt:

   ```text
   Repair the catalog cache-coherency defect: search results must reflect additions, updates, and removals after a query has been cached. First delegate to scout: ask which files and tests define the search cache and how it is invalidated. Then make the smallest production change, keep the public API and caching, and do not change tests. Run npm run check. Then delegate to reviewer: ask it to review your diff of lib/catalog.js against the three cache tests. Report Change / Check / Review.
   ```

5. While the Turn runs, open **Agent graph**. Capture the Scout helper run under
   the Lead, then Scout's answer naming file paths.
6. Capture the Lead's edit of `lib/catalog.js`, then the Reviewer helper run
   under the Lead with its findings.
7. After the Turn completes and the required check passes, reload, reopen the
   Session, and open **History**. Show the completed Turn, the passed required
   check, and the Scout and Reviewer answers.

## Visible proof

- Team and budget shows one Lead that may change files, two read-only helpers
  with explicit limits, and the required check before the Turn is sent.
- The execution graph shows Scout and Reviewer helper runs delegated by the Lead,
  in that order.
- Scout answers with file paths; Reviewer answers about the Lead's diff.
- Only `lib/catalog.js` changes, and `npm run check` passes as the host-run
  required check.
- The completed Turn and both helper answers survive reload.

## Durable evidence

```bash
export AXO_DEMO_URL='http://127.0.0.1:18080'
export AXO_DEMO_ROOT='/private/tmp/axocoatl-one-app-showcase-harbor-team'
# The local API needs this daemon's token; curl reads the header from stdin.
axo_api() { printf 'Authorization: Bearer %s\n' "$(cat "$AXO_DEMO_ROOT/data/local-api-token")" | curl -sS -H @- "$@"; }
axo_api "$AXO_DEMO_URL/api/sessions"
```

Copy the `Film · Lead with helpers` Session id, then:

```bash
export AXO_SESSION_ID='ses-paste-the-id-here'
axo_api "$AXO_DEMO_URL/api/sessions/$AXO_SESSION_ID/team"
axo_api "$AXO_DEMO_URL/api/sessions/$AXO_SESSION_ID/turns?history_version=2"
git -C "$AXO_DEMO_ROOT/workspace" status --short
git -C "$AXO_DEMO_ROOT/workspace" diff --stat
npm --prefix "$AXO_DEMO_ROOT/workspace" run check
```

Copy the native turn id and retain
`GET /api/sessions/{session_id}/turns/{turn_id}/control-plane`. Its graph must
contain the Lead node plus one Scout and one Reviewer helper node, each joined to
the Lead by a `delegated_by` edge, and the required-check record for
`npm run check`. The team response must show the approved helper limits and
`required_checks`. Git status must show only `lib/catalog.js`, and the host check
must pass. Use the native `execution-v2/` records under the same data root as the
durable authority.

## Recording beats

1. `team-review`: the applied team with the Lead, both read-only helpers, their
   limits, and the required check.
2. `scout`: the Lead-to-Scout delegation and Scout's answer with file paths.
3. `review`: the Lead's `lib/catalog.js` edit and the Lead-to-Reviewer
   delegation with its findings.
4. `checked-result`: the completed Turn with the passed required check and both
   helper answers in History after reload. This is the poster.

Target 35–50 seconds after editing. Compress model idle time only; keep the
delegation order and the check result readable.

## Cleanup

1. Wait for the Turn to become completed, failed, or cancelled and capture the
   team, turn, control-plane, and Git evidence.
2. Close the Session from **All sessions**, then stop the daemon.
3. Prepare a fresh root for another take. Preserve a failed take's evidence;
   never delete a running Session container to interrupt the Turn.

## Known constraints

- The Lead must call `delegate` twice, Scout before Reviewer. A take in which
  the Lead skips a helper, a helper fails, or the required check fails is kept
  as rehearsal evidence and recorded again.
- Helper limits are reserved from the Lead's budget while the helper runs and
  returned when it finishes; a helper whose limits exceed the Lead's cannot be
  approved.
- **Explore several ways** is a different execution shape and is not used here.
