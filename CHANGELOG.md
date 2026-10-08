# Changelog

All notable changes to Axocoatl are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [1.3.0] - 2026-10-07

### Added
- **Loadouts.** A loadout is a versioned YAML file (`axocoatl.loadout/1`) that declares
  a whole run: Agents with roles (`writer`, `explorer`, `planner`, `worker`,
  `integrator`) and models or model parameters, required checks with a timeout each, an
  optional required reviewer, an egress allowlist and routes, budgets with a wall clock,
  and the prompt. Unknown fields are refused and each kind's shape is validated. Three
  are built in: `fix`, `qa` and the opt-in `audit`. Your own loadouts are `*.yaml` or
  `*.yml` files in `loadouts/` beside the configuration file the daemon started with, read
  on each listing and each run (at most 128 regular files of at most 64 KiB; the built-in
  ids are reserved; a file that cannot be used is listed with its error). Each has a
  SHA-256 digest, and a run keeps the exact text it ran. `GET /api/loadouts`,
  `GET /api/loadouts/{id}` and `POST /api/loadouts/validate` list, show and validate them,
  as do `axocoatl loadouts list|show|validate`. **Settings → Loadouts** shows each one's
  YAML, parameters, warnings and graph in a read-only lattice, and the command that runs
  it; nothing in the workbench runs a loadout.
- **Loadout Sessions run under network egress with the `hardened` workload.** A Session a
  run creates runs under `network: egress` (or `none`, when the loadout says so), with
  Agents' commands, setup and required checks as the non-root writer user, read-only
  Agents as the non-root helper user, and tool calls and checks under the supervisor's
  `--harden`, whatever `sandbox.network` says. Its egress lists are the daemon's plus the
  loadout's, for that Session only. Global defaults do not change. A loadout cannot ask
  for `bridge` or the `image` workload, and a host that cannot provide egress and the
  `hardened` workload (rootful Podman, E2B) fails the run with exit code 5 instead of
  falling back. Session records carry the binding as `loadout` (absent for every other
  Session), and a run's Session loads again when the daemon restarts, with its
  loadout's network additions.
