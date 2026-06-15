# Multi-provider routing — local model + frontier model in one workflow

Axocoatl lets **each agent pick its own provider**. So in a single workflow you
can route the cheap, high-volume steps to a small local model (Ollama) and
reserve an expensive frontier model for the one step that actually needs the
big context window and tool-calling. Same DAG, mixed providers, very different
cost per agent.

```
cargo run -p multi-provider
```

No API keys — it uses two mock providers with one canned reply per role, so the
run is CI-safe and deterministic.

## The graph

```
        triage ──────▶ drafter ──────▶ synthesizer
        (local)         (local)         (frontier)
        └──────────────────────────────────▶┘
        (synthesizer depends on BOTH triage and drafter)
```

| agent       | provider      | model               | tier      | why this tier                          |
|-------------|---------------|---------------------|-----------|----------------------------------------|
| triage      | `local-small` | `llama3.2:3b`       | local     | classify + route — trivial, runs free  |
| drafter     | `local-small` | `llama3.2:3b`       | local     | first pass — high volume, runs free     |
| synthesizer | `frontier`    | `claude-sonnet-4-6` | frontier  | needs big context + tool-calling        |

## When to mix providers — the research→summarize pattern

The pattern this example demonstrates is general: **fan the cheap work out to a
local model, then converge on a frontier model for the one step that needs
it.**

- The early steps (triage, drafting, research, extraction, classification) are
  high-volume and forgiving — a small local model handles them for free and
  keeps your data on the box.
- The final step (synthesis, judgment, a customer-facing answer) benefits from
  the strongest model and the largest context window, because it reads
  *everything upstream produced* and its quality is what ships.

That asymmetry is exactly where per-agent provider selection pays off: most of
the token volume runs free locally, and you only pay frontier prices on the
single step that earns it.

## The two providers

The example gives the two tiers genuinely different `capabilities()` and price,
because that is what a router inspects to decide where a step belongs:

| capability          | `local-small` (llama3.2:3b) | `frontier` (claude-sonnet-4-6) |
|---------------------|-----------------------------|--------------------------------|
| `max_context_tokens`| 8,192                       | 200,000                        |
| `tool_calling`      | false                       | true                           |
| `reasoning`         | false                       | true                           |
| price               | free (runs locally)         | ~$3 / 1M in, $15 / 1M out      |

Each agent's `AgentConfig.provider` / `.model` record the choice — the same two
fields a YAML agent sets via `provider:` and `model:`.

## What the run prints

The payoff is the per-agent provider + cost table. Two cheap local steps and
one frontier step run in the same DAG, and the cost lands almost entirely on
the one agent that needed the frontier model:

```
Per-agent provider + cost:
  agent         tier          model                    tokens   cost (USD)
  triage        local-small   llama3.2:3b                 270     $0.00000
  drafter       local-small   llama3.2:3b                 270     $0.00000
  synthesizer   frontier      claude-sonnet-4-6          2020     $0.01350

Cost contrast:
  local tier:      540 tokens  →  $0.00000  (2 agents, runs on your box)
  frontier tier:  2020 tokens  →  $0.01350  (1 agent, hosted)
  the frontier step is 100% of the $0.01350 total ...
```

The activation order (`triage → drafter → synthesizer`) is **not scripted** —
it emerges from the `EventLattice`, exactly as in the
[`stigmergic-workflow`](../stigmergic-workflow) example. The new thing here is
the *provider per agent*, not the coordination. `synthesizer` has two
dependencies, so its threshold is `0.5 × 2 = 1.0` and it fires only once
**both** `triage` and `drafter` have completed.

(The mock costs are illustrative public list prices, not a live quote.)

## Live mode — real Ollama + a real frontier model

[`axocoatl.multi-provider.yaml`](./axocoatl.multi-provider.yaml) runs the same
workflow shape against a real local model and a real hosted model.

> **Cost warning.** The `synthesizer` agent calls a hosted frontier model
> (Anthropic Claude Sonnet) on every run — that step costs real money
> (~$3 / 1M input + $15 / 1M output tokens at list price). `triage` and
> `drafter` run locally via Ollama and are free. The YAML sets a `token_budget`
> with `overflow_policy: abort` on the frontier agent so a runaway loop can't
> ring up a surprise bill.

### 1. Pull the local model

```sh
ollama serve &
ollama pull llama3.2:3b
```

### 2. Set the frontier key in your environment (never commit it)

```sh
export ANTHROPIC_API_KEY=sk-ant-...
```

The YAML reads it via `${ANTHROPIC_API_KEY}` interpolation, so the secret stays
out of the file. An unset variable interpolates to an empty string.

### 3. Preflight with `axocoatl doctor`

```sh
axocoatl doctor --config axocoatl.multi-provider.yaml
```

`doctor` checks that Ollama is reachable, that each Ollama agent's model is
pulled (and tells you the exact `ollama pull` to run if not), and that the
Anthropic provider has a non-empty API key — so you catch a missing key
*before* a run, not mid-bill.

### 4. Run it

```sh
axocoatl dev --config axocoatl.multi-provider.yaml
```

To use OpenAI instead of Anthropic, switch the `synthesizer` agent's
`provider:` to `openai`, set its `model:` (e.g. `gpt-4o`), and add an `openai:`
entry under `providers:` with `api_key: "${OPENAI_API_KEY}"`. The agent code
does not change — only the config.

## Where this lives in the real runtime

- The `LlmProvider` trait + `ProviderCapabilities`:
  [`crates/axocoatl-llm/src/provider.rs`](../../crates/axocoatl-llm/src/provider.rs)
- Real providers swapped in for the mocks: `axocoatl_llm_ollama::OllamaProvider`,
  `axocoatl_llm_anthropic::AnthropicProvider`, `axocoatl_llm_openai::OpenAiProvider`
- The per-agent `provider:` / `model:` config fields:
  [`crates/axocoatl-config/src/types.rs`](../../crates/axocoatl-config/src/types.rs)
- The coordination this builds on: the
  [`stigmergic-workflow`](../stigmergic-workflow) example and
  [`crates/axocoatl-coordination`](../../crates/axocoatl-coordination)
- Architecture overview: [`docs/ARCHITECTURE.md`](../../docs/ARCHITECTURE.md)
```
