# Explore several Ways

This is Axocoatl's signature decision loop: independent candidate
implementations, common evidence, and one deliberate Keep.

## Claim

Ways start from the same repository snapshot and request, execute in independent
repository clones and Podman sandboxes, and remain separate until the operator
runs common Checks against each protected candidate snapshot, compares Outcome
and Route, asks a Judge to rank both passing non-empty survivors, and chooses
**Keep this one**.

## Do not claim

- Ways are not a multi-agent handoff and do not share a writable workspace.
- Judge is advice, not an automatic merge or release decision.
- Keep does not merge, commit, push, or deploy.
- This recording uses one deterministic local Ollama-compatible fixture
  provider for all four configured roles. It proves the Axocoatl lifecycle,
  protected evidence, comparison, Judge, and Keep boundaries—not provider
  diversity, model quality, or a benchmark result.

## Start or reset

After closing earlier demo Sessions and stopping the daemon, prepare the
isolated Harbor root:

```bash
export AXO_WAYS_ROOT=/private/tmp/axocoatl-one-app-showcase-harbor-ways-fixture
AXOCOATL_DEMO_ROOT="$AXO_WAYS_ROOT" \
  ./demo/one-app/prepare.sh --scenario harbor-catalog
```

In one terminal, start the capture-only deterministic provider:

```bash
node demo/one-app/films/fixtures/harbor-ways-provider.mjs
```

In another, start the exact release binary with the capture configuration:

```bash
AXOCOATL_DATA_DIR="$AXO_WAYS_ROOT/data" \
AXOCOATL_SOCKET_PATH="$AXO_WAYS_ROOT/run/axocoatl.sock" \
RUST_LOG=info \
  ./target/release/axocoatl dev \
  -c demo/one-app/films/fixtures/harbor-ways.capture.yaml
```

The product is at `http://127.0.0.1:18092`; the fixture provider is bound only
to `127.0.0.1:18110`. Create a **Single agent** Session for
`$AXO_WAYS_ROOT/workspace` using **Minimal Coder**. Keep the detected demo
image and `npm run check`. The provider's only model label is the explicit
`harbor-ways-fixture`—never describe it as qwen or as an Ollama model.

## Browser actions

1. Open **Explore several ways** before entering the task.
2. Configure exactly two Ways:
   - **Minimal Coder** / `harbor-ways-fixture`
   - **Invariant Defender** / `harbor-ways-fixture`
3. Enable **Plan first** with **Acceptance Planner**.
4. Paste this exact task into the Ways composer:

   ```text
   Repair the catalog cache-coherency defect. Search results must reflect additions, updates, and removals after a query has been cached. Preserve the public API and caching, do not change tests, run npm run check, and report Diagnosis / Decision / Evidence / Tradeoff.
   ```

5. Review the proposed plan, choose **Check models**, and start both Ways.
6. Show both attempt cards updating independently. Wait until both are terminal.
7. Choose **Review outcomes**. Compare:
   - **Outcome** and changed paths;
   - the actual diffs;
   - **Route**, including tools, files, and commands;
   - known-zero local cost without calling remote pricing free.
8. Run **Checks** with `npm run check` against every surviving Way.
9. Confirm both Ways remain non-empty and pass all six checks. If either does
   not, retain the failed evidence for diagnosis and record a new take; the
   12-film release contract does not accept a one-survivor fallback.
10. Select **Evidence Judge** and choose **Judge**. Require two unique ranks and
    preserve both candidates after the recommendation.
11. Inspect the recommendation, then make the operator decision with
    **Keep this one**.
12. In the primary Terminal run:

    ```bash
    npm run check
    ```

13. Open Source Control. The kept patch must be present and uncommitted in the
    primary workspace.

## Visible proof

- Both Ways display the same task but independent live state and Route.
- Each candidate has its own diff and common Check result.
- Both six-check results are tied to the protected base/candidate evidence,
  not a later mutation of a live clone.
