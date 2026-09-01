# Axocoatl demonstration catalog

This catalog starts from the product model, not from a list of UI controls. Each
demonstration must answer one user question, exercise the real runtime, and leave
visible evidence that the claim is true. A recording is made only after its
scenario can be reset and repeated from a fresh Axocoatl data directory.

## The concepts we must not collapse together

| Concept | What it is | What it is not |
| --- | --- | --- |
| Workspace | A durable, user-named identity for one authorized project directory that can contain several Sessions. | A chat, a path label, or an attempt. |
| Session | The durable work context around one workspace: conversation, mode, tools, sandbox, files, terminal, Preview, and Git state. | One model response. |
| Turn | One accepted request and its durable execution evidence inside a Session. | The whole Session. |
| Single-agent Session | The ordinary path: one configured Agent works in the Session sandbox and conversation. | A reduced demo mode. It is the default product path. |
| Multi-agent Session | A fixed set of Agents shares one Session and sandbox. Agents run in dependency order and downstream Agents receive earlier outputs. | Parallel competing solutions. |
| Agent graph | The visible dependency and live-status view for the Agents in a Session. | The event lattice. |
| Ways | Independent candidate implementations created from the same repository snapshot and prompt, each in its own repository clone and sandbox. | A multi-agent handoff inside one Session. |
| Event lattice | Typed events published by Skills and runtime activity, then consumed by Automations, webhooks, and observers. | The Agent graph or a visual workflow editor. |
| Automation | A durable executable DAG with triggers, inputs, run history, and optional human interrupts. | An Agent Session or a scheduled chat macro. |
| Coordinator | A separate dynamic path that decomposes work, auctions tasks by capability and budget, runs workers, and synthesizes a result. | The fixed dependency order used by a multi-agent Session. |

## 1.0 film portfolio

[`films/portfolio.json`](films/portfolio.json) is the authoritative release
manifest. It fixes the 12 slugs, complete Showcase order, additional page
placements, scenario and fixture, beats, evidence, duration, media paths,
poster beat, and provenance path. The Showcase is the complete directory; Home,
Concepts, and Why reuse selected films without changing their identity.

All twelve entries are accepted against the 1.0 scenario, media, and provenance
contracts. The final replacement pass rejected stale, incomplete, duplicate-frame,
and false-success takes before promoting the coherent Session Workbench,
Workspace/Sessions/Turns, Several Ways, Git Last turn, and shared-core-memory
recordings. Older pairs remain reference material only.

The 12 provenance files are restored byte-for-byte from their earliest commit
after a history audit found later source and binary rewrites made without
recapture. Their source and binary fields are first-committed declarations; the
capture binary bytes are not preserved for independent authentication. The
`v1.0.1` incident attestation audits the frozen tag's source/binary-only rewrite,
binds all 55 changed paths (43 non-recording plus 12 provenance rewrites), and
proves both frozen and restored 153-artifact sets plus the remaining protected
filmed-product content. It does not relabel any take as a `v1.0.1` capture.

### Product foundation

| Showcase | Film | User question | Required proof | Additional placement |
| ---: | --- | --- | --- | --- |
| 1 | **Single-agent workbench** | “Can I do ordinary coding work here?” | One Agent repairs the visible defect; Files, diff, six checks, Preview, Source Control, and completed Turn agree. | Home 1 |
| 2 | **Workspace, Sessions, and Turns** | “What persists, and where does my work live?” | One named Workspace owns two Sessions; the exact completed Turn survives switching and reload. | Concepts 1; Why 1 |
| 3 | **Durable context and exact Stop** | “Does interruption stay under my control?” | Once/Session context, live reconnect, exact Stop, cancelled History, and a clean next Turn all remain honest. | Home 2; Concepts 2 |
| 4 | **Sandbox, Terminal, and Preview** | “Where does the code actually run?” | Session-owned container identity, runtime-authority label, real check, published port, and same-checkout Preview line up. | Concepts 3 |

### Collaboration and decision ownership

