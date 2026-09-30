# Workspace knowledge across Sessions

This prospective 1.1.0 film replaces the legacy shared-core-memory demonstration
in the active portfolio. The historical film, source images, scenario, and
provenance remain unchanged reference material.

## Claim

Reviewed Markdown notes belong to a Workspace and retain revisions, typed links,
backlinks, exact source references, and provenance. Another native Session in the
same Workspace can retrieve that accepted knowledge without sharing chat history.

## Do not claim

This is not legacy core-memory injection, universal recall, automatic Obsidian
sync, a resolved call graph, or proof that a model answer is correct. Source
freshness concerns the recorded file bytes. A model proposal remains a proposal
until explicitly accepted or published under the native evidence contract.

## Preparation

Follow [Native Session capture setup](../films/NATIVE-CAPTURE.md) with the
isolated Signal Desk fixture and exact release candidate at
`http://127.0.0.1:18080`. Retain the explicit configuration, data root, binary
hash, source digest, installed local Qwen model identity, and Session limits.
Start from a native data root and create a Ready Session with the existing
**Open workspace…** and **New session** journey. Do not reuse a historical film
root. Do not change fixture source code merely to create a desired badge.

## Browser actions

1. In the first Session, open **Knowledge** beside the composer, then **Code
   index** and **Refresh code index**. Inspect an actually parsed fixture source,
   including its path/hash and any partial or unsupported status.
2. Under **Notes**, choose **New note**. Create a decision titled `Incident
   status contract` describing one verifiable invariant in that source. Add the
   exact indexed source path and SHA-256; save and retain note ID/revision.
3. Create a finding titled `Incident status evidence` with a concrete observation
   from that same file. Add a typed `supports` link to the decision and save.
4. Open the decision's backlink and **Graph**. Confirm the two note IDs and source
   reference agree; navigate back to the source in the existing editor.
5. Stop the daemon deliberately and restart the same release binary with the
   same explicit configuration/data root. Do not rerun fixture preparation.
   Reload and inspect the same note IDs and revisions.
6. Create a different Session in the same Workspace. Confirm its chat is separate
   and its **Knowledge** inspector shows the same accepted records. Review the
   native model and bounded limits in **Team and budget**.
7. Send this request without manually attaching the answer:

   ```text
   Use workspace_knowledge to retrieve Incident status contract. State its note ID and revision and quote its recorded invariant. Do not edit files or publish a new note. If retrieval fails, report that failure.
   ```

8. Inspect the real retained tool result and completed answer. The exact note
   ID/revision and quoted invariant must match the accepted record. A fabricated
   answer, missing retrieval call, or failed native turn is not an accepted take.

## Durable evidence

Retain actual responses from these endpoints before and after restart:

- `GET /api/workspaces/{workspace_id}/sessions`
- `GET /api/sessions/{writer_session_id}/knowledge`
- `GET /api/sessions/{reader_session_id}/knowledge`
- `GET /api/sessions/{reader_session_id}/turns?history_version=2`
- The native Turn's retained activation, provider/tool transcript, and accepted
  input evidence reached through the existing Session history/coordination API.

The knowledge views must agree on Workspace ID, note IDs/revisions, links,
backlinks, source hashes, and human provenance. Record distinct Session IDs and
reader Turn ID. Preserve actual knowledge storage and native execution records
under the same data root. API evidence alone does not replace the visible
creation, backlink navigation, restart, Session switch, and tool-result inspection.

## Recording beats

1. `write`: the saved decision and its exact source reference.
2. `backlinks`: the finding-to-decision relationship in the inspector/graph.
3. `restart`: the same accepted revision after actual daemon restart.
4. `separate-sessions`: second Session and its shared Workspace knowledge.
5. `recall`: real native retrieval and matching completed answer.

Target 35–50 seconds. Compress idle inference time only. Keep any failure in
retained take evidence, and never label a historical capture as this new take.

## Cleanup

Retain all evidence before closing the two Sessions through **All sessions**.
Stop only the demo daemon. Leave the Ollama service and historical recordings
untouched. Reset the marked fixture root only after the take is accepted.
