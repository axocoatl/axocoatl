# Claims ledger

Every public sentence about how well Axocoatl, a model or a setup performs is listed here
with the evidence behind it, the setup it was measured in, and its status. `BRAND.md`
sets the rule: a performance or quality claim needs a measured result, stated with its
setup and limits. This file is how a reader, or a reviewer of a change, checks that.

Covered surfaces: `README.md`, `llms.txt`, `docs/` (product, architecture and
troubleshooting documents), the documentation site (`sites/docs`) and the marketing site
(`sites/marketing`, except the release history on its changelog page). Text the product
itself shows, such as a warning message or a built-in loadout's description, is listed
too.

`sites/docs/scripts/check-content.mjs` enforces part of this on every docs build: each
`measured:` block on a public surface must have its id in this file, a withdrawn claim
must not reappear, a page that cites Claude Opus results must label them as Claude Code
subagents and not Axocoatl, and a page that cites measured review results next to
OpenRouter must say that OpenRouter reviewers were not measured.
`sites/marketing/scripts/validate.mjs` also refuses the withdrawn claim.

## Terms

- **Evidence** paths are in the `axocoatl-growth` repository, under `evidence/`, with
  the pre-registrations under `experiments/`. Each evidence directory holds the raw runs,
  logs and the analysis the numbers come from.
- **Harness** is what ran the agents:
  - *Axocoatl X.Y.Z*: a release build of Axocoatl, native Sessions, tools in rootless
    Podman, host-run checks. Only these are Axocoatl results.
  - *Plain loop*: a minimal agent loop written for the benchmark, calling Ollama
    directly, without Axocoatl.
  - *Claude Code subagents*: Claude Opus (`claude-opus-5-5`) agents started as workflow
    subagents of one Claude Code session, on the host, without Axocoatl. These are never
    Axocoatl results, and every public mention says so.
- **Status**:
  - *measured*: the number is what the evidence shows, for the setup stated;
  - *measured, exploratory*: measured, but the study was not pre-registered and says
    itself that it makes no claim; published only with that label;
  - *sensitivity analysis*: the pre-registered analysis could not decide it, and the
    number comes from an analysis the study names as such;
  - *withdrawn*: removed from public surfaces because nothing measured it;
  - *not measured*: a setup the product supports that has no measurement; public pages
    say so where it matters.

## Measured blocks

Public surfaces state measured numbers between markers (`<!-- measured: id -->` in
Markdown and HTML, `{/* measured: id */}` in MDX) so a block can be found and replaced in
one edit. Each id:

| Block id | Appears on | Claims |
| --- | --- | --- |
| `axocoatl-1.1.0 2026-10-01` | `README.md`, `llms.txt`, docs `understand/what-we-measured`, docs `workbench/fix-qa-audit`, marketing home and concepts pages | R1–R12 |
| `claude-code-subagents single-vs-multi 2026-10-06` | `README.md`, `llms.txt`, docs `understand/what-we-measured`, docs `workbench/fix-qa-audit` | S1–S6 |
| `claude-code-subagents qa 2026-10-04..06` | docs `understand/what-we-measured`, docs `workbench/fix-qa-audit` | Q1–Q6 |

## Review benchmark (R)

Pre-registered bank of 6 Python maintenance tasks, frozen 2026-09-22, scored by 25
withheld tests the agents never see. Writer qwen3-coder 30B on local Ollama, temperature
0, one run per task (n = 6 tasks × 1 run per arm). Evidence directory:
`evidence/benchmark-2026-10-01/` (its `README.md` maps each published sentence to a file).