| Showcase | Film | User question | Required proof | Additional placement |
| ---: | --- | --- | --- | --- |
| 5 | **Multi-agent dependency handoff** | “How do Agents collaborate rather than compete?” | The architect → reviewer edge exists before execution; activation is sequential; both non-empty outputs survive reload. | — |
| 6 | **Several checked Ways** | “What if the implementation choice is uncertain?” | Two independent non-empty candidates pass the same protected Check, expose Outcome and Route, receive unique Judge ranks, and leave Keep to the operator. | — |
| 7 | **Git ownership and Last turn** | “What exactly did Keep change, and who owns Git?” | The kept patch remains uncommitted; Last turn shows exact hunks and rehydrates after restart; staging stays optional. | Why 2 |

### Configured runtime and durable execution

| Showcase | Film | User question | Required proof | Additional placement |
| ---: | --- | --- | --- | --- |
| 8 | **Runtime configuration in Settings** | “Where do Agents, Skills, MCP, and Automations belong?” | All four are reachable inside Settings; the same selected Session remains visible behind the tour; a real completed run exposes Result. | Concepts 4 |
| 9 | **Skill event to Automation Result** | “How does Axocoatl react to typed events?” | One typed Skill event creates one matching run, crosses its Interrupt, and retains exact `final_content` after reload. | Concepts 5 |
| 10 | **Session MCP approval** | “Can an Agent safely use an external tool?” | Pending approval survives reload and 30 seconds; Deny dispatches zero calls; a fresh Allow once dispatches exactly one; completed tool evidence persists. | Concepts 6 |
| 11 | **Shared core memory across Sessions** | “What can deliberately persist across Agents?” | One Agent writes the shared block; another Session/Agent recalls the exact nonce after daemon restart while transcripts remain separate. | Concepts 7 |
| 12 | **Human-in-the-loop Automation recovery** | “Can a durable workflow stop for judgment and recover?” | A top-level Interrupt survives daemon restart, upstream nodes do not replay, Resume continues the same run, and completed history exposes Result. | — |

Coordinator and provider-routing films remain deliberate non-films for 1.0.
They are not missing members of this portfolio and must not appear as empty or
speculative Showcase cards.

## Deliberate non-films

A2A, raw webhooks, HTN primitives, and tool-hook internals are legitimate
technical capabilities, but they do not yet have a clear first-party journey in
the browser workbench. They should be demonstrated in documentation or runnable
examples until a user can discover and complete them in `/`.

The Coordinator film is also held back: Session-scoped coordinators currently
run as ordinary Session Agents, while Automation coordinator progress and
Automation completion use different run identities. Until that correlation is
wired, a film could show activity but not a trustworthy completed journey.

## Fixture set

The launch films use three small repositories so the product does not look like
one feature wrapped around one discount bug.

- **`northstar-storefront`** — a visible order-total defect. Best for ordinary
  single-agent work, context, Terminal, Preview, Git, and durable Turns.
- **`harbor-catalog`** — a stale search-cache contract with more than one
  defensible invalidation strategy. Best for deterministic Ways and comparison,
  plus the design-only architect → reviewer handoff.
- **`signal-desk`** — a noisy incident-correlation failure plus logs and a
  runbook. Best for event-lattice, Automation, and the Settings/runtime tour;
  the failed incident-repair handoff is not an active launch-film path.

Every scenario uses a fresh marked temporary root, a fresh Git repository copied
from one immutable fixture, and a fresh Axocoatl data directory. This prevents an
old transcript, retained attempt, memory value, Automation run, or Git change
from becoming accidental “proof” in a later recording.

## Evidence standard

A film is ready only when all of the following are true:

1. The scenario starts from a documented clean state.
2. The action is performed in the current browser app, not a mock shell.
3. The runtime result is independently inspectable in the relevant durable
   store, API, Git checkout, event record, or Automation run.
4. Reload or navigation does not erase the evidence.
5. The voiceover/caption describes the actual execution shape: sequential
   dependency handoff, parallel Ways, or event reaction as appropriate.
6. The recording contains one main idea and is short enough to understand
   without narration.
