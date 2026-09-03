# Axocoatl marketing site

Vanilla HTML, Web Components, and CSS. The public site tells one product story:
an open-source, local-first coding workbench built around one durable folder-anchored
Session. Conversation, context, files, terminal, Preview, tools, history, and Git are
the core work surface. Settings owns Agents, Skills, MCP servers, and Automations and
shows each Agent's provider/model assignment. Normal onboarding writes one owner-only
configuration for the current OS user; explicit project-local YAML remains an advanced
operator path. Isolated Ways, Checks, comparison, and Keep are an optional decision mode
when one answer is not enough.

## Local preview

Build the exact deployment payload, then serve it:

```bash
node scripts/validate.mjs
node scripts/build.mjs /tmp/axocoatl-marketing
cp ../../scripts/install.sh /tmp/axocoatl-marketing/install.sh
python3 -m http.server 8000 --directory /tmp/axocoatl-marketing
```

Open `http://localhost:8000/` and inspect desktop, narrow, light, dark, keyboard,
and reduced-motion states.

## Public pages

| Path | Purpose |
|---|---|
| `/` | Workbench positioning and product breadth |
| `/concepts` | Workspace, Session, Turn, sandboxed work surface, Settings, shared core memory, events, MCP approval, optional Ways, and runtime boundaries |
| `/why` | Why agent work needs a durable workspace |
| `/showcase` | Complete grouped directory of twelve product-film concepts, with the ordinary Session loop first and an evidence contract for each film |
| `/install` | Supported installation paths and first run |
| `/pricing` | License and user-owned infrastructure costs |
| `/integrations/openrouter` | OpenRouter provider setup |
| `/changelog` | Published release history |

## Deployment payload

`scripts/build.mjs` copies an explicit allowlist into a clean output directory, including
the repository-root `llms.txt` as the AI-readable product narrative at `/llms.txt`. The
validator keeps that file aligned with the visible 1.0 story and rejects the retired
runtime-first narrative. The deployment expects twelve product-film MP4/JPEG pairs in
`assets/films/`. The manifest contains twelve `ready` entries. Each MP4/JPEG pair has
exact source-frame, capture-record, staged-frame, durable-evidence, and shipped-media
hashes. A history audit restored all 12 provenance JSONs byte-for-byte from their first
commit after later source and binary rewrites made without recapture. Those source and
binary fields are first-committed declarations; the capture binary bytes are not
preserved for independent authentication. The portable and exact-source verifiers are
the normal acceptance boundary. The one-time `v1.0.1` incident attestation separately
audits the frozen tag's source/binary-only rewrites and complete 55-path delta.
Compatibility never relabels an existing take as a capture of the patch binary. Films
may appear on more than one page when the same product evidence answers a different
visitor question.
The normal single-Agent Session remains the homepage proof, followed by Turn durability
before optional Ways. Why pairs its Session and Git-control claims with visible evidence.
Concepts explains the workbench and runtime mechanisms in depth. Showcase is the complete
grouped directory: the Session loop first, execution choices second, and supporting runtime
proof last. It embeds each of the twelve film slugs exactly once in the portfolio's declared
order. Historical videos, source GIFs, old workbench mocks, and the private brand
reference stay in the repository but do not ship to the public site.

| Film slug | Embedded placement | What the recording must prove |
|---|---|---|
| `session-workbench` | `/`, `/showcase` | One Agent does ordinary repository work inside the Session workbench. |
| `workspace-sessions-turns` | `/concepts`, `/why`, `/showcase` | One Workspace groups multiple Sessions; returning to one restores its accepted Turn. |
| `durable-turn` | `/`, `/concepts`, `/showcase` | An active Turn reconnects after reload and Stop leaves an honest History state. |
| `sandbox-terminal-preview` | `/concepts`, `/showcase` | The Terminal identifies the Session's local Podman sandbox and checkout, runs the repository check, and serves the application opened in Preview through a published port. |
| `multi-agent-handoff` | `/showcase` | Systems Architect → Critical Reviewer run sequentially in one Custom Session, with a dependency edge, ordered active states, and separate durable outputs. |
| `several-ways` | `/showcase` | Independent attempts retain Outcome, Route, diffs, Checks, and optional Judge while the decision is unresolved; Keep selects one result and completed cleanup ends the candidate record. |
| `git-last-turn` | `/why`, `/showcase` | Keep returns an uncommitted result to the primary checkout; Source Control → Last turn filters the current Git diff to paths attributed to that Turn before optional staging. |
| `settings-runtime` | `/concepts`, `/showcase` | Agents, providers, Skills, MCP servers, and Automations remain configuration inside one product. |
| `event-lattice-automation` | `/concepts`, `/showcase` | A Skill publishes `ReleaseCandidateReady`; its matching trigger starts an inspectable Automation run. |
| `mcp-approval` | `/concepts`, `/showcase` | A normal Session Turn pauses for approval before a deterministic local MCP call, then retains bounded tool evidence and the final answer in History. |
| `shared-core-memory` | `/concepts`, `/showcase` | One Agent writes an explicit shared core-memory block; after daemon restart, another configured Agent receives it in a separate Session whose transcript remains distinct. |
| `automation-hitl-recovery` | `/showcase` | A blocking review waits at a top-level Interrupt and resumes with operator guidance from recorded node state. |

Every slug requires both `assets/films/<slug>.mp4` and
`assets/films/<slug>.jpg`. Do not satisfy the build with placeholder bytes: strict
validation probes H.264/yuv420p, 1280×720, 24 fps, no audio, fast-start placement,
duration, and the exact MJPEG poster. It also verifies page placement, distinct beat
frames, capture and staged-sequence hashes, durable evidence, the first-committed
binary declaration, the recorded source digest, and the shipped-media provenance
record. Release-specific compatibility proves the immutable tag/tree, all 55 Git
changes (43 non-recording plus 12 audited provenance rewrites), frozen and restored
153-artifact aggregates, the exact runtime-changed paths, and a verifier-owned
protected surface. It does not authenticate absent capture-binary bytes or claim the
films were captured with `v1.0.1`; ordinary source-bound verification remains strict.

`ax-product-film` pauses playback offscreen and exposes one Play, Pause, or Replay
control. The homepage proof may start muted when it becomes visible; supporting films
stay click-to-play. Reduced-motion preferences disable automatic playback while
preserving explicit playback. `scripts/validate.mjs` checks the film sources and
accessibility attributes alongside page metadata, headings, local links and assets,
image alternatives, current product vocabulary, and selected brand-language rules
before deployment.

Per-film runtime evidence contracts live under
[`../../demo/one-app/scenarios/`](../../demo/one-app/scenarios/). The repeatable editing and
encoding contract lives in
[`../../demo/one-app/films/SHOT-MANIFEST.md`](../../demo/one-app/films/SHOT-MANIFEST.md); a
film is not ready to replace its public pair until that manifest includes its full beat and
poster contract. The encoder produces 1280×720 H.264/yuv420p MP4 files with a 24 fps output
rate and fast-start metadata, plus matching JPEG posters. Source frames must come from the
current browser app and satisfy the scenario's visible and durable evidence gates before
encoding.

Cloudflare Pages deployment is defined in
`../../.github/workflows/marketing-deploy.yml`. The canonical installer is staged
at `/install.sh` during that workflow.
