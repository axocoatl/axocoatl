# Axocoatl marketing site

Vanilla HTML, Web Components, and CSS. The public site tells one product story:
run coding agents unattended; agents write the code, Axocoatl writes the receipt.
Claude Code, Codex or a model you choose works in a sandbox with its model credential
kept on the host. Fix loadouts require checks and review on the final files; QA and
audit use their own workflows. The daemon signs finished loadout outcomes and, on
download, retained evidence from finished native dashboard turns. A refusal before
admission has no receipt; signing failures remain explicit. Verify receipts separately. Isolation, the record and model
choice are the evidence behind the receipt, not the pitch (`BRAND.md`). A lead writes, read-only helpers answer and review, and opt-in required checks
and review decide when a turn is done, all inside one durable folder-anchored Session. Conversation, context, files,
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
| `/` | The current dashboard first: Install and Open your first Session actions, a real 1.5.3 task from workspace opening to receipt download, review and receipt excerpts, actual stills, receipt limits and evidence links, loadouts, mechanisms and qualified historical measurements. Copyable commands stay on Install and in the guides |
| `/compare` | Mechanisms only: questions to ask of any setup (who runs the checks, who reviews, where the credential lives, network, record, CI, changes, spend) answered for a loadout run, and how that differs from running Claude Code or Codex directly, a devcontainer, or a parallel-agent workbench, without naming or describing other products beyond links to their own documentation |
| `/concepts` | What the host enforces and where it stops (isolation, any model per Agent, a short measured summary), then Workspace, Session, Turn, sandboxed work surface, execution shapes, completion conditions and write scopes, Settings, workspace knowledge, events, compatibility MCP approval, optional Ways, and runtime boundaries |
| `/why` | Why an agent's "done" is not evidence: the measured self-review result, the host deciding, the receipt, why a local binary, and when to use the workbench |
| `/showcase` | Complete directory of the twelve product films, grouped by what each proves (the record, isolation, model choice, the runtime), with an evidence contract for each film. Its source is `noindex` and left out of the navigation, the footer and the sitemap. While the films are pending (`demo/one-app/films/PENDING` names the version) the build leaves the page out, since it would show only notes, and `validate.mjs` refuses a pending build that has it; it returns with the recordings |
| `/install` | Supported installation paths, first run, a headless `axocoatl run`, and Claude Code or Codex as a loadout's writer |
| `/pricing` | License and user-owned infrastructure costs |
| `/integrations/openrouter` | OpenRouter provider setup |
| `/changelog` | Published release history |

## Films pending for a release

When `demo/one-app/films/PENDING` names the CLI version, that version's films are not
recorded and the site ships without them. `scripts/build.mjs` reads the same
declaration as `scripts/verify-film-gate.sh`: it replaces each `<ax-product-film>` with
a static note that names what the film will show (no media, no link), or removes it
when the element has `pending="omit"`, and ships no film media or portfolio.
`scripts/validate.mjs` applies every non-film rule to that build, refuses any film
element or film link in it, skips the film media, provenance and asset rules and says
which it skipped. `--strict-films` is refused while films are pending.
`./scripts/verify-marketing-gate.sh portable|source-bound <out>` builds and validates
that payload; `source-bound` still runs the installer tests and `--release-bound`
(the portfolio and the newest changelog entry name the CLI version). CI, the release
and `marketing-deploy` therefore deploy the pending site. Delete `PENDING` with the
recordings and the full film contract applies again.

## Dashboard walkthrough and historical media

The current `assets/demo/dashboard-flow-1.5.3/` package replaces the older
saved-run tour, task walkthrough and required-review demonstration on the homepage,
README and documentation site. It records a new task operated through the actual
released 1.5.3 dashboard in explicit **Dark** mode: Workspace, Session, Team & budget,
request, work, artifact inspection and signed receipt download. It is a functional
example; it does not extend the historical model-quality measurements.

The package has three clips: `dashboard-flow.mp4`, `review.mp4` and `receipt.mp4`.
Each has its own original JPG poster, captions and chapter VTT. `integration.json`
provides their filenames, dimensions, durations and exact chapter times. Render the
marketing and docs chapter buttons, and README timestamp links, from that manifest.
The shared `components/ax-video-chapters.js` seeks the corresponding video; native
controls remain usable without JavaScript, and there is no autoplay. Keep captions
enabled by default and stills available at full size. Preserve the capture's actual
aspect ratio instead of cropping or forcing a widescreen frame.

`provenance.json` binds the released executable, captured sources, edit timeline,
rendered media, artifact evidence and signed receipt files. `timeline.json` records
which actual dashboard frames appear at which playback times. The source JPEG
archives are split below the hosting service's per-file limit; no source frame is
redrawn. `artifact-evidence.json` preserves the request, baseline, attempted patches,
checks, independent artifact assessment and interventions. Earlier attempts and
receipts remain distinct from the final result. State effective models and outcome
from retained runtime evidence and the actual receipt, not merely the latest
configuration file or a model's answer.

