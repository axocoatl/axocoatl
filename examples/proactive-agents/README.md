# Proactive Agents — scheduled runs and event-triggered automation

Most of Axocoatl runs **reactively**: something hands an agent input, the agent
runs, it stops. **Proactive agents** are the autonomous half — nobody prompts
them. They sit on the `EventLattice` and act on their own when a **trigger**
fires:

- `trigger.type: schedule` — wake on a fixed interval (`every: 30s`, `1h`, …).
- `trigger.type: on_event` — wake whenever a named lattice event occurs
  (e.g. `AgentFailed`).

This is the *agent-acts-on-its-own* half of **Always-On**. The other half is the
Always-On **Service** (`axocoatl service install`), which keeps the daemon
*process* alive 24/7 so the triggers have something to fire inside. Proactive
agents make the agents *act* while that process runs.

## What's here

| File | What it is |
|------|------------|
| `main.rs` | A self-contained mock demo: a simulated `AgentFailed` event activates an ops agent, with the same gating the daemon applies. |
| `axocoatl.proactive.example.yaml` | A real config (parses against the live schema) with **both** a `schedules:` block and a `proactive:` block. |

## Run the demo

```bash
cargo run -p proactive-agents
```

No API keys — it uses a mock LLM. The demo:

1. Loads `axocoatl.proactive.example.yaml` through the **real**
   `axocoatl_config::parse_config` (the same parser the daemon uses), so the
   YAML is validated against the live schema.
2. Reads the two real `proactive:` entries it parsed.
3. Spawns the `ops` agent as a real `ractor` actor and wires an event-triggered
   runner onto a real `EventLattice`. The runner mirrors
   `crates/axocoatl-daemon/src/proactive.rs`: event-name match → `enabled` gate →
   cooldown → fire.
4. Publishes events and shows what the watcher does with each.

### Expected output

```
=== Axocoatl: Proactive Agents (schedule + on_event triggers) ===

Loaded .../axocoatl.proactive.example.yaml (parsed by axocoatl_config::parse_config — the same parser the daemon uses).
  2 agent(s), 1 workflow(s), 1 schedule(s), 2 proactive agent(s).

Proactive agents on this config:
  • hourly-briefing [enabled ] agent=secretary  trigger=schedule  · every 30s
  • failure-watch  [enabled ] agent=ops        trigger=on_event  · AgentFailed

...

[1] Publishing a lattice event: AgentFailed (coder timed out)
    ⚡ 'failure-watch' ACTIVATED — `AgentFailed` matched its on_event trigger.
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
    SKIPPED (disabled) — toggling `enabled` takes effect live ...

4 events published; the watcher fired 1 time(s). ...
```

The headline is event `[1]`: a simulated `AgentFailed` causes the ops agent to
**activate on its own** with a diagnostic prompt — no user, no orchestrator, the
lattice did it. Events `[2]`–`[4]` show the three guardrails the daemon applies:
wrong-type events are ignored, a second failure inside the cooldown window is
suppressed (this is what stops a self-loop if the diagnosis itself emits
`AgentFailed`), and a disabled watcher ignores its trigger.

## proactive vs the `schedules:` section

`axocoatl.yaml` has **two** background-automation blocks. They are not the same:

| | `schedules:` | `proactive:` |
|---|---|---|
| Fires a… | whole **workflow** (a multi-agent DAG) | single **agent** |
| Trigger types | time only (`every:`) | time **or** event (`schedule` / `on_event`) |
| Targets | a `workflows:` entry by id | one `agent:` directly |

A **schedule** is "re-run this pipeline on a clock." A **proactive agent** is
"let this one agent watch the world and act when something happens." Use a
schedule for a recurring multi-step job (nightly release checks); use a
proactive agent for an autonomous watcher (diagnose every failure) or a single
recurring touch (hourly status briefing).

For scheduled multi-agent DAGs and the lattice mechanics behind them, see the
`stigmergic-workflow` example.

## Run it in a real daemon

The same config runs under the daemon (this needs a provider — Ollama by
default, or edit `providers:` for a hosted key):

```bash
# Validate the config against the real schema first.
axocoatl validate examples/proactive-agents/axocoatl.proactive.example.yaml

# Run it in development mode (verbose logs, foreground).
axocoatl dev -c examples/proactive-agents/axocoatl.proactive.example.yaml
```

With the daemon running you'll see (via `tracing`):

- `proactive agent firing` log lines each time a trigger fires — the
  `hourly-briefing` schedule wakes every `30s` (set to `1h` in production), and
  `failure-watch` fires whenever an `AgentFailed` event lands on the lattice.
- Live per-agent state (the `ProactiveState` table — `config`,
  `last_fired_unix`, `last_outcome`, `run_count`), which the daemon exposes at
  `/api/proactive`.

### Enabling / disabling

Each proactive entry has an `enabled` flag. The daemon reads it **live** on every
fire, so toggling it (via the dashboard or by editing the config and reloading)
takes effect without a restart — exactly what event `[4]` in the demo shows.

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

The **Service** keeps the *process* alive; the **proactive agents** make the
agents *act* while it's alive. You need both for true always-on autonomy.

## Tuning for local testing

The schedule intervals in the example YAML are set to `30s` so you don't wait an
hour to see a fire. In production you'd use realistic cadences (`1h`, `6h`,
`24h`). The interval grammar is `<number><unit>` with units `s`/`m`/`h`/`d`
(see `parse_interval` in `axocoatl-daemon`).
