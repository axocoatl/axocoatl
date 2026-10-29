# Axocoatl marketing site

Vanilla HTML, Web Components, and CSS. The public site tells one product story:
run coding agents unattended; agents write the code, Axocoatl writes the receipt.
Claude Code, Codex or a model you choose works in a sandbox with its model credential
kept on the host. Fix loadouts require checks and review on the final files; QA and
audit use their own workflows. The daemon signs finished loadout outcomes, but a
refusal before admission has no receipt and a signing failure is reported without
changing the run verdict. Verify receipts separately. Isolation, the record and model
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
| `/` | The current dashboard first: Install and Open your first Session actions, a real Files screenshot, a short walkthrough and four views of one accepted 1.5 run, receipt limits and evidence links, loadouts, mechanisms and qualified historical measurements. Copyable commands stay on Install and in the guides |
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

## Optional evidence excerpts and video

The homepage uses the existing `<!-- optional-demo: … -->` slots independently of
pending portfolio films. A slot renders and its declared assets are copied only
when all exist; with none it is omitted, and a partial set fails the build.

The `dashboard-1.5` slot contains real captures of the installed Axocoatl 1.5
app reviewing one completed CLI-launched fix run. The hero shows its actual Files
diff. The walkthrough and four stills show the conversation, code change, check and
review, and signed receipt. Captions distinguish this saved loadout run from an
interactive workbench turn recorded on 1.5.0, before native turn receipts. The media
is not a live model execution or a performance demonstration. Native video controls remain available
without JavaScript, and each still opens at full size.

`assets/demo/dashboard-1.5/provenance.json` binds the captures, recording, exact
binary, and frozen accepted artifact. The original public evidence remains linked;
its two recovered reviewer tool errors are retained. The older literal excerpts
under `verified-fix-1.5` remain as source evidence, but no longer form the homepage
walkthrough.

The dashboard assets have their own strict verifier, invoked for both source and
built payloads even while portfolio films are pending:

```bash
node assets/demo/dashboard-1.5/verify-media.mjs --repo ../.. --probe
```

This requires `ffmpeg` and `ffprobe`; the deployment installs them when the dashboard
video ships. It does not mark any of the twelve pending product films accepted or
reuse their historical provenance. Keep original captures immutable, include their
exact provenance, capture metadata and source-frame archive in the optional slot, and inspect the actual
frames for private data before publication.

The separate `dashboard-task-1.5` slot follows a new native workbench task below
that saved-run tour. All nine assets must exist before the build includes it:
`dashboard-task.mp4`, `result.jpg`, `chapters.vtt`, `captions.vtt`, `provenance.json`,
`source-capture.tar.gz`, `timeline.json`, `verify-task.mjs`, and
`artifact-evidence.json`. The source and built payload validators run its full `--repo <repository-root> --probe` verification;
pending portfolio films do not exempt the task recording from these checks.

This four-turn prepared pagination task started with a local Qwen3-Coder 30B lead
and read-only GPT-OSS 20B Scout, then finished with GPT-OSS 20B as the lead and helpers
off. The team required `npm test`, with no required model review or hosted model
calls. The final host check passed all eight tests, all four original tests were
retained, and the artifact passed independent review.

The recording retains malformed tool-call failures, operator recovery, and requests
to restore coverage after file review found two removed original tests. Scout also
reported tests it had not run; test evidence comes from the host and independent
artifact checks. Codex operated the actual browser UI. Captured segments are combined, waiting is sped up 6×
(selected correction waits 8×), and private folder navigation and unrecorded gaps
are omitted; the provenance and source-to-playback timeline document the edits. This is neither a one-prompt success
nor a performance or comparative-quality measurement. Passing the host's checks did
not replace independent artifact review. `artifact-evidence.json` retains the final
accepted assessment alongside the earlier failed assessment, actual patches and
test output.

Keep the four chapter labels aligned with the capture: Open workspace, Send request,
Follow agents & recover, and Review and correct. Explanatory captions identify
stages, recovery and the model change in the silent video; the caption track is
enabled by default and the separate chapter track remains available. Use native
video controls without autoplay and keep the result available at full size. This ordinary workbench turn
must not be presented as producing a signed receipt.

The `required-review-1.5` slot replaces the historical review clip on the homepage.
It requires all nine files before the whole section appears: `required-review.mp4`,
`result.jpg`, `captions.vtt`, `chapters.vtt`, `provenance.json`,
`artifact-evidence.json`, `source-capture.tar.gz`, `timeline.json`, and
`verify-review.mjs`. The complete set must reach the built payload for verification.
The new invoice input-validation task uses a local GPT-OSS 20B Lead, a read-only
local Qwen3-Coder 30B required reviewer, and the host-required `npm test` check.
It is separate from the unchanged Axocoatl 1.1.0 comparison above it: its models and
outcomes must not be attributed to that benchmark.

The first required review approved, but independent artifact checks found that the
Lead had removed a finite-price guard, allowing an oversized price to become
Infinity. A dashboard follow-up requests restoration and another cycle of required
checks and review. A reviewer is evidence, not a guarantee. Preserve the first
approved-but-defective result alongside the follow-up; add a final pass claim only
after the follow-up and its independent acceptance finish.

Capture the actual app with its explicit Dark setting, including the poster.
Name the AI assistant as the UI operator and disclose trimmed or accelerated waits
in the caption and provenance. Keep default captions, native video controls and a
full-size result link. Add outcome claims only after inspecting the completed run
and independent artifact assessment; preserve failed attempts and interventions.
This native Session has no signed run receipt. Add chapter buttons only once their
timestamps match the final recording.

The retired `cross-model-review.mp4` and `cross-model-review.jpg` remain historical
Axocoatl 1.1.0 evidence in the repository, with their original benchmark attribution
in `docs/CLAIMS.md` R11. They are no longer included by the homepage's optional slot.
`.gitignore` keeps MP4 and JPEG files under `assets/demo/` tracked.

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
