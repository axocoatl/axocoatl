# Crash recovery — resume a multi-agent workflow from a checkpoint

This is the README hero demo in code: **kill the process mid-run and the
workflow restarts from its last checkpoint, not from zero.**

<p align="center">
  <img src="../../docs/img/demo.gif" alt="Kill the server mid-task — the agent restarts from its last checkpoint, not from zero. 100% local." width="760">
</p>

```
cargo run
```

No API keys — it uses a mock LLM with one canned reply per role.

## The workflow

```
        researcher ──output──▶ summarizer
        (step 1)               (step 2 — needs step 1's output as context)
```

The program runs this pipeline across **two process lifetimes** in one binary:

1. **First run.** The researcher executes. We persist a checkpoint to disk —
   step 1 done (output captured), step 2 pending — then *crash*: every actor and
   the in-memory workflow struct is dropped. Only the `.ckpt` file survives.
2. **Restart.** A brand-new, empty workflow is reconstructed **purely from the
   checkpoint on disk**. The persisted state says the researcher finished, so
   step 1 is skipped — its agent is never spawned — and the summarizer resumes,
   fed the researcher's persisted output as upstream context.

## What proves the skip is real

The restart phase has **zero** in-memory knowledge of the first run. It rebuilds
the work list only from `CheckpointStore::load_latest`. The decision to skip is
driven by the persisted `output: Some(_)` on disk — not a flag in code.

To make that unfakeable, each step's mock LLM counts its own calls. The final
report asserts:

| step       | phase 1 | phase 2 | total |
|------------|---------|---------|-------|
| researcher | 1       | 0       | 1     |
| summarizer | 0       | 1       | 1     |

The researcher's model is invoked exactly once — never again after the crash.
The summarizer's is invoked once, after the restart, using the researcher's
output read back from disk.

## The persistence is the real runtime mechanism

State is persisted exactly the way the production coordinator persists a
resumable run: the workflow state is serialized to JSON and stored in
`AgentCheckpoint.behavior_state` via `CheckpointStore`, which writes a bincode
snapshot atomically (temp file + rename), restricts it to owner-only, and prunes
to the last 3 versions. `load_latest` reads the highest version back. This
example uses that same store and the same `behavior_state` JSON convention — it
is not a toy of its own.

The checkpoint store decodes defensively: a corrupt or schema-changed checkpoint
is logged and discarded (start fresh) rather than bricking the agent, because a
checkpoint is a regenerable cache of session state, never a source of truth.

A temp dir under `std::env::temp_dir()` holds the checkpoints and is removed at
the end of the run.

## Where this lives in the real runtime

- `CheckpointStore`, `AgentCheckpoint`, the atomic write + prune + defensive
  decode: [`crates/axocoatl-memory/src/checkpoint.rs`](../../crates/axocoatl-memory/src/checkpoint.rs)
- The production resumable orchestration this mirrors (`OrchestrationState`
  serialized into `behavior_state`):
  [`crates/axocoatl-actor/src/coordinator.rs`](../../crates/axocoatl-actor/src/coordinator.rs)
- Checkpoint section of the architecture overview:
  [`docs/ARCHITECTURE.md`](../../docs/ARCHITECTURE.md#memory-tiers) — see the
  Tier-2 checkpoint row and the "resumable" note under **Coordinator role**.

## Related examples

- [`customer-support`](../customer-support) — session resume for a *single*
  agent across a simulated crash (conversation memory, not a multi-step DAG).
- [`stigmergic-workflow`](../stigmergic-workflow) — how a multi-step DAG's
  running order emerges from the `EventLattice` with no central orchestrator.
