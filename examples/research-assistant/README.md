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

For the product path with configured providers, start from a fresh data
directory so the `research-and-summarize` legacy record in the root
`axocoatl.yaml` seeds a manual Automation, then run
`axocoatl workflow run research-and-summarize -i "..."`. That command uses the
canonical explicit Automation DAG executor.