| Id | Claim as published | Where | Evidence | Harness | Status |
| --- | --- | --- | --- | --- | --- |
| R1 | One Agent, one pass: 17 / 25 withheld tests | README, llms.txt, what-we-measured, fix-qa-audit, marketing home and concepts | `axocoatl-1.1.0/RESULTS-X32k.md`, arm A | Axocoatl 1.1.0 | measured |
| R2 | The same writer with a Required review by a stronger local model, gpt-oss 120B (up to 2 rounds): 19 / 25 at 2.6× the tokens (1,324,504 against 515,217) | same | `axocoatl-1.1.0/RESULTS-X32k.md`, arm X (reviewer `gpt-oss:axocoatl-review`, 32k context) | Axocoatl 1.1.0 | measured |
| R3 | The reviewer reported 15 defects and 14 were real; the writer applied the one false alarm, which cost a test; 6 real findings came in the last round, after which Axocoatl stops revising; one task went from 1/4 to 4/4 | README, llms.txt, what-we-measured, marketing home and concepts | `axocoatl-1.1.0/NOTES-X32k.md` (per-round table and totals) | Axocoatl 1.1.0 | measured |
| R4 | The same Agent reviews its own work: 17 / 25 at 3.5×; final files byte-identical to one pass | README, llms.txt, what-we-measured, marketing home | `plain-loop/RESULTS.md`, arm B | Plain loop | measured |
| R5 | A fresh reviewer on the same model: 17 / 25 at 2.3×; it approved every change, including three with real defects | README, llms.txt, what-we-measured, marketing home and concepts | `plain-loop/RESULTS.md`, arm C | Plain loop | measured |
| R6 | The same model writes its own tests (4 of 6 tasks run): one more test at 9× to 15×; when its tests failed it often rewrote the tests, not the code | README, what-we-measured | `plain-loop/FOLLOWUP.md`, arm D | Plain loop | measured |
| R7 | A stronger reviewer (gpt-oss 120B): 20 / 25 at 3.3×; 9 real defects, 2 false alarms | README, llms.txt, what-we-measured | `plain-loop/FOLLOWUP.md`, arm E | Plain loop | measured |
| R8 | A panel of three same-model critics: 17 / 25 at 4.7×; together with R4 and R5, "another look by the same model added nothing … at 2.3× to 4.7× the tokens" | fix-qa-audit | `walled-tests/panel-P-Q/PANEL.md`, arm P | Plain loop | measured |
| R9 | "Extra tokens helped when they bought a stronger model's judgment, not more looks from the same model." | README, llms.txt, what-we-measured, marketing home | R1–R8 | Axocoatl 1.1.0 and plain loop | measured, for this bank and these two local models only |
| R10 | Axocoatl's token counts are higher than the plain loop's for the same work | what-we-measured | `axocoatl-1.1.0/NOTES-X32k.md`; `README.md` of the directory | Both | measured |
| R11 | Demo video: "A real Axocoatl Session from the benchmark above, trimmed but not edited" | marketing home (optional demo slot) | `axocoatl-1.1.0/NOTES-X32k.md` (invoice_settlement arm X, start and end trimmed); frames in `axocoatl-1.1.0/screens/` | Axocoatl 1.1.0 | measured (provenance) |
| R12 | Product text: the same-model warning, "A same-model second look measured no gain; choose a different reviewer model" (run warning) and "a same-model second look measured no gain" (loadout validation) | `crates/axocoatl-session/src/run_outcome.rs`, `crates/axocoatl-config/src/loadout.rs`; shown by the API, Settings, Team and budget, `axocoatl run` and the record | R4, R5, R8 | Plain loop | measured, for this bank only |

Limits stated on the public pages: one run per task, 6 tasks, one language, two local
models; tokens, not time or money; no estimate of run-to-run variance.

## Default team against one Agent (T)

| Id | Claim as published | Where | Evidence | Setup | n | Status |
| --- | --- | --- | --- | --- | --- | --- |
| T1 | On one small test task (two short modules with documented contracts; a local qwen3-coder 30B-class model; single runs), a single Agent was as accurate as the lead with helpers and used the fewest tokens | docs `understand/coordination`, `understand/what-we-measured`; `BRAND.md` | `evidence/launch-eval-1.1.0-2026-09-30/README.md` (results table and "Honest reading") | `qwen3-coder:30b` on local Ollama, temperature 0; Axocoatl `release/1.1.0` builds | 4 runs, one per arm per build | measured |
| T2 | When the lead asked the reviewer helper to check its change, the reviewer found real defects the tests did not catch, though not every time | docs `understand/coordination` | same | same | same | measured |
| T3 | One Agent is the cheaper choice for small tasks; helpers and review cost extra tokens | README, llms.txt, docs index, `configure/agents`, `understand/coordination`, `understand/what-we-measured`, marketing home | T1; R4–R8 | same | same | measured, for small tasks with a local 30B-class model |
| T4 | A single Agent that reviewed its own work matched the best stigmergic team's score at 28–48% of the tokens | docs `understand/what-we-measured`, `docs/PRODUCT.md` (non-goals) | `evidence/signal-field-live-2026-09-28/README.md`, `solo-review-control/SUMMARY.md` (407,280 tokens against runs 6 and 10) | `qwen3-coder:axocoatl-launch` on local Ollama; Axocoatl development build (`codex/session-coordination-lattice`) | single runs | measured |

