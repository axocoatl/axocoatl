# Native Session capture setup for 1.1.0

Use this setup for the new workbench, Workspace/Session, context/Stop, sandbox,
team handoff, Ways, Git, and workspace-knowledge takes. Historical `start.sh`
instructions target the existing-root compatibility path and must not silently
stand in for a native recording.

1. Prepare the selected fixture using `demo/one-app/prepare.sh` in a new marked
   temporary demo root. Record its actual workspace path. Preparation creates
   `data/`; leave that directory untouched and unused for this native take.
2. Choose a **different, absent** path, such as `$AXO_DEMO_ROOT/native-data`, for
   first native startup. Assert absence before the first launch. A restart uses
   that same existing native path; never reset or recreate it between beats.
3. Use a reviewed explicit configuration file (`$AXO_CAPTURE_CONFIG`) with the
   required Agent strategies, the fixture image, loopback port 8080, and an
   actual local Ollama endpoint. Record its exact bytes/hash. Do not infer a
   repository-local configuration from cwd.
4. Native Ollama admission requires cloud disabled. Run a separate owned local
   Ollama service with `OLLAMA_NO_CLOUD=1` if the user's existing service does not
   meet that contract. Do not stop or reconfigure the user's service. Verify
   `/api/status` and retain the actual model tag/digest/context evidence. A
   derived local tag may set a bounded context for existing weights; record it
   accurately rather than presenting it as a different trained model.
5. Start the exact candidate binary with explicit paths:

   ```bash
   export AXO_NATIVE_DATA="$AXO_DEMO_ROOT/native-data"
   # First launch only; a restart intentionally reuses this directory.
   test ! -e "$AXO_NATIVE_DATA"
   AXOCOATL_DATA_DIR="$AXO_NATIVE_DATA" \
   AXOCOATL_SOCKET_PATH="$AXO_DEMO_ROOT/run/native.sock" \
     ./target/release/axocoatl dev -c "$AXO_CAPTURE_CONFIG"
   ```

6. In `http://localhost:8080`, use **Open workspace…** and create the scenario's
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

## Freeze and accept recordings

Keep every current portfolio entry at the stable `required` status. Freeze the
source and film contract before building the exact capture binary, then retain
its source digest and binary hash. Capture and review actual UI and durable
evidence before writing passed per-beat evidence or provenance. Acceptance is
derived from those verified artifacts; do not change a manifest status after
capture. Only the four existing recording-output trees are excluded from the
source digest. A source change requires a new freeze and matching captures.
