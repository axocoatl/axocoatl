# Skill, event feed, and Automation

The prospective 1.1.0 take uses the existing configured Automation/runtime
surface. Its run evidence is separate from native Session activation evidence;
showing Settings or an Automation does not establish native Session MCP or
legacy memory support.

This film shows Axocoatl reacting to a typed event without turning the event
substrate into another product destination.

## Claim

Firing the configured **Release candidate ready** Skill publishes a typed
`ReleaseCandidateReady` event on the event feed. The canonical Automation
dispatcher matches the **Release gate review** Automation's `on_event` trigger
by that event name, starts the durable Automation, records its run, and parks its
operator-review Interrupt.

## Do not claim

- The event feed is not the Agent graph or the Automation canvas.
- The recent-events endpoint is an in-memory, bounded observation window. The
  durable evidence in this scenario is the Automation run, not a permanent
  global event ledger.
- The trigger matches the event name, not the Skill: any producer of
  `ReleaseCandidateReady` would start the same Automation. The film shows the
  one Skill that declares it.
- The dispatcher applies a per-Automation cooldown to event-triggered runs, so
  fire the Skill only once per root; a quick second fire can be ignored.
- Firing a Skill is not itself a multi-agent Session or a Coordinator run.

## Start or reset

Use a fresh Signal Desk root so the event fixture starts from known data without
reusing another scenario's runtime state:

```bash
./demo/one-app/prepare.sh --scenario signal-desk
AXOCOATL_DEMO_ROOT=/private/tmp/axocoatl-one-app-showcase-signal-desk \
  ./demo/one-app/start.sh
```

After `/health/ready` responds, seed the canonical Automations in a second
terminal:

```bash
AXOCOATL_DEMO_ROOT=/private/tmp/axocoatl-one-app-showcase-signal-desk \
  ./demo/one-app/seed-runtime-demos.sh
```

No Session Turn is required. The Skill and Automation are configured runtime
objects in Settings.

## Browser actions

1. Open **Settings → Skills**.
2. Choose `ReleaseCandidateReady` under **By event** so the list shows the
   Skills that emit it. Select **Release candidate ready**, show its declared
   `ReleaseCandidateReady` event, then choose **Fire this Skill** once.
3. After firing, show the Skills list still filtered to
   `ReleaseCandidateReady` while the rail exposes the downstream waiting item.
   The producer and exact payload are verified through the API evidence below,
   not presented as a raw-event screen that the product does not have.
4. Open **Settings → Automations → Release gate review · event-driven**. Its
   trigger reads `⊛ on ReleaseCandidateReady`.
5. Open **Runs**. Wait for the new run to progress from running to interrupted.
6. Show the two-node graph: `gate-review → operator-review`.
7. Choose the rail item **⏸ 1 waiting** or open the Interrupt panel. Confirm
   that the pending item belongs to `release-gate-review` and node
   `operator-review`.
8. Resume with this exact operator value:

   ```text
   Evidence received. Keep the release gated until repository checks and remaining risks are reviewed by an operator.
   ```

9. Reload the page, reopen **Runs**, and show the terminal completed run with
   its node history and exact recorded `final_content` Result.

## Visible proof

- The Skill declares the named typed event, and firing it produces the first
  visible downstream waiting state without a manual Automation Run.
- The Automation's trigger names the event, not the Skill.
- No manual Automation **Run** button is used before the run appears.
- The event-triggered run appears in the canonical Automation's run history.
- The gate-review Agent output flows to a top-level Interrupt and resumes under
  explicit operator ownership.
- Reloaded completed history exposes the recorded Result, not only a status
  badge.

## Durable and API evidence

```bash
export AXO_DEMO_URL='http://127.0.0.1:18080'
export AXO_DEMO_ROOT='/private/tmp/axocoatl-one-app-showcase-signal-desk'
# The local API needs this daemon's token; curl reads the header from stdin.
axo_api() { printf 'Authorization: Bearer %s\n' "$(cat "$AXO_DEMO_ROOT/data/local-api-token")" | curl -sS -H @- "$@"; }
axo_api "$AXO_DEMO_URL/api/events/recent"
axo_api "$AXO_DEMO_URL/api/automations/release-gate-review/runs"
axo_api "$AXO_DEMO_URL/api/interrupts"
find "$AXO_DEMO_ROOT/data/automation/runs-v1/release-gate-review" \
  -maxdepth 1 -type f -print
```

Copy the run id from the run list, then:

```bash
export AXO_RUN_ID='paste-the-run-uuid-here'
axo_api \
  "$AXO_DEMO_URL/api/automations/release-gate-review/runs/$AXO_RUN_ID"
sed -n '1,260p' \
  "$AXO_DEMO_ROOT/data/automation/runs-v1/release-gate-review/$AXO_RUN_ID.json"
```

The recent-events response should contain `ReleaseCandidateReady` produced by
`skill:release-candidate-ready`. The run JSON is the durable causal outcome to
retain after that observation window expires.

## Recording beats

1. Start on the Skill row, filtered by its declared event.
2. Fire once; hold briefly on the declared event plus downstream waiting state,
   then cut directly to the automatically created run under the
   `on ReleaseCandidateReady` trigger.
3. Show the two-node graph reaching the operator Interrupt.
4. Reveal the waiting item, enter the operator decision, and resume.
5. Reload and end on completed Runs history with the exact recorded Result,
   not on the transient success toast.

Target 30–45 seconds after editing. Keep the transition from Skill to new run
continuous enough that the causal link is unmistakable.

## Cleanup

1. Resolve the `operator-review` Interrupt and capture the completed run JSON.
2. Close any demo Session opened only for navigation, then stop `start.sh` with
   Ctrl-C.
3. Reset with:

   ```bash
   ./demo/one-app/prepare.sh --scenario signal-desk
   ```

   Re-run `seed-runtime-demos.sh` with the same `AXOCOATL_DEMO_ROOT` only
   after the fresh daemon is healthy.

## Known constraints

- Recent events are retained in a 200-entry in-memory ring buffer and do not
  survive daemon restart.
- The dispatcher prevents overlapping automatic runs of the same Automation.
- The top-level Interrupt is durable. Arbitrary in-flight provider/tool work is
  not reconstructed after a crash.
- The Automation store at `$AXO_DEMO_ROOT/data/automations.json` is canonical
  after seeding; editing legacy YAML later does not mutate that live store.