## One agent or several, same model (S)

Pre-registered study (`experiments/single-vs-multi/PREREGISTRATION.md`, Amendments 1 and
2). One agent (SOLO) against a planning agent, 2 to 6 parallel workers in isolated copies
and an integrating agent (ORCH), every agent Claude Opus (`claude-opus-5-5`, effort
`xhigh`) in Claude Code 2.1.286 as workflow subagents. Evidence:
`evidence/single-vs-multi/RESULTS.md`. Harness: **Claude Code subagents, not Axocoatl.**

| Id | Claim as published | Where | Evidence | n | Status |
| --- | --- | --- | --- | --- | --- |
| S1 | On work that fits in one context (research, independent modules, a coupled feature change), one agent and several both scored 100% | README, llms.txt, what-we-measured, fix-qa-audit | Tables 2 and 4 | research 5 pairs; parallel-code 2 pairs; coupled-code 2 pairs (protocol analysis; Table 5 with 5 pairs each agrees) | measured (a tie at the ceiling: it cannot show a difference either way) |
| S2 | There, several agents took 1.6× to 4.9× as long | same | Table 4: 1.58× (parallel-code), 3.59× (research), 4.93× (coupled-code) | same | measured; the parallel-code and coupled-code verdicts rest on 2 pairs each |
| S3 | …and used 4× to 5.5× the tokens | same | Table 4, tokens without cache reads: 4.04×, 5.20×, 5.53× | same | measured |
| S4 | On an audit larger than one context (5.2 to 5.7 million characters of source, 150 planted defects over 5 instances), recall was 11 points higher (99% against 87%; interval +4 to +19 points) | same; in words without numbers in `CHANGELOG.md` (1.3.0, `audit`) | "The answer" and "Large-audit" sections: recall 0.987 against 0.873, +11.3 points, 97.06% interval [+4.0, +19.3] | 5 blocks with the excluded attempts put back; 0 valid pairs under the protocol | **sensitivity analysis**: the pre-registered exclusion rule excluded 9 of 10 attempts |
| S5 | …precision 8 points lower, about 3× the tokens and the same wall-clock time | same; "about three times the tokens" in `CHANGELOG.md` (1.3.0, `audit`) and the `audit` built-in's description (P1) | precision 0.853 against 0.937 (−8.4 points [−17.8, +1.4]); tokens without cache reads 3.01× [2.66, 3.68]; wall-clock 0.92× [0.85, 1.03] | same | sensitivity analysis |
| S6 | "Several agents paid off only where one agent could not hold the work, and there they traded precision and tokens for recall." | what-we-measured | S1–S5, Q1–Q5 | as above | measured interpretation, Claude Code subagents only |

## QA studies (Q)

Harness: **Claude Code subagents, not Axocoatl**; model Claude Opus (`claude-opus-5-5`).
Every finding had to come with an executable reproduction that a validator re-ran.

