# Reddit

Post to /r/rust, /r/LocalLLaMA, and /r/MachineLearning. Same title,
different bodies tuned to the subreddit's interest. Cross-posting is
acceptable on Reddit; the bodies below are tailored, not duplicated.

---

## /r/rust

### Title

Axocoatl — a multi-agent runtime in pure Rust, with supervised actors and stigmergic coordination

### Body

Just shipped v0.1.0 of Axocoatl, an open-source Rust runtime for
self-coordinating multi-agent systems. Posting here because the
internals might be of more interest than the marketing copy.

The runtime is a workspace of ~20 crates. Highlights:

- **Actors via `ractor`.** Each agent is a supervised actor. Crashes
  trigger restarts from the last checkpoint; mailboxes are preserved.
  Token budgets are enforced before the LLM call inside the actor's
  message handler.
- **Coordination via an event lattice.** Agents publish events when
  they complete work. Downstream agents declare `depends_on` and a
  threshold; the lattice activates them when reached. No central
  scheduler. Implementation in `axocoatl-coordination`.
- **Memory in four tiers.** Session / checkpoint / long-term /
  semantic. Tier 4 uses Candle to run `all-MiniLM-L6-v2` (384-dim
  BERT) for embeddings — pure Rust, no ONNX, no C++ stdlib link
  issues. Falls back to a feature-hashing embedder when built with
  `--no-default-features`.
- **Sandboxed sessions.** Podman bind-mounted to a directory. Agent
  tools (`read_file`, `write_file`, `bash`, `spawn_terminal`) run
  inside; the dashboard sees every action live.
- **Provider trait** with implementations for Ollama, OpenAI,
  Anthropic, Mistral, Gemini, OpenRouter. Per-agent selection.
- **Dashboard** at `localhost:8080` served by Axum + `rust_embed`.
  Vanilla HTML + Web Components + Monaco. No React.

Total release binary: 13 MB stripped. 318 tests across the workspace.
`cargo fmt --check`, `cargo clippy -D warnings`, `cargo test
--workspace` all green in CI.

Code: https://github.com/axocoatl/axocoatl
Concepts page (with a live lattice you can watch pulse):
https://axocoatl.ai/concepts

Apache-2.0. Curious what you think — particularly about the
coordination model and the choice to skip ONNX for embeddings.

---

## /r/LocalLLaMA

### Title

Axocoatl — local-first multi-agent runtime that runs as a system service

### Body

Shipped v0.1.0 of Axocoatl, an open-source agentic runtime built for
local-first use. Built for the case where you want agents that *run
your business* in the background, not a chat interface you babysit.

Local LLM relevance:

- **Ollama is the default provider.** `axocoatl onboard` picks it
  unless you opt out. No API keys needed for the happy path.
- **Per-agent provider routing.** Use a 4-bit Llama for cheap
  classification, a frontier cloud model for the one agent that needs
  it. Same workflow, two providers.
- **Local neural memory.** Tier-4 semantic retrieval uses an embedded
  BERT model running through Candle — pure Rust, runs on CPU,
  downloads ~90 MB once and caches forever. No ONNX.
- **No telemetry. Air-gappable.** The runtime makes zero outbound
  calls except the ones you wire to an LLM provider.
- **Runs as a service** via systemd or launchd. Set it up once, your
  agents run while you sleep.

Concrete use cases people are running:

- Release pulse: a coordinator agent reads git activity every Monday
  morning and drafts release notes for review.
- Support triage: a proactive agent watches an inbox, classifies
  tickets, attaches customer history, routes the thread.
- Daily briefing: a planner reads your calendar and lattice
  interrupts at 8am and lands a 5-bullet summary in your inbox.
- Bug bisection: open a directory session, point at a failing test,
  let the agent run `git bisect` inside the sandbox.

13 MB Rust binary. Apache-2.0.

https://axocoatl.ai · https://github.com/axocoatl/axocoatl

Happy to answer questions about the local memory tier, provider
routing, or how the session sandbox handles bind-mounts on weird
WSL2 setups (we already found two cgroup-delegation bugs in podman
during testing).

---

## /r/MachineLearning

### Title

[P] Axocoatl: an open-source multi-agent runtime with stigmergic coordination

### Body

Wanted to share Axocoatl, an open-source Rust runtime for multi-agent
systems we just shipped at v0.1.0. The architectural angle that might
be of research interest is the coordination model.

Instead of a central scheduler routing tasks between agents, Axocoatl
uses a **stigmergic event lattice**: agents declare which events they
react to and publish events when they complete work. The lattice
accumulates signals and activates downstream agents when their
thresholds are crossed. The system has no global plan — agents
self-organize through the shared event space, similar in spirit to ant
pheromone trails (where the name comes from).

Concrete properties:

- Composability without rewiring. Add an agent that listens for
  `TaskCompleted{researcher}` and it joins the workflow automatically.
- Multi-agent auctions for the same Skill. When two agents hold a
  Skill that reacts to the same event, the lattice runs a quick
  auction based on token budget remaining, current status, and model
  capability.
- HTN symbolic planning is layered on top for cases where you want
  hierarchical task decomposition; agents can publish sub-tasks back
  into the lattice.

Memory: four tiers (session / checkpoint / long-term / semantic).
Tier 4 is a 384-dim BERT (`all-MiniLM-L6-v2`) running through Candle —
pure Rust, no ONNX, no C++ runtime — with cosine-similarity retrieval.
Recall is real on semantically related queries with no shared
keywords.

Local-first: runs against Ollama by default, swaps to OpenAI /
Anthropic / Mistral / Gemini / OpenRouter per agent.

We've been running it in production for ~6 weeks on small workflows
(release notes, support triage, daily briefings). The supervised actor
model + checkpointing means agents survive crashes that would tear
down a Python-based system.

Code: https://github.com/axocoatl/axocoatl
Architecture deep-dive: https://docs.axocoatl.ai/guides/architecture
Concepts page (with a live lattice demo):
https://axocoatl.ai/concepts

318 tests in CI. Apache-2.0. Welcome critique on the coordination
model in particular — I'm aware it doesn't generalize to every
multi-agent topology and would be curious where you think it breaks
down.
