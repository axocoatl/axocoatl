---
title: Ollama quickstart
description: "End-to-end walkthrough of running Axocoatl against a local Ollama provider."
---

# Axocoatl Local Testing Guide

Complete hands-on testing plan for all Axocoatl features using Ollama + llama3.2.

---

## Prerequisites

### Start Ollama
```bash
ollama serve &
```
Ollama listens on `http://localhost:11434`. The model `llama3.2` (2GB) is already pulled.

### Verify Ollama is running
```bash
ollama list
# Should show: llama3.2:latest

curl -s http://localhost:11434/v1/chat/completions \
  -H "Content-Type: application/json" \
  -d '{"model":"llama3.2","messages":[{"role":"user","content":"Say hello"}],"max_tokens":10}'
```

### Stop Ollama (when done)
```bash
pkill ollama
```

---

All commands run from `~/repos/Axocoatl`.

---

## Phase 1: Foundation — Config & Toolchain

**1.1 — Config validation**
```bash
cargo run -p axocoatl-cli -- validate axocoatl.yaml
```
Expect: "Config is valid" — lists 3 agents (assistant, researcher, summarizer), 1 workflow, Ollama provider.

**1.2 — Config rejection (bad YAML)**
```bash
cat > /tmp/bad-config.yaml << 'EOF'
agents:
  - id: ""
    provider: ollama
    model: llama3.2
  - id: assistant
    provider: ollama
    model: llama3.2
    token_budget:
      per_execution: 100
      per_call: 200
      overflow_policy: abort
  - id: assistant
    provider: ollama
    model: llama3.2
providers:
  ollama:
    base_url: "http://localhost:11434"
EOF
cargo run -p axocoatl-cli -- validate /tmp/bad-config.yaml
```
Expect: Errors for empty agent ID, per_call > per_execution, and duplicate agent ID.

**1.3 — Env var interpolation**
```bash
cat > /tmp/env-config.yaml << 'EOF'
agents:
  - id: assistant
    provider: ollama
    model: llama3.2
providers:
  ollama:
    base_url: "${OLLAMA_URL}"
EOF
export OLLAMA_URL="http://localhost:11434"
cargo run -p axocoatl-cli -- validate /tmp/env-config.yaml
```
Expect: Parses successfully with `${OLLAMA_URL}` replaced by the env var value.

**1.4 — Init scaffolding**
```bash
cargo run -p axocoatl-cli -- init /tmp/test-axocoatl
ls -la /tmp/test-axocoatl/
cat /tmp/test-axocoatl/axocoatl.yaml
rm -rf /tmp/test-axocoatl
```
Expect: Directory created with template YAML, .env.example, and data/ directory.

---

## Phase 2: Single-Agent Chat (Core Pipeline)

**2.1 — Basic chat (in-process)**
```bash
cargo run -p axocoatl-cli -- chat --agent assistant --config axocoatl.yaml
```
Type: `What is 2+2?`

Expect: LLM responds with an answer, token counts displayed (input/output/total).

**2.2 — Multi-turn conversation**

Still in the same chat session, type:
```
What did I just ask you?
```
Expect: LLM references your previous question about 2+2 (session memory working).

**2.3 — Token tracking per turn**

Send a few more messages, observe the token count display after each.

Expect: Counts increment with each turn. Total accumulates across the session.

**2.4 — System prompt injection**

The assistant agent has system_prompt "You are a helpful assistant powered by Axocoatl." — ask:
```
What are you?
```
Expect: Response references being an assistant/Axocoatl. Ctrl+C to exit.

---

## Phase 3: Token Budget Enforcement

**3.1 — Tight budget with abort policy**

Create a test config:
```bash
cat > /tmp/budget-test.yaml << 'EOF'
agents:
  - id: budget-agent
    provider: ollama
    model: llama3.2
    token_budget:
      per_execution: 200
      per_call: 100
      overflow_policy: abort
providers:
  ollama:
    base_url: "http://localhost:11434"
EOF
cargo run -p axocoatl-cli -- chat --agent budget-agent --config /tmp/budget-test.yaml
```
Send a message, then keep chatting until budget is exhausted.

Expect: Execution stops with a budget exceeded error once tokens hit 200.

**3.2 — Warn policy**

