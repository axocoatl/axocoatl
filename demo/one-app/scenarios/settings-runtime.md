# Runtime configuration stays in Settings

This film is a short orientation through the configuration that extends one
Axocoatl workbench. The same selected Session remains visible behind Settings,
so configuration never looks like a second product.

## Claim

Agents, Skills, MCP servers, and Automations are discoverable, configurable
sections inside Settings. The selected Workspace and Session stay visibly
anchored behind the tour. Completed Automation runs expose their recorded
terminal Result in run history.

## Do not claim

- Settings is not another dashboard or an alternate app route.
- Listing a provider, Agent, Skill, MCP server, or Automation does not prove
  that every configuration is healthy. This scenario uses the prepared,
  verified demo records only.
- An Automation run is not part of the canonical Session transcript.
- The local weather MCP server is deterministic fixture infrastructure, not a
  live third-party integration.

## Start or reset

Close prior demo Sessions, stop the daemon, and prepare a fresh Signal Desk
root so this tour can reuse the same Session and completed event-driven run as
the Automation film:

```bash
./demo/one-app/prepare.sh --scenario signal-desk
AXOCOATL_DEMO_ROOT=/private/tmp/axocoatl-one-app-showcase-signal-desk \
  ./demo/one-app/start.sh
```

After `/health/ready` responds, seed the canonical Automation records:

```bash
./demo/one-app/seed-runtime-demos.sh
```

Create or reopen one **Single agent** Session in the prepared Workspace. Run
the **On Skill Automation** path from
[the event scenario](event-lattice.md) before recording so one completed run
with non-empty `final_content` is available in run history.

## Browser actions

1. Establish the selected Workspace, Session name, and Conversation, then open
   **Settings**.
2. Open **Agents** and select **Critical Reviewer** once. At the top of the
   drawer, show its configured provider, model, and role. Then scroll only that
   drawer until the checked **architect** dependency is visible. Do not change
   the selected Agent or toggle the dependency.
3. Open **Skills**. Select **Release candidate ready** and show the declared
   `ReleaseCandidateReady` event. Do not fire it again.
4. Open **MCP servers**. Select **weather** and show the local stdio server and
   discovered `get_weather` tool without changing a permission.
5. Open **Automations**. Select the completed Skill-triggered Automation, open
   **Runs**, expand the terminal run, and show its non-empty **Result**.

## Visible proof

- Settings contains the four runtime-configuration sections without changing
  the page route or selected Session.
- Agent dependency, Skill event type, MCP tool discovery, and Automation run
  history are each visible in their own section.
- The completed Automation run renders a labeled, non-empty Result.
- The original Session remains visibly selected behind every Settings section.

## Durable and API evidence

```bash
export AXO_DEMO_URL='http://127.0.0.1:18080'
export AXO_DEMO_ROOT='/private/tmp/axocoatl-one-app-showcase-signal-desk'
curl -sS "$AXO_DEMO_URL/api/agents"
curl -sS "$AXO_DEMO_URL/api/skills"
curl -sS "$AXO_DEMO_URL/api/mcp/servers"
curl -sS "$AXO_DEMO_URL/api/mcp/tools"
curl -sS "$AXO_DEMO_URL/api/automations"
```

Copy the completed Automation and run ids shown in Settings, then inspect the
same canonical run record through its API and file:

```bash
export AXO_AUTOMATION_ID='paste-the-automation-id-here'
export AXO_RUN_ID='paste-the-run-uuid-here'
curl -sS \
  "$AXO_DEMO_URL/api/automations/$AXO_AUTOMATION_ID/runs/$AXO_RUN_ID"
sed -n '1,280p' \
  "$AXO_DEMO_ROOT/data/runs/$AXO_AUTOMATION_ID/$AXO_RUN_ID.json"
```

The API and file must contain the same terminal status and `final_content`
shown as Result. A legacy run without that optional field is valid historical
data but is not acceptable evidence for this film.

## Recording beats

1. Start in the named Session and open Settings.
2. Capture Critical Reviewer identity at the top of its Agent drawer, then the
   checked `architect` dependency after scrolling the same drawer.
3. Capture Skills and MCP servers as separate beats with one configured proof
   point visible in each section.
4. Open Automations last, expand the completed run, and hold on Result.
5. End on the completed Automation run's labeled Result, with one unchanged
   `session_id` visibly selected behind all six frames.

Target 20–30 seconds after editing. Use direct cuts between Settings sections;
do not turn this into a slow tour of every control.

## Cleanup

1. Capture the completed run API/file evidence before closing the Session.
2. Resolve any unrelated pending Interrupts, close the demo Session, and stop
   `start.sh` with Ctrl-C.
3. Reset with `./demo/one-app/prepare.sh --scenario signal-desk`. Seed
   Automations only after the fresh daemon is healthy.

## Known constraints

- Saved MCP permission scopes are durable configuration. This film does not
  create or delete one.
- Automation `final_content` is optional only for backward compatibility with
  older run records. Every newly completed recording run must contain it.
- Provider availability is demonstrated separately; opening an Agent record is
  not a provider health check.
