# Research Assistant Example

A 2-agent pipeline: a **Researcher** produces raw findings, a **Summarizer**
condenses them into a final answer. Demonstrates agent spawning as ractor
actors, custom `AgentBehavior`, token-budget enforcement, and agent-to-agent
coordination.

This example uses **mock LLM providers** — no API key or Ollama needed.

## Run

```bash
cd examples/research-assistant
cargo run
```

## Expected output

The researcher agent emits mock findings for a sample query, the summarizer
condenses them, and the final summary plus per-agent token usage is printed.

## What it shows

- Spawning agents via `AgentActor`
- Custom `AgentBehavior` implementations
- `TokenBudget` / `OverflowPolicy` enforcement
- Session memory across turns
- Message-passing coordination between two agents

For the real stigmergic workflow engine driving this pattern with live LLMs,
see the `research-and-summarize` workflow in the root `axocoatl.yaml` and run
`axocoatl workflow run research-and-summarize -i "..."`.