Change to `overflow_policy: warn` in the file above, restart chat.

Expect: Warning printed but execution continues past budget.

---

## Phase 4: HTTP API (Dev Mode)

**4.1 — Start dev server**
```bash
cargo run -p axocoatl-cli -- dev axocoatl.yaml
```
Expect: Prints startup info — config loaded, 3 agents spawned, IPC socket path, HTTP at `http://0.0.0.0:8080`.

*Open a second terminal for the following:*

**4.2 — Health endpoints**
```bash
curl -s http://localhost:8080/health | jq .
curl -s -o /dev/null -w "%{http_code}" http://localhost:8080/health/ready
curl -s -o /dev/null -w "%{http_code}" http://localhost:8080/health/live
```
Expect: Health returns `{status: "healthy", agents: 3}`. Ready returns 200. Live returns 200.

**4.3 — List agents**
```bash
curl -s http://localhost:8080/api/agents | jq .
```
Expect: JSON array with 3 agents (assistant, researcher, summarizer) showing id, name, provider, model.

**4.4 — Execute agent via API**
```bash
curl -s -X POST http://localhost:8080/api/agents/assistant/execute \
  -H "Content-Type: application/json" \
  -d '{"input": "What is the capital of France?"}' | jq .
```
Expect: `{"output": "Paris..."}` from Ollama.

**4.5 — Agent status**
```bash
curl -s http://localhost:8080/api/agents/assistant/status | jq .
```
Expect: Agent status (Idle after execution completes).

**4.6 — Invalid agent**
```bash
curl -s -X POST http://localhost:8080/api/agents/nonexistent/execute \
  -H "Content-Type: application/json" \
  -d '{"input": "hello"}' | jq .
```
Expect: Error response with "not found".

**4.7 — WebSocket streaming**
```bash
# Install websocat if needed: cargo install websocat
echo '{"agent_id":"assistant","input":"Count to 5"}' | websocat ws://localhost:8080/ws
```
Expect: Streamed text deltas arriving incrementally, then usage stats and done signal.

---

## Phase 5: Workflow Execution (Stigmergic Coordination)

This is the key feature — multi-agent workflows driven by the EventLattice.

**5.1 — List workflows**
```bash
curl -s http://localhost:8080/api/workflows | jq .
```
Expect: JSON array with 1 workflow: `research-and-summarize` with agents `[researcher, summarizer]` and entry_point `researcher`.

**5.2 — Execute workflow via API**
```bash
curl -s -X POST http://localhost:8080/api/workflows/research-and-summarize/execute \
  -H "Content-Type: application/json" \
  -d '{"input": "What is photosynthesis?"}' | jq .
```
Expect:
- `agent_outputs` array with 2 entries: researcher's detailed answer, then summarizer's concise summary
- `output` field contains the summarizer's final content (last agent in chain)
- `completed_agents` shows `["researcher", "summarizer"]`
- `total_tokens` shows combined usage
- In the dev terminal, you should see logs showing: researcher activating, completing, then summarizer activating with the researcher's output as context

**5.3 — Verify agent ordering**

Check the dev terminal logs. You should see:
```
Workflow started — initial activation
Activating agent in workflow  agent=researcher
Agent completed in workflow   agent=researcher
Activating agent in workflow  agent=summarizer
Agent completed in workflow   agent=summarizer
Workflow completed
```
This confirms stigmergic coordination: researcher's TaskCompleted event pushed the summarizer's pheromone signal past its threshold, triggering automatic activation.

**5.4 — Workflow with different input**
```bash
curl -s -X POST http://localhost:8080/api/workflows/research-and-summarize/execute \
  -H "Content-Type: application/json" \
  -d '{"input": "Explain the Rust ownership model"}' | jq .
```
Expect: Researcher gives detailed Rust ownership explanation, summarizer condenses it.

**5.5 — Invalid workflow**
```bash
curl -s -X POST http://localhost:8080/api/workflows/nonexistent/execute \
  -H "Content-Type: application/json" \
  -d '{"input": "test"}' | jq .
```
Expect: Error "Workflow not found: nonexistent".

---

## Phase 6: Workflow CLI Commands