- **`axocoatl run`.** `axocoatl run <loadout> --task "…"` runs a loadout headless against
  the running daemon, found from `--url`, `AXOCOATL_URL` or the configuration and
  authenticated with `AXOCOATL_TOKEN` or the local API token; it never starts one.
  `--model role=provider:model` and `--param name=value` set parameters; `--check` and
  `--setup` give the check and setup commands. It prints progress to standard error and
  a summary (or, with `--json`, the Outcome as the only output on standard output, with
  Keep's lines on standard error), and exits 0 when the run passes, 1
  when a required check failed, 2 when it needs attention (review not passed, a finding
  unanswered, anything not covered, a check that did not run, a budget or the wall clock
  exhausted, findings the loadout fails on, Keep failed), 3 for usage errors, 4 when the
  daemon is unreachable or refuses the token, 5 for infrastructure errors, 6 when
  interrupted and 7 when another run or Session's turn holds the repository's Workspace
  (busy: run it again later; `POST /api/runs` answers `409` with
  `"code": "workspace_busy"`, and JUnit shows `<error type="busy">`). `--junit` writes JUnit of checks, check reports, review, adjudications,
  findings and coverage, with anything not covered as a failure. `--record` writes the
  run's whole record (manifest, loadout text, Outcome, Session, the team as applied with
  each slot's `reset_history`, tools and definition, turns, the Session's versioned
  History, every network-record event and every run event) as one JSON Lines bundle
  ending with its line count and SHA-256, which `axocoatl record verify` checks. The
  bundle's header carries the time the run finished, so `--record` and every later
  download of a finished run are the same bytes while its record and Session do not
  change. Both are written even when the run fails. A run that was never admitted (the
  daemon refused it or could not be reached, or the flags were refused) still gets its
  `--junit` file, whose verdict is an `<error type="busy">`, `"usage"` or `"error"` with
  `axocoatl.exit_code`; there is no run to record, so `--record` writes nothing and says
  so. Usage reads "cost unknown (reserved
  up to $X)" in the summary, the JUnit `axocoatl.usage` property, the Run outcome panel
  and the pull request body when a call's cost is not known (`usage.cost_known: false`),
  such as a Codex writer's. Ctrl-C stops the run and exits 6. A run needs its
  repository's Workspace to itself: when another Session's turn holds it, the run is
  refused at once, before anything is created, naming that Session and turn (busy;
  retry later), and once a run has what its Outcome needs it stops each of its own turns
  that did not complete (phase `closing_turn`), so the next run is admitted. Loadout runs
  need native Session history: a data root the daemon creates has it, and so does one
  made beforehand, with `mkdir` or `axocoatl secret set`, that no daemon has used and
  that holds no Session; a data root an earlier Axocoatl used needs
  `axocoatl session upgrade --confirm` first, and until then a run exits 5 with a
  message that says so. A `tokens` budget too small for one model call (2,048 plus the
  Agent's `max_output_tokens`, 8,192 when unset) is refused by `axocoatl loadouts
  validate`, and a run whose Ollama model's loaded context does not fit its budget is
  refused before anything is created, naming the budget and the minimum (exit code 3).
  When a required check failed, the JUnit verdict's message names that check first. The API is `POST /api/runs`,
  `GET /api/runs`, `GET /api/runs/{run_id}`, `/events`, `/stop`, `/junit` and `/record`;
  runs are kept under `loadout-runs/` in the data root and outlive their Session.
- **Built-in `fix` loadout.** One writer, the repository's check command as a required
  check, and a required review by `reviewer_model` for up to 3 rounds. The reviewer
  numbers its findings, and when the host sends them back the writer must answer every
  one in an `ADJUDICATIONS` block with accept or reject and a reason. Each answer is
  recorded and shown in the run output, JUnit, the Run outcome panel, the pull request
  body and the record; a finding left unanswered makes the run need attention. When the
  reviewer runs the writer's model, Axocoatl warns (`same_model_reviewer`) in loadout
  validation, the API, Settings, Team and budget, the run output and the record. Like
  the qa and audit blocks, `ADJUDICATIONS` is read after a heading in any case and with
  or without Markdown marks, fenced or not, or as an answer that is only the JSON; a
  block that is not valid JSON is reported, never guessed at.
- **Built-in `qa` loadout.** One browser explorer with `browser` and `browser_check`,
  writing only under `axocoatl-qa/`, which the run creates before the explorer's turn
  when the repository has none; a run whose `axocoatl-qa` is a symbolic link or not a
  directory is refused (exit 3). No scouts, merge, reviewer or verifier. Each finding
  names a Playwright reproduction that the host re-runs on the build under test and, when
  `reference_url` is given, on a clean reference build: confirmed only when it fails on
  the first and passes on the second, "fails on clean build" when it fails on both,
  reproduced without a reference. Every re-run is a `browser_check` call in the Session's
  network record. Areas the explorer did not reach or was blocked on, and
  everything left when it stopped on a provider refusal or classifier stop, a provider
  failure or its budget, are listed as not covered, and the run needs attention.
- **Built-in `audit` loadout (opt-in).** Runs only when named. A planner splits the
  scope into 2 to 8 areas in a structured block, one read-only worker per area runs in
  parallel with a fresh context, and an integrator merges their findings; a failed area
  is not covered. An invalid plan gets one retry that quotes the error, and a second
  leaves the whole scope not covered; when integration has no result, the workers'
  findings are reported unmerged and integration is listed as not covered. A worker's
  `FINDINGS` and `NOT_REACHED` keys are read in any case. A not-reached entry that names
  another planned area is left to that area's worker, and one that names a repository
  path that does not exist is a `note` on standard error and in the record; neither is
  a gap. The attention line counts areas, not entries. Its documentation states the trade-off measured with Claude Code
  subagents, not through Axocoatl: more recall at lower precision and about three times
  the tokens on an audit larger than one context, as a sensitivity analysis.
- **External agents.** A loadout's writer can be the Claude Code CLI
  (`runtime: claude-code`) or the Codex CLI (`runtime: codex`), run inside the Session
  container as the non-root writer user under `--harden`, admitted, granted and captured
  like a native writer. Its model traffic goes through a route whose credential the
  daemon adds on the host, so the container holds only a placeholder; the Session's
  certificate authority is trusted through `NODE_EXTRA_CA_CERTS` and `SSL_CERT_FILE`.
  Every model call is in the network record, and the program's JSON output is parsed into
  the activation's evidence and answer. Spend is bounded by an up-front reservation, the
  route's request count and the program's own usage report, not per call. Each route
  allows only the program's model calls, since every allowed request gets the
  credential; Claude Code's requests for its account's policy limits and remote
  settings are refused and recorded, and it runs without them. A refused request's 403
  hint names the route Axocoatl added for the writer, and one on a loadout's own route
  names the loadout's route, never a `sandbox.egress.routes` entry. When the model API
  answers a program's call through its route with `401`, the not-covered reason says
  first that the stored credential (such as `claude-code-oauth`) was rejected and to
  store it again with `axocoatl secret set <name>`, piping in only the token.
- **`axocoatl secret set|list|remove`.** Stores a route credential, such as the output
  of `claude setup-token`, read from standard input only (a pipe or a file; a terminal,
  where the value would show, is refused), as an owner-only file under `secrets/` in the
  data root. It is never printed, logged or recorded. A loadout route's
  `credential` resolves to a `credentials` entry first, then to a stored secret.
  `secret set` warns on standard error, and still stores the value, when it has
  whitespace inside it, starts with `Bearer `, looks like JSON or holds several tokens,
  or, for the secret Claude Code's route sends, does not start with `sk-ant-oat01-`; the
  warning never shows the value.
- **`axocoatl recipe build|list`.** Builds a Session image from Axocoatl's pinned recipes
  (`claude-code`, `codex`, `e2e`, combinable) with Podman and trusts it by its recorded
  image id.
- **The e2e check.** A loadout check can run tester-army/e2e (`e2e@0.18.0`, Apache-2.0)
  inside the Session container from the `e2e` recipe (Node 24.21.0 and Playwright's
  Chromium headless shell). The check forces `E2E_TELEMETRY_DISABLED=1`, mounts
  `.e2e/cache` read-only, sends e2e's model calls through a route with the key added on
  the host, and parses its JSON report, bound to the run by a digest marker, into the
  check's result in the Outcome. An OpenRouter model that OpenRouter's catalog does not
  list with tool calls and image input is refused. Under
  `network: egress` an e2e check's process gets its own egress credential, as a writer's
  shell does, so it reaches the loadout's routes and allowlist with every connection
  recorded; every other required check still has no network there.
- **Keep as PR.** `axocoatl run --keep branch|pr`, **Keep as PR** in the Run outcome
  panel and `POST /api/sessions/{id}/keep-pr` commit a passing run's changed paths to a
  new branch with host `git` through a temporary index, leaving HEAD, the index, the
  current branch and the working tree unchanged, and refuse paths that were dirty before
  the run or that changed after it ended. Opt-in, they push the branch without `--force`, never to the remote's default
  branch, and open a pull request with `gh` whose body lists the check results, the review
  verdict and adjudications, findings, everything not covered, warnings and the run id.
  Keep also refuses paths that a turn outside the run changed, and runs no hook, filter,
  fsmonitor, signing program or credential helper the repository configures.
  `axocoatl service install` now records the directories of `git` and `gh` on the
  service's `PATH` after Podman's.
- **Per-check timeouts.** A Team and budget edit's `check_options`, aligned with
  `required_checks`, gives each required check a name, a timeout from 1 second to 30
  minutes (3 minutes by default, as before), a report (which a loadout run reads for its
  e2e checks) and, with `egress: true`, an egress credential for the check's process
  under `network: egress`. Teams applied before this release keep working unchanged.
- **`sandbox.egress.host_ollama`.** An opt-in route from Session containers to an Ollama
  server on a loopback port of this computer, as `https://ollama.host.axocoatl.internal`
  under `network: egress` (set as `OLLAMA_HOST`), with every request and response in the
  network record. Off by default; when `sandbox.network` is `bridge` or `none`, only
  loadout Sessions, which always run under egress, reach it, and `axocoatl validate`
  warns. `axocoatl network reload` turns it on, moves it or turns it
  off live, and names under `axocoatl.internal` are refused otherwise.
- `GET /api/sessions/{id}/team` returns `warnings`, such as the same-model reviewer
  warning, and the Team and budget review shows them.
- `docs/CLAIMS.md`, a ledger of every public performance or quality claim with its
  evidence, model, harness, sample size and status, and the claims withdrawn.
- **Reasoning models in native OpenRouter Sessions.** A model whose OpenRouter catalog
  entry describes reasoning, such as `anthropic/claude-sonnet-5.5` (reasoning mandatory)
  or `openai/gpt-5.6-sol` (reasoning on by default), now runs in a native Session
  instead of being refused. Every call sends the documented `reasoning` setting: the
  new per-Agent `sampling.reasoning_effort` (`max`, `xhigh`, `high`, `medium`, `low`,
  `minimal` or `none`), or the model's own default effort when it is unset. An effort the
  model does not list, `none` for a model whose reasoning is mandatory, and an effort for
  a non-reasoning model are refused by name when Team & budget is applied. Each call's
  `max_tokens` is its output limit plus a reasoning allowance sized from OpenRouter's
  share for the effort (four times the output at `high`, once at `medium`, a quarter at
  `low`, at least OpenRouter's 1,024-token minimum), so reasoning does not starve the
  answer. Reasoning tokens are output in usage, grants and cost. The reasoning blocks of
  a tool-calling response are kept on its first tool call and sent back unmodified with
  the tool results, as OpenRouter documents, and reasoning text streams to the Session.

### Changed
- **Transient provider errors are retried once.** A native provider call that fails with
  429, a 5xx status, a timeout or a reset connection before the provider sent anything is
  retried once, after its
  `Retry-After` (at most 30 seconds) or 2 seconds, on the same pinned model and endpoint,
  as a new call with its own reservation; the failed call keeps its accounting. 400, 401,
  402 and 403, refusals and safety stops are not retried, and a completion the provider
  refused is now reported as a refusal. Each retry is recorded on the activation's
  evidence and, in a run, as a `provider_retry` event.
- **Required reviews number their findings and ask for an answer to each.** Every
  required review, in the workbench too, asks the reviewer to number its findings `F1`,
  `F2`, … and, when the host sends them back, asks the lead to answer each one in an
  `ADJUDICATIONS` block with accept or reject and a reason. A `fix` loadout run records
  those answers; elsewhere they are part of the lead's reply.
- **What fails is listed as not covered.** A helper, slot or area that ends without a
  result keeps a failure class (`provider_refusal`, `provider_failure`,
  `provider_rejected`, `budget`, `blocked`, `not_reached`, `runtime_limit`, `stopped`,
  `other`), and a run's Outcome lists it as not covered with the reason. The lead still
  receives a failed helper's error as before.
- **Public claims withdrawn or labeled.** "Small local models are a first-class target"
  is withdrawn from the README, the product document, `llms.txt` and the docs: nothing
  measured it; the mechanisms it named are still described. Pages that cite the measured
  cross-model review now say that its reviewer was a local gpt-oss 120B and that
  reviewers through OpenRouter have not been measured. Results measured with Claude Opus
  are labeled everywhere as Claude Code subagents, not Axocoatl.
- **Session data is converted to segment files when a Session is first opened.** Each
  journal's single file is checked against the bounds it was written under, its records
  are copied into segments, and a small head file replaces it last, so a crash during
  the conversion leaves the old file to convert again. Once converted, a Session cannot
  be opened by 1.2.0 or earlier, which refuses the new head file rather than misread it.
  Before a Session's first conversion, its whole directory is copied, once, to
  `backups/before-segments/<key>/session` in the data root, with a `backup.json` naming
  the Session written last; `<key>` is the SHA-256 of the Session id. With the daemon
  stopped, copying that directory back over `execution-v2/<key>` restores the files
  1.2.0 wrote (see Upgrade in the docs). If the copy cannot be made, the Session is not
  converted and does not open. The backups stay until you remove them.
- `sandbox.egress.record_max_events` is ignored: a Session's network record keeps every
  event. It is still accepted, and `axocoatl validate`, `axocoatl doctor` and daemon start
  warn that it can be removed. `GET /api/sessions/{id}/network` returns `record` as
  `{events, bytes, gaps}`, without `max_events` and `full`, and a `record_unavailable`
  refusal now means only that the record could not be written.
- The Ways decision history's `records` and `aggregate_bytes` limits count the decisions
  kept and their stored bytes; deleted decisions no longer count toward them, so deleting
  one frees room. One stored change (patch) may be up to 64 MiB, and the total is bounded
  only by `aggregate_bytes`.

### Fixed
- **Native OpenRouter reserves each call's own request, not the context window.** A
  call reserved the endpoint's whole context window plus its output, about 1.06 million
  tokens and $2.08 on a 1M-context model, so a modest grant could never make one. A call
  now reserves the byte length of the exact request body plus 4,096 tokens for the
  provider's chat template, plus the reasoning tokens of earlier tool turns it sends
  back, capped at the endpoint's prompt limit; plus its output limit and reasoning
  allowance. It still settles to OpenRouter's reported usage and cost, and usage beyond
  the reservation is refused with its spend kept (OpenRouter documents that `max_tokens`
  caps reasoning on most providers, not all). Team & budget checks that the smallest
  call fits the approved tokens and money; without `sampling.max_tokens`, the response
  allowance takes at most half of the whole-call capacity. An output limit and effort
  whose sum passes the endpoint's output limit, or whose streamed text could pass the
  1 MiB a Session keeps of one response, is refused by name rather than cut; reasoning
  text streams in merged deltas. An Agent's `token_budget` with `abort` reserves the
  whole response, reasoning included, and the replayed reasoning in its input estimate.
  Because a call reserves more as its prompt grows, an Agent asks for its final answer
  while it can pay for this call and a next one grown by this call's response, and a
  lead starts a helper only when it can still pay for the call that reads the answer.
- **Native OpenRouter prices every component it can be billed for.** An endpoint was
  refused when it priced anything beyond prompt, completion and cache reads, which
  excluded every current frontier model (cache writes and `web_search` are priced on
  both models above, and `openai/gpt-5.6-sol` has tiered prices). The cost ceiling now
  uses the highest input rate of the prompt, cache read and cache write prices and the
  highest output rate of the completion and internal reasoning prices, at the highest
  tier, plus any per-request fee; `max_price` carries the same ceilings. Web search,
  image and audio prices are retained as features the request cannot start: it sends
  text and function tools only, no server tools, no `modalities` or
  `web_search_options`, refuses `:online` model ids, and turns the web plugin off
  explicitly, which overrides an account default. Context compression is turned off the
  same way. `max_price` leaves out image and audio, which OpenRouter filters on even
  for text. A response that reports a server tool, web citations, or image or audio
  output is refused. An endpoint that charges for anything else is refused and the
  error names the component. A provider tag such as `openai` qualifies when no other
  variant shares it; service-tier endpoints (`openai/flex`, `openai/fast`) are never
  selected and never shadow a provider tag, `:nitro` and `:floor` ids are refused, and
  a response at another tier is refused. The cheapest qualifying endpoint that accepts
  the Agent's sampling settings is selected first; a setting no endpoint accepts is
  named. Profiles retained by 1.2.0 keep working unchanged.
- **`browser` and `browser_check` accept `null` for an optional argument and explain
  every refusal.** A model that gave an optional argument as `null`, such as
  `"snapshot": null`, had the whole call refused, and a refusal said only which rule
  was broken, so a model could retry until it found a call that ran but showed it
  nothing (`"snapshot": "none"`). An optional argument given as `null`, at the top
  level or in a step, its target or the viewport, now counts as not given, so
  `"snapshot": null` returns the default `aria` snapshot. Each refusal names the
  argument, shows what was received, says what is accepted and ends with one valid
  call, on the call's own URL when it has one. A step written as code, such as
  `{"code": "await page.content();"}`, is refused with the actions a step can take
  and the advice to call with just the `url` and read the snapshot. The tool
  descriptions now give the usual sequence (read the snapshot, then act on what it
  showed) and say that steps are actions, not code. Every top-level property of the
  schema has a type and a description that states its default and accepted values, for
  chat templates that show a model nothing else of the schema.
- `web_search`'s `max_results`, `web_fetch`'s `max_chars` and `start_paragraph`, the
  `path` of `list_dir` and `grep`, the `rows` and `cols` of `spawn_terminal`, the
  `tail_lines` of `read_terminal` and the `delimiter` of `text_split` given as `null`
  now take their defaults instead of refusing the call.
- **A native Agent is no longer stopped at 128 tool rounds while its budget has room.**
  A lead could stop at exactly 128 rounds with tokens, spending and time left. An
  activation may now run one tool round per invocation its grant allows, up to 1,024,
  so the grant normally ends a long run before the round limit does. An Agent's new
  `max_tool_rounds` (1 to 1,024) sets a lower limit for its activations. An activation
  that reaches its limit fails with "This Agent reached its tool-round limit for this
  activation (N rounds)", classed `round_limit` with **Continue** as the suggested next
  step, where it was classed `other` with "inspect"; failures written by 1.2.0 are read
  the same way.
- **A long Session no longer runs out of room to record its work.** A Session recorded
  at most 256 tool calls and repository captures over its life, and the call after that
  left its runtime needing recovery ("invocation audit capacity exhausted"). Its other
  journals had lifetime caps too: 4,096 content records or 64 MiB, 65,536 turn records
  or 256 turns, 4,096 Team revisions, saved Agent state for 4,096 activations, and
  50,000 network events or 2,000 screenshots, past which new connections were refused. A
  Session now keeps every record for as long as it exists. Each journal appends to an
  open segment file and, once it holds its bounded number of records or bytes, seals it
  with a SHA-256 digest that the next segment names, so a reordered, removed or changed
  segment is refused; a write cut short by a crash is dropped on open and an interrupted
  seal is completed. Memory holds the open segment and a small index per sealed one, not
  the whole history. Bounds that apply to one turn or one record stay: a turn records up
  to 4,096 steps and 8 MiB and up to 4,096 budget claims, about 2,000 tool calls. When a
  turn has no room for another tool call, each Agent is asked for its final answer
  without tools, as when its budget runs out, an Agent's call it cannot record is
  declined before anything is written, and a capture it cannot record is kept as
  unavailable; the next turn starts with fresh room.
- **Continuing a Team turn runs the work that depends on the restarted Agents.** When a
  lead depended on two helpers that failed on a provider error, Continue restarted the
  helpers but left the lead, which had never started, blocked, so the turn stopped
  again with nothing left to continue. Work that never started and depends on restarted
  work now waits for it in the new epoch and runs once it is accepted; work that also
  depends on failed work left unselected stays blocked until that is continued too.
- **Closing or creating a Session no longer waits for another Session's turn.** Closing
  (or deleting) an idle Session while another Session's turn held their Workspace waited
  for the Workspace and failed after 60 seconds with a `500` that blamed the Session
  being closed, and creating a Session on that Workspace (`POST /api/sessions`,
  `POST /api/workspaces/{id}/sessions`) answered only when the turn ended, which for a
  turn that needs attention could be never. Both are now refused at once with `409` and
  `"code": "workspace_busy"`, naming the Session, run and turn that hold the Workspace,
  and nothing changes; try again once that turn ends. Attachment, environment, file and
  Git changes, Reopen and the Ways reads, checks, judge and Keep are refused the same way
  (`409`) instead of waiting. An operation that is not a turn, such as another Session's
  creation, is waited for at most 10 seconds, then the request is refused, naming it.
  When a lifecycle action does time out, its message says what it waited for. A turn
  that cannot start because the Workspace is held is busy too, and a loadout run then
  exits 7, not 5.
- **Close removes a Session's runtime volumes.** Closing a Session left its
  `axo-egr-`, `axo-egi-`, `axo-svc-` and `axo-ca-` Podman volumes behind, hundreds after
  many runs. Close now removes them: their sockets and trust files are filled again
  when the Session starts. The Node dependency volume still stays until the Session is
  deleted, so Reopen reuses the installed dependencies.
- **`GET /api/sessions/{id}/export` exports native Sessions.** Without
  `history_version` it answered `400` for a Session whose History holds native
  execution, such as every loadout run's. It now exports such a Session in the
  versioned form, as JSON or Markdown, like the record bundle and the other History
  reads; `GET /api/session-turns/search` with a `session_id` does the same. An unknown
  Session is a `404`, not a `400`.

### Security
- The docs site's build dependencies are updated: `http-cache-semantics` 4.3.0 fixes
  GHSA-ch52-4w7c-c8xp, so its reviewed exception is removed; `sharp` 0.35.5, with
  libvips 1.3.4, fixes GHSA-wq5f-xc86-pv6w; `source-map-js` 1.2.2 fixes
  GHSA-68fv-2mgg-jv7q; and `smol-toml` 1.9.0 fixes GHSA-r4xh-jqrq-34v2. None of them is
  part of the shipped binary.

## [1.2.0] - 2026-10-03

### Added
- `sandbox.network: egress`. A Session container has no network interface other than
  loopback, and an Agent's commands reach only the hosts and ports listed under
  `sandbox.egress`, through an egress proxy that runs in its own container
  (`axo-egr-<session>`, from a local image built from the bundled supervisor). The
  list takes presets (`npm`, `yarn`, `pypi`, `crates`, `go`, `github`, `alpine`,
  `debian`, `ubuntu`), host names, `*.` subdomain wildcards and IP ranges, plus
  `private_destinations`, `sidecar_network`, `max_connections` and
  `record_max_events`. Each `bash` command of a writing Agent, each approved setup
  command, readiness provisioning and each terminal gets its own proxy credential
  through an environment file; read-only helpers, required checks and background
  tasks get none. Names are checked against the list before they resolve, and
  resolve on the host. Loopback, link-local and other special addresses are refused,
  and so are the Podman host gateways (`192.168.127.1` and `192.168.127.254` in a
  Podman machine, `10.88.0.1`, and the gateway of `sidecar_network`) even inside a
  listed range; other private addresses are refused unless listed. A credential that
  ends, or a host that is revoked, while a connection is being decided admits
  nothing. Every allowed connection, and every refusal of a connection with a valid
  credential, is written to the Session's network record before it opens; refusals of
  connections without a valid credential are recorded up to 20 at once and then one
  every 5 seconds, and the rest are counted in a `limit` event, so a process in the
  container cannot fill the record. When the
  record is full, setup commands, provisioning and terminals still run, without
  network. The Session container's first process is Axocoatl's bridge, which serves
  only the proxy, so the image `ENTRYPOINT` does not run. The Session container holds
  no port socket: a service forwarder (`axo-svc-<session>`) that joins only its
  network namespace serves each exposed port as a socket, and a separate
  `axo-pvw-<session>` container publishes those sockets on host loopback for Preview;
  neither connects out. A read-only helper, whose shell may not open TCP connections,
  cannot reach the Session's apps over TCP or through the port sockets; an app that
  listens on a Unix socket of its own, or on UDP, is still reachable. Ways attempts use their Session's
  proxy and decision point, with credentials of their own that the network record
  names with the attempt (`binding.attempt_id`). `validate`, `doctor` and daemon start warn about wildcard entries,
  CDN-fronted presets, hosts that accept uploads and ranges that contain a Podman host
  gateway. Under `bridge` and `none` they warn that `allow`, `private_destinations`
  and `routes` do nothing; the browser's own proxy still uses `sidecar_network` and
  `max_connections`, and `record_max_events` caps the network record in every mode.
  `doctor` says what an Agent's commands reach: the `allow` list, the route hosts and
  hosts allowed for one Session, while readiness provisioning reaches the Alpine,
  Debian and Ubuntu mirrors. When Axocoatl's config file is inside a Session's Workspace, **Session
  network** and `GET /api/sessions/{id}/network` warn that Agents can read it. See the
  Sandboxes and Security pages for what it does not cover.
- `sandbox.workload` (`mode: auto | hardened | image`, `writer_user`, `helper_user`).
  `auto`, the default, is `hardened` under `network: egress` and `image` (the image's
  own user, as before) under `bridge` and `none`. A hardened Session container keeps
  root only for its first process and readiness provisioning: Agents' commands, setup
  commands, terminals, required checks and Axocoatl's own commands run as
  `writer_user` (default `1000:1000`, home `/home/axocoatl`), and every process of a
  read-only helper as `helper_user` (default `1001:1001`), both with no Linux
  capabilities and no way to gain one. A helper cannot read the environment of a
  writer's processes, so it cannot borrow a writer's egress credential, and cannot
  signal them. The Workspace is mapped to the writer with `--userns=keep-id`, the
  egress proxy's socket sits in a directory only root can enter, and a Node project's
  dependency volume is handed to the writer. It needs rootless Podman: under rootful
  Podman `auto` falls back to `image` with a `doctor` warning and `hardened` refuses to
  start a Session. `doctor` prints the users, and E2B refuses `hardened`.
- Each native Session can keep a network record, an append-only log of egress
  decisions, connection closes, policy changes and web-tool calls, and
  `GET /api/sessions/{id}/network` reads it. Reading never creates a record.
- The bundled execution supervisor gains an egress proxy mode, a loopback/Unix-socket
  bridge mode and a socket probe, which `network: egress` uses.
- `mcp_servers[].inherit_env: false` starts a stdio MCP server with only `PATH`,
  `HOME`, `USER`, `LANG`, `LC_*` and `TMPDIR` from the daemon's environment, plus its
  own `env`, so it does not see provider API keys. The default, `true`, keeps today's
  behaviour.
- Under `network: egress`, a person can allow one exact host for one Session, and
  revoke it, with `POST /api/sessions/{id}/network/allow` and `…/revoke`. The change
  is recorded in the Session's network record and applies to new connections at once.
  The **Session network** panel, opened from an Agent's details in the Session graph,
  shows the policy, allowed and refused counts and refused connections, with
  **Allow for this Session** on hosts the list refused. An activation's details show
  its tool calls' network activity as `network` evidence.
- Native `web_search` and `web_fetch` for Agents whose `tools` list them.
  `web_search.provider: searxng` searches through a SearXNG container that Axocoatl
  runs with local Podman (pinned image, loopback-only port, no capabilities, started
  on the first search and removed when the daemon stops), or through your own
  instance with `managed: false` and `url`. A `web_fetch` block enables
  `web_fetch`, which reads one public page as numbered paragraphs. It refuses
  private, loopback, link-local and other special addresses, this computer's own
  interface addresses (such as its global IPv6 address) and addresses on a network
  it is directly connected to (an IPv4 prefix of /16 or longer, an IPv6 prefix of /48
  or longer, never a point-to-point link), including names that resolve to them and every
  redirect hop (at most five), reads only HTML, text, Markdown, JSON and XML, and
  caps the body at `max_bytes`. Both run on the host, not in the Session container.
  Each result and page has a `source_id` for citations (`[S1a2b3c4d]`,
  `[S1a2b3c4d ¶3]`). Before a call sends anything it appends a `web_request` event to
  the Session's network record with the URL it fetches or its query's hash, and when
  it finishes a `web` event with the tool call, activation, Agent, source ids and
  content hashes; a search records its query's hash, not its text. If either cannot
  be written the call fails. The turn's control plane adds `sources` evidence per
  activation, marking which sources its final answer cites, and Team & budget marks
  Agents that list a web tool with a **web** badge. Native Sessions refuse the web
  tools under `sandbox.network: none`; under `egress` they run as under `bridge`,
  outside the `sandbox.egress` list. In attempts made with Explore several ways an
  Agent that lists them runs without them. See Configure > Web research.
- Legacy (1.0-format) Sessions get `web_fetch` for an Agent whose `tools` list names
  it, never under `sandbox.network: none`, and `web_search` through SearXNG when
  `provider: searxng`. Legacy calls are not recorded.
- **Browser tools for native Sessions.** With a `browser` block in the configuration,
  Agents whose `tools` list them get `browser` and `browser_check`. `browser` opens a
  URL in a fresh headless Chromium, runs up to 40 steps (click, fill, select, check,
  press, wait, expect text, reload, back; no script step), and returns the page's
  accessibility snapshot, console errors, dialogs and failed or refused requests as
  text, with the Playwright line for each step. `browser_check` runs one Playwright test
  file from the repository (with the files it imports by relative path) or given as
  `script`, with one worker and no retries, and returns each test's status and first
  error. Both run in a per-Session browser container with no network interface other
  than loopback, a read-only root, no capabilities and no Workspace mount. It reaches
  the Session's exposed ports through Unix sockets served by a separate forwarder
  container that joins the Session container's network namespace (under `egress`, the
  one that serves Preview); the Session container never sees the sockets, so a
  read-only helper's shell cannot reach the apps through them. It reaches the hosts listed
  under `browser.allow` only through Axocoatl's egress proxy, which checks each host
  against that list, resolves it on the host, refuses special and unlisted private
  addresses and records every decision. Without declared
  hosts Chromium gets no proxy at all. Each call gets its own proxy
  credential, passed on the driver's standard input and revoked when the call ends.
  `browser_check` runs alone and the browser container is replaced after it, so
  nothing its test leaves reaches another call. Read-only helpers and required
  reviewers get `browser` when their own template lists it; `browser_check` counts as
  a tool that can change files. The tools are not offered in Ways attempts.
- Screenshots never reach the model. Each `browser` call, and each failing
  `browser_check`, keeps a screenshot beside the Session's network record (at most
  64 MiB per Session), and a new `browser` event records the call, its tool call,
  activation and Agent, URLs, status and screenshot digest. A call that fails is
  recorded too, with the reason. `GET /api/sessions/{id}/network/screenshots/{sha256}`
  returns a screenshot, and `GET /api/sessions/{id}/network` shows the browser's
  egress sidecar and policy once declared hosts are used.
