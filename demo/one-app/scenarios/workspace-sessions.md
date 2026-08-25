# Workspace, Sessions, and Turns

This is the orientation film from the
[demonstration catalog](../DEMO-CATALOG.md). It establishes the product's
durable hierarchy before any advanced runtime feature appears.

## Claim

One authorized workspace can contain several named Sessions. Each Session has
its own durable transcript, and each accepted request becomes a durable Turn
inside that Session. Leaving a Session and returning through **All sessions**
restores the same Turn.

## Do not claim

- A Workspace is not a chat and a Session is not one model response.
- Two Sessions anchored to the same directory do not have isolated files. They
  have separate conversations and execution state, but they intentionally see
  the same working tree.
- This scenario does not prove cloud sync, multi-user collaboration, or a
  cross-device account service.
- The deterministic local fixture provider makes this hierarchy/reload proof
  repeatable. It is not evidence of model quality or provider diversity.

## Start or reset

Close every Session owned by an earlier demo from **All sessions**, stop the
daemon with Ctrl-C, then prepare a fresh Harbor Catalog root:

```bash
export AXO_WORKSPACE_FILM_ROOT=/private/tmp/axocoatl-one-app-showcase-workspace-film
AXOCOATL_DEMO_ROOT="$AXO_WORKSPACE_FILM_ROOT" \
  ./demo/one-app/prepare.sh --scenario harbor-catalog
```

Start the capture-only provider and exact release binary in separate terminals:

```bash
node demo/one-app/films/fixtures/harbor-ways-provider.mjs
```

```bash
AXOCOATL_DATA_DIR="$AXO_WORKSPACE_FILM_ROOT/data" \
AXOCOATL_SOCKET_PATH="$AXO_WORKSPACE_FILM_ROOT/run/axocoatl.sock" \
RUST_LOG=info \
  ./target/release/axocoatl dev \
  -c demo/one-app/films/fixtures/harbor-ways.capture.yaml
```

The app is at `http://127.0.0.1:18092`. If preparation reports a surviving
`axo-ses-*` container, restart its owning demo daemon and close that Session;
do not remove an unidentified container.

## Browser actions

1. Open `http://127.0.0.1:18092` and choose **Open workspace…** from the
   Workspace switcher.
2. Choose the prepared `workspace` directory and name the Workspace
   `Harbor Catalog`.
3. Choose **New session** beside **Sessions**. Name it `Catalog orientation`,
   keep **Single agent**, select **Minimal Coder**, keep the detected image,
   then choose **Create session**.
4. Return to that Session and send this exact prompt:

   ```text
   Read package.json and report its npm scripts in one sentence. Do not edit any file.
   ```

5. Wait for the Turn to complete.
6. Return to the rail with `Harbor Catalog` selected and choose **New
   session**.
7. Name it `Catalog second session` and create another **Single agent**
   Session using **Invariant Defender**.
8. Switch between the two named Sessions. End by reopening
   `Catalog orientation`; its exact user request, streamed tool evidence, and
   answer must still be present.
9. Reload the page once and reopen `Catalog orientation` from **All sessions**.

## Visible proof

- The named Workspace groups both Sessions while its canonical path remains
  visible as secondary identity.
- The two Session rows have separate Agent identities and transcripts.
- The completed Turn survives switching Sessions and a page reload.
- The conversation shows one accepted user request and one terminal assistant
  result rather than treating the whole Session as one response.

## Durable evidence

In a second terminal:

```bash
export AXO_DEMO_URL='http://127.0.0.1:18092'
export AXO_DEMO_ROOT='/private/tmp/axocoatl-one-app-showcase-workspace-film'
curl -sS "$AXO_DEMO_URL/api/sessions"
ls -l "$AXO_DEMO_ROOT/data/sessions"
grep -F 'report its npm scripts' \
  "$AXO_DEMO_ROOT/data/session-history/turns.v1.jsonl"
```

Copy the `ses-...` id for `Catalog orientation` from the first response, then:

```bash
export AXO_SESSION_ID='ses-paste-the-id-here'
curl -sS "$AXO_DEMO_URL/api/sessions/$AXO_SESSION_ID/turns"
```

The Session JSON files prove two durable owners. The turn endpoint and JSONL
ledger prove that the reopened transcript is backed by the canonical Turn
store rather than browser state.

## Recording beats

1. Start on the Workspace browser, register `Harbor Catalog`, then create
   the first named Session.
2. Send the short repository-orientation Turn.
3. Cut from the completed Turn to **All sessions**.
4. Create and rename the second Session in the same workspace.
5. Switch back to `Catalog orientation`, reload, and end on the restored Turn.

Target 25–35 seconds after editing. Keep both Session names and the workspace
path readable; the persistence reveal is the point of the film.

## Cleanup

1. Capture the API and ledger evidence before changing ownership state.
2. In **All sessions**, use each Session's context menu and choose **Close**.
3. Stop the release daemon and deterministic provider with Ctrl-C.
4. The next `AXOCOATL_DEMO_ROOT="$AXO_WORKSPACE_FILM_ROOT"
   ./demo/one-app/prepare.sh --scenario harbor-catalog` moves this marked root
   to a timestamped backup and creates a fresh one. Do not delete the root or
   its containers by hand.

## Known constraints

- Only one normal Turn may be active in a Session at a time.
- Closing a Session is different from deleting it. Use **Close** for rehearsal
  cleanup; deletion also removes that Session's turn-owner records.
- The same directory can be opened by more than one Session. Avoid file-changing
  prompts in this orientation scenario so shared working-tree effects do not
  distract from transcript ownership.
