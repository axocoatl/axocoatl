# A2A server — expose an agent over the Agent-to-Agent protocol

A2A (Agent-to-Agent) is Axocoatl's **cross-framework interop** protocol. It lets
an agent built in *some other* system discover one of your agents and hand it a
task over plain HTTP — no shared process, runtime, or language. Two endpoints
carry the whole exchange:

```
GET  {endpoint}/.well-known/agent.json   → the Agent Card (discovery)
POST {endpoint}/tasks                     → submit a task, get a result
```

This example runs **both halves in one process** so it is self-verifying with
no external tools and no API keys:

```
cargo run
```

It spawns a real agent actor, stands up the A2A server on an ephemeral
`127.0.0.1` port, then — acting as a separate A2A client — (a) fetches the Agent
Card and (b) submits a task and prints the result. The LLM is a mock with one
canned reply; everything else (`build_a2a_router`, `A2AClient`, `AgentActor`) is
the real runtime.

## What it does, step by step

```
┌─────────────────────────── this binary ───────────────────────────┐
│                                                                    │
│  AgentActor ("echo-bot")        A2A server (axum)      A2A client  │
│       ▲                          GET /.well-known/…  ◀──discover── │
│       │ execute_agent()          POST /tasks         ◀──send_task─ │
│  TaskHandler ──────────────────────┘                              │
│       (maps an inbound A2A task onto a real agent execution)       │
└────────────────────────────────────────────────────────────────────┘
```

1. Spawn `echo-bot` as a ractor `AgentActor` — the same actor the daemon spawns.
2. Bind an ephemeral localhost port and publish an **Agent Card** pointing at it.
3. Serve the card + task endpoint with the crate's `build_a2a_router`.
4. As a client: `discover()` the card, then `send_task()` and await the result.
5. Assert the round-trip (status `Completed`, output reflects the input), so
   `cargo run` fails loudly if the flow ever regresses.

## Expected output (abridged)

```
Spawned agent 'echo-bot' as a ractor actor.
A2A server bound on http://127.0.0.1:<port> (ephemeral port).

[client] GET http://127.0.0.1:<port>/.well-known/agent.json  (discovery)
Discovered Agent Card:
  id           : echo-bot
  capabilities : ["echo"]
  ...
[client] POST http://127.0.0.1:<port>/tasks  (submit task 'task-001')
Task result:
  task_id : task-001
  status  : Completed
  output  : {"content":"echo-bot received: \"Hello from a foreign A2A agent!\""}

Verified end-to-end: discovery returned the card, the task ran through
the real agent actor, and the result came back over HTTP. ✓
```

## The same flow with `curl`

The example talks to the server over real HTTP, so the identical wire shape
works from `curl`. The crate's `build_a2a_router` (used here) mounts the task
route at `/tasks`. Pick the `<port>` the example prints, or point these at a
running daemon (see the next section for the daemon's paths and auth).

Fetch the Agent Card:

```sh
curl -s http://127.0.0.1:<port>/.well-known/agent.json | jq .
```

```json
{
  "id": "echo-bot",
  "name": "Echo Bot",
  "description": "A minimal Axocoatl agent exposed over A2A; echoes the caller's input.",
  "version": "0.1.0",
  "endpoint": "http://127.0.0.1:<port>",
  "capabilities": ["echo"],
  "input_schema": { "type": "object", "properties": { "input": { "type": "string" } }, "required": ["input"] },
  "output_schema": { "type": "object", "properties": { "content": { "type": "string" } } },
  "authentication": { "scheme": "none", "endpoint": null }
}
```

Submit a task (`receiver_id` names the agent to run — its card `id`):

```sh
curl -s -X POST http://127.0.0.1:<port>/tasks \
  -H 'content-type: application/json' \
  -d '{
        "id": "task-001",
        "sender_id": "external-client",
        "receiver_id": "echo-bot",
        "input": { "input": "Hello from a foreign A2A agent!" },
        "context": { "workflow_id": null, "correlation_id": "corr-001", "token_budget": null },
        "timeout_secs": 30
      }' | jq .
```

```json
{
  "task_id": "task-001",
  "status": "Completed",
  "output": { "content": "echo-bot received: \"Hello from a foreign A2A agent!\"" },
  "error": null
}
```

A2A here is **request/response**: `POST /tasks` blocks until the agent finishes
and returns the terminal `A2ATaskResult` (`status: Completed` or `Failed`). The
`TaskStatus` enum (`Pending`/`Running`/`Completed`/`Failed`/`Cancelled`) leaves
room for a future streaming/poll transport; this example exercises the
synchronous path end to end.

## Running against the daemon instead

`axocoatl-server` mounts the same A2A types behind **absolute** paths and its
auth layer, so the routes differ slightly from the bare crate router used above:

| | this example (`build_a2a_router`) | daemon (`axocoatl-server`) |
|---|---|---|
| Card | `GET /.well-known/agent.json` | `GET /.well-known/agent.json` |
| Task | `POST /tasks` | `POST /a2a/tasks` |
| Auth | none | bearer token (server auth layer) |
| `receiver_id` | the single exposed agent | any agent id from the card's `capabilities` |

Against a daemon, the card lists every configured agent in `capabilities`, and
you address one by setting the task's `receiver_id` to its id. The task body and
result shape are otherwise identical to the `curl` calls above (add
`-H 'authorization: Bearer <token>'`).

## When to use A2A vs MCP vs the HTTP execute endpoint

- **A2A** (this example) — *agent calls agent, across frameworks.* The unit of
  work is a delegated **task**; discovery is a published Agent Card. Reach for it
  when something outside Axocoatl needs to treat one of your agents as a peer it
  can hand work to. Framework-neutral and public.
- **MCP** — *an agent calls a tool.* Your agent is the **client**; an MCP server
  exposes capabilities (functions, resources) the agent pulls in mid-execution.
  The direction is inverted from A2A: A2A is "someone delegates a whole task to
  my agent," MCP is "my agent reaches out for a capability."
- **`POST /api/agents/{id}/execute`** — *your own app drives your own agent.*
  The daemon's internal HTTP API. It is Axocoatl-shaped and not a cross-vendor
  contract; use it for first-party UIs and scripts. A2A is the public,
  framework-neutral face of the same underlying agent execution.

## Where this lives in the real runtime

- A2A types, client, server scaffold, `TaskHandler`:
  [`crates/axocoatl-a2a`](../../crates/axocoatl-a2a)
- Production wiring (`/.well-known/agent.json`, `/a2a/tasks`, auth):
  `axocoatl-server/src/routes.rs` (`a2a_agent_card`, `a2a_receive_task`) and
  `axocoatl-server/src/lib.rs`
- The agent actor an inbound task runs through:
  [`crates/axocoatl-actor`](../../crates/axocoatl-actor)
- Architecture overview: [`docs/ARCHITECTURE.md`](../../docs/ARCHITECTURE.md)
  (see the Protocols section)