- `axocoatl browser install` builds the browser image,
  `localhost/axocoatl-browser:pw1.60.0`, from a Containerfile and lock files embedded in
  Axocoatl (Node 22 by digest, Playwright 1.60.0, Playwright's headless Chromium).
  `axocoatl doctor` reports whether it is present and what the browser can reach.
- Native Session admission accepts `browser` and `browser_check` in an Agent's `tools`
  and refuses them, with the reason, when no `browser` block is configured or the
  backend is E2B, and refuses `browser_check` for an Agent with `writes: []`.
- Under `network: egress`, the browser reaches the hosts listed under `browser.allow`
  through the Session's own egress proxy, under a `browser` policy kept apart from the
  Session's list: a browser credential is checked only against `browser.allow`, and an
  Agent's only against `sandbox.egress`. Each of those connections is written to the
  Session's network record with the browser call that made it, and a refused one can
  be allowed for that Session in the `browser` scope. The proxy tells the two apart
  by credential, not by container: while a `browser_check` call runs, its test holds
  the browser credential and can pass it to a process in the Session container, which
  then reaches hosts listed only under `browser.allow` (recorded under the browser
  call), and a test given an Agent's credential can use it from the browser container.
- `axocoatl network reload` and `POST /api/network/reload` read the daemon's
  configuration file again, validate it, and apply `sandbox.egress.allow`,
  `sandbox.egress.private_destinations`, `sandbox.egress.routes`, `credentials`,
  `browser.allow` and `browser.private_destinations` to new and running Sessions:
  each running Session records its new policy (`source: config_reload`), new
  connections use it at once, and open connections that no rule allows any more, or
  whose route changed, are closed. The report lists every entry each list gains and
  loses. A running Session whose new policy cannot
  be recorded keeps its old one and is listed under `failed`; the command then exits
  with status 1, the route answers `503`, and the next reload, even of the same file,
  tries that Session again. Other changed settings are listed as needing a restart
  and are not applied; an invalid file changes nothing, and neither does a file
  inside a Session's Workspace, which its Agents can edit.
- `request_network_access`: under `network: egress` a writer Agent that lists it can
  ask for one exact host it was refused, with a reason. The request is recorded as a
  `proposal` and waits in **Session network**, where you approve or reject it
  (`POST /api/sessions/{id}/network/proposals/{proposal_id}/approve` and `/reject`);
  approval is the per-Session allow, recorded with the proposal's id. The tool call
  waits up to `wait_secs` (default 120, at most 600) and returns the decision, or
  `pending`. Nothing approves a request by itself, and no Agent can approve one.
- In a hardened Session container (`sandbox.workload`, the default under `egress`),
  every process of an Agent's tool call in a native Session, a read-only helper's
  included, and every required check runs under a seccomp filter from the execution
  supervisor, on top of Podman's profile: `ptrace`, cross-process memory access,
  `userfaultfd`, `perf_event_open`, `bpf`, kernel keyrings, module loading, mounts,
  `setns`, new user namespaces and packet, vsock, Bluetooth and key sockets fail with
  `EPERM`, and `clone3` and `io_uring` with `ENOSYS`, so the C library and libuv fall
  back to older calls. Each such command also runs in a Landlock domain of its own, so
  it cannot trace a process it did not start or read its memory or environment.
  Terminals you open, setup commands, background tasks and Ways checks run without the
  filter. It needs Landlock (Linux 5.13 or later).
- In a hardened egress Session the container reaches the proxy only through the
  proxy's identity socket (in a volume of its own, `axo-egi-<session>`), and the
  container's first process starts each connection with the identity of the program
  that opened it, read from `/proc`; it keeps `CAP_SYS_PTRACE` for this. A network
  record's `open` event carries it as `peer` (`pid`, `uid`, `gid`, `exe`,
  `exe_sha256`, `ancestors`, `error`). **Session network** lists the latest
  connections, with a **Program** column when the record names programs, and an
  activation's network summary names the programs behind each destination. The record
  names programs; allow entries and routes cannot yet be limited to some. The egress
  control protocol is version 2.
- `sandbox.egress.routes` and `credentials`, under `network: egress`. A route names
  one host whose HTTPS traffic the daemon ends itself: the egress proxy answers a
  `CONNECT` to the route's host and port with a relay that carries the client's TLS
  bytes to the daemon, which ends TLS with a certificate from a certificate authority
  made for each Session (ECDSA P-256, key kept in memory, 30 days; a 24-hour
  certificate per host) and connects to the upstream itself, only to the addresses
  it resolved and checked for the connection. On a route's ports the route decides,
  whatever `allow` lists: a plain-HTTP request gets `403 tls_required` and a process
  kind the route does not serve `403 route_not_for_binding`. Each request must come
  from a process kind the route
  serves, carry the route host as its TLS server name and `Host`, have a canonical
  path (no dot segments, including `..;` path-parameter forms, and no escaped or
  twice-escaped separators), ask for no upgrade, carry no method, URL or host override
  header (`X-HTTP-Method-Override`, `X-Original-URL`, `Forwarded`, `X-Forwarded-*` and
  the like, also with `_` in place of `-`) and match the route's rules (methods, path globs with `*` and `**`,
  required query parameters, which a raw `;` in the query never satisfies) or an
  `access` preset; anything else gets a JSON refusal naming the missing rule. A route's
  credential comes from an environment variable of the daemon or an owner-only file
  outside every Workspace, is read when a request needs it, and is added as
  `Authorization: Basic` or a named header after the client's own credentials are
  removed; it never enters a container, the egress proxy, the record or the logs.
  Credentialed routes refuse compressed responses unless `allow_encoded_responses` is
  set, stop a response whose status line, headers, body or trailers carry the
  credential before that part reaches the client, and remove `Set-Cookie` and
  `Set-Cookie2` unless `allow_set_cookie` is set, counting them in the `response`
  event (`cookies_dropped`). A token the host issues in a response body, such as from
  a login or `/token` endpoint the rules allow, is not the credential and reaches the
  container. Every route request is written to
  the Session's network record before it is sent (`request`), and how it ended after
  (`response`); a relayed connection's `close` says why the daemon ended it (for
  example `sni_mismatch`). Activation evidence and **Session network** list them.
  A Session that has routes when its runtime starts gets the authority's certificate
  in a read-only volume (`axo-ca-<session>`) at `/etc/axocoatl/ca`, and the
  credentials of the process kinds a route serves set `SSL_CERT_FILE`,
  `CURL_CA_BUNDLE`, `REQUESTS_CA_BUNDLE`, `PIP_CERT`, `GIT_SSL_CAINFO`,
  `CARGO_HTTP_CAINFO`, `NODE_EXTRA_CA_CERTS` and `DENO_CERT` to it, plus each route's
  `env_placeholders`. Validation refuses `${...}` and plain values in `credentials`
  and routes without repeating a value written in `credentials.<name>.env`, `file`,
  `inject.format` or `inject.basic.username`, warns about credentialed routes that
  allow every path (including login and token endpoints), set `allow_set_cookie` or
  `allow_encoded_responses`, and about stdio
  MCP servers that inherit `env` credentials, and `axocoatl doctor` reports routes
  and whether each credential's variable or file is there. See Configure >
  Credentials and routes.

### Changed
- `web_search.provider` must be `searxng` or the legacy `tavily`; any other non-empty value is
  a configuration error, and `tavily` draws a warning because only legacy Sessions
  use it. Native Sessions refuse `tavily`.
- `sandbox.backend: e2b` now requires `sandbox.network: bridge` when the configuration
  loads. Before, `network: none` with E2B was accepted and refused only when a
  remote Session was prepared.

### Fixed
- A store reopened while the daemon started a terminal or another process could fail
  with "Resource temporarily unavailable" (os error 35 on macOS, 11 on Linux), because
  the starting process briefly shared the store's lock; it now waits up to 250 ms for
  the lock.
- **`axocoatl onboard` and `axocoatl doctor` check the Ollama server the way native
  Sessions do.** Onboarding wrote a configuration for an Ollama server whose cloud
  features were on, and doctor reported it OK, although native Sessions refuse such a
  server. Onboarding now asks for the Ollama server URL and checks that it is a
  loopback address, runs Ollama 0.20.6 and reports its cloud features disabled in
  `GET /api/status`. A URL that is not a loopback address is refused and asked for
  again. When another check fails it says which one and how to fix it: add
  `{"disable_ollama_cloud": true}` to `~/.ollama/server.json`, or start the server
  with `OLLAMA_NO_CLOUD=1`, then restart Ollama. It offers to check again, to use a
  different server URL, or to continue. Doctor reports each of these requirements as
  a required check, so a server with cloud features on now fails doctor, and
  `onboard --install-daemon` does not install the service for it.
- **Onboarding no longer defaults Ollama to `llama3.2`.** It lists the models installed
  on the chosen server and suggests a coding model for Lead and Scout, preferring a
  Qwen3-Coder tag, and a different, larger model for Reviewer when one is installed,
  preferring gpt-oss. Both are chosen from a list that preselects the suggestion, or
  typed in. Cloud and embedding models are never suggested. With no local model
  installed it suggests `qwen3-coder:30b`.
