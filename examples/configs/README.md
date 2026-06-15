# Config gallery

Minimal, copy-pasteable `axocoatl.yaml` recipes — one per common use case. These
are YAML + CLI only (no Rust). The Rust `examples/` crates teach library
embedding; this gallery is the on-ramp for `axocoatl validate` and `axocoatl dev`.

Every config here passes `axocoatl validate`. Each file's header comment lists its
prerequisites, run command, and expected output.

| File | Agents | Demonstrates |
| --- | --- | --- |
| [research-pipeline.yaml](research-pipeline.yaml) | 2 | `depends_on` workflow — summarizer waits on researcher |
| [feature-dev.yaml](feature-dev.yaml) | 5 | linear DAG (architect → planner → coder → reviewer → docs) |
| [incident-response.yaml](incident-response.yaml) | 3 | Skills (`emits` / `reacts_to`) + an ops agent |
| [local-only.yaml](local-only.yaml) | 2 | Ollama, no API keys — data never leaves the box |
| [mcp-tools.yaml](mcp-tools.yaml) | 1 | a single MCP server (stdio transport) |

## Running an example

```sh
# Validate any config without starting the daemon:
axocoatl validate examples/configs/research-pipeline.yaml

# Run one in dev mode (verbose, no daemonization):
axocoatl dev -c examples/configs/research-pipeline.yaml
```

Most examples need a local Ollama (`ollama serve && ollama pull llama3.2`) — no
cloud API key required. The `mcp-tools` example additionally needs `npx` on your
PATH for the filesystem MCP server.
