# Memory recall — core memory, semantic recall, and consolidation

Axocoatl's memory is a four-tier hierarchy. The
[`customer-support`](../customer-support) example demonstrates the bottom two
tiers (the in-process session transcript and crash-recovery checkpoints). This
example demonstrates the **top two** — agent-managed core memory and semantic
recall — plus the **sleep-time consolidation** pass that connects them.

This is the "Tell it once — it remembers" workflow from the project README's
[See it work](../../README.md#see-it-work) section (the `docs/img/memory.gif`
demo), as a runnable program: store a preference in one session, open a
brand-new session, and it still knows.

```
cargo run -p memory-recall
```

No API keys, **no network, no model download** — it uses a mock LLM and the
pure-Rust lexical embedding backend. Pass `--with-embeddings` to use the neural
backend instead (it downloads `all-MiniLM-L6-v2`, ~90 MB, on first run):

```
cargo run -p memory-recall -- --with-embeddings
```

## The four tiers

```
  ┌─────────────────────────────────────────────────────────────────────┐
  │ Tier 1  SessionMemory     in-process conversation transcript          │  customer-support
  │ Tier 2  CheckpointStore   crash-recovery state snapshots              │  customer-support
  │         DailyLogMemory    append-only raw activity log (recall_timeframe)
  │ ─────────────────────────────────────────────────────────────────── │
  │ Tier 3  CoreMemoryStore   agent-edited curated blocks ◀──────┐        │  THIS EXAMPLE
  │         (persona / human / project)   in the prompt each turn │       │
  │                                                    promote up │       │
  │ Tier 4  SemanticMemory    vector recall of past exchanges ────┘       │  THIS EXAMPLE
  │         (passive injection + recall_search)        consolidation      │
  └─────────────────────────────────────────────────────────────────────┘
```

Tiers 3 and 4 are **file-backed**, so they survive across sessions. A freshly
spawned actor pointed at the same data dir reloads them — that is the whole
cross-session mechanism, no orchestrator threading state through by hand.

## What the example does, in three phases

| Phase | Session | What happens | Tier(s) exercised |
|-------|---------|--------------|-------------------|
| 1 — Store | A | User states a preference → agent calls `core_memory_set` → written to the `human` block; the exchange is also persisted for recall | 3 (write) + 4 (store) |
| 2 — Recall | B (new actor) | Fresh agent reloads the `human` block into its prompt, recalls A's exchange (**passively** + via `recall_search`), then is told a project convention it leaves uncurated | 3 (read) + 4 (recall) |
| 3 — Consolidate | B (idle) | `on_consolidate` reads recent Tier-4 activity and **promotes** the convention up into the `project` block | 4 → 3 (promotion) |

Each phase prints the relevant memory state **before and after**, read straight
off disk, so you can see the state actually change.

## The three agent-callable memory tools

The agent edits and queries its own memory through tools the model decides to
call (here, a mock model emits those calls; in a real app the LLM does):

- **`core_memory_set`** (Tier 3) — overwrite a block's value. There are also
  `core_memory_append` and `core_memory_replace` for incremental edits.
- **`recall_search`** (Tier 4) — semantic search over past sessions and earlier
  in this conversation.
- **`recall_timeframe`** (Tier 2 daily log) — read a specific day's raw activity.

## Recall config knobs

Tier-4 recall is tuned per agent via `AgentConfig.memory.recall`
([`RecallConfig`](../../crates/axocoatl-core/src/agent.rs)). The same three knobs
govern **both** the passive injection path and the agent-driven `recall_search`
tool, so the two always agree on the relevance bar:

| Knob | Default | Effect |
|------|---------|--------|
| `passive_inject` | `true` | Inject the top-k semantic hits into the prompt every turn. Set `false` to make the agent rely solely on calling `recall_search` itself. |
| `top_k` | `5` | How many semantic hits to retrieve (passive injection, and the `recall_search` default `k`). |
| `min_score` | `0.15` | Minimum cosine similarity for a hit to count as relevant. Hits below this are dropped on both paths. |

The example prints these knobs at startup and shows the recall hit's score, so
you can see how `min_score` would gate it.

## Why two embedding backends

Tier 4 turns each memory into a vector and recalls by cosine similarity. There
are two backends behind one seam
([`crates/axocoatl-memory/src/semantic.rs`](../../crates/axocoatl-memory/src/semantic.rs)):

- **Neural** (`all-MiniLM-L6-v2` via Candle) — similarity reflects *meaning*, so
  "terse answers" and "concise responses" match with no shared words. Downloads
  ~90 MB on first use. Enabled with `--with-embeddings`.
- **Hashed lexical fallback** — signed feature hashing over words + character
  trigrams. Similarity reflects *word overlap* only, but it needs no download and
  runs offline. The default here, and what keeps CI free of network and weights.

The demo content is chosen to recall under either backend.

## Where this lives in the real runtime

- The four-tier memory types:
  [`crates/axocoatl-memory`](../../crates/axocoatl-memory) — `CoreMemoryStore`,
  `SemanticMemory`, `DailyLogMemory`, `CheckpointStore`, `SessionMemory`.
- The behavior that wires them together (passive injection, the recall + core
  tools, `on_consolidate`):
  `crates/axocoatl-actor/src/default_behavior.rs`.
- The agent-callable tools:
  `crates/axocoatl-actor/src/core_memory_tools.rs` and
  `crates/axocoatl-actor/src/recall.rs`.
- The consolidation trigger (idle-gated): `consolidate_agent` in
  `crates/axocoatl-actor/src/actor_impl.rs`; the daemon drives it on an interval.
- Architecture overview: [`docs/ARCHITECTURE.md`](../../docs/ARCHITECTURE.md).