- **The Always-On Service finds Podman and keeps a log.** `axocoatl service install`
  wrote a service definition with no environment, so a launchd service could not find
  a Homebrew Podman in `/opt/homebrew/bin`, and launchd discarded its output. Install
  now records a `PATH` with the directory of the `podman` found at install time plus
  the standard system directories, and `CONTAINER_CONNECTION` and `CONTAINER_HOST` when
  they are set; it prints what it recorded and warns when it finds no `podman`. The
  systemd user unit records the same variables and keeps logging to the journal. On
  macOS, stdout and stderr go to `~/Library/Logs/Axocoatl/daemon.log`, created
  owner-only. The definition never contains provider keys, and a `CONTAINER_HOST`
  that contains a password is not recorded. Reinstall and restart an existing service
  to pick this up.

### Compatibility
- A data root used with this version may be refused by 1.1.2 and earlier once a
  Session has a network record, because they do not know the record's directory.
- Removing a Session's runtime now also removes its `axo-egr-`, `axo-brw-`, `axo-pvw-`
  and `axo-svc-` containers, and deleting it also removes its `axo-egr-`, `axo-egi-`,
  `axo-svc-` and `axo-ca-` volumes. Removing a Session container also removes containers that joined its
  network namespace (`podman rm --depend`).

### Security
- The docs site's build dependency `http-cache-semantics`, used by `astro`, has a
  high-severity advisory (GHSA-ch52-4w7c-c8xp) with no fixed version published. The
  flaw needs a shared cache serving several users; `astro` uses the package only to
  compute cache lifetimes of remote images during a build, and the docs site has
  none. The docs dependency audit now accepts reviewed exceptions from
  `sites/docs/audit-exceptions.json`, and this one lapses on 2026-11-01, as soon as a
  fixed version is published, or when another package starts to depend on it. It is
  not part of the shipped binary.

## [1.1.2] - 2026-10-02

### Security
- **The local API now requires a per-daemon token.** Before, when Axocoatl listened on
  loopback with no `server.auth` credentials, any local process, and code in a Session
  container that could reach the host's loopback address, could use the HTTP API and
  WebSockets without a credential. On first start the daemon now writes a random token
  to `local-api-token` in the data directory (mode `0600`) and reuses it on every
  restart. `axocoatl dev` and `axocoatl serve` print a sign-in link when run in a
  terminal, and the new `axocoatl url` command prints it again. Opening the link sets an
  HttpOnly, SameSite=Strict cookie for that browser. Scripts send
  `Authorization: Bearer <token>` or `x-api-key: <token>`. Health probes, the sign-in
  page and static assets stay public. Configured `api_keys` and `bearer_tokens` work as
  before, and `allow_unauthenticated: true` keeps the API open for an authenticating
  proxy. On loopback it keeps the Host check against DNS rebinding, so a proxy on the
  same host must send `Host: localhost`. To rotate the token, stop Axocoatl, delete the
  file and start it again. After upgrading, open the link from `axocoatl url` and reload
  any workbench tab that was already open. `axocoatl service install`, `start` and `status` point to
  `axocoatl url`, because a service's own sign-in hint goes to its log.
- A loopback listener now also holds its port on the other loopback address family
  (`127.0.0.1` and `[::1]`), and does not start if another process already listens
  there. Browsers try `[::1]` first for `localhost`, so such a process could otherwise
  receive the sign-in link and the workbench's requests.
- An empty configured credential, such as an unset `${ENV}` in `server.auth`, no longer
  matches an empty `x-api-key` header. Credentials are compared in constant time.
- Request logs show a `token=` query value as `token=REDACTED`, and the Preview proxies
  never forward the sign-in cookie to Session apps.

## [1.1.1] - 2026-10-01

### Added
- **No TCP for read-only shells.** The `bash` commands of a read-only helper or required
  reviewer (`writes: []`) can no longer open a TCP connection or bind a TCP port, on any
  address including loopback, as well as being unable to write, create, rename or
  delete repository files. The
  kernel refuses both through Landlock network rules, whatever the sandbox's `network`
  setting. Landlock does not cover UDP, so name lookups and other UDP traffic still
  follow `network`, nor a socket that listens on a random port without binding first.
  This needs Landlock ABI 4 (Linux 6.7 or later). On an older
  kernel, including Linux 6.2 to 6.6 where 1.1.0 restricted only writes, a read-only
  Agent's `bash` commands do not run and its read-only file tools still work. Writers'
  shells and Axocoatl's own repository captures are unchanged. Both embedded execution
  supervisors are rebuilt from this source and re-pinned.

### Changed
- The README, docs and marketing site describe Axocoatl as the harness to run coding
  agents on your own machine or infrastructure, with isolation built in, a complete
  record of every step and any model, and publish what was measured on a pre-registered
  benchmark run through Axocoatl 1.1.0: a Required review by a stronger local model
  raised a local writer from 17 to 19 of 25 withheld tests at 2.6 times the tokens, while
  the same model looking again gained nothing. A new docs page explains how to set up
  cross-model review, and the home page shows a recording of one of those reviews.

### Fixed
- The release workflow looked up its draft with GitHub's release-by-tag endpoint, which
  never returns drafts, so publishing always failed; it now finds the one draft for the
  tag in the release list.
- The marketing site builds and deploys while a release's films are pending, with each
  film placement replaced by a short note.

### Security
- The docs site's build dependency `devalue` is updated to 5.9.4 for high-severity
  advisories (GHSA-j22f-vq7h-c4qm and others). It is not part of the shipped binary.

## [1.1.0] - 2026-09-30

Axocoatl's founding thesis was stigmergy: Agents coordinating through signals left
on shared work, with no Agent in charge. It was built during 1.1.0 development and
measured, and a single Agent that reviewed its own work matched the best team's score
at 28–48% of the tokens. 1.1.0 therefore moves to a lead that writes and read-only
helpers it delegates to, opt-in required checks and review that the host runs on the
exact final tree, per-Agent write scopes, and durable grants and budgets, with every
activation, tool call and budget decision recorded in the Session. The default team is
offered, not proven better than one Agent, and a single Agent remains the cheapest
choice for small tasks. Removed with the thesis: pheromone activation, the signal
field, the standing-work inbox, the legacy coordinated-turn path (multi-Agent turns on
a 1.0 data root now require `axocoatl session upgrade --confirm`), the Coordinator's
auction and HTN planner, the model-facing `coordination_control` tool, the external
harness adapter and the `axocoatl-coordination` crate.

### Security
- Update rustls to 0.23.45 for RUSTSEC-2026-0285, which fixes TLS 1.3 handshake
  messages being accepted across encryption-level boundaries.
- Rebuild the vendored Monaco editor with DOMPurify 3.4.16 for GHSA-p98j-92pf-mc4p
  (Axocoatl never uses the affected `IN_PLACE` mode), update the vendored
  markdown-it to 14.3.2 for GHSA-253c-mchw-3w2r, and update the docs site's undici
  to 8.11.2.

### Added
- **Workspace knowledge.** Native Sessions can share versioned Markdown decisions,
  conventions, and findings within a Workspace. Notes retain source hashes, typed
  relationships, backlinks, provenance, and revision conflicts. Agent proposals stay
  separate until accepted publication; isolated Ways additionally require their
  exact retained Keep selection. Rejected or superseded work does not publish
  automatically. The Session's Knowledge inspector supports editing, proposal review,
  source/evidence navigation, a knowledge graph, explicit Markdown import/export, and exact-revision
  attachment to chat.
- **Source-aware code context.** A bounded, rebuildable source index parses Rust,
  JavaScript, TypeScript/TSX, and Python definitions and import syntax. The observed
  code map supports targeted reads, and source references expose changed or unavailable
  evidence. This syntax index does not claim complete reference or call-graph resolution.
- **Per-Agent write scopes.** An Agent's `writes` list, or its **May change** choice in
  Team & budget, names the repository paths it may change in a native Session; `[]`
  makes a read-only helper. `write_file` and `edit_file` refuse other paths before any
  effect. A read-only Agent is not offered them, and its own `bash` commands run under a
  kernel write restriction that keeps the repository unchanged. A change outside a
  writer's paths made any other way, found by comparing its Before and After captures,
  fails that activation and is kept for review. A helper cannot be given a wider scope
  than its lead.
- **Knowledge-directed investigation.** Investigate in chat prepares a source-checking
  request with the selected note revision. A stored note does not itself authorize
  execution or establish readiness.
- **Explicit Session storage upgrade.** `axocoatl session upgrade --confirm`
  converts a stopped legacy data root while retaining its writer fence, history,
  checkpoint archives, and cumulative usage. Unrecorded historical roles produce
  an empty future conversation instead of guessed or resumed private state.
  Interrupted conversions resume from the exact recorded source.
- **Recorded Agent execution inspector.** The Session's existing Agent graph now
  opens in View mode. Select an Agent and activation to inspect its retained input,
  output, partial output, usage, and causal evidence. Ordinary and directly targeted
  turns remain inspectable alongside native team turns; missing or unknown evidence
  is labeled explicitly. Historical evidence does not itself authorize controls;
  native actions require the host's exact current capability or explicit revalidation.
  Saved evidence references reopen their original activation after reload. Unstarted
  descendants show Blocked when their current dependency cannot complete, while
  later Retry generations preserve the earlier failure evidence.
- **Native Session execution and controls.** New data roots use one canonical turn
  controller for ordinary Send, dependent Agents, and the helpers a lead delegates to.
  Team & budget reviews immutable definitions and explicit limits for future turns, and
  refuses limits that cannot pay for what an edit names with `422` and the reason.
  The Session inspector exposes exact generation controls and reviewed Add/Replace
  edits. Guide and Revise retain the human instruction, selected context, and
  attachments. Existing legacy roots retain their compatibility path.
- **Lead `delegate` tool.** In a native Session, an Agent whose Team & budget approval
  names helper templates gets a `delegate` tool. It hands one self-contained task to a
  read-only helper, which starts in its own empty conversation, and waits for the answer.
  Answers over 8192 bytes are cut for the lead; the full answer stays in Session History.
  Each helper is an Add Agent command from the lead, an optional node in the turn graph,
  and a child grant whose limits are reserved from the lead's budget. Several `delegate`
  calls in one model response run their helpers at the same time; each is admitted
  against the graph and reservations the ones before it left. A helper is not
  started when its limits would leave the lead too little to read the answer. A failed or
  refused helper reaches the lead as a tool error. The same helper and task in one turn
  return the earlier result; a call whose helper was never started can be made again. A
  return lost to a restart is read back without running the helper again, including when
  the restart came before the helper was admitted. Older helper answers stay whole in
  later requests until the context runs short. A helper must be read-only: its template
  has no tool that writes files or runs commands, or it has `writes: []`, which withholds
  `write_file` and `edit_file` and runs its `bash` where it cannot write, create, rename
  or delete repository files.
  Any other helper is refused, and the refusal says to set `writes: []` on it.
  A Coordinator template in a native Session team runs as such a lead over its approved
  Worker templates. Legacy Sessions keep the Coordinator's own decomposition. The Agent
  graph draws a "delegated" edge from a lead to each helper, animated while the helper
  runs, and the control-plane read reports it as a `delegated_by` edge.
- **Reviewed partial finish.** Native cooperative turns can be finished partially
  with explicit human confirmation of selected accepted results, work to stop,
  never-started work, and missing checks. Safe settlement and usage evidence remain
  required; normal Finish still requires the declared work and conditions.
- **Embedded process supervision.** The single Axocoatl executable includes its Linux
  process supervisor for x86_64 and aarch64. Local startup prepares the matching payload
  automatically. Native repository tools and checks retain exact process-settlement
  evidence through cancellation; unknown effects remain unresolved. Provider and tool
  calls share cumulative grant reservations and the Stop gate. The native provider
  boundary currently accepts reviewed local Ollama profiles with finite inference
  bounds, retaining incomplete token observations separately from known-zero API cost.
- **Native OpenRouter credit billing.** Eligible static text/tool models use a retained
  endpoint with output and price ceilings, no automatic retry or fallback, and durable
  whole-call budget reservations. Onboarding declares credit billing and supplies an
  explicit output limit. Missing final usage remains unknown. BYOK support is deferred;
  the supported account configuration has no connected provider keys.
- **Required checks for Session teams.** Team & budget can name commands the host runs
  in the Session's repository after the required Agents of every native turn finish,
  each between two repository captures of the accepted candidate and charged to the
  first required Agent whose own tools include `bash`. That Agent keeps enough of its
  budget to run the checks twice, once and after one Continue; Apply refuses a smaller
  invocation limit, and a lead that pays cannot hand that allowance to a helper. A turn
  completes only when every check passes and the repository is unchanged; a failure
  leaves it needing attention. When the checks cannot be paid for, the turn says why and
  Continue does not offer to rerun them. The turn's controls show each check's command,
  state, exit code and output, and above them whether the checks passed together on the
  current tree and, if not, why. Continue on any check, or on restarted work, runs every
  check again between fresh captures; an Agent other than the read-only required
  reviewer that finishes after the checks captured the repository makes them not ready.
  A check whose record cannot be read shows as unavailable instead of hiding the turn.
  Team & budget shows each check's arguments as a shell reads them and keeps a check's
  exact argument list unless you change its line. Required checks do not run on
  Explore several ways attempts, and Team & budget and Explore several ways say so.
- **Required review for Session teams.** Team & budget can name a read-only Worker
  template as the team's required reviewer, with 1 to 3 rounds and its own budget.
  The host, not the lead, runs it after the required Agents finish and the required
  checks pass, in a fresh conversation shown the request, each required Agent's final
  answer and the turn's change against the tree it began with, bounded. The turn
  completes only when it answers `VERDICT: APPROVE` about that exact result; the verdict
  is bound to the answers and tree it judged. A verdict line anywhere in the answer
  counts; an answer with none, or with verdict lines that disagree, is asked again once
  about the same result while a round remains, and otherwise fails closed.
  `VERDICT: CHANGES` sends the findings to the lead as a revision in a new epoch, and
  the checks and review run again, until the rounds run out and the turn needs
  attention with the findings. Apply refuses a reviewer that could change files, a
  budget that cannot pay for every round, and a lead that cannot run once per round.
  The turn controls show the verdict, findings and round.
- **Retained Ways decisions.** Native Ways retain bounded candidate Outcomes, Routes,
  diffs, Checks, usage, Judge evidence, the human choice, and cleanup state after Keep
  or finishing without keeping. History storage limits are explicit; capacity failure
  preserves recovery evidence. History supports search, export, context attachment,
  and a separate explicit deletion action. Keep remains an uncommitted Git decision.
