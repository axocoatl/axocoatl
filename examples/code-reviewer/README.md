# Code Reviewer Example

A 3-agent **coordinator** system. A `CoordinatorBehavior` decomposes a code
review into subtasks and delegates to worker agents (Reader, Analyzer,
Reporter), then synthesizes the final review.

This example uses **mock LLM providers** — no API key or Ollama needed.

## Run

```bash
cd examples/code-reviewer
cargo run
```

## Expected output

The coordinator decomposes a sample code snippet into subtasks, each worker
agent processes its part, and a combined review report is printed.

## What it shows

- `CoordinatorBehavior` orchestrating worker agents
- `WorkerConfig` defining worker capabilities
- `DefaultAgentBehavior` with mock providers
- Parallel worker execution and result synthesis
