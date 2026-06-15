# Skills — event-driven lattice activation (`emits` / `reacts_to`)

A **Skill** in Axocoatl is not a tool an agent calls directly. It is a
lattice-aware *capability declaration*: it says which events it `emits` and
which it `reacts_to`, and the lattice routes it. Publish an event a skill reacts
to, and **every agent that holds that skill activates** — fan-out, no central
picker.

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

## How a skill routes (the actual mechanism)

This mirrors the runtime exactly:

- **Firing** a skill publishes each of its `emits` strings to the lattice as
  `EventType::Custom(name)` — what `POST /api/skills/{id}/fire`
  (`axocoatl-server/src/routes.rs::fire_skill`) and the in-session `SkillTool`
  (`axocoatl-daemon/src/skill_tool.rs`) both do.
- A `Custom` event deposits a signal of strength `0.5` onto every registered
  agent (`EventLattice::publish`, the `Custom(_) => 0.5` arm in
  `crates/axocoatl-coordination/src/lattice.rs`).
- The lattice's `Custom` signal is **event-name-blind** by design. The
  event→holder routing is the layer on top: a `reacts_to` index maps the event
  name to the skills that react to it, and only *those* skills' holders are
  registered at threshold `0.5` for the fire. A single matching event (`0.5`)
  crosses exactly that set, and `publish()` returns it — the "plain fan-out, no
  central picker" the [Skills doc](../../sites/docs/src/content/docs/concepts/skills.mdx)
  describes.

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

## Skills vs Workflows

Both ride the same `EventLattice`; they differ in *how the order is decided*.

|                | **Skills** (this example)                  | **Workflows** ([`stigmergic-workflow`](../stigmergic-workflow)) |
|----------------|--------------------------------------------|-----------------------------------------------------------------|
| routing        | event capability match (`reacts_to`/`emits`) | fixed `depends_on` DAG                                        |
| who runs       | *every* holder of a reacting skill (fan-out) | the agent whose join threshold is crossed                    |
| topology       | none — declared per skill, composed by the lattice | a defined graph shape                                     |
| add an agent   | give it the skill — no rewiring            | edit the graph's edges                                          |
| threshold rule | `0.5` per holder (one `Custom` event = `0.5`) | `0.5 × N` for a downstream agent with `N` deps               |

Add a new agent that also holds an existing skill — no rewiring. Move a skill to
a different agent — the lattice routes there next time. Add a new event someone
reacts to — it fires automatically.

## The declarative form

[`axocoatl.yaml`](axocoatl.yaml) is the same two skills and three holders as a
daemon config. `main.rs` reproduces that mechanism standalone with a mock LLM so
it runs with no daemon and no keys.

## Where this lives in the real runtime

- Skill config (`emits` / `reacts_to` / `agents` / `prompt`):
  `SkillConfigYaml` in [`crates/axocoatl-config/src/types.rs`](../../crates/axocoatl-config/src/types.rs)
- Firing a skill into the lattice:
  [`crates/axocoatl-daemon/src/skill_tool.rs`](../../crates/axocoatl-daemon/src/skill_tool.rs)
  and `fire_skill` in [`axocoatl-server/src/routes.rs`](../../axocoatl-server/src/routes.rs)
- `EventLattice`, the `Custom(_) => 0.5` signal, pheromone state:
  [`crates/axocoatl-coordination`](../../crates/axocoatl-coordination)
- Concept docs: [`sites/docs/.../concepts/skills.mdx`](../../sites/docs/src/content/docs/concepts/skills.mdx)
