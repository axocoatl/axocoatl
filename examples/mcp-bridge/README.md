# MCP bridge — client and server

[MCP](https://modelcontextprotocol.io) (the Model Context Protocol) is how an
agent reaches a tool that lives in another process. Axocoatl speaks it both
ways:

- **as a client** — it connects to external MCP servers and folds their tools
  into the agent tool set;
- **as a server** — it exposes its own agents as MCP tools any host can call.

This example covers both. The **client** path is a runnable Rust program. The
**server** path is configuration plus the steps to register Axocoatl in a host.

---

## Path A — MCP client (runnable)

```
cargo run -p mcp-bridge
```

No API keys, no `npx`, no network. The only external process is a child this
binary spawns of **itself** in `--mcp-server` mode — a trivial stdio MCP server
exposing one tool, `get_weather`.

What the run does, in order:

1. **Stand up the server.** `mcp-bridge --mcp-server` exposes `get_weather(city)`
   over stdio, defined with `rmcp`'s `#[tool]` / `#[tool_router]` macros — the
   same surface a real third-party server presents.
2. **Discover, through the real `McpToolRegistry`.** `connect_server` spawns the
   child via `TokioChildProcess`, runs the MCP initialize handshake, lists the
   server's tools, and indexes them under the qualified name
   `mcp__weather__get_weather`.
3. **A mock LLM emits a tool call** for that discovered name (it isn't hard-coded
   — the name comes from the registry).
4. **The call goes over a real MCP connection.** The agent dispatches it through
   the product's `ToolExecutor`, which routes the `mcp__weather__get_weather`
   call to the live client the registry keeps in its connection pool and invokes
   `call_tool`. The weather string the agent prints came back across that
   connection.
5. **The model writes the final answer** from the tool result.

Expected output (abridged):

```
[1] Connecting to the 'weather' MCP server via McpToolRegistry…
    discovered 1 tool(s) across 1 server(s):
      • mcp__weather__get_weather  (server=weather, bare=get_weather)  — Get the current weather for a city.

[2] User asks: What's the weather in London?
  → agent calls MCP tool `mcp__weather__get_weather` with {"city":"London"}
  ← MCP server returned: {"text":"Weather in London: 13°C, overcast with light drizzle."}

[3] Final agent answer:
    Here's what I found — Weather in London: 13°C, overcast with light drizzle.
    tool call recorded: mcp__weather__get_weather {"city":"London"} → {"text":"Weather in London: 13°C, overcast with light drizzle."}
```

The result depends on the `city` argument (London vs Tokyo vs SF return
different strings), which only works because the argument really crossed the
wire to the server.

### How the call path works

`McpToolRegistry::connect_server`
([`crates/axocoatl-mcp/src/registry.rs`](../../crates/axocoatl-mcp/src/registry.rs))
does the discovery handshake and **keeps the client alive** in a per-server
connection pool. The `ToolExecutor` MCP backend
([`crates/axocoatl-tools/src/executor.rs`](../../crates/axocoatl-tools/src/executor.rs))
routes an `mcp__server__tool` call to that live client via
`McpToolRegistry::call_tool`.

So this example drives the real runtime path end to end: discovery **and**
execution both go through the same registry + executor the daemon wires into its
agents — there is no out-of-band client. Nothing is mocked except the LLM: the
tool list, the call, and the returned string all cross a real MCP stdio
connection.

---

## Path B — MCP server (config + host registration)

The other direction: expose your Axocoatl agents so an external host (Claude
Desktop, another Axocoatl instance, any MCP client) can call them.

[`axocoatl.example.mcp.yaml`](./axocoatl.example.mcp.yaml) defines one agent,
`weather`. Serve it over stdio with the CLI:

```sh
axocoatl mcp serve -c examples/mcp-bridge/axocoatl.example.mcp.yaml
```

This bootstraps the daemon and speaks MCP on stdin/stdout, exposing each agent
as an `agent_<id>` tool. With the config above, a client will discover one tool:
`agent_weather`, taking `{ "input": "<text>" }`. (Logs go to stderr; stdout is
the JSON-RPC channel.)

### Register it in a host's `mcp.json`

Most MCP hosts read a JSON config that launches each server as a stdio
subprocess. Add Axocoatl as one entry:

```json
{
  "mcpServers": {
    "axocoatl": {
      "command": "axocoatl",
      "args": [
        "mcp",
        "serve",
        "-c",
        "/absolute/path/to/examples/mcp-bridge/axocoatl.example.mcp.yaml"
      ]
    }
  }
}
```

Notes that make this work in practice:

- Use an **absolute path** for `-c` — the host launches the command from its own
  working directory, not yours.
- If `axocoatl` isn't on the host's `PATH`, set `command` to the absolute path of
  the binary (e.g. `target/release/axocoatl`).
- After the host restarts, it lists `agent_weather` and calls it with
  `{ "input": "What's the weather in Tokyo?" }`.

You can verify the server independently of any host with the CLI's own client
commands, which connect using the same registry this example exercises:

```sh
# In one shell, point a SECOND config's mcp_servers at the serving instance,
# then list what it exposes:
axocoatl mcp tools  -c your-client-config.yaml
axocoatl mcp servers -c your-client-config.yaml
```

---

## Transports: stdio vs streamable HTTP

`McpTransportType`
([`crates/axocoatl-mcp/src/registry.rs`](../../crates/axocoatl-mcp/src/registry.rs))
has two arms, and `mcp_servers:` entries in YAML pick between them with the
`transport:` field:

| Transport | YAML `transport:` | Use it for | Key fields |
|---|---|---|---|
| **stdio** | `stdio` | Local servers launched as a child process (filesystem, git, the official `npx` reference servers, this example). Axocoatl spawns `command` + `args` and talks over the child's stdin/stdout. | `command`, `args`, `env` (the child reads its API key/token from `env`) |
| **streamable HTTP** | `streamable_http` (or `http`) | Remote/hosted servers reached over HTTP. | `url`, `headers` (bearer tokens / API keys) |

This example uses **stdio** because the trivial server is a local subprocess.
For a remote server you would not change any of the discovery or call code — only
the `McpTransportType` (or the `transport:` line in YAML). SSE was removed
upstream in rmcp 0.11; streamable HTTP is the remote transport now.

```yaml
# stdio — local subprocess
- name: filesystem
  transport: stdio
  command: npx
  args: ["-y", "@modelcontextprotocol/server-filesystem", "/tmp"]
  env: {}

# streamable_http — remote server
- name: remote-tools
  transport: streamable_http
  url: https://mcp.example.com/mcp
  headers:
    Authorization: "Bearer ${REMOTE_MCP_TOKEN}"   # ${VAR} is interpolated at load
```

---

## Where this lives in the real runtime

- MCP client registry, transports, qualified names:
  [`crates/axocoatl-mcp/src/registry.rs`](../../crates/axocoatl-mcp/src/registry.rs)
- MCP server (`agent_<id>` tools), `AgentExecutor`:
  [`crates/axocoatl-mcp/src/server.rs`](../../crates/axocoatl-mcp/src/server.rs)
- Per-call human-in-the-loop approval + persisted permissions:
  [`crates/axocoatl-mcp/src/approval.rs`](../../crates/axocoatl-mcp/src/approval.rs),
  [`crates/axocoatl-mcp/src/permissions.rs`](../../crates/axocoatl-mcp/src/permissions.rs)
- The `axocoatl mcp serve` command: `axocoatl-cli/src/main.rs` (`cmd_mcp_serve`)
- The production tool-execution loop this example's agent mirrors:
  `crates/axocoatl-actor/src/default_behavior.rs`
- Architecture overview: [`docs/ARCHITECTURE.md`](../../docs/ARCHITECTURE.md)