The homepage uses `<!-- optional-demo: … -->` slots independently of the pending
portfolio films. A slot renders and its declared assets are copied only when all
exist; with none it is omitted, and a partial set fails the build. The current full
package is mandatory in the site validator even when portfolio films are pending.
Declare every `provenance.files` entry plus `provenance.json` in the current receipt
slot, so the built payload contains all evidence and the verifier, not only MP4s.

The strict verifier runs for source and built payloads:

```bash
node assets/demo/dashboard-flow-1.5.3/verify-media.mjs \
  --dir assets/demo/dashboard-flow-1.5.3 --repo ../.. --probe
```

It requires `ffmpeg` and `ffprobe`. Keep `assemble-media.py` and `ASSEMBLY.md` with
this package so its documented source-to-playback transformation is reproducible.
Inspect the actual frames for private data before publication. Captions and adjacent
copy must disclose edited waits, failed attempts and operator interventions. Add
outcome claims only after inspecting the retained result and independent artifact
assessment. A failed review must not become an approval through editing; a signed
receipt does not establish that the code is correct.

The older folders remain unchanged as historical source evidence:

- `dashboard-1.5/`: navigation of a completed CLI-launched 1.5.0 loadout run.
- `dashboard-task-1.5/`: a four-turn 1.5.0 native pagination task with recovery and
  no native turn receipt.
- `required-review-1.5/`: a separate 1.5.0 invoice-validation task and review,
  including a defective approved result and correction; no native turn receipt.
- `verified-fix-1.5/`: literal evidence excerpts from the older loadout run.
- `cross-model-review.mp4` and `.jpg`: the historical 1.1.0 benchmark capture,
  attributed in `docs/CLAIMS.md` R11.

Those recordings are not current demonstrations of 1.5.3 and do not establish the
new receipt workflow. Retain their versions, evidence and verifiers. `.gitignore`
keeps MP4 and JPEG files under `assets/demo/` tracked. The separate twelve-film
portfolio contract below is unchanged by this walkthrough.

## Deployment payload

`scripts/build.mjs` copies an explicit allowlist into a clean output directory, including
the repository-root `llms.txt` as the AI-readable product narrative at `/llms.txt`. The
validator keeps that file aligned with the visible workbench story and rejects the retired
runtime-first narrative. The deployment expects twelve product-film MP4/JPEG pairs in
`assets/films/<recording_version>/`. The current unrecorded portfolio targets `v1.5.3`; each
entry has the stable status `required`, which records an obligation, not acceptance.
Acceptance requires the complete capture and evidence contract. Missing required media
fails validation even outside strict mode, and ffprobe is required to verify the media,
unless the films are pending (see above).
Each accepted MP4/JPEG pair needs exact source-frame, capture-record, staged-frame,
durable-evidence, and shipped-media hashes. New versioned captures never rewrite
old media or provenance to claim the old recording used the new binary. Films
may appear on more than one page when the same product evidence answers a different
visitor question.
The normal single-Agent Session remains the homepage proof, followed by Turn durability
before optional Ways. Why pairs its Session and Git-control claims with visible evidence.
Concepts explains the workbench and runtime mechanisms in depth. Showcase is the complete
directory, grouped by what each film proves: the record first, isolation second, model and
approach choices third, and the runtime underneath last. Measured numbers appear once per
page, between `<!-- measured: … -->` and `<!-- /measured -->`. It embeds each of the twelve film slugs exactly once in the portfolio's declared
order. Source GIFs, old workbench mocks, and the private brand reference stay in the
repository but do not ship to the public site.

| Film slug | Embedded placement | What the recording must prove |
|---|---|---|
| `session-workbench` | `/`, `/showcase` | One Agent does ordinary repository work inside the Session workbench. |
| `workspace-sessions-turns` | `/concepts`, `/why`, `/showcase` | One Workspace groups multiple Sessions; returning to one restores its accepted Turn. |
| `durable-turn` | `/`, `/concepts`, `/showcase` | An active Turn reconnects after reload and Stop leaves an honest History state. |
| `sandbox-terminal-preview` | `/concepts`, `/showcase` | The Terminal identifies the Session's local Podman sandbox and checkout, runs the repository check, and serves the application opened in Preview through a published port. |
| `multi-agent-handoff` | `/showcase` | The default Lead delegates to read-only Scout and Reviewer helpers approved in Team and budget, is the only Agent that changes files, and completes after the required check passes; both helper answers remain after reload. This recording does not prove required review. |
| `several-ways` | `/showcase` | Independent attempts retain Outcome, Route, diffs, Checks, and optional Judge while unresolved; Keep selects one uncommitted result. This recording does not prove the newer native retained-decision History flow. |
| `git-last-turn` | `/why`, `/showcase` | Keep returns an uncommitted result to the primary checkout; Source Control → Last turn filters the current Git diff to paths attributed to that Turn before optional staging. |
| `settings-runtime` | `/concepts`, `/showcase` | Agents, providers, Skills, MCP servers, and Automations remain configuration inside one product. |
| `event-lattice-automation` | `/concepts`, `/showcase` | A Skill publishes `ReleaseCandidateReady`; an `on_event` trigger for that event starts an inspectable Automation run. |
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