| Id | Claim as published | Where | Evidence | n | Status |
| --- | --- | --- | --- | --- | --- |
| Q1 | On a web app with 16 planted bugs, one explorer found 97.5% of the bugs at 98.8% precision | what-we-measured, fix-qa-audit | `evidence/qa-web-bench-frontier/RESULTS.md`, arm CS | 5 runs | measured, exploratory |
| Q2 | Adding a verifier gave 100% and 100%, but it removed nothing: the reports were already at 100% and 100% before it ran | same | same, arms CV and CVP (CV's reports before the verifier) | 5 runs | measured, exploratory |
| Q3 | A lead with two scouts found 98.8% at 95.7% precision with 2.1× the tokens | same | same, arm CT (10,678,788 against 5,162,563 mean tokens per run) | 5 runs | measured, exploratory |
| Q4 | On a larger app with 66 planted bugs, a lead with three scouts found 2.3 points more than one explorer, not significant (p = 0.77) | same | `evidence/qa-team-scale/RESULTS.md`, block 2 primary test: +0.023, 95% CI [−0.038, 0.094], exact permutation p = 0.7742; pre-registration `experiments/qa-team-scale/PREREGISTRATION.md` | SOLO 10 runs, TEAM 5 runs (11 TEAM attempts set aside, mostly for running on two models) | measured |
| Q5 | …and used 31% more tokens | same | same: 20.14 M against 15.38 M tokens per run | same | measured |
| Q6 | In every setup each finding had to come with an executable reproduction | fix-qa-audit | the two studies' designs (`experiments/qa-web-bench/FRONTIER-ADDENDUM.md`, `experiments/qa-team-scale/PREREGISTRATION.md`) | — | design fact |

## Product text that cites a measurement (P)

| Id | Text | Where | Basis | Status |
| --- | --- | --- | --- | --- |
| P1 | `audit` built-in description: "Measured with Claude Code subagents, not Axocoatl: about three times the tokens of one agent (a sensitivity analysis)" | `crates/axocoatl-config/loadouts/audit.yaml`, shown by `GET /api/loadouts`, Settings and `axocoatl loadouts` | S4, S5 | sensitivity analysis, Claude Code subagents; the description carries the label itself, as do the docs page `workbench/fix-qa-audit` and the file's header comment. |
| P2 | The same-model warning | see R12 | R4, R5, R8 | measured, plain loop |

## Withdrawn claims

| Claim | Where it was | Why withdrawn | Replaced by |
| --- | --- | --- | --- |
| "Small local models are a first-class target." | `README.md` (Any model, per Agent), `docs/PRODUCT.md` (One product); repeated as the `llms.txt` bullet "Small local models:", the docs heading "Reliability for small local models" (`understand/architecture`), "Small local models get specific handling" (marketing home), "Native execution handles small local models specifically" (marketing concepts), "Native activations are built to keep working on small local models" (`docs/ARCHITECTURE.md`) and the advice "short, plain instructions suit small local models" (docs `configure/agents`) | Nothing measured that small local models do well in Axocoatl. The measurements that exist point the other way for hard work: through Axocoatl, a local gpt-oss 120B explorer found 1% to 4% of 16 planted bugs in every QA setup tried (`evidence/qa-web-bench/RESULTS.md`, arms A0–A4, 3 to 6 runs each, pre-registered). | The mechanisms, stated as mechanisms: an early stream end and a transient provider failure are retried once, older tool output is masked, an overflowing request is trimmed, and what still fails is recorded as not covered. |
| A hosted reviewer through OpenRouter presented as part of the measured cross-model result | docs `configure/cross-model-review` (description and introduction), `understand/what-we-measured` ("How this maps to Axocoatl"), docs index card, `README.md` and `llms.txt` measured sections, marketing home and concepts pages | The measured cross-model result (R2, R7) used a local gpt-oss 120B reviewer. No OpenRouter reviewer was measured. | How to configure an OpenRouter reviewer stays documented; every page that cites the measured result says the reviewer was local and that OpenRouter reviewers were not measured. |

## Not measured

These are supported and documented, and have no measurement. Public pages do not make
quality or performance claims about them.

- Runs of the built-in `fix`, `qa` and `audit` loadouts through Axocoatl 1.3.0, and
  any `custom` loadout. The S and Q studies shaped their design; they were not run
  through Axocoatl.
- External agents (Claude Code CLI, Codex CLI) run inside an Axocoatl Session.
- The e2e check (tester-army/e2e) as a required check.
- Any reviewer or Agent through OpenRouter.
- Speed of helpers or of parallel workers through Axocoatl (no speed claims).
- Whether the default team, or any team, beats one Agent through Axocoatl beyond T1.

## Adding a claim

1. Measure it, with the setup written down before the run where possible.
2. Put the evidence in `axocoatl-growth/evidence/<study>/`, with a `README.md` that maps
   each sentence you intend to publish to a file.
3. Add a row here: the exact claim, where it appears, the evidence file, the model and
   harness, n and status.
4. On the public surface, put the numbers between `measured:` markers with an id listed
   above, state the setup and limits next to them, and label anything not run through
   Axocoatl with its harness.
5. Run `npm run check:content` in `sites/docs` and `node sites/marketing/scripts/validate.mjs`.