- **Default team.** The configurations `axocoatl onboard` and `axocoatl init` write, and
  the repository's example configurations, define Lead, an autonomous Agent that edits
  files, and Scout and Reviewer, Workers with `writes: []` that Lead can delegate to;
  onboarding also keeps the plain Assistant for `axocoatl chat`. `init` now writes a local
  Ollama configuration instead of an OpenAI one. A Worker no longer has to belong to a
  coordinator-led workflow: outside every workflow it is a helper template. A new Session
  starts with one Agent. When its team is that one Agent and it may delegate, Team &
  budget offers every Worker template with `writes: []` as its helpers, not pre-selected,
  and says they cost extra tokens; selecting **Let this Agent delegate to helpers** drafts
  them, and every limit is still entered and applied by the person. Required review stays
  opt-in. **Add Agent** adds the template chosen beside it. A configuration without such
  Workers behaves as before.

### Changed
- **`workspace_knowledge` follows the `tools` allowlist.** A native Agent is offered the
  tool, and the host admits a new call, only when its `tools` list includes
  `workspace_knowledge`; calls already recorded still load. The default team does not list
  it; the one-app demo's `coder` does. Its description no longer suggests that an Agent may not change code, and it
  tells the Agent not to report its work or final answer through it.
- **An Agent that lists no tools is reported.** In a native Session an Agent's `tools`
  list is exact, so an Agent that lists none cannot read or change files; only a legacy
  (1.0-format) Session still gives it the baseline tools. `axocoatl validate`,
  `axocoatl doctor` and daemon startup warn about each Agent that is not a Worker and
  lists no tools, and the Team & budget review says such an Agent can only answer from
  the conversation. The one-app demo's coding Agents and the E2B example list their tools.
- **Default team prompts and `delegate` guide the lead to use its helpers.** The `delegate`
  description tells the lead to ask a helper to find the relevant code and tests before a
  change and to review the change against the task, documented contracts and tests before
  it finishes. Helper limits are stated in steps, where a step is one model call or one
  tool call, instead of "tool calls". The default Lead prompt says to look first, make the
  change, run the check command from `AXOCOATL.md` and check the diff against the task,
  and asks scout and reviewer only if it has helpers; the Reviewer prompt and the required
  review ask for every documented contract and edge case to be checked one by one.
- **Multi-Agent turns on a 1.0 data root require the Session upgrade.** On a data root
  that still uses the 1.0 format, a Session turn that would run two or more Agents is
  refused before it starts, with a message to stop Axocoatl, make a cold backup, and run
  `axocoatl session upgrade --confirm`. Single-Agent turns and a request targeted at one
  Agent keep working.
- **Session History reads answer native Sessions in the versioned form.**
  `GET /api/sessions/{id}/turns`, `/turns/{turn_id}` and `/messages` without
  `history_version` returned `400` for a Session with native turns. They now answer
  such a Session as `history_version=2`, whose entries each carry their
  `history_version`; every other Session keeps the exact legacy shape. The HTTP
  reference documents both versions with an example, and the WebSocket reference now
  documents `session-needs-attention`, `activation-stream` (which native Sessions send
  instead of `token`, `reasoning` and `tool-call`) and `activation-control-changed`.

### Fixed
- **A call to a tool that does not exist no longer throws away the Agent's work.** Small
  local models sometimes finish with their answer as text next to a call to a tool
  nobody declared (a `report` or `answer` call). The whole activation used to fail with
  "provider returned a tool call that was not declared in the request", so a lead was
  told its reviewer "did not finish" and shipped the bug the reviewer had found. Now
  the call never runs and records nothing; the model gets a tool error ("`report` is
  not an available tool. Available tools: … If you are done, answer without calling a
  tool.") and continues, and the next response completes the activation. It counts as
  a tool round for the round limit and the loop guard. When the host has withheld tools
  to ask for the final answer (the loop guard or the budget wrap-up), a tool call in the
  response starts no further round: its text is the answer and the call is dropped, and
  a response with no text ends the activation with a plain failure saying the model was
  asked for its final answer and called a tool instead. Malformed calls (bad arguments, an empty name, or a name no
  provider accepts) still fail as before.
- **A response refused after the model finished keeps its usage.** When a provider
  completed a response and reported its usage but the response was refused (a
  malformed tool call), that usage was lost: the activation's usage became unknown and
  the call kept its whole reservation (36,864 tokens with a 32k local model). The
  native Ollama provider now reads the response to its end before refusing it, and the
  reported usage is recorded and settles the call. A helper activation that fails is
  now logged at WARN with its reason.
- **A message sent while a turn needs attention asks where it should go.** Send used
  to be disabled, and a request through the API failed with only "resolve the current
  unfinished turn". Now Send offers two explicit choices: continue the turn with the
  message, as a Revise of an Agent with an accepted answer, or finish the turn as it is
  and send the message as a new request, which selects every accepted final answer so
  it carries into the next turn's conversation. The choice says plainly what is lost:
  an Agent that failed or was interrupted has no accepted answer, so its work in that
  turn does not carry forward, and continuing with a message is unavailable when no
  Agent has one. The API refusal now names both options and their endpoints.
- **Token and cost limits now count what model calls used, not what they reserved.**
  Each call reserves its whole context plus output (36,864 tokens with a 32k local
  model) before it is sent, and that reservation used to stay charged, so a 1,457,714
  token limit allowed only 39 calls. When the provider reports a call's complete usage
  (and a known cost), only that stays charged and the rest is available again; an
  interrupted call, an incomplete measurement or a call lost in a crash keeps its whole
  reservation. A helper's limits are reserved from its lead while it runs; when it
  finishes or is stopped the lead is charged only what it used and can delegate again,
  and running it again reserves its limits again. Turns recorded before this keep their
  exact totals.
- **Native Agents get the repository's `AXOCOATL.md`.** Native activations never received
  project instructions, so an Agent ran `npm test` where the file said `npm run check`.
  A lead, its helpers and the required reviewer are now given the `AXOCOATL.md` at the
  root of their checkout (up to 64 KiB) in the system prompt, as the compatibility path
  does. When the activation's starting capture lists the file, only the exact bytes it
  recorded are used.
- **`list_dir` with an empty path lists the repository root** instead of failing with
  `ls: cannot access ''`.
- **`glob` matches path patterns.** Since 1.0 it ran `find . -name PATTERN`, which
  compares only a file's name, so any pattern with a `/` (`**/*.test.js`, `lib/*.js`,
  `**/manifest*.js`) silently found no files. Patterns now follow the same rules as
  write scopes: `*` and `?` stay within one path segment, `**` spans directories, a
  pattern without `/` matches a file name at any depth, and one with `/` is matched
  from the repository root (so `./*.js` names only the root's). Results are sorted, relative to the root (no leading
  `./`), and skip `.git`, `node_modules`, `target` and similar directories unless the
  pattern names them; when nothing matches, the result says so and how patterns are
  read. Patterns are limited to 1 KiB, and path matching (write scopes included) no
  longer takes exponential time on patterns with many `*`.
- **A long tool loop on a small-context model no longer forgets its own work every
  round.** A request was fitted against 85% of the context window on top of its full
  answer allowance, so a 32,768-token model started removing tool rounds with more than
  4,000 tokens still free; it then kept only the last two rounds, and because every
  request is rebuilt from the whole history, each later request removed them again: the
  Agent re-read the same files until its budget ran out. A request now fits when it and
  its answer allowance fit the window less a 1/32 margin, counted the way the provider
  counts once a call shows its tokenizer counts more than the local one. When rounds
  must go, the oldest go first, enough to leave room to grow, and they stay out for the
  rest of the activation, so later requests keep fitting with the same prefix until new
  work overflows it; the note that says so stays. Stale tool output is masked only as
  far as the context needs (keeping at most 32,768 tokens whole), so a small model keeps
  what it just read.
