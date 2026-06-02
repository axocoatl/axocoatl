# Customer Support Example

A single agent demonstrating the **4-tier memory system**: session transcript,
persistent checkpoints, skills (reusable prompt templates), and session resume
after a simulated crash.

This example uses a **mock LLM provider** — no API key or Ollama needed.

## Run

```bash
cd examples/customer-support
cargo run
```

## Expected output

The agent handles a support conversation, a crash is simulated, and the
session is restored from the checkpoint store — demonstrating state durability.

## What it shows

- Tier 1 `SessionMemory` (in-memory transcript)
- Tier 2 `CheckpointStore` (crash-recovery snapshots)
- `SkillRegistry` prompt templates
- Session resume after a simulated crash