- Outcome can be equal while Route and tradeoff differ.
- Judge explains a ranking but leaves the choice with the operator.
- Keep moves one checked candidate into the primary working tree without
  committing it.

## Durable evidence

Before Keep, copy the Session id and inspect the persisted attempt set:

```bash
export AXO_DEMO_URL='http://127.0.0.1:18092'
export AXO_DEMO_ROOT='/private/tmp/axocoatl-one-app-showcase-harbor-ways-fixture'
export AXO_SESSION_ID='ses-paste-the-id-here'
curl -sS "$AXO_DEMO_URL/api/sessions/$AXO_SESSION_ID/variants/results"
find "$AXO_DEMO_ROOT/workspace/.axo-variants" -maxdepth 5 -type f -print
```

Copy `attempt_set.id` from the results response, then:

```bash
export AXO_ATTEMPT_SET_ID='paste-the-attempt-set-uuid-here'
curl -sS \
  "$AXO_DEMO_URL/api/sessions/$AXO_SESSION_ID/variants/status?attempt_set_id=$AXO_ATTEMPT_SET_ID"
curl -sS \
  "$AXO_DEMO_URL/api/sessions/$AXO_SESSION_ID/variants/trajectories?attempt_set_id=$AXO_ATTEMPT_SET_ID&baseline=0"
```

Retain the attempt set's `base_sha` and `base_tree`, both candidate verdicts,
and the protected ref/receipt material before Keep. The accepted film requires
two terminal non-empty passing verdicts and one Judge result with unique ranks.

After Keep:

```bash
git -C "$AXO_DEMO_ROOT/workspace" status --short
git -C "$AXO_DEMO_ROOT/workspace" diff
npm --prefix "$AXO_DEMO_ROOT/workspace" run check
find "$AXO_DEMO_ROOT/workspace/.axo-variants" \
  -path '*/receipts/keep-*.json' -type f -print
curl -sS "$AXO_DEMO_URL/api/sessions/$AXO_SESSION_ID/turns"
```

The pre-Keep APIs and manifests prove independent candidates and verdicts. The
post-Keep receipt, canonical Turn, and uncommitted Git diff prove the deliberate
adoption boundary.

## Recording beats

1. Open Ways, show the two Agent strategies and Plan first, then start.
2. Intercut independent live tool activity without hiding a blocked or failed
   state.
3. Move to Outcome, diff, and Route only after both Ways settle.
4. Run the same Check against both, show the Judge rationale, then pause on the
   operator's selection.
5. Choose Keep, open Source Control, and end on the green primary check plus
   uncommitted kept change.

Target 55–75 seconds after editing. The actual run can take several minutes;
compress idle model time, not lifecycle transitions or evidence.

## Cleanup

1. Resolve the attempt set with **Keep this one** or **Finish without keeping**.
   Never stop the daemon with live Ways solely to force a visual reset.
2. Capture the pre/post-Keep evidence and close the Session from
   **All sessions**.
3. Stop the release daemon and deterministic provider with Ctrl-C.
4. Reset with:

   ```bash
   AXOCOATL_DEMO_ROOT=/private/tmp/axocoatl-one-app-showcase-harbor-ways-fixture \
     ./demo/one-app/prepare.sh --scenario harbor-catalog
   ```

   The script moves the previous marked Harbor root to a timestamped backup.

## Known constraints

- Ways currently require a single-agent Session and the local Podman backend.
- The deterministic provider is recording infrastructure. It returns two
  deliberately distinct valid tool routes so the film can verify Axocoatl's
  decision lifecycle repeatably; it is not included in the product binary.
- Session attachments, Skills, MCP tools, and configured web search are withheld
  from Ways because their external effects do not yet have attempt-scoped
  rollback semantics.
- Checks require every Way to be terminal. Judge requires at least two passing,
  non-empty candidates. A live operator may skip Judge and decide from Checks
  when only one passes, but that degraded path is not an accepted release film.
- Keep is resumable but locks to the originally selected Way once its apply
  transaction begins.