- **An Agent near the end of its budget answers instead of failing, and a spent budget
  says so plainly.** When what is left of the Agent's own token guard (once it has spent
  some) or of its Session budget (tokens, model and tool calls or spending, less what the
  host holds back for its checks) cannot pay for another tool round and an answer, the
  next request goes without tools and asks for the final answer, which is recorded.
  When a limit does stop the Agent, the failure names it ("This Agent reached its token
  limit for this activation (600,000 tokens; 619,018 needed)…", "The Session budget for
  this Agent is used up: 1,000 of its 1,457,714 tokens remain and the next model call
  needs 36,864.") instead of `Token budget exceeded: used …` or `LLM provider error:
  Invalid request for ollama: provider admission failed: … authority budget or storage
  capacity exhausted`.
- **An Agent remembers its earlier turns however much work they took.** Once an
  Agent's conversation grew past 85% of its model's context window, the next turn kept
  only the last 15 earlier messages and then skipped ahead to a person's message; a turn
  with many tool rounds has none there, so everything before the new request was dropped,
  with no summary. In the 1.1.0 live eval a lead's 33,510-token first turn on a
  32,768-token model was cut to 5,860 tokens and it began turn 2 with no record of turn
  1's request or answer, while a smaller first turn was carried whole. This dates from
  1.0. Earlier turns now shrink only as far as needed, least useful first: long tool
  output and arguments are elided, then tool rounds are left out (every request and
  final answer stays, with a note of how many rounds were left out; files changed are
  in the repository), and only then do the oldest turns go, never the most recent
  completed one. Leaving tool rounds out to fit a request also keeps earlier turns'
  answers.
- **An Agent stuck restating its answer is asked for it.** In the 1.1.0 eval a solo
  Agent that had finished repeated its answer through 18 rounds of `bash`
  `echo "✅ …"`, about 540,000 tokens with no change, until the budget wrap-up stopped
  it. When the last three tool rounds only printed text (`bash` commands made of
  `echo`, `printf`, `true`, `:`, `cd` or `pwd`, with no redirection, pipe or
  substitution), or the last four repeated the same calls with the same results, the
  next request now goes without tools and asks for the final answer, as it does at the
  end of a budget. Editing and running the same test again, reading different files and
  polling a terminal never trigger it.
- **`GET /api/sessions/{id}/turns/{turn_id}/grants` returns each grant's usage**, as
  the HTTP reference already said. Each grant now has a `usage` object with the
  `activations`, `invocations`, `tokens` and `cost_microunits` it has been charged: a
  running model call's reservation, then what the call reported once it settled. The
  other fields are unchanged.
- **A misspelled `sandbox.network` no longer leaves the network on, and Podman no
  longer copies host proxy variables into containers.** Any `sandbox.network` other
  than exactly `bridge` or `none` (for example `None`, `off` or `disabled`) was
  treated as `bridge`; `axocoatl validate`, `axocoatl doctor` and daemon start now
  refuse it with an error naming the two accepted values, and `doctor` prints the
  configured network. Every `podman run` now passes `--http-proxy=false`, so the
  host's `HTTP_PROXY`, `HTTPS_PROXY`, `FTP_PROXY` and `NO_PROXY` values, which can
  include a proxy user name and password, no longer reach commands in the container.
- **Paused turns no longer deadlock on a cancelled re-preparation.** Opening Files or
  Terminal after a restart re-prepares a Ready local environment; if that request was
  dropped (for example by navigating away) or the daemon shut down, the environment
  became Failed while a paused turn pinned its generation, and Continue, Finish, Stop
  and rebuild were all refused. A cancelled or shutdown-interrupted local
  re-preparation now removes its container and dependency volume and returns to Ready
  at the same generation. Rebuilding the unchanged plan of a failed local environment
  while a paused turn waits prepares it again at that generation. A crash
  mid-preparation and every E2B preparation still end Failed as before.
- **Native activations keep their budget for the host's checks and their context bounded.**
  An activation keeps enough invocations for the host to observe its changes and
  run required checks, and always leaves room for the model to answer after a tool
  round is declined; a declined call is a tool error, not a failed activation.
  Tool output, and long arguments of the model's own earlier calls, older than the
  latest three to five tool rounds of an activation are replaced by a short
  placeholder in later requests (the Session history keeps them), so repeated file
  contents stop growing every request; a request that would still overflow a small
  local context keeps only the latest round whole, and then leaves out the earliest
  tool rounds with a note, instead of failing.
- **An Ollama stream that ends without its final chunk is retried once.** This includes
  a response Ollama ends with an error record, such as a tool call it could not parse.
  The retry
  is charged to the same grant, preflighted against what is left, recorded in the
  activation's stream, and does not repeat. History shows only the retried text;
  partial output marks where the abandoned attempt ended. A failed activation now
  states its failure class (provider stream, budget, context limit, scope, capture,
  admission) and a suggested next step.
- **Reviewed native tool iteration.** Native ordinary Agent turns now derive their
  tool-round limit from the reviewed invocation allowance, with a 128-round safety
  ceiling. They no longer stop at the compatibility default of ten rounds while
  reviewed capacity remains. Every provider and tool invocation still requires
  its own durable grant admission; the round ceiling grants no extra authority.
- **Supported first-use provider choices.** Onboarding offers Ollama and OpenRouter,
  matching native Session provider admission. Direct OpenAI and Anthropic adapters
  remain available through manual compatibility configuration.
- **Native repository backend admission.** Native Session execution rejects E2B
  before provider or repository work because its backend does not implement the
  required process-supervision interface. Legacy E2B execution remains available.
- **OpenRouter JSON budget admission.** JSON output uses OpenRouter's single-request
  token bound; Ollama's two-pass repair allowance no longer rejects otherwise eligible
  OpenRouter profiles. Explicit output, token, and cost limits remain enforced.
- **Retryable daemon shutdown ownership.** Failed or cancelled shutdown waits retain
  the daemon and pending Agent joins for checked retry. Session repository cleanup
  keeps its registered controller and Workspace ownership until the lifecycle action
  succeeds. Standalone CLI commands retry incomplete shutdown and exit unsuccessfully
  if cleanup still cannot finish.
- **Exact Session-team authority.** Configuration validation now rejects empty or
  duplicate workflow identities, unresolved or duplicate roster members,
  out-of-roster entry Agents, ambiguous Coordinator/Worker ownership, and
  autonomous dependency graphs that are not closed acyclic DAGs. Persisted
  Sessions and History survive later team renames or removal; a new turn fails
  before mutation until the named team is restored or a new Session is created.
- **Agent tool allowlists cover memory tools.** Non-empty Agent tool allowlists now apply
  uniformly to executor, recall, and core-memory tools, so an Agent cannot receive an
  undeclared memory capability.
- **Crash-safe team Session continuity.** Lattice and Custom actors, plus
  a Coordinator selected by a single-Agent Session, now stage checkpoints behind
  the canonical Session turn. A completed turn
  promotes each Agent's own causal transcript; failed, cancelled, interrupted,
  or crash-recovered turns restore the prior transcript, clear private
  orchestration state, and retain incurred provider usage. Startup resolves any
  unfinished checkpoint phase from durable History before actors can resume,
  with a one-time accounting-safe adoption of pre-transaction caches. Before a
  pending transaction publishes `Committing`, Axocoatl validates its staged
  Agent namespaces and identities, accepts only exact canonical 16-digit
  checkpoint filenames, and decodes the selected checkpoint set. Malformed
  staged state fails closed while the manifest remains `Pending` and retryable.
  Legacy
  single-Agent Coordinators import checkpoint-only History before that adoption,
  then rebuild completed conversation without reviving private plans or Worker state.
  Transaction-scoped Agents can still read Tier 2–4 memory, but semantic auto-store,
  daily-log archive writes, core edits, and core consolidation now fail closed for
  the whole turn until those stores gain transactional promotion. Ordinary
  single-Agent failed, cancelled, or interrupted boundaries likewise retain stop ownership and
  block cached retries or new turns until the actor is proven stopped and rebuilt from History.
- **Fail-closed release retries.** Normal releases and the incident-locked v1.0.1
  recovery now use deterministic archives, run-scoped byte-identical handoffs,
  complete stable-release frontier checks, exact Git tag and GitHub Release
  metadata, API plus sparse-index crate proofs, and pre-mutation asset
  reconciliation. Re-running a completed release accepts only the same exact
  public latest release and never overwrites published state.
- **Local CI parity.** One repository preflight now exercises workflow contracts,
  installation, browser-product, documentation, marketing, film, release-order,
  publication-retry, cross-Linux, MSRV, formatting, lint, test, documentation-test,
  and release-build gates before a change reaches pull-request CI.
- **Truthful film provenance.** The 12 launch-film provenance records are restored
  byte-for-byte to their first committed capture declarations. A separate,
  verifier-enforced v1.0.1 compatibility attestation binds the frozen release
  source and audited delta without claiming that the films were captured with the
  v1.0.1 binary.

### Removed
- The `axocoatl-coordination` crate. Its event feed moved to `axocoatl_core::event_feed`;
  the rest (pheromone activation, auction, HTN, the signal field and the legacy turn
  scheduler) was removed. Version 1.0.0 stays on crates.io.
- Pheromone threshold activation. The event lattice is now a plain event feed:
  it no longer registers Agents, accumulates signal or keeps every event in
  memory for the life of the process. Per-Agent `activation_threshold` and
  `activation_decay` are ignored with a warning. The `stigmergic-workflow` and
  `skills-lattice` examples and the routing benchmark are gone. The feed moved
  from `axocoatl-coordination` to `axocoatl_core::event_feed` as `EventFeed`
  (`LatticeEvent` is now `FeedEvent`); WebSocket `event` frames, webhook payloads
  and `GET /api/events/recent` are unchanged. The daemon only ever published the
  events a Skill declares, so the sample configs, the `proactive-agents` example
  and the docs now watch Skill events instead of `AgentFailed` or `TaskCompleted`.
- Skill `reacts_to`, `agents` and `prompt`. They never ran anything: firing a
  Skill only publishes the events in its `emits` list. A config that still sets
  them loads, and the daemon warns that they are ignored. `GET /api/skills` no
  longer returns them, a fired Skill's event payload no longer carries
  `agents_holding`, and Settings → Skills groups Skills by the events they emit.
- The Coordinator's worker auction. Each subtask now goes to the first declared
  Worker, in declaration order, whose callable tools cover its required tools,
  and falls back to an ad-hoc Worker as before. When several Workers can do a
  subtask the first one gets it (the auction picked the last), and a Worker's
  token budget no longer affects the choice. The `coordinator-plan` stream frame
  drops its `score` and `bids` fields, and the run view no longer shows bids.
- The model-facing `coordination_control` tool. A native Agent with a delegated grant
  could use it to inspect and submit graph controls, replace future Agents, attach
  knowledge to follow-ups, and propose grant expansions. A lead now adds helpers only
  through `delegate`; Stop, Guide, Revise, Retry, Add Agent, and grant changes remain
  human controls. Stored records from the tool still load: a call whose return was lost
  stays unknown, an accepted Agent revision that was never applied is marked failed, and
  retained grant proposals still appear with the turn's grants.
- The Coordinator's symbolic HTN planner. A Coordinator always decomposes its
  task with its model; a workflow's `htn_methods_file` is ignored with a
  warning. The `HtnPlanner`, `FrontierResolver` and `LlmFrontierResolver`
  library types and the `htn-planner` example are gone.

## [1.0.1] — unpublished draft

The first-run corrections below are included in 1.1.0. The earlier 1.0.1 release
remained a draft; its date is not a public-release claim.

### Fixed
- **User-level first run.** `axocoatl onboard` now writes one owner-only user
  configuration and platform data directory instead of creating an unrelated project
  folder. Plain config-aware commands resolve that configuration consistently from any
  working directory. Hosted credentials use a masked prompt and a private `0600`
  configuration file. Project-local YAML remains available only through an explicit
  path; repositories become Workspaces through **Open workspace…** in the app. The
  combined `onboard --install-daemon` flow refuses shell-only credential placeholders
  that the generated service cannot inherit.
- **Release dependency gate.** The lockfile now selects non-yanked `chacha20`
  0.10.2 instead of the yanked 0.10.1 transitive release.

## [1.0.0] — 2026-08-25

### Changed
- **Stable product release.** Axocoatl's local-first workbench, durable Session runtime,
  normal and multi-Way coding loops, review/recovery controls, Skills, Automations, MCP
  approvals, and release artifacts make up the 1.0 product release.
- **Hardened dependency floor.** Source builds now require Rust 1.88. The PDF,
  spreadsheet, HTTP/2, MCP, terminal, OpenAI, concurrency, and random-number dependency
  lines are updated past current RustSec findings; CI runs a daily deny-by-default RustSec
  audit and rejects yanked lock entries. New Agent checkpoints use a versioned Postcard
  envelope. A narrowly isolated, size-limited Bincode 2.0.1 reader remains only to recover
  0.1.x checkpoint transcripts; the temporary unframed Postcard shape written during 1.0
  launch development is also recognized. Because the two markerless encodings can overlap,
  an exact dual-valid file uses the shipped 0.1.x Bincode interpretation; raw Postcard is
  selected only when the legacy reader does not match. Migration commits the complete transcript to
  canonical Session History as one crash-safe event before writing a higher-version Postcard
  cache. Corrupt or oversized newest caches fall back to an older valid version instead of
  hiding recoverable history.
- **Bounded checkpoint reconstruction.** Canonical Session History remains complete while
  restart projection keeps the newest whole turn segments within an 8 MiB message budget and
  the checkpoint's 64 MiB encoded envelope. Exact final-size validation prevents a very long
  Session from blocking daemon startup or its next turn, and a projected tail never begins
  with an orphan tool result.

### Added
- **Reviewed Session environments.** Session creation now persists a visible runtime/setup
  lifecycle before repository tools can run. Detected commands such as `npm ci` remain exact,
  unapproved proposals; Files, Source Control, Preview, Terminal, normal turns, and Ways
  operations that start work or inspect a live checkout stay gated until the environment is
  durably Ready, while durable evidence and Keep/Discard recovery remain reachable. File
  operations use that same sandbox boundary. Local Podman accepts a fixed curated-image set
  without arbitrary-image trust, verifies or provisions its required repository commands,
  masks a root Node project's host dependencies with a Linux-local volume, and fails with
  manual guidance instead of silently installing Podman or creating its VM. E2B uses one
  daemon-global template and rejects a per-Session OCI image rather than substituting it. Its
  exact runtime identity and remote root remain durable: Close and graceful shutdown pause a
  Ready runtime, Reopen/restart reconnect, and failed or interrupted preparation retains checked
  teardown. Only Delete Session or Change/Rebuild runtime is a deliberate destructive transition
  from Ready. A durable per-generation creation token reconciles ambiguous provider
  responses; if provider access cannot prove the result, **Review setup** exposes an exact-token
  manual-cleanup confirmation rather than creating a replacement or dropping ownership.
- **Durable named Workspaces.** Authorized project directories now have persistent Workspace
  identities independent of their Sessions. The rail scopes Sessions to the selected Workspace,
  **Open workspace…** is separate from **New session**, and legacy path-owned Sessions are
  migrated without changing their ids, transcripts, or execution directories.
- **One browser workbench at `/`.** A persistent workspace/session rail and chat
  spine now hosts the Conversation canvas, Files and Source Control, Preview,
  contextual Ways, focused attempt review, the Terminal dock, Agent graph, and
  Settings instead of splitting them into peer product destinations.
- **Heterogeneous attempts in a session.** A turn can select a different agent and
  model for each attempt, show live state across sessions, compare Outcome and
  Route, run a stored repository check command, and keep a result in the session
  checkout. Parallel attempts currently require an autonomous single-Agent Session on local
  Podman; E2B and multi-agent modes remain available for normal session execution.
- **Native embedded UI modules.** The browser app now uses buildless custom elements
  under `axocoatl-server/static/ui/`, served from `/ui/*` as native ES modules.
- **Canonical durable Session turns.** A Session-owned append-only ledger now records
  the accepted request, structured context references, per-agent outputs, and running,
  completed, failed, cancelled, or interrupted lifecycle. Stable turn and idempotency
  identities make reconnect and retries explicit; older single-agent checkpoint
  transcripts are migrated exactly once when canonical history is first read. A legacy turn
  without a completed assistant response is retained as interrupted, including any readable
  partial assistant output, rather than being presented as complete or silently dropped.
- **Session history controls.** The one app rehydrates canonical turns, searches within
  one Session or across Sessions with case-insensitive literal text matching, exports
  Markdown or JSON, and rewinds visible history at a durable turn boundary. Rewind is a
  logical append-only ledger operation; the current daemon limits it to an autonomous
  single-Agent Session so it can reconstruct that actor's checkpoint. The retained raw-message-count
  request remains a compatibility form.
- **Session-owned attachment context.** The composer accepts drag/drop or file-picker
  uploads as immutable, content-addressed context with **Once** and **Session** scope,
  preview/download, explicit extraction status, and bounded ingestion. Images are capped
  at 10 MiB, other documents at 25 MiB, and cached extracted/OCR text at 256 KiB. This
  context is wired to normal Session turns; isolated Attempts do not currently receive it.
  Accepted one-turn references are re-consumed from the canonical ledger after restart, and
  removing used Session context deactivates future inclusion while retaining the historical
  relation and blob pin so prior turns still open their exact bytes.
- **Durable bounded tool evidence.** Canonical turns record tool start/result events before
  they are broadcast. JSON export keeps the bounded structured events; Markdown renders a
  shorter Route preview and labels truncation. Oversized values remain bounded audit evidence,
  not provider-replay history. Complete provider response groups retain original call order,
  original provider arguments separately from hook-transformed execution arguments, assistant
  content, and bounded native replay metadata. Restart reconstruction is atomic per group: a
  malformed, incomplete, or truncated member stays visible as Route evidence but is omitted
  from the model-facing checkpoint rather than being replayed inaccurately.
- **Context-faithful Retry guard.** A historical turn with attachment or structured composer
  context cannot be reproduced from visible text alone, so inline Retry is unavailable for
  that turn; reattach the context in a new request instead.
- **Exact cooperative Stop for Session turns.** The browser supplies a durable turn id
  and Stop must match that active Session and turn. Provider streaming can be dropped
  immediately; an already-started side-effecting tool runs to a safe boundary before the
  turn becomes cancelled, preserving honest partial output and usage.
- **Pluggable session isolation.** Rootless Podman remains the local default; an
  E2B Cloud remote sandbox is an opt-in configured backend for normal sessions.
  The 1.0 backend targets E2B Cloud, not third-party E2B API implementations.
- **Opt-in provider rate-limit fallback.** A configured `provider:model` backup is
  tried once when the primary rate-limits before streaming any tokens. Plain-text-only
  histories remain per-call; once a response starts a tool exchange, the exact selected
  slot/provider/model stays pinned across later turns and restart while that native transaction
  remains in history. Missing, conflicting, or stale route markers fail closed rather than
  replaying provider-native ids or signatures to another API. A known smaller fallback rejects
  an already oversized request locally; unknown custom-model constraints remain
  endpoint-validated. This applies to global and normal Session Agents, including each
  declared Coordinator Worker. Ways remain primary-only until effective-route cost evidence
  can identify and price a fallback honestly.
- **Per-agent sampling config.** Autonomous, Coordinator, and declared Worker executions accept
  `temperature`, `top_p`, `max_tokens`, and `response_format` in YAML. Provider support varies.
- **Per-request overrides on agent execute.** `POST /api/agents/{id}/execute`
  accepts optional `system_override` and `model_override` for a single call.
- **Stateless per-request execution.** An isolated one-shot mode that runs a
  request without persisting to the agent's session, memory, or checkpoints —
  useful for evaluation. It performs one provider inference and advertises no
  tools; work that requires a tool loop belongs on a normal stateful execution.
- **Outbound event webhooks.** An opt-in dispatcher can POST signed
  (HMAC-SHA256) events from the lattice feed to configured endpoints, with
  bounded retries and secret redaction. A default install makes no outbound
  webhook requests.
- **One live automation runtime.** `AutomationStore` now drives manual,
  interval, lattice-event, and Skill triggers in both `dev` and `serve`. CRUD,
  enable/disable, cadence, and trigger edits reconcile without a restart;
  legacy workflow/schedule/proactive YAML seeds a missing store file once instead of
  registering a second set of runners.
- **Automation creation in Settings.** The Automation explorer can create a
  canonical manual, interval, event, or Skill-triggered record with a valid
  Input → Agent starter DAG, then open it directly in the graph editor.

### Fixed
- **Reliable Ollama Plan and Judge control calls.** Schema-bearing Plan and Judge calls now
  apply a call-local JSON response constraint (native where the provider supports it) and ask
  Ollama for `reasoning_effort: "none"`, without changing the selected autonomous Agent's
  ordinary Session, Skill, Automation, or tool-turn reasoning behavior. The Ollama adapter
  keeps any unexpected reasoning in the reasoning channel and rejects a reasoning-only
  terminal response instead of accepting a blank answer. Malformed scope JSON now stops after the first
  measured call, and a parsed plan must contain a concrete non-test implementation step and
  acceptance evidence before Ways can use it.
- **Complete provider-usage accounting.** Agent activations now retain input, output, and
  reasoning usage on success, cooperative cancellation, and measured failure. A dispatched
  call without terminal usage makes completeness sticky across later calls and checkpoint
  restart, so Settings, Session History, Ways controls, Automation checkpoints, HTTP,
  WebSocket, IPC, and CLI surfaces show a known subtotal instead of an exact-looking zero.
  Plan first, model checks, and Judge remain separate from each Way's execution economics;
  failed, timed-out, and invalid Plan/Judge responses still report their control-call usage.
- **Safe legacy Session rewind boundaries.** The retained raw-message-count endpoint now
  resolves exact boundaries from the canonical transcript rather than assuming every Turn is
  two messages. Failed or cancelled Turns with no Assistant output are retained correctly,
  and a count that would split a Turn fails closed.
- **Vendored browser dependency security and attribution.** The embedded Monaco 0.56 AMD
  graph is built with DOMPurify 3.4.13 and markdown-it 14.3.0, gated against known advisories,
  and packaged with Monaco's upstream notices plus the exact sanitizer licenses and
  attribution required by the shipped JavaScript.
- **Provider-native tool-loop continuity.** Provider-safe request-local tool aliases are
  deterministic, bounded, collision-checked, and decoded before hooks or dispatch. Parallel
  calls and results retain original provider order. Anthropic content blocks and thinking
  signatures, Gemini thought signatures, and original provider arguments survive live
  follow-ups and durable restart projection without substituting transformed execution data.
  Malformed arguments, incomplete streams, invalid terminal sequences, and partial cancelled
  calls fail closed instead of executing or being recorded as a completed response.
- **Fail-closed tool recovery and parallel dispatch.** Response-text tool recovery is limited
  to the effective Ollama route and to names the request actually offered; other providers keep
  response text as text instead of fabricating provider-native replay state. Recovered and
  structured responses reject a 129th tool call before hooks or dispatch, candidate parsing is
  bounded, and a panicking parallel task retains its originating call identity and position so
  another call's success cannot be attributed to it.
- **Bounded Coordinator provider calls.** Decomposition, unresolved HTN frontiers, and
  synthesis reserve output headroom against the exact configured model window before
  dispatch. When older context must be reduced, the Coordinator omits only completed
  User/plain-Assistant text at User boundaries while preserving System messages, the current
  request suffix, attachments, and canonical Session History. This request-local projection
  is distinct from the summarization pipeline used by stateful autonomous Agents.
- **Bounded provider transport failures.** Provider adapters reject redirects, cap error bodies
  and stream events, apply request and total-stream deadlines, and avoid reflecting secrets in
  surfaced errors. OpenAI-compatible, Anthropic, Gemini, Mistral, and Ollama response parsing is
  covered at malformed, parallel, and provider-native replay boundaries.
- **Control-plane storage and upgrade safety.** On supported Unix hosts, the daemon now
  retains one opened data-root capability and performs descendant state I/O relative to it,
  rejecting symlink traversal and multiply linked managed files. Bootstrap acquires an
  external per-root lease, the 0.1-compatible in-root lease, and the opened directory inode
  lock before runtime reconciliation or mutable state reads. It removes only inspected
  non-current Podman containers whose immutable identity and bind mounts prove they expose the
  data or lease root, then verifies those roots still name the opened directories. Local Session and
  Attempt sandboxes mask either protected root when it is below the Workspace and reject a
  Workspace at or beneath one. Current checkpoints, Agent memory, and Automation runs use
  versioned portable namespaces; bounded exact-name legacy recovery validates embedded
  identity where available, writes only the current location, and preserves the legacy source.
- **Recoverable Checks and Keep.** Attempt cleanup now stops Podman containers
  without inheriting Podman's default grace period, allows enough time for VM/client
  overhead, and preserves the last-known comparison evidence on transient refresh
  failures. Keep validates its durable journal through Git-owned scratch storage, so
  an applied result can resume and finish even when `.axo-variants/` is ignored by the
  repository. An exact cross-tab Keep or Discard settlement now re-reads durable Results,
  canonical History, and current Git state before restoring the primary runtime, so a
  completed decision cannot leave another tab showing a stale comparison.
- **Conversation-first responsive workbench.** The Session transcript and composer now own a
  centered primary canvas instead of competing with restored dashboard panes. Files, Preview,
  attempt review, and Agent graph open as focused surfaces with an explicit return; wide-screen
  pinning remains under More. The Ways inspector reserves width only while explicitly open and
  becomes an overlay at compact sizes, while the Session rail becomes off-canvas below 720 px.
  Existing Files/editor/Source Control, Terminal, History, context, graph, Attempts, comparison,
  Checks, Judge, Keep, and Settings behavior remains reachable.
- **Live-safe History hydration.** A delayed canonical History response now merges and repaints
  the longer same-turn live projection, including per-Agent text, reasoning, and correlated tool
  evidence, instead of briefly replacing already-visible output with an older durable prefix.
- **Automation editor validity and dialog access.** Map nodes now require an
  Agent, Tool, or Subgraph body; Subgraph nodes require a known Automation.
  Invalid references block save and run with inline guidance. Settings dialogs
  have accessible names, contain keyboard focus, and restore prior focus when
  they close.
- **Durable, deterministic Automation outcomes.** Bootstrap marks orphaned
  persisted `running` records as `failed` with a retained restart reason, and
  failed-node checkpoint diagnostics survive reload. Completed output now joins
  all executed runtime sinks in declaration order, including terminal Tool,
  Map, and Subgraph results.
- **Restart-safe Automation approvals.** Top-level runs parked at an Interrupt
  are rebuilt from their durable checkpoint after daemon restart, reappear in
  the rail, and continue after operator input without replaying completed nodes.
  New runs persist an immutable Automation/input snapshot, while run status and
  Interrupt checkpoints transition through one atomic file replacement. The
  Runs drawer now bypasses stale browser cache and polls open history in place.
- **Deterministic attempt judging.** Judge prompts now require every surviving
  attempt exactly once with unique ranks `1..N`, using the lower attempt index
  as the deterministic tie-break when outcomes are otherwise equivalent.
- **One live Automation runtime.** The persisted Automation store now drives manual,
  scheduled, lattice-event, and Skill-triggered execution in both `dev` and `serve`.
  Store edits take effect without restart, legacy YAML is first-boot seed data only,
  compatibility workflow/schedule/proactive endpoints project canonical records, and
  provider execution no longer holds the daemon or store lock. Automatic runs are
  single-flight, cool down at dispatch and completion, and retain last-run/count/error
  observations without letting one failure stop trigger dispatch.
- **Attempt-set ownership, isolation, and cleanup.** Parallel attempts now create a
  hidden snapshot of tracked changes and non-ignored untracked files, then give every
  attempt an independent no-origin Git clone in a dedicated Podman container. Durable
  set identity namespaces actors, clones, containers, and artifacts; persisted lane
  state, natural-language outputs, and review evidence survive reloads while the set is
  unresolved; stale actions conflict; and cleanup stops containers and joins actors before
  removing exact derived paths. Attempt actors receive the same request-local, provider-safe
  projection of prior Session context: User and plain Assistant text remain ordered while
  historical System and provider-native tool-transaction groups are omitted from model-facing
  Way history. History retains the full canonical turn record, with bounded tool evidence.
  Attempt actors cannot write shared
  core memory and do not receive Skills, MCP tools, or configured web search while those effects
  lack set-scoped rollback.
- **Resumable Keep and honest attempt cost.** Keep now requires a completed,
  non-empty attempt with a passing Check, records `applying`, `applied`, and
  `transcript_recorded` phases, and resumes the same selected attempt after an apply,
  transcript, or cleanup failure. It leaves the delta uncommitted for Git review. After
  cleanup, durable Session History retains the selected task, output, and turn attribution;
  candidate Routes, diffs, Checks, Judge ranking, and cost evidence end with the attempt set.
  Ollama at a configured loopback endpoint has a known-zero model API charge;
  incomplete usage and unconfigured remote prices—including non-loopback Ollama—remain
  explicitly unknown instead of appearing as a complete `$0.00` total.
- **Lightweight chat transcript isolation.** Separate chats and forks now run from
  their own stored history instead of the configured agent's live or
  checkpoint-restored Tier-1 transcript.
- **One persistent daemon surface.** `serve` now starts the same IPC service as
  `dev`, so the installed background service supports session-oriented CLI
  commands as well as the browser/API. The default socket is stable per user,
  protected by owner-only permissions, and startup will not unlink a live daemon
  or a non-socket path. Service definitions run from the config directory so
  relative runtime data stays attached to that project.
- **Release-package hygiene.** Every publishable crate archive now retains the
  `Apache-2.0` SPDX expression and includes the repository's license text. Raw
  host-specific resource measurements stay local; the resource guide documents
  the reproducible benchmark and validation commands without publishing a dirty
  machine-specific result as a product claim. The server crate carries exact
  package-local mirrors of its embedded Lattice and brand assets, rejects source
  drift and unexpected files, enforces a reviewed archive-size ceiling, and is
  compiled from its extracted `.crate` before the release can publish.
- **Source-build instructions** now target `axocoatl-cli`, the package that produces
  the binary and embeds the browser app. A root-only build compiles the placeholder
  workspace package and can otherwise leave stale UI bytes.
- **Coordinator decomposition parsing** now tolerates the surrounding prose that
  reasoning models emit around the JSON subtask list.
- **macOS workspace build** for `axocoatl-isolation`.

### Removed
- Unwired Wasmtime, Firecracker, and youki isolation prototypes and their feature flags.
  They were never selectable workbench backends; 1.0 ships only the product's reviewed
  rootless Podman path and configured E2B Cloud remote path.
- The standalone `/app` and page-level `/variants` product shells. Their workflows
  now live in the session-centered app at `/`, the only interactive browser route.
- The Studio destination, directoryless lightweight Chat destination, and
  cross-chat Files browser destination from the one-app navigation. Their
  underlying lattice, chat, FileStore, REST, and WebSocket compatibility
  surfaces remain available to integrations; they are not hidden product pages.
  Session-native history and attachment context now cover the corresponding workbench
  needs without recreating either peer destination.

## [0.1.4] — 2026-06-13

### Added
- **Coordinator run view in the dashboard.** A coordinator's Layer-2 work —
  decomposing a goal, auctioning each subtask to a worker by capability and
  budget, running them in parallel, then synthesizing — is now a live drill-in
  view: goal → the auction (the winning worker and the runner-up bids per
  subtask) → each worker's status and output → the final synthesis. Workers are
  driven by the coordinator (they are not lattice nodes), so this is the surface
  that shows the team. A `CoordinatorReporter` trait keeps the actor crate
  decoupled from the daemon's stream types, and the run id is threaded through
  `AgentInput.context`.
- **Prebuilt `aarch64-unknown-linux-gnu` binaries.** The workspace now uses
  rustls instead of native-tls/openssl, so it cross-compiles to ARM Linux; that
  target is built and published alongside the other release binaries, with a CI
  job guarding the cross-build.

### Fixed
- **Ollama tool calls emitted as text are now recovered.** Some local models
  (e.g. qwen3-coder) return tool calls as `<function=…>` text in the message
  content instead of structured `tool_calls`. Axocoatl now parses that fallback
  form, so those models can drive tools — write files, run commands — instead of
  silently doing nothing.

## [0.1.3] — 2026-06-11

### Fixed
- **Coordinator workers now run on a configured model instead of `gpt-4o`.**
  Spawned workers inherited `AgentConfig::default()`'s `gpt-4o`, so on a
  local-only (Ollama) provider every worker returned `404 model 'gpt-4o' not
  found` and the coordinator could never synthesize. `WorkerConfig` now carries a
  model: declared workers use their own configured model, and ad-hoc workers
  (spawned when no pooled worker bids) inherit the coordinator's.
- **`bash_background` no longer kills the dev server it's asked to start.** A
  trailing `&` double-backgrounds the command (the tool already backgrounds it),
  so the wrapper shell exits and SIGHUPs the process — a dev server dies on
  startup and leaves its port stuck (`Errno 98` on the next bind). The tool now
  strips a single trailing `&` (leaving `&&` and a mid-command `&` untouched).
- **Demo config (`axocoatl.yaml`): the `coder` agent now uses `qwen3:8b`.**
  `qwen2.5-coder:14b` does not support tool-calling through Ollama — it returns
  tool calls as text content rather than structured calls, so a coder session
  never executed them and could not write files or run commands. `qwen3:8b` emits
  structured `tool_calls`; its system prompt requests `/no_think` to reduce reasoning
  output on runtimes that honor that soft prompt switch.

## [0.1.2] — 2026-06-11

### Added
- **Agent-managed core memory (MemGPT/Letta-style blocks).** Tier 3 is now a set
  of named, character-limited, agent-editable blocks (default: `persona`,
  `human`, `project`) rendered into the system prompt every turn. The agent
  curates them mid-conversation via three tools — `core_memory_append`,
  `core_memory_replace`, `core_memory_set` — and an edit is visible on the very
  next request (same turn). Blocks are per-agent by default; a block marked
  `shared` is backed by a process-wide registry so multiple agents see each
  other's edits (team memory). Configure per agent under `memory.core`. This is
  the curated top of the hierarchy. Canonical Session or Chat history remains
  the transcript authority; Tiers 2 and 4 are derived recall stores.
- **Background "sleep-time" memory consolidation.** When enabled, a daemon loop
  periodically asks registered **idle autonomous Agents** to run an LLM
  memory-manager pass (`on_consolidate`) that promotes durable facts from recent
  Tier-4 activity into the right core-memory block and tidies them — promotion-only,
  never evicting Tier 4. The Agent self-gates on idle time so a pass never fires
  mid-conversation. Declared Coordinator Workers are not polled by this loop, and
  stopping an Agent starts no provider or memory work. Tunable under
  `consolidation` (`enabled`, `idle_threshold_secs`, `interval_secs`).
- **Agent-driven memory recall (MemGPT/Letta-style).** Retrieval is now hybrid:
  the top-k semantic hits are still injected passively each turn, and the agent
  can also pull on demand with two new tools — `recall_search` (semantic search
  over Tier-4 memory) and `recall_timeframe` (read the Tier-2 daily log for a date
  or range). The recall tools are agent-scoped (owned by the behavior, since they
  reach a *specific* agent's per-agent stores), advertised to the model, and
  dispatched in the existing tool loop alongside executor tools. A standing
  capability hint plus a post-compaction note tell the agent what's recallable so
  the tools get used. Recall is tunable per agent via `memory.recall`
  (`passive_inject`, `top_k`, `min_score`), inherited by coordinator workers.
- **Coordinator role — hierarchical task decomposition with worker agents.** An
  agent with `role: coordinator` decomposes a goal into subtasks, assigns each to
  the best-fit worker by **auction** (tool-capability match + remaining token
  budget), runs the workers **in parallel**, and synthesizes their outputs into a
  single answer. Decomposition is HTN-symbolic when methods are configured (an
  `HtnPlanner` expands compound tasks; an `LlmFrontierResolver` fills only the
  frontiers the methods don't cover) and LLM-driven otherwise. Workers are
  first-class agents — their own configured provider, model, tools, budgets,
  sampling, hooks, and Session-scoped memory/checkpoints — and they are torn down after every
  pass (on success and on every error path) so nothing leaks. The Coordinator owns Tier-1
  conversation and an internal orchestration checkpoint; its provider loop does not expose
  Tier 2–4 recall. A terminal Completed, Cancelled, Failed, or Interrupted Session turn clears
  private orchestration state, so a later turn decomposes fresh rather than auto-resuming
  finished subtasks. A fully failed
  worker set surfaces an error rather than a hollow result. Per-agent activation
  thresholds are configurable, and coordinator/worker role invariants are
  validated at config load.
- **Automatic context compaction with real LLM summarization.** As a session
  grows toward the model's context window, old turns are now **summarized** (via
  the agent's own provider). When Tier-2 daily log memory is configured, a bounded
  structured archive is written before compaction; canonical Session or Chat
  history remains the transcript authority. Compaction is always on and
  runs before each request, so long conversations keep their early context
  instead of forgetting it. The 5-stage `CompressionPipeline`'s LLM stages
  (microcompact, autocompact) are now wired to a concrete `LlmSummarizer`, whose
  own summarization tokens count against the agent's budget.
- **OpenAI-compatible servers + per-agent model.** The `openai` provider now
  honors a configurable `base_url`, so it targets any OpenAI-compatible endpoint
  (LM Studio, MLX/oMLX, vLLM, and others), not just `api.openai.com`. Each agent's
  configured `model` is sent as a per-request override, so a shared provider uses
  the agent's model, including in the summarizer and the consolidation pass. Stdio
  MCP servers now receive their configured env vars (e.g. an API key), and four
  catalog entries were repointed from nonexistent npm packages to their `uvx` /
  PyPI equivalents. (Initial PR by first-time contributor Andris Gauračs.)

### Changed
- **Tier 3 is no longer a shared key-value fact store.** The old daemon-global
  `LongTermMemory` (one `long_term.bin` for all agents, written by a session-end
  LLM extraction in `on_stop`) is **retired**, replaced by per-agent core-memory
  blocks. Any existing `{data_dir}/memory/long_term.bin` is obsolete and may be
  deleted; no migration is performed.
- **`overflow_policy` now controls the local token guard: `abort` (default) or
  `warn`.** Context management is automatic and independent of the budget, so
  the old `summarize` policy is no longer a distinct behavior — it is accepted
  as a deprecated alias for `warn`. `abort` refuses locally over-budget calls
  and surfaces provider-reported overruns, but it is not an absolute provider
  billing cap.

### Removed
- Dead `ContextCompressor` (superseded by the wired `CompressionPipeline`).

## [0.1.1] — 2026-06-09

### Added
- **Variants — run one prompt several ways, right in the conversation.** Fan a
  turn out into N parallel attempts (the ⑂ control in the composer, configurable
  from 1 up to 100) and keep the one you like. Each attempt is a real agent
  working in isolation — its own `git worktree` + branch (`axo/variant-{i}`)
  inside the session's container, separate from the others and from your working
  tree. The attempts appear as live **option-pills** at the head of the
  assistant's turn: flip between them as they stream, glance at each one's
  changed-files summary, and **keep** one (reply to it, or a single Keep) — which
  silently merges its branch into your working tree and dissolves the rest. A
  heavy fan-out degrades gracefully: a failed attempt settles on its own, and a
  failed worktree set rolls back cleanly rather than leaving debris. The agent's
  `bash` tools run rooted at each attempt's worktree, so a variant's shell edits
  stay on its own branch. New routes under `/api/sessions/{id}/variants` (start,
  status, adopt, discard); `SessionSandbox::attach` reuses one container across
  worktrees.
- **A conversation-forward cockpit you configure, not a grid you're handed.**
  The session cockpit's hardwired three-pane layout is now an N-surface engine
  (Files, Activity, Browser, Terminal, Agent graph) that tiles, resizes,
  collapses, and reorders generically — but the resting state is calm: a freshly
  opened session is **just the conversation**. Surfaces show up when they're
  useful. The agent's edits land as a **change card** ("Changed N files", tap a
  file for an inline diff); a running dev server lands as a **preview card**
  ("Open" brings the browser in). You add the file tree, terminal, or agent
  graph yourself from a **Panes** menu when you want them, and the files pane's
  editor collapses to nothing when no file is open so it never sits there empty.
  The per-turn model/agent-target pickers and the Panes toggles are small
  on-theme web components (`ax-select`, `ax-toggle`) rather than stock browser
  controls. Layout, sizes, and order persist.
- **Unified, polished conversation UI across the Chat tab and the Sessions
  Activity pane.** The two surfaces now share one rendering layer:
  - Messages render with **markdown-it** (tables, nested/task lists,
    blockquotes, highlighted code) instead of the old hand-rolled renderer.
  - One **tool-call card** with a verb header ("▸ Bash: …", "◆ Read …",
    "◍ Search the web: …"), a collapsible result, and web-search citations —
    identical in both tabs.
  - A shared **"thinking…" indicator** from the moment a turn is sent until
    the first token, tool call, or reasoning chunk.
  - Agent **reasoning** now renders in the Sessions pane (a collapsible block,
    matching Chat), and session messages use the same prose styling as Chat.
  - **Per-message actions on Chat turns** — Copy, Rewind (user turns), and
    Retry + Fork (assistant turns) — all branch via `POST /api/chat/{id}/fork`,
    leaving the parent chat intact.
- **Persisted session transcripts with Retry and Rewind.** A directory
  session's conversation now survives reopening the cockpit — it rehydrates
  from the session agent's checkpoint via the new
  `GET /api/sessions/{id}/messages` (user/agent turns + tool cards). Each turn
  carries actions: **Copy**, **Rewind** (drop this turn onward and re-ask), and
  **Retry** (regenerate the reply), backed by a new
  `POST /api/sessions/{id}/rewind` that truncates the checkpoint and resumes the
  next turn from the truncated state.
- **Git-native sessions: a live Source Control pane.** A directory session is
  now (auto-)a git repo — `git init` + a baseline commit on first use if the
  folder isn't already one (existing repos used as-is). git runs inside the
  session sandbox, on the bind-mounted folder. A VS Code-style **Source
  Control** tab in the cockpit's files pane shows the agent's working-tree
  changes live (branch + changed files with A/M/D/U badges + a count badge),
  opens each change as a **Monaco diff** (HEAD vs working), and supports
  **commit**, per-file **discard**, and **branch switching** from a dropdown.
  An open diff **stays live** — it re-fetches as the agent keeps editing and
  clears itself once the file is committed or reverted — and binary or
  oversized (>512 KB) files report a sentinel instead of dumping bytes into the
  editor. New routes under `/api/sessions/{id}/git`: `status`, `diff`,
  `branches`, `commit`, `discard`, `checkout`. This is the substrate for
  parallel branch "Variants" (next).

### Fixed
- **A lingering session sandbox container no longer breaks new sessions.** A
  container left running by a prior daemon run (a crash, a kill, or a fresh
  data dir) keeps holding its published host ports, so the next session that
  publishes overlapping ports fails to start its rootless port-forwarding proxy
  ("proxy already running") and hard-fails — e.g. the auto-started terminal
  errors on open. The daemon now reaps orphaned `axo-ses-*` containers on
  startup, and treats "proxy already running" as a recoverable port conflict
  (the session opens without that port's forwarding rather than failing).
- **Multi-turn tool-calling round-trip now works on every provider.** Agents
  could be handed tools, but the conversation could not continue after a tool
  ran: the agent loop never recorded the assistant's tool-call turn before the
  tool results, and the results carried no `tool_call_id`, so every follow-up
  request was malformed and rejected by the provider APIs. The full loop —
  model emits a tool call → the tool runs → its result is fed back → the model
  continues — now works on Ollama, OpenAI, OpenRouter, Anthropic, Gemini, and
  Mistral, in both the chat path and resumable sessions. Verified end-to-end
  against each provider's live API.
  - `ToolCall` moved into `axocoatl-core` (re-exported from `axocoatl-llm`) so
    the universal message model can reference it. `ChatMessage` and the
    persisted `StoredMessage` now carry an assistant turn's `tool_calls` and a
    tool result's `name` + `tool_call_id`; new fields are `#[serde(default)]`
    for backward compatibility.
  - The agent loop appends the assistant tool-call turn before dispatching and
    tags each result with its originating call, so the replayed conversation is
    well-formed for every provider's native format (OpenAI `tool_calls` +
    `role: tool`, Anthropic `tool_use`/`tool_result` blocks, Gemini
    `functionCall`/`functionResponse`).
  - Streaming tool-call deltas accumulate by provider `index`. OpenAI, Mistral,
    OpenRouter, and Ollama send the call id only on the first SSE chunk and key
    later argument fragments by index, so tool arguments split across many
    chunks now assemble correctly instead of fragmenting into bogus calls.
  - Gemini and Mistral now send tool definitions and parse tool calls; their
    `capabilities()` report `tool_calling: true`.
- **Tool calling on the OpenAI and Anthropic providers.** Both built the
  outbound chat request without attaching the tool definitions, so models on
  these providers never received the available tools and could not make tool
  calls — only the Ollama provider sent tools. OpenAI now attaches converted
  tools via a shared `build_chat_request` used by both `chat` and `chat_stream`;
  Anthropic attaches `tools` in `build_request_body`. Adds regression tests
  asserting the tool definitions reach the request.
- **Gemini and Mistral providers were non-functional for agents.** The agent
  runtime always streams (`stream_chat` → `provider.chat_stream`, no fallback),
  but both providers' `chat_stream` returned "Streaming not yet implemented", so
  any agent on `provider: gemini` or `provider: mistral` failed on its first
  turn. Implemented real token-by-token SSE streaming for both — Gemini via
  `streamGenerateContent?alt=sse`, Mistral via `stream: true` — matching the
  Anthropic provider's `reqwest_eventsource` pattern, with unit-tested chunk
  parsers.
- **Gemini targeted an endpoint that cannot do function calling.** The provider
  used the `v1` endpoint, which serves the current models but rejects the
  `tools` field outright (`Unknown name "tools"`) and has no `systemInstruction`
  field — so it can never make a tool call. Moved to `v1beta`, which serves the
  current models (e.g. `gemini-2.5-flash`) *and* supports both `tools` and
  `systemInstruction`; restored native `systemInstruction` instead of folding
  the system prompt into the first user turn. Verified end-to-end against the
  live Gemini API.
- **A corrupt or outdated checkpoint no longer prevents an agent from starting.**
  Checkpoint load now discards an undecodable snapshot (corruption, or a schema
  change across an Axocoatl upgrade) with a warning and starts fresh, instead of
  failing agent startup with a fatal deserialization error. A checkpoint is a
  regenerable cache, never a source of truth.

## [0.1.0] — 2026-06-03

First public release. The framework is functional end-to-end with a real LLM
(local via Ollama, or any configured provider).

### Added
- **Stigmergic multi-agent coordination**: EventLattice pheromone-signal
  activation wired into the daemon. Agents in a workflow self-activate via a
  `depends_on` DAG—no central orchestrator. HTN and auction types were library
  primitives in this release, not part of the live daemon execution path.
- **Workflow execution**: `axocoatl workflow list|run`, `POST /api/workflows/{id}/execute`,
  and IPC support. Entry agents activate directly; downstream agents cascade
  via `TaskCompleted` events.
- **Full command surface** — previously stubbed commands now functional:
  `tokens report`, `agents status`, `agents restart`, `mcp servers`, `mcp tools`.
- **MCP integration**: daemon connects to configured MCP servers at bootstrap
  (stdio + streamable-http transports).
- **Developer experience**: `axocoatl onboard` interactive setup wizard and
  `axocoatl doctor` environment health check.
- **Distribution**: one-line install script and prebuilt binaries for Linux
  x86_64 and macOS; published to crates.io.
- Root `README.md`, `CHANGELOG.md`, `.gitignore`, user-facing
  `docs/ARCHITECTURE.md` and `docs/TROUBLESHOOTING.md`.

### Changed
- Workspace and all crates renamed from **Nexus** to **Axocoatl**.
- Version bumped from `0.0.1` (name-reservation placeholder) to `0.1.0`
  (first real release).
- Examples are now part of the workspace build and each has a README.

### Fixed
- Workflow coordination bug where the initial `UserInput` event spuriously
  activated downstream agents in parallel instead of cascading after their
  dependencies completed.
- `LICENSE` copyright attribution corrected to "Axocoatl Contributors".
- Zero compiler warnings across the workspace.

[1.0.1]: https://github.com/axocoatl/axocoatl/releases/tag/v1.0.1
[1.0.0]: https://github.com/axocoatl/axocoatl/releases/tag/v1.0.0
[0.1.4]: https://github.com/axocoatl/axocoatl/releases/tag/v0.1.4
[0.1.3]: https://github.com/axocoatl/axocoatl/releases/tag/v0.1.3
[0.1.2]: https://github.com/axocoatl/axocoatl/releases/tag/v0.1.2
[0.1.1]: https://github.com/axocoatl/axocoatl/releases/tag/v0.1.1
[0.1.0]: https://github.com/axocoatl/axocoatl/releases/tag/v0.1.0
