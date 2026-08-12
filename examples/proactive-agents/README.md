# Automatic Automations — legacy seed and event guards

Axocoatl persists one canonical `AutomationStore`. Its graphs can run manually,
on a fixed interval, when a lattice event type matches, or when a specific Skill
publishes. `axocoatl dev` and `axocoatl serve` use the same live dispatcher.

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
| `main.rs` | An offline mock: parses legacy YAML, projects canonical Automations, then illustrates event-name, enabled, and cooldown guards on a real lattice and actor. It is not the production dispatcher. |
| `axocoatl.proactive.example.yaml` | Valid first-boot migration input containing legacy workflow, schedule, and proactive records. |

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
4. Publishes on a real `EventLattice` and illustrates event-name match → canonical
   `enabled` gate → demo cooldown → actor activation.

Production adds the pieces an offline helper cannot prove: one store-watching
schedule/event/Skill dispatcher, a live pre-execution record check, single-flight
ownership, and cooldown at both dispatch and completion.

### Expected output

```
=== Axocoatl: legacy triggers → canonical Automations ===

Loaded .../axocoatl.proactive.example.yaml (parsed by axocoatl_config::parse_config — the same parser the daemon uses).
  2 agent(s), 1 workflow(s), 1 schedule(s), 2 proactive agent(s).

First-boot AutomationStore projection:
  - daily-briefing         [enabled ] nodes=1  trigger=manual
  - pro:failure-watch      [enabled ] nodes=1  trigger=on_event · AgentFailed
  - pro:hourly-briefing    [enabled ] nodes=1  trigger=schedule · every 30s
  - sched:briefing-run     [enabled ] nodes=1  trigger=schedule · every 30s

...

[1] Publishing a lattice event: AgentFailed (coder timed out)
    'pro:failure-watch' ACTIVATED — `AgentFailed` matched its OnEvent trigger.
    The ops agent ran with its diagnostic prompt:

      DIAGNOSIS
      ─────────
      Triggering context:
        An agent just failed. Diagnose the likely cause and suggest a concrete fix.

      Failing event payload:
      { "agent_id": "coder", "error": "provider timeout after 30s", "workflow": "feature-dev" }

      Likely cause: the failing agent hit an unhandled provider error ...
      Suggested fix:
      1. Re-run the failed agent with an OverflowPolicy::Warn budget ...

[2] Publishing an unrelated event: TaskCompleted
    IGNORED (no trigger match) — `TaskCompleted` is not the watcher's target event ...

[3] Publishing a SECOND AgentFailed immediately (within the 30s cooldown)
    SKIPPED (cooldown) — the cooldown stops a failure storm from re-firing ...

[4] Setting enabled=false on the watcher, then publishing AgentFailed again
    SKIPPED (disabled) — the canonical `enabled` gate prevents the run ...

4 events published; the watcher fired 1 time(s). ...
```

Event `[1]` shows the data path: a simulated `AgentFailed` activates the projected
Agent node with its diagnostic prompt. Events `[2]`–`[4]` illustrate the matching,
cooldown, and enabled principles. The production guarantees come from
`automation_runtime`, not this example-only delivery helper.

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