**6.1 — List workflows via CLI**
```bash
cargo run -p axocoatl-cli -- workflow list -c axocoatl.yaml
```
Expect: Table showing workflow ID, name, agents, entry point.

**6.2 — Run workflow via CLI (in-process)**

Without a running daemon:
```bash
cargo run -p axocoatl-cli -- workflow run research-and-summarize \
  -i "What causes earthquakes?" -c axocoatl.yaml
```
Expect: Bootstraps in-process, runs workflow, prints per-agent outputs with token counts, then final output.

**6.3 — Run workflow via CLI (IPC)**

With dev server running in another terminal:
```bash
cargo run -p axocoatl-cli -- workflow run research-and-summarize \
  -i "What is dark matter?" -c axocoatl.yaml
```
Expect: "Connected to daemon via IPC" — executes faster (no bootstrap), same output format.

---

## Phase 7: Memory & Persistence

**7.1 — Checkpoint creation**

After any chat session, check for checkpoint files:
```bash
ls -la ./data/checkpoints/
```
Expect: `.ckpt` files exist for agents you chatted with.

**7.2 — Checkpoint restoration**

Start chat, have a conversation, Ctrl+C, then restart:
```bash
cargo run -p axocoatl-cli -- chat --agent assistant --config axocoatl.yaml
```
Ask: `What did we talk about before?`

Expect: If session is restored from checkpoint, LLM may recall prior context.

**7.3 — Sessions list**
```bash
cargo run -p axocoatl-cli -- sessions list --agent assistant
```
Expect: Lists checkpoint files with timestamps.

**7.4 — Checkpoint pruning**

Have 4+ chat sessions. Check that only the 3 most recent checkpoints remain:
```bash
ls ./data/checkpoints/ | wc -l
```
Expect: 3 or fewer checkpoint files per agent.

---

## Phase 8: IPC Communication

**8.1 — Chat via IPC (two terminals)**

Terminal 1:
```bash
cargo run -p axocoatl-cli -- dev axocoatl.yaml
```

Terminal 2:
```bash
cargo run -p axocoatl-cli -- chat --agent assistant --config axocoatl.yaml
```
Expect: Chat says "Mode: connected to daemon (IPC)", not "in-process".

**8.2 — IPC fallback**

Without a running daemon:
```bash
cargo run -p axocoatl-cli -- chat --agent assistant --config axocoatl.yaml
```
Expect: Falls back to in-process mode, still works.

---

## Phase 9: Multi-Agent Configuration

**9.1 — Verify all 3 agents**

With dev server running:
```bash
curl -s http://localhost:8080/api/agents | jq '.[].id'
```
Expect: `"assistant"`, `"researcher"`, `"summarizer"`

**9.2 — Execute each agent independently**
```bash
curl -s -X POST http://localhost:8080/api/agents/researcher/execute \
  -H "Content-Type: application/json" \
  -d '{"input": "Explain quantum entanglement"}' | jq .output

curl -s -X POST http://localhost:8080/api/agents/summarizer/execute \
  -H "Content-Type: application/json" \
  -d '{"input": "Explain quantum entanglement"}' | jq .output
```
Expect: Researcher gives a detailed answer. Summarizer gives 1-2 sentences. Different personalities confirmed.

**9.3 — Chat with specific agent**
```bash
cargo run -p axocoatl-cli -- chat --agent summarizer --config axocoatl.yaml
```
Expect: Concise responses matching the summarizer persona.

---

## Phase 10: Built-in Tools

**10.1 — Attempt tool trigger**

In a chat session, try:
```
Use the echo tool to echo "hello world"
```
If tool calling works: tool call displayed, echo result shown, LLM incorporates result.
If it doesn't trigger: expected — llama3.2 has limited function calling support.

**10.2 — Verify tools are registered**

Check the dev server startup output — it should mention registering built-in tools (echo, json_keys, text_split).

---

## Phase 11: Serve Mode (Production)

**11.1 — Start serve mode**

Ctrl+C the dev server, then:
```bash
cargo run -p axocoatl-cli -- serve axocoatl.yaml
```
Expect: HTTP server starts, no IPC socket created. API endpoints still work.

