# Automatic Automations — legacy seed and event guards

Axocoatl persists one canonical `AutomationStore`. Its graphs can run manually,
on a fixed interval, when an event with a given name is published on the event
feed, or when a specific Skill publishes. The daemon publishes one kind of event:
firing a Skill publishes each event named in its `emits` list. `axocoatl dev` and `axocoatl serve` use the same live dispatcher.

The old `workflows:`, `schedules:`, and `proactive:` YAML shapes remain as
first-boot migration input. When `automations.json` does not exist, the daemon projects
them into canonical records once. From then on, Settings and `/api/automations` own live
edits; config reload is not another trigger registry.

This is the *agent-acts-on-its-own* half of **Always-On**. The other half is the
Always-On **Service** (`axocoatl service install`), which keeps the daemon
*process* alive 24/7 so the triggers have something to fire inside. Proactive
agents make the agents *act* while that process runs.

## What's here

| File | What it is |
|------|------------|
| `main.rs` | An offline mock: parses legacy YAML, projects canonical Automations, fires the configured Skills on a real event feed, then illustrates event-name, enabled, and cooldown guards on a real actor. It is not the production dispatcher. |
| `axocoatl.proactive.example.yaml` | Valid first-boot migration input containing two Skills and legacy workflow, schedule, and proactive records. |

## Run the demo

```bash
cargo run -p proactive-agents
```

No API keys — it uses a mock LLM. The demo:

1. Loads `axocoatl.proactive.example.yaml` through the **real**
   `axocoatl_config::parse_config` (the same parser the daemon uses), so the
   YAML is validated against the live schema.
2. Projects those sections through `Automation::from_legacy`, the conversion used
   to seed `AutomationStore`.
3. Spawns the projected `ops` Agent node as a real `ractor` actor.
4. Fires the configured `build-failed` and `deploy-finished` Skills on a real
   `EventFeed`, publishing the same events `POST /api/skills/{id}/fire` does, and
   illustrates event-name match → canonical `enabled` gate → demo cooldown →
   actor activation.

Production adds the pieces an offline helper cannot prove: one store-watching
schedule/event/Skill dispatcher, a live pre-execution record check, single-flight
ownership, and cooldown at both dispatch and completion.

### Expected output

```
=== Axocoatl: legacy triggers → canonical Automations ===

Loaded .../axocoatl.proactive.example.yaml (parsed by axocoatl_config::parse_config — the same parser the daemon uses).
  2 agent(s), 2 Skill(s), 1 workflow(s), 1 schedule(s), 2 proactive agent(s).

First-boot AutomationStore projection:
  - daily-briefing         [enabled ] nodes=1  trigger=manual
  - pro:failure-watch      [enabled ] nodes=1  trigger=on_event · BuildFailed
  - pro:hourly-briefing    [enabled ] nodes=1  trigger=schedule · every 30s
  - sched:briefing-run     [enabled ] nodes=1  trigger=schedule · every 30s

...

[1] Firing the 'build-failed' Skill, which publishes ["BuildFailed"]
    'pro:failure-watch' ACTIVATED — `BuildFailed` from skill:build-failed matched its OnEvent trigger.
    The ops agent ran with its configured input:

      DIAGNOSIS
      ─────────
      Triggering context:
        CI reported a failing build. Diagnose the likely cause and suggest a concrete fix.

      Likely cause: a change landed whose tests were not run locally, ...
      Suggested fix:
      1. Re-run the failing job and compare its lockfile with the last green build.
      ...

[2] Firing the 'deploy-finished' Skill, which publishes ["DeployFinished"]
    IGNORED (no trigger match) — `DeployFinished` is not the watcher's target event ...

[3] Firing 'build-failed' AGAIN immediately (within the 30s demo cooldown)
    SKIPPED (cooldown) — the cooldown stops a burst of failures from re-firing ...

[4] Setting enabled=false on the watcher, then firing 'build-failed' again
    SKIPPED (disabled) — the canonical `enabled` gate prevents this Automation ...

4 events published; the watcher fired 1 time(s). ...
```

Event `[1]` shows the data path: firing a Skill publishes its declared event, which
activates the projected Agent node. A Skill event's payload is only
`{"fired_by_skill": "<id>"}`, so the Agent reads the Automation's configured input,
as it does in the daemon. Events `[2]`–`[4]` illustrate the matching, cooldown,
and enabled principles. The production guarantees come from `automation_runtime`,
not this example-only delivery helper.

## What the legacy sections become

The seed conversion preserves the old intent while producing one runtime shape:

| Legacy input | Canonical record |
|---|---|
| `workflows:` | Manual Automation with Agent nodes and dependency edges. |
| `schedules:` | `sched:<id>` Schedule Automation containing the referenced workflow graph. |
| `proactive:` | `pro:<id>` Schedule or OnEvent Automation with one Agent node. |

After import, any record can be edited into a richer graph or changed to Manual,
Schedule, OnEvent, or OnSkill in Settings. The YAML sections are not consulted
again unless a later daemon starts with a fresh data directory that has no
`automations.json` file.

## Run it in a real daemon

Use a fresh data directory to demonstrate first-boot import. This needs Ollama by
default, or a configured hosted provider:

```bash
# Validate the config against the real schema first.
axocoatl validate examples/proactive-agents/axocoatl.proactive.example.yaml

# Import once, start the canonical runtime, and open the app.
AXOCOATL_DATA_DIR=/tmp/axocoatl-proactive-example \
  axocoatl dev -c examples/proactive-agents/axocoatl.proactive.example.yaml
```

With the daemon running:

- **Settings → Automations** shows the four projected records.
- `/api/schedules` and `/api/proactive` project compatibility views with last
  run, outcome, error, and count observations.
- The `pro:hourly-briefing` and `sched:briefing-run` records fire every `30s`.
- Firing **Settings → Skills → Build failed**, or the API call below, publishes
  `BuildFailed` and starts `pro:failure-watch`.

```bash
# The local API needs the daemon's token; curl reads the header from stdin.
printf 'Authorization: Bearer %s\n' "$(cat /tmp/axocoatl-proactive-example/local-api-token)" |
  curl -H @- -X POST http://127.0.0.1:8080/api/skills/build-failed/fire
```

### Enabling / disabling

Toggle the canonical record in **Settings → Automations** or update it through
`/api/automations/{id}`. The shared dispatcher sees the persisted change without
a daemon restart. Editing the YAML or reloading config does not update an existing
Automation store.

### Install as an Always-On Service

To keep the daemon running 24/7 (so the schedules and watchers fire even after
you log out), install it as an OS background service (systemd on Linux, launchd
on macOS):

```bash
axocoatl service install -c examples/proactive-agents/axocoatl.proactive.example.yaml
axocoatl service start
axocoatl service status     # is it installed + running?
axocoatl url -c examples/proactive-agents/axocoatl.proactive.example.yaml  # workbench sign-in link
axocoatl service stop
axocoatl service uninstall
```

The **Service** keeps the process alive. The same canonical Automation runtime
used by `dev` and `serve` decides what fires while that process is alive.

## Tuning for local testing

The schedule intervals in the example YAML are set to `30s` so you don't wait an
hour to see a fire. In production you'd use realistic cadences (`1h`, `6h`,
`24h`). The interval grammar is `<number><unit>` with units `s`/`m`/`h`/`d`
(see `parse_interval` in `axocoatl-daemon`).
