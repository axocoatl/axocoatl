# Axocoatl marketing site

Vanilla HTML, Web Components, and CSS. The public site tells one product story:
an open-source, local-first harness for coding agents, where a lead writes, read-only
helpers answer and review, and opt-in required checks and review decide when a turn
is done, all inside one durable folder-anchored Session. Conversation, context, files,
terminal, Preview, tools, history, and Git are the core work surface. Settings owns Agents, Skills, MCP servers, and Automations and
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
| `/` | Harness positioning: lead and helpers, required checks and review, and the Session workbench |
| `/concepts` | Workspace, Session, Turn, sandboxed work surface, execution shapes, completion conditions and write scopes, Settings, workspace knowledge, events, compatibility MCP approval, optional Ways, and runtime boundaries |
| `/why` | Why agent work needs a durable workspace |
| `/showcase` | Complete grouped directory of twelve product-film concepts, with the ordinary Session loop first and an evidence contract for each film |
| `/install` | Supported installation paths and first run |
| `/pricing` | License and user-owned infrastructure costs |
| `/integrations/openrouter` | OpenRouter provider setup |
| `/changelog` | Published release history |

## Deployment payload

`scripts/build.mjs` copies an explicit allowlist into a clean output directory, including
the repository-root `llms.txt` as the AI-readable product narrative at `/llms.txt`. The
validator keeps that file aligned with the visible workbench story and rejects the retired
runtime-first narrative. The deployment expects twelve product-film MP4/JPEG pairs in
`assets/films/<recording_version>/`. The current portfolio targets `v1.1.0`; each
entry has the stable status `required`, which records an obligation, not acceptance.
Acceptance requires the complete capture and evidence contract. Missing required media
fails validation even outside strict mode, and ffprobe is required to verify the media.
Each accepted MP4/JPEG pair needs exact source-frame, capture-record, staged-frame,
durable-evidence, and shipped-media hashes. New versioned captures never rewrite
old media or provenance to claim the old recording used the new binary. Films
may appear on more than one page when the same product evidence answers a different
visitor question.
The normal single-Agent Session remains the homepage proof, followed by Turn durability
before optional Ways. Why pairs its Session and Git-control claims with visible evidence.
Concepts explains the workbench and runtime mechanisms in depth. Showcase is the complete
grouped directory: the Session loop first, execution choices second, and supporting runtime
proof last. It embeds each of the twelve film slugs exactly once in the portfolio's declared
order. Source GIFs, old workbench mocks, and the private brand reference stay in the
repository but do not ship to the public site.

| Film slug | Embedded placement | What the recording must prove |
|---|---|---|
| `session-workbench` | `/`, `/showcase` | One Agent does ordinary repository work inside the Session workbench. |
| `workspace-sessions-turns` | `/concepts`, `/why`, `/showcase` | One Workspace groups multiple Sessions; returning to one restores its accepted Turn. |
| `durable-turn` | `/`, `/concepts`, `/showcase` | An active Turn reconnects after reload and Stop leaves an honest History state. |
| `sandbox-terminal-preview` | `/concepts`, `/showcase` | The Terminal identifies the Session's local Podman sandbox and checkout, runs the repository check, and serves the application opened in Preview through a published port. |
| `multi-agent-handoff` | `/showcase` | Systems Architect → Critical Reviewer run in configured dependency order in one Custom Session; two distinct Agent outputs remain after reload. This recording does not prove the newer inline Coordination card or revision generations. |
| `several-ways` | `/showcase` | Independent attempts retain Outcome, Route, diffs, Checks, and optional Judge while unresolved; Keep selects one uncommitted result. This recording does not prove the newer native retained-decision History flow. |
| `git-last-turn` | `/why`, `/showcase` | Keep returns an uncommitted result to the primary checkout; Source Control → Last turn filters the current Git diff to paths attributed to that Turn before optional staging. |
| `settings-runtime` | `/concepts`, `/showcase` | Agents, providers, Skills, MCP servers, and Automations remain configuration inside one product. |
| `event-lattice-automation` | `/concepts`, `/showcase` | A Skill publishes `ReleaseCandidateReady`; its matching trigger starts an inspectable Automation run. |
| `mcp-approval` | `/concepts`, `/showcase` | A compatibility Session Turn pauses for approval before a deterministic local MCP call, then retains bounded tool evidence and the final answer in History. |
| `workspace-knowledge` | `/concepts`, `/showcase` | A source-linked note and typed backlink survive restart, and a separate native Session retrieves the exact accepted note revision. |
| `automation-hitl-recovery` | `/showcase` | A blocking review waits at a top-level Interrupt and resumes with operator guidance from recorded node state. |

Every slug requires both `assets/films/<recording_version>/<slug>.mp4` and
`assets/films/<recording_version>/<slug>.jpg`. Do not satisfy the build with placeholder bytes: strict
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
