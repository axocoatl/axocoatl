# Skills — standalone lattice fan-out (`emits` / `reacts_to`)

This standalone example builds a `reacts_to` index over the reusable
`EventLattice` primitives, then fans matching events out to every configured
holder. That index and activation queue belong to the example. In the product
daemon, an enabled Skill is a callable session tool that publishes its `emits`
events; `reacts_to` is metadata, and an `OnSkill` or `OnEvent` Automation is the
reachable reaction path.

This example fires the exact skill from the docs (`code-review-checklist`),
held by two agents, and lets the lattice activate both at once. Each holder then
emits an event a *second* skill reacts to, so the routing chains — again with
nobody scheduling it.

```
cargo run
```

No API keys — it uses a mock LLM that returns one canned JSON review per agent.

## The fan-out

```
  CodeReady ─┐
             ├─▶ skill: code-review-checklist   (reacts_to CodeReady)
             │      holders: reviewer, coder          ← one event, BOTH fire
             │        reviewer ──emits──▶ ReviewComplete
             │        coder    ──emits──▶ ReviewComplete
             │
  ReviewComplete ─▶ skill: deploy-gate          (reacts_to ReviewComplete)
                       holder: deployer                ← activated by the chain
                         deployer ──emits──▶ DeployApproved
```

One published event (`CodeReady`) lands on the lattice and **two** holder agents
activate together. Each one finishes and emits `ReviewComplete`, which a
different skill (`deploy-gate`) reacts to — so `deployer` fires next without any
code wiring the two skills together. `DeployApproved` is terminal: nothing
reacts to it, and the cascade stops.

## How this standalone example routes

Two pieces are shared with the product runtime:

- **Firing** a skill publishes each of its `emits` strings to the lattice as
  `EventType::Custom(name)` — what `POST /api/skills/{id}/fire`
  (`axocoatl-server/src/routes.rs::fire_skill`) and the in-session `SkillTool`
  (`axocoatl-daemon/src/skill_tool.rs`) both do.
- A `Custom` event deposits a signal of strength `0.5` onto every registered
  agent (`EventLattice::publish`, the `Custom(_) => 0.5` arm in
  `crates/axocoatl-coordination/src/lattice.rs`).
- The lattice's `Custom` signal is **event-name-blind** by design.

The example adds the rest: an event→holder `reacts_to` index, temporary holder
registrations at threshold `0.5`, an activation queue, and a completed guard.
The product daemon does not contain that holder-routing layer. It broadcasts
the event notification to its actual subscribers, including the canonical
Automation trigger runtime.

A run guard stops a `(skill, holder)` binding from firing twice. Because both
reviewers emit `ReviewComplete`, the second one finds `deployer` already ran and
the cascade converges — the example prints that explicitly.

## Skill prompt ≠ agent system prompt

This is the distinction the example makes visible. Each holder agent has its
**own** `system_prompt` — its standing role:

| agent    | system prompt (role)                                       |
|----------|------------------------------------------------------------|
| reviewer | "You are a senior reviewer. You care about correctness…"   |
| coder    | "You are the implementing engineer. You review for…"       |
| deployer | "You are the release gate. You only ship green reviews."   |

The **skill** carries a *separate* `prompt` template (the task), handed to
whichever holder activates for that skill. The agent's voice stays constant; the
skill supplies the work. In code (`HolderAgent::execute`) the skill prompt
arrives as a per-call `system_override`, with the agent's own `system_prompt` as
the fallback. The run prints both lines for every activation so you can see they
differ.

## Two standalone coordination shapes

These two examples both drive the `EventLattice` signal primitives themselves;
they differ in how their example-owned queues decide what runs.

|                | **Skills** (this example)                  | **Workflows** ([`stigmergic-workflow`](../stigmergic-workflow)) |
|----------------|--------------------------------------------|-----------------------------------------------------------------|
| routing        | event capability match (`reacts_to`/`emits`) | fixed `depends_on` DAG                                        |
| who runs       | *every* holder of a reacting skill (fan-out) | the agent whose join threshold is crossed                    |
| topology       | none — declared per skill, composed by the lattice | a defined graph shape                                     |
| add an agent   | give it the skill — no rewiring            | edit the graph's edges                                          |
| threshold rule | `0.5` per holder (one `Custom` event = `0.5`) | `0.5 × N` for a downstream agent with `N` deps               |

Those fan-out properties describe this example. In the product, add an
`OnSkill` Automation to match the Skill producer or an `OnEvent` Automation to
match an emitted event type.

## The declarative form

[`axocoatl.yaml`](axocoatl.yaml) uses the real Skill configuration schema for the
same two declarations and three holders. `main.rs` adds the standalone routing
harness and a mock LLM so the demonstration runs with no daemon and no keys.

## Product-runtime boundary

- Skill config (`emits` / `reacts_to` / `agents` / `prompt`):
  `SkillConfigYaml` in [`crates/axocoatl-config/src/types.rs`](../../crates/axocoatl-config/src/types.rs)
- Firing a skill into the lattice:
  [`crates/axocoatl-daemon/src/skill_tool.rs`](../../crates/axocoatl-daemon/src/skill_tool.rs)
  and `fire_skill` in [`axocoatl-server/src/routes.rs`](../../axocoatl-server/src/routes.rs)
- Matching an emitted type or exact Skill producer to a canonical Automation:
  [`crates/axocoatl-daemon/src/automation_runtime.rs`](../../crates/axocoatl-daemon/src/automation_runtime.rs)
- `EventLattice`, the `Custom(_) => 0.5` signal, pheromone state:
  [`crates/axocoatl-coordination`](../../crates/axocoatl-coordination)
- The `reacts_to` holder index and activation queue are implemented only in this
  example's `main.rs`.
- Concept docs: [`sites/docs/.../concepts/skills.mdx`](../../sites/docs/src/content/docs/concepts/skills.mdx)
