# MCP tool discovery and approval

This film proves the safe external-tool boundary through a deterministic local
MCP server and a compatibility Session Turn. The prospective 1.1.0 take must
record that execution owner explicitly; native Session turns do not yet expose
the compatibility MCP registry.

## Claim

Axocoatl discovers `get_weather` from a real stdio MCP server, exposes the
qualified tool only to the configured Weather Agent, durably restores a
pending decision after reload, denies one call without dispatch, sends a fresh
city argument across the MCP connection after **Allow once**, and records that
tool start and result on the Session Turn.

## Do not claim

- The weather value is deterministic fixture data, not a live forecast.
- MCP is not mocked, but the external service behind this example is local and
  makes no network request.
- **Allow once** does not create a reusable permission rule.
- An approved tool result is evidence that the call ran, not a guarantee that
  every external MCP server is trustworthy.

## Start or reset

Use a separate compatibility data root with the current release candidate.
`prepare.sh` leaves `data/` for the daemon, which would create a native root; a
native Session is not offered MCP tools (native admission accepts only the
repository tools and `workspace_knowledge`). An existing empty `data/` directory
opens in the 1.0 format instead, so create it before the first start. Do not
relabel a native Session or alter execution metadata to force this scenario.
After closing prior Sessions and stopping the daemon:

```bash
export AXOCOATL_DEMO_ROOT=/private/tmp/axocoatl-one-app-showcase-mcp-compat
./demo/one-app/prepare.sh
# Compatibility root: an existing empty data directory opens in the 1.0 format.
mkdir -m 700 "$AXOCOATL_DEMO_ROOT/data"
./demo/one-app/start.sh
```

`start.sh` builds `mcp-bridge`, validates the MCP server entry in
[the demo configuration](../axocoatl.demo.yaml), and starts the stdio child at
daemon bootstrap. After creating the Weather Session, retain
`GET /api/sessions/{id}/team` and require `history_version: legacy_v1` before
sending the first request. A root that reports `execution_v2` is native; prepare
a new root instead of changing it.

Before opening a Session, confirm discovery in a second terminal:

```bash
export AXO_DEMO_ROOT='/private/tmp/axocoatl-one-app-showcase-mcp-compat'
# The local API needs this daemon's token; curl reads the header from stdin.
axo_api() { printf 'Authorization: Bearer %s\n' "$(cat "$AXO_DEMO_ROOT/data/local-api-token")" | curl -sS -H @- "$@"; }
axo_api http://127.0.0.1:18080/api/mcp/servers
axo_api http://127.0.0.1:18080/api/mcp/tools
```

## Browser actions

1. Create a **Single agent** Session for the prepared Northstar workspace and
   select **Weather MCP Agent**.
2. Send:

   ```text
   What is the current weather in London? You must call mcp__weather__get_weather before answering.
   ```

3. When **Allow MCP tool call?** appears, show:
   - the Session-scoped Agent id ending in `:weather`;
   - display tool `get_weather`;
   - server `weather`;
   - arguments containing `{"city":"London"}`.
4. Reload while this approval is pending. Wait until at least 30 seconds have
   elapsed since the modal first appeared, then confirm the same Agent, tool,
   server, and London arguments remain actionable.
5. Choose **Deny**. Confirm that the Turn shows the denied tool error and no
   weather result. The MCP bridge dispatch evidence captured below must remain
   zero for this call.
6. Send a fresh request in the same Session:

   ```text
   What is the current weather in Tokyo? You must call mcp__weather__get_weather before answering.
   ```

7. Confirm the new approval contains `{"city":"Tokyo"}` and choose **Allow
   once**. Do not choose an `always` option for this film.
8. Wait for the Turn to complete. The deterministic result must report:

   ```text
   Weather in Tokyo: 22°C, clear.
   ```

9. Reload once more. Open **History** or the Turn's Route evidence and show the tool start,
   returned value, and final Agent answer together.
10. Open **Settings → MCP servers** only after the completed Turn if a short
   discovery recap is useful.

## Visible proof

- The exact Agent, server, display tool, and JSON arguments are visible before
  consent; the durable Turn records the qualified tool name.
- The Agent remains blocked across reload and beyond the old 30-second hook
  timeout until the person chooses a decision.
- Deny records a visible failure and produces zero MCP dispatches.
- A fresh **Allow once** produces exactly one dispatch and the returned value
  depends on the city sent over the protocol.
- Tool start/result evidence and final answer remain on the durable Turn after
  reopening History.

## Durable and API evidence

```bash
export AXO_DEMO_URL='http://127.0.0.1:18080'
export AXO_DEMO_ROOT='/private/tmp/axocoatl-one-app-showcase-mcp-compat'
# The local API needs this daemon's token; curl reads the header from stdin.
axo_api() { printf 'Authorization: Bearer %s\n' "$(cat "$AXO_DEMO_ROOT/data/local-api-token")" | curl -sS -H @- "$@"; }
axo_api "$AXO_DEMO_URL/api/mcp/servers"
axo_api "$AXO_DEMO_URL/api/mcp/tools"
axo_api "$AXO_DEMO_URL/api/mcp/permissions"
axo_api "$AXO_DEMO_URL/api/sessions"
```

Copy the Weather Session id, then:

```bash
export AXO_SESSION_ID='ses-paste-the-id-here'
axo_api "$AXO_DEMO_URL/api/sessions/$AXO_SESSION_ID/team"
axo_api "$AXO_DEMO_URL/api/sessions/$AXO_SESSION_ID/turns"
grep -F 'mcp__weather__get_weather' \
  "$AXO_DEMO_ROOT/data/session-history/turns.v1.jsonl"
```

After **Allow once**, the permissions endpoint should remain without a saved
rule for this call. The canonical Turn ledger should show the denied London
Turn with its synthesized denied error record but zero external MCP dispatches,
and the allowed Tokyo Turn with exactly one bounded tool start/result pair,
city argument, returned weather text, and final answer.

## Recording beats

1. Briefly reveal the discovered Weather Agent/tool, then send London.
2. Hold on the approval, reload, cross 30 seconds, and show it still actionable.
3. Deny and show the Turn error plus zero dispatch evidence.
4. Send Tokyo, choose **Allow once**, and show exactly one call resume.
5. Reload and end on the deterministic Tokyo answer with durable tool evidence.

Target 25–35 seconds after editing. Do not pre-authorize the tool; the human
decision is the film's center.

## Cleanup

1. Resolve the approval and wait for the Weather Turn to become terminal.
2. Capture the Turn evidence and permissions response, then close the Session
   from **All sessions**.
3. Stop `start.sh` with Ctrl-C and reset with the same `prepare.sh` and
   `mkdir -m 700 "$AXOCOATL_DEMO_ROOT/data"` steps. A fresh data directory
   guarantees that no saved permission from another rehearsal suppresses the
   approval modal.

## Known constraints

- MCP approval is fail-closed on timeout or lost decision transport.
- Pending approval is durable and must remain fail-closed across reload. A take
  that loses the modal, auto-allows, or dispatches after Deny is rejected.
- Saved `Allow this agent` and `Allow always` choices have broader scopes and
  appear in Settings permissions. This film intentionally proves only the
  one-call scope.
- Tool arguments/results are bounded in the canonical ledger; oversized values
  are retained as marked audit previews rather than executable replay state.
