# Native Session capture setup for 1.1.0

Use this setup for the new workbench, Workspace/Session, context/Stop, sandbox,
Lead-with-helpers, Ways, Git, and workspace-knowledge takes. `prepare.sh` and
`start.sh` produce a native root; the Automation films use the same scripts.

1. Prepare the selected fixture with `demo/one-app/prepare.sh --scenario <name>`
   in a new marked temporary demo root (`AXOCOATL_DEMO_ROOT`). Record its actual
   workspace path. Preparation does not create `data/`: the daemon creates it on
   first start, and only a data root the daemon creates itself gets the native
   Session format. Assert `data/` is absent before the first launch. A restart
   reuses that same existing data root; never reset or recreate it between beats.
2. Point Podman at the intended machine before preparing or starting. When the
   default connection is not the machine to use, export `CONTAINER_CONNECTION`
   for both scripts; `prepare.sh` starts the default machine only when
   `podman info` fails, so a set connection keeps it from starting another one.
3. `start.sh` serves the explicit reviewed configuration
   `demo/one-app/axocoatl.demo.yaml` on loopback port 18080, or another file named
   by `AXOCOATL_DEMO_CONFIG` (the Lead-with-helpers film uses
   `axocoatl.team.yaml`). It prints the configuration path and SHA-256; record
   both. Do not infer a repository-local configuration from cwd.
4. Native Ollama admission requires cloud disabled. `start.sh` reads the Ollama
   port from `AXOCOATL_DEMO_OLLAMA_PORT` (default 11434), requires
   `/api/status` to report cloud models disabled, and requires every model the
   configuration names. Run a separate owned local Ollama service with
   `OLLAMA_NO_CLOUD=1` if the user's existing service does not meet that
   contract. Do not stop or reconfigure the user's service. Retain the actual
   model tag/digest/context evidence. A derived local tag may set a bounded
   context for existing weights; record it accurately rather than presenting it
   as a different trained model.
5. Start the exact candidate binary with explicit paths:

   ```bash
   export AXOCOATL_DEMO_ROOT=/private/tmp/axocoatl-one-app-showcase-harbor-catalog
   export AXOCOATL_DEMO_OLLAMA_PORT=11434   # a cloud-disabled local service
   export CONTAINER_CONNECTION=...          # only when not the default machine
   # First launch only; a restart intentionally reuses this directory.
   test ! -e "$AXOCOATL_DEMO_ROOT/data"
   AXOCOATL_DEMO_BIN="$PWD/target/release/axocoatl" ./demo/one-app/start.sh
   ```

   The daemon keeps its data in `$AXOCOATL_DEMO_ROOT/data` and its socket in
   `$AXOCOATL_DEMO_ROOT/run/axocoatl.sock`. Axocoatl publishes the storefront's
   logical Preview port 8765 on a dynamic loopback port, so host port 8765 does
   not need to be free.

6. Open the `Sign in:` link the launcher printed (or run
   `AXOCOATL_DATA_DIR="$AXOCOATL_DEMO_ROOT/data" axocoatl url -c demo/one-app/axocoatl.demo.yaml`),
   then use **Open workspace…** and create the scenario's
   Session. Review its image/setup explicitly and require Ready. Retain
   `GET /api/sessions/{id}/team` and require `history_version: execution_v2`
   before sending any recorded Turn. If it reports `legacy_v1`, preserve that
   evidence and start an actually native root; never edit owner records.
7. Open **Team and budget**, select **Edit** and the Agent, review the exact
   provider/model and maximum output, enter finite activation/invocation/token
   limits, zero local cost, and a future expiry. **Preview changes** must succeed;
   inspect the returned profile and **Apply to this Session**. Repeat for every
   required role. A template alone is not approval of its limits.
   The configured hard per-call budget must fit the observed context plus the
   requested output; increasing only the Session total cannot override a lower
   Agent per-call cap. JSON repair profiles may reserve two passes. Review the
   actual native Preview error instead of silently clamping or changing limits.
8. Rehearse without marking a film accepted. After all journeys work, freeze the
   final source and portfolio, build the final release candidate, and capture
   all 12 final takes. A debug rehearsal is not release provenance.

Record Workspace/Session/Turn IDs, execution version, actual limits, configuration,
model identity, and release binary identity in the take's evidence. Retain public
API projections plus their canonical native execution records under the selected
data root. Compatibility Session transcripts are not native authority. A daemon
restart must be coordinated with other active rehearsals; never interrupt another
Session merely to obtain a restart shot.

## Films pending for a released version

`demo/one-app/films/PENDING` names a product version that shipped before its films
were recorded (1.1.0 did). While it matches the CLI version, CI, preflight and the
release pass the film gate on the manifest alone and say so, and a source-bound film
proof fails. The marketing gate still builds and validates the site, without films:
each film placement becomes a static note naming what the film will show (or is
removed when the element has `pending="omit"`), no film media or portfolio ships,
and that site is what CI checks and the release and `marketing-deploy` deploy.
Delete the file in the same commit that adds the recordings; the gate refuses new
captures while it still declares the version.

## Freeze and accept recordings

Keep every current portfolio entry at the stable `required` status. Freeze the
source and film contract before building the exact capture binary, then retain
its source digest and binary hash. Capture and review actual UI and durable
evidence before writing passed per-beat evidence or provenance. Acceptance is
derived from those verified artifacts; do not change a manifest status after
capture. Only the four existing recording-output trees are excluded from the
source digest. A source change requires a new freeze and matching captures.