**11.2 — Verify API works in serve mode**
```bash
curl -s http://localhost:8080/health | jq .
curl -s -X POST http://localhost:8080/api/agents/assistant/execute \
  -H "Content-Type: application/json" \
  -d '{"input": "Hello"}' | jq .
curl -s -X POST http://localhost:8080/api/workflows/research-and-summarize/execute \
  -H "Content-Type: application/json" \
  -d '{"input": "What is gravity?"}' | jq .
```
Expect: All endpoints work identically to dev mode.

---

## Phase 12: Coordination Subsystem (Unit Tests)

```bash
cargo test -p axocoatl-coordination -- --nocapture
```
Expect: All tests pass — event lattice publish/subscribe, pheromone decay, threshold activation, HTN decomposition, auction scoring.

---

## Phase 13: Workflow Graph (Unit Tests)

```bash
cargo test -p axocoatl-graph -- --nocapture
```
Expect: All tests pass — graph construction, topological sort, cycle detection, parallel groups.

---

## Phase 14: MCP Protocol (Unit Tests)

```bash
cargo test -p axocoatl-mcp -- --nocapture
```
Expect: MCP server frame and tool discovery tests pass.

---

## Phase 15: WASM Isolation (Unit Tests)

```bash
cargo test -p axocoatl-isolation -- --nocapture
```
Expect: WASM compilation, sandbox execution, fuel metering tests pass.

---

## Phase 16: A2A Protocol (Unit Tests)

```bash
cargo test -p axocoatl-a2a -- --nocapture
```
Expect: Agent card serialization and task submission tests pass.

---

## Phase 17: Daemon Internals (Unit Tests)

```bash
cargo test -p axocoatl-daemon -- --nocapture
```
Expect: Workflow execution tracker, context building, cycle guard, IPC serialization tests all pass.

---

## Phase 18: Stress & Edge Cases

**18.1 — Long conversation**

Start a chat and send 20+ messages back and forth.
Expect: No crashes, memory stable, checkpoints keep rolling.

**18.2 — Rapid-fire API requests**
```bash
for i in $(seq 1 10); do
  curl -s -X POST http://localhost:8080/api/agents/assistant/execute \
    -H "Content-Type: application/json" \
    -d "{\"input\": \"Count to $i\"}" &
done
wait
```
Expect: All requests complete (some may queue), no panics.

**18.3 — Ollama restart recovery**

Mid-chat, stop Ollama (`pkill ollama`), send a message (expect error), restart Ollama (`ollama serve &`), send another message.
Expect: Graceful error on failure, recovery after restart.

**18.4 — Empty input**
```bash
curl -s -X POST http://localhost:8080/api/agents/assistant/execute \
  -H "Content-Type: application/json" \
  -d '{"input": ""}' | jq .
```
Expect: Handled gracefully (error or empty response, no panic).

**18.5 — Full test suite**
```bash
cargo test --workspace
```
Expect: 258+ tests, 0 failures.

---

## Phase 19: Benchmarks

```bash
cargo bench --bench token_efficiency
cargo bench --bench routing_latency
cargo bench --bench actor_throughput
cargo bench --bench isolation_startup
```
Expect: Benchmarks complete. TOON shows 20-35% token savings vs JSON.

---

## Quick Reference

| Action | Command |
|--------|---------|
| Start Ollama | `ollama serve &` |
| Stop Ollama | `pkill ollama` |
| Check Ollama | `ollama list` |
| Validate config | `cargo run -p axocoatl-cli -- validate axocoatl.yaml` |
| Dev mode (IPC+HTTP) | `cargo run -p axocoatl-cli -- dev axocoatl.yaml` |
| Serve mode (HTTP only) | `cargo run -p axocoatl-cli -- serve axocoatl.yaml` |
| Interactive chat | `cargo run -p axocoatl-cli -- chat --agent assistant --config axocoatl.yaml` |
| List workflows | `cargo run -p axocoatl-cli -- workflow list -c axocoatl.yaml` |
| Run workflow (CLI) | `cargo run -p axocoatl-cli -- workflow run research-and-summarize -i "query" -c axocoatl.yaml` |
| Run workflow (API) | `curl -X POST localhost:8080/api/workflows/research-and-summarize/execute -H 'Content-Type: application/json' -d '{"input":"query"}'` |
| Run all tests | `cargo test --workspace` |
| Run benchmarks | `cargo bench` |
