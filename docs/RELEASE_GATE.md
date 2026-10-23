# Release gate: a brand-new user, from zero

No Axocoatl release is published until a machine that has never seen Axocoatl
goes from the published install command to a first passing, signed run on
every default path, following only the published copy, unless the logged
override described below is used. Every live run before
1.5 reused a machine that was already set up: a running Podman machine,
Ollama with models already pulled, a `/usr/local/bin` the founder can write
to, and `~/.local/bin` already on `PATH`. This gate exists so that cannot
happen again.

The gate has four tiers. Tier A runs in CI on every pull request and again in
the release workflow. Tier B runs in CI on macOS. Tiers C and D are live runs
that a person starts on real machines; their results are committed as a
signed evidence bundle. `release.yml` refuses to publish without that bundle.

## Required paths

`release.yml` reads this list. Each path is one file of evidence in the
bundle, and each must say `pass: true`.

<!-- release-gate:required-paths:start -->
- `A-x86`: Tier A, CI, ubuntu-24.04 (x86_64), `scripts/release-gate/newuser-linux.sh`
- `A-arm`: Tier A, CI, ubuntu-24.04-arm (aarch64), the same script
- `C1-local`: Tier C, the gate Mac from zero, local models through Ollama
- `C1-claude-code`: Tier C, Claude Code as the writer, `--keep pr` and the
  verify-receipt Action
- `C1-codex`: Tier C, Codex as the writer, billed to the ChatGPT plan
- `C1-openrouter`: Tier C, writer and reviewer on OpenRouter, and Claude Code
  with an OpenRouter reviewer
- `C1-service`: Tier C, the Always-On Service from onboarding's Step 6 to
  `service uninstall`
- `C1-workbench-webkit`: Tier C, the workbench in Safari, from the sign-in
  link to a commit, then its Previews and cookies
- `C1-recovery`: Tier C, a run that starts a stopped Podman machine
- `C1-real-repo`: Tier C, timed runs on the pinned real repositories
- `D1`: Tier D, Ubuntu 24.04 VM, a new user made with `adduser`, OpenRouter
  models, the Always-On Service (`newuser-linux.sh --live`)
- `C2-local` (when the public copy names 16 GB): Tier C on a 16 GB Apple-silicon Mac
- `C2-openrouter` (when the public copy names 16 GB): the same Mac, OpenRouter reviewer
<!-- release-gate:required-paths:end -->

The C2 paths are required as soon as `README.md`, `llms.txt`,
`docs/CLAIMS.md`, the marketing site or the docs site name 16 GB anywhere
(the checker prints each `file:line`). Any mention counts, including "not
tested on 16 GB": describe a memory floor without naming 16 GB, or record C2.

A bundle may hold other paths too (the Ubuntu 22.04 refusal leg, Tier B,
`C1-workbench-chromium`). They are reported, and a failing one is a
warning, not a refusal.

## Required steps

A path passes when every step it recorded as required passed, but a path
that never recorded a step proves nothing about it. So each sentence of the
public text about what was verified relies on a step listed here, and
`check-evidence.mjs` requires each one in that path's evidence, recorded as
required and passed. A step the script skipped (a `--yes` run skips every
step that only a person answering live can show), recorded as optional, or
never recorded fails the path, naming the step. The list is read from the
tagged commit, like the paths, and `evidence.test.mjs` fails when a step
named here is not a whole quoted string in the script that records its
path (`newuser-linux.sh` for A and D, `newuser-mac.sh` for C) or in
`lib.sh`. Restate the public text (see "Restating what was verified") from
these steps.

<!-- release-gate:required-steps:start -->
- `A-x86`
  - `start without axocoatl on PATH`
  - `install with the published command`
  - `a new shell finds axocoatl`
  - `installed binary is the tarball's`
  - `onboard --yes --writer ollama with no terminal`
  - `onboard ran no sudo and no package manager through PATH`
  - `onboard ran no sudo by any path (system journal)`
  - `--no-pull pulled nothing`
  - `doctor: ready, no FAIL in sandbox, models, writers, daemon`
  - `axocoatl dev serves on its configured port`
  - `the README's command passed on every fixture, each receipt and record verified`
  - `a check program that is absent: exit 3`
  - `refused before any model call (0 tokens)`
  - `reviewer anthropic:x: exit 3`
  - `no Session, container or model call for the refused run`
  - `onboard without Podman: exit 1`
  - `doctor FAILs the subuid range with the usermod fix`
  - `copy-paste lint (scripts/check-copy-paste.mjs)`
- `A-arm`
  - `start without axocoatl on PATH`
  - `install with the published command`
  - `a new shell finds axocoatl`
  - `installed binary is the tarball's`
  - `onboard --yes --writer ollama with no terminal`
  - `onboard ran no sudo and no package manager through PATH`
  - `onboard ran no sudo by any path (system journal)`
  - `--no-pull pulled nothing`
  - `doctor: ready, no FAIL in sandbox, models, writers, daemon`
  - `axocoatl dev serves on its configured port`
  - `the README's command passed on every fixture, each receipt and record verified`
  - `a check program that is absent: exit 3`
  - `refused before any model call (0 tokens)`
  - `reviewer anthropic:x: exit 3`
  - `no Session, container or model call for the refused run`
  - `onboard without Podman: exit 1`
  - `doctor FAILs the subuid range with the usermod fix`
  - `copy-paste lint (scripts/check-copy-paste.mjs)`
- `C1-local`
  - `the account started without Axocoatl, its data or stored secrets`
  - `the account started without Podman or a Podman machine`
  - `the account started without Ollama or its settings`
  - `install with the published command`
  - `a new shell finds axocoatl`
  - `onboarding was answered live in the terminal`
  - `onboarding installed Podman with Homebrew`
  - `onboarding installed Ollama with Homebrew`
  - `onboarding created the Podman machine, and it runs`
  - `Ollama's cloud models are off (doctor)`
  - `onboarding ended Ready with a passing first run`
  - `doctor: ready, no FAIL`
  - `the README's command passed on every fixture, each receipt and record verified`
- `C1-claude-code`
  - `no Claude Code sign-in was stored before this path`
  - `onboarding was answered live in the terminal`
  - `Claude Code was signed in through the browser during onboarding`
  - `doctor: ready, no FAIL`
  - `the home page's fix-claude-code command passed on node-deps and python-pytest`
  - `--keep pr opened a pull request`
  - `the verify-receipt Action verified the pull request's receipt`
  - `reviewer anthropic:opus is refused: exit 3`
  - `the refused run left no Session`
- `C1-codex`
  - `no Codex sign-in was stored before this path`
  - `onboarding was answered live in the terminal`
  - `Codex billing was set to the ChatGPT plan`
  - `Codex was signed in with ChatGPT in the browser during onboarding`
  - `doctor: ready, no FAIL`
  - `the site's fix-codex command passed on node-deps and python-pytest`
- `C1-openrouter`
  - `no OpenRouter key was stored before this path`
  - `onboarding was answered live in the terminal`
  - `OpenRouter was connected through the browser sign-in during onboarding`
  - `config.yaml holds no OpenRouter key`
  - `onboarding asked for different writer and reviewer models`
  - `doctor: ready, no FAIL`
  - `axocoatl run fix passed on node-deps and python-pytest with the OpenRouter models`
  - `fix-claude-code with the OpenRouter reviewer passed on node-deps`
- `C1-service`
  - `onboarding was answered live in the terminal`
  - `onboarding installed and started the Always-On Service (Step 6)`
  - `onboarding restarted the service after the configuration changed (Step 7)`
  - `a fix through the Always-On Service passed, its receipt and record verified`
  - `axocoatl service status: installed and running`
  - `axocoatl service stop: the service is not running`
  - `axocoatl service uninstall: not installed, its unit removed`
- `C1-workbench-webkit`
  - `the sign-in link opened the workbench`
  - `Open workspace refuses the home folder`
  - `Open workspace accepts the repository`
  - `New session starts`
  - `Team & budget is pre-filled and Apply works`
  - `one turn edits a file`
  - `Source Control lists the turn's change to src/paginate.js`
  - `a commit from the workbench succeeds`
  - `the commit is authored by this account's identity`
  - `the Preview shows the Session's app in the workbench`
  - `Open full preview is disabled in Safari, and the workbench says why`
  - `a Preview address typed into a tab gets Axocoatl's refusal page`
  - `a redirect from another site to a Preview gets Axocoatl's refusal page`
  - `a Command-click inside the Preview gets Axocoatl's refusal page`
  - `a Domain=localhost cookie flood from another page leaves the workbench as the docs say`
  - `Manage Website Data removes the planted localhost cookies`
  - `Ctrl+C stops axocoatl dev with exit 0`
- `C1-recovery`
  - `podman machine stop`
  - `the Podman machine is stopped before the run`
  - `a run with the machine stopped passed, its receipt and record verified`
  - `the run started the stopped machine`
- `C1-real-repo`
  - `a timed run on every pinned real repository ended with a verdict, its receipt verified`
- `D1`
  - `the account started without Axocoatl, its data or stored secrets`
  - `the account started without Podman`
  - `the gate removed this account's subordinate ids; another user's range overlaps 100000-165535`
  - `onboarding was answered live in the terminal`
  - `onboarding installed Podman with apt`
  - `onboarding added a subordinate id range that no other user's overlaps`
  - `OpenRouter was connected through the browser sign-in during onboarding`
  - `onboarding did not install Ollama`
  - `doctor: ready, no FAIL in sandbox, models, writers, daemon`
  - `axocoatl dev serves on its configured port`
  - `the README's command passed on every fixture, each receipt and record verified`
  - `this login has a systemd user session (the service needs one)`
  - `onboarding installed and started the Always-On Service (Step 6)`
  - `onboarding restarted the service after the configuration changed (Step 7)`
  - `a fix through the Always-On Service passed, its receipt and record verified`
  - `axocoatl service status: installed and running`
  - `axocoatl service stop: the service is not running`
  - `axocoatl service uninstall: not installed, its unit removed`
<!-- release-gate:required-steps:end -->

Every run on a fixture or a real repository, and the `--keep pr` run, also
checks that no accepted output of the run holds tool-call markup a model
wrote as text (`<function=` or `</tool_call>`). A turn that ended on such
text ran nothing it meant to run. The check
(`scripts/release-gate/record-answers.mjs` and `gate_markup_check` in
`lib.sh`) needs no daemon:

- It reads every answer the run accepted from the Session history in the
  run's record: each activation the record marks `currently_accepted`,
  whose output is the answer. A refused, failed or replaced activation's
  output was never accepted and is not read. It reads only a whole record
  (its end line's line count and digest, as `axocoatl record verify`
  checks). A history it cannot read, an accepted activation whose output
  the record does not hold, or a run that passed or whose checks failed
  with no accepted answer fails the step.
- It reads every line the run added to a file since the commit the record
  says it started from (`repo_head`), in commits and in the working tree,
  and every new file. It leaves out the record, receipt and JUnit report
  the gate asked for, which quote what the run read. Lines the run left
  alone may quote markup (a parser's own tests, a changelog) and are not
  read.

A run whose accepted output holds markup fails, and with it the summary
step of its path.

## How the release workflow enforces it

`release.yml` (on a `v*` tag) adds three things to the existing gates:

1. **Release gate — new-user evidence** runs first, next to the product test
   gate, and the build needs it, so nothing is built, drafted or published to
   crates.io without it. It runs
   `node scripts/release-gate/check-evidence.mjs --version X.Y.Z --tag-commit SHA`.
2. **Release gate — new user (Linux)** runs Tier A again on the release
   workflow's own built archives (x86_64 on ubuntu-24.04, aarch64 on
   ubuntu-24.04-arm), and the GitHub draft needs it.
3. **Reverify and publish GitHub Release** runs the evidence check again
   before it touches the release.

`check-evidence.mjs` passes only when all of this holds:

- `.axocoatl/gate-keys` lists at least one key (lines as
  `axocoatl receipt key` prints them; `#` comments allowed).
- A bundle `release-evidence/<version>/<commit>/` exists whose `<commit>` is
  the tag's commit, or an ancestor of it from which the tag changes nothing
  but `release-evidence/`, `.axocoatl/gate-keys` and
  `.axocoatl/gate-override-actors`. That is how evidence is committed for the
  commit it tested: the tested commit is not changed, only evidence (and, the
  first time, the gate key) is added on top of it, and the tag goes on the
  evidence commit or a later evidence-only commit.
- `manifest.dsse.json` is a DSSE envelope of `manifest.json`'s exact bytes,
  signed by a key in `.axocoatl/gate-keys` (payload type
  `application/vnd.axocoatl.release-evidence+json`, so a receipt can never
  pass for a manifest or the reverse).
- Every file in the bundle is in the signed manifest with its SHA-256, and
  nothing else is in the bundle (no extra files, no symbolic links).
- The manifest is for this version and commit, and every required path above
  has a file whose `pass` is `true`, whose required steps all passed (a
  skipped required step fails the path), which holds every step "Required
  steps" lists for it, passed, and whose version and commit match.

When it refuses, it names every missing or failing path and why, and the job
fails.

### The override (the only way past a failing gate)

A failing gate can be overridden only by a manual run of the release workflow
on the tag, started by a GitHub login listed in
`.axocoatl/gate-override-actors`, with the phrase typed exactly:

```bash
gh workflow run release.yml --ref v1.5.0 \
  -f new_user_gate_override='publish v1.5.0 without the new-user gate'
```

- The phrase names the version; any other text, or an override on a tag push,
  still fails.
- The phrase is public, so knowing it is not enough. `check-evidence.mjs`
  accepts it only when both the run's actor (`github.actor`) and the login
  that ran it (`github.triggering_actor`, which differs on a re-run) are
  listed in `.axocoatl/gate-override-actors`, read from the tagged commit.
  It lists the founder's login (`cubanerick`), added in a reviewed commit,
  and nobody else; an empty list lets nobody override. The workflow contract
  checker (`scripts/check-workflow-contracts.rb`) fails CI if either evidence
  check stops passing both actors.
- The run logs a warning annotation, writes the override, the actor and every
  missing path to the job summary, and GitHub keeps the input on the run page.
- Only the evidence check is overridden. Tier A in the release workflow still
  runs and must pass.
- It is for the founder only, and only when a live tier cannot run for a
  reason outside the product (for example, a broken gate machine). Say in the
  release notes which paths were not run.

## Evidence

A bundle is a directory, committed to this repository:

```text
release-evidence/1.5.0/<commit>/
  A-x86.json               one file per path: steps, times, bytes, versions
  A-x86/                   that path's transcript, logs, receipts, records
  C1-local.json
  C1-local/
  ...
  manifest.json            every file with its SHA-256, and each path's pass
  manifest.dsse.json       manifest.json signed with the gate's signing key
```

A path file (`axocoatl.release-evidence.path/1`) holds:

- `pass`, true only when every required step passed;
- `steps`, each with `name`, `status` (`pass`, `fail` or `skip`), `required`,
  `exit_code`, `seconds`, `bytes_downloaded` and a `detail`;
- `host` (OS, version, kernel, architecture, memory, CPUs, user);
- `tools` (Podman, Ollama, git, Node and the `axocoatl --version` it ran);
- `attachments`, the files in its folder: the full transcript (written as it
  happens, so it holds every question and answer), each step's log, the
  install command as run, for each fixture the exact command, its output,
  the receipt, the JUnit and the record bundle, `answers.txt` (each question
  the person answered, the answer, who and when) and `logs/onboarding/`
  (the onboarding logs written during the path: every command onboarding
  ran and its exit status, never a key).

### Signing

The manifest is signed by the product itself: `axocoatl receipt
sign-evidence manifest.json` (a hidden subcommand; it signs only a manifest
whose `schema` is `axocoatl.release-evidence/1`, and only with a key that
already exists). The receipts inside are the product's own receipts, so the
bundle shows the product's signing working on the machine that ran the gate.

The signing key lives in the gate account's own signing data root,
`~/.axocoatl-gate-signer` (`--signer DIR` or `GATE_SIGNER_DIR` to move it),
apart from Axocoatl's data. `reset-mac-account.sh` removes Axocoatl's data
before every run (with the product's own receipt key in it) and never touches
the signer, so the key listed in `.axocoatl/gate-keys` is the one that signs
every release. `collect-evidence.sh` always names that data root itself
(`AXOCOATL_DATA_DIR` and `--config`), so neither the product's data root nor
a developer's `AXOCOATL_DATA_DIR` is ever used to sign.

### Collecting

`scripts/release-gate/collect-evidence.sh` assembles a bundle:

- It copies path evidence from the Tier C and D runs (`--from`) and downloads
  the CI artifacts (`newuser-*`) of a CI run (`--ci-run`). It accepts a run
  only when GitHub says it is a completed `push` run of
  `.github/workflows/ci.yml` in `axocoatl/axocoatl` (`--github-repo`) whose
  head commit is the candidate commit. A fork's pull request controls its own
  run and could write evidence that claims any commit, so it is refused
  before anything is downloaded.
- It refuses evidence of another version or commit.
- It refuses anything that looks like a credential: OpenRouter, Anthropic,
  OpenAI and AWS keys, GitHub tokens (including `github_pat_`), JWTs (the
  ChatGPT sign-in's tokens), private keys, sign-in links with a token, and
  files named like a secret (`receipt-signing-key`, `local-api-token`,
  `id_*`, `*.pem`, anything under `secrets/`).
- It writes the manifest, signs it and decides as the release will
  (`evidence.mjs verify-bundle --required docs/RELEASE_GATE.md`): the bundle
  must verify and every required path must pass; another failing path is a
  warning.
- It always compares each path's wall time and download with the previous
  release's (REGRESSION above 20%) and prints the next steps. It exits 1 only
  when the release would refuse the bundle.

## Tier A: CI, every pull request and every release

`scripts/release-gate/newuser-linux.sh --tarball DIST/axocoatl-vX.Y.Z-<target>.tar.gz`
on a GitHub-hosted runner (ubuntu-24.04 and ubuntu-24.04-arm; job
"New user (Linux)" in `ci.yml`, "Release gate — new user (Linux)" in
`release.yml`):

1. A fresh `HOME` seeded from `/etc/skel`, as `adduser` does, the new user's
   login shell (`/bin/bash`), and a `PATH` without `~/.local/bin` or any
   `axocoatl`.
2. The README's install command, read from `README.md` and run exactly as
   written (`curl -fsSL https://axocoatl.ai/install.sh | sh`), with
   `scripts/install.sh`, which the site serves byte for byte. A `curl` shim
   serves its GitHub requests from the just-built tarball through the real
   curl (`file://` URLs) and refuses any other URL. `GITHUB_PATH` and the
   installer's override variables are unset, as in a new user's shell. The
   gate checks where it installed and that the binary is the tarball's. Then
   new shells must find it: `bash -l` (a new login) and `bash -i` (a new
   terminal window). When the installer could not change a profile itself
   (there is no terminal to ask on) and printed the line to add, the gate adds
   that line, as the user is told to, and checks again. An `export` for the
   current shell alone fails.
3. A scripted model server (`scripts/release-gate/stub-model-server.mjs`):
   an Ollama 0.40.6 on 127.0.0.1 with cloud models off, `qwen3-coder:30b`
   and `gpt-oss:20b` "installed", Axocoatl's two contract probes answered as
   Ollama answers them, and an OpenAI-compatible endpoint. Its writer reads
   each fixture's buggy file and fixes it with `edit_file`
   (`scripts/release-gate/stub-fixes.json`); its reviewer approves.
4. `axocoatl onboard --yes --writer ollama --ollama-url <stub> --writer-model
   ollama:qwen3-coder:30b --reviewer ollama:gpt-oss:20b --no-pull --start none
   --no-smoke-test --port <free>` with no terminal, with `sudo` and the
   package managers replaced by shims that fail and log. It must exit 0, pull
   nothing, and write `run.models` and the port. No `sudo` may run, through
   `PATH` or by any path: on CI the gate also reads the system journal for
   sudo commands run during each guarded step (as the runner user, or through
   the runner's passwordless sudo when only root can read it; the gate's own
   reads are not counted), and a journal it cannot read fails the step
   there.
5. `axocoatl doctor --json` must say `"ready": true` with no `fail` item in
   any section. Plain `axocoatl doctor` must exit 0 and read as a new user
   reads it: `Ready for: … axocoatl run fix` (on the Claude Code and Codex paths, that
   writer's own `fix-claude-code` or `fix-codex`), a last line `Ready: yes`, and no
   `[FAIL]` line.
6. `axocoatl dev` must serve on the configured port: its `/health` names its
   version and a start after the configuration last changed, as onboarding
   reads it.
7. On each fixture in `tests/fixtures/newuser/` (`node-plain`, `node-deps`,
   `python-pytest`, `rust`, `go`), the README's `axocoatl run fix` command
   exactly as written, plus `--junit`, `--record` and `--receipt` when it
   lacks them, must exit 0. `node-plain`, onboarding's first-run sample, has
   another bug than the README's task describes, so it runs the command its
   own README gives (`"command": "fixture-readme"` in `fixtures.json`). Each
   run's plan line must name the image the fixture needs (and the setup,
   for `node-deps` and `python-pytest`). The documented
   `axocoatl receipt verify receipt.json --key "$(axocoatl receipt key)" --repo .`
   and `axocoatl record verify` must exit 0, the receipt must match the
   record, and no accepted output may hold tool-call markup (see "Required
   steps"). One step then says whether the command passed on every fixture.
8. Negative cases:
   - A repository whose check program is absent exits 3 within 15 s with no
     model call.
   - `--model reviewer=anthropic:x` exits 3 with no Session, container or
     model call.
   - Onboarding without Podman on `PATH` prints the install command, exits 1
     and runs no `sudo`.
   - With `--system-changes`, an `/etc/subuid` without the user's range (while
     Podman's pause process may still map the old one) makes onboarding name
     the `usermod --add-subuids` fix without running sudo, and doctor FAIL
     with it. `--system-changes` is
     refused unless `GITHUB_ACTIONS=true`. The case runs only after both
     files are backed up and the copies match. If restoring them fails, the
     backup and the work directory are kept and the gate says where they are.
9. `scripts/check-copy-paste.mjs`, when the tree has it. When `sites/docs` has
   not installed its YAML parser (js-yaml), the gate runs `npm ci` there first,
   as its own step. The lint never passes without it: a failed install fails its
   own step, and the lint then fails every YAML block.
10. Ctrl+C stops `axocoatl dev` with exit 0 (recorded, not required).

Every request the gate makes to a daemon, the scripted model server or
another local server is bounded (`gate_curl`: 2 s to connect, 5 s in all),
so one that accepts the connection and never answers fails its step
instead of hanging the gate. What answers on a daemon's port is decided as
onboarding decides it (`gate_daemon_state`): an Axocoatl `/health` names
its version; a daemon that started before the configuration last changed,
or does not say when it started, runs an older configuration; and a
request that failed is never taken for a running daemon.

The ubuntu-22.04 leg (`--expect-refusal "Podman 3.4.4"`) checks only that
onboarding and doctor refuse cleanly and name Podman 3.4.4.

CI uploads each leg's evidence as the artifact `newuser-<label>`; collect it
with `collect-evidence.sh --ci-run <run id>`. The gate step runs whenever the
archive was built, even when the job's self-checks before it failed, so CI
shows how far the product gets.

Supporting checks, run in CI's workflow-contracts job:

- `node --test scripts/release-gate/*.test.mjs`: the stub, the fixture table,
  the command extraction, the evidence format, the release decision (with
  the required steps, each of which a gate script must record by that
  name), the override list, and the Safari steps' pages.
- `scripts/release-gate/test-release-gate.sh`. It checks:
  - the scripts' safety refusals;
  - the installer step, with an installer whose `/usr/local/bin` is a locked
    temporary directory;
  - what answers on a daemon's port (current, older configuration, before
    1.5.0, not Axocoatl, an HTTP error, a server that never answers, which
    must fail within the bound, nothing at all), and that every request the
    scripts make is bounded;
  - the tool-call markup check, on records shaped as a 1.5.0 run writes
    them (`testdata/record-history-1.5.0.json` is a real run's history): a
    clean run passes, even when its record, receipt, report or a line it
    left alone quotes markup; markup in an accepted answer, in a line it
    added, a new file or a commit fails; a needs-attention run with no
    accepted answer passes; and the transcript shape the daemon's API
    serves, no accepted answer from a run that passed, a cut-off record,
    a missing record and an unknown starting commit fail
    (`record-answers.test.mjs` covers the reader);
  - the operator's answers in `answers.txt`, and the onboarding logs kept;
  - what `newuser-mac.sh` runs: every required C1 path by default, the
    Source Control question (Last turn N of at least 1, and the Safari
    driver decides the same), the flushed transcripts, the driver's usage,
    and the docs' strict verify-receipt workflow;
  - Tier B's Podman stand-in;
  - `collect-evidence.sh`: a key made with `--init-signer`, kept through a
    simulated reset, signs a bundle that `check-evidence.mjs` then passes;
    CI runs that are not the canonical push run are refused, and
    credential-shaped content is refused.

  It also runs `newuser-linux.sh` end to end with `GATE_SIMULATION=1`,
  against a stand-in `axocoatl` (`scripts/release-gate/testdata/fake-axocoatl.mjs`,
  which follows the shared contract) and the real stub. That covers Tier A
  (every step "Required steps" lists for it is recorded), a fixture the
  model cannot fix, a turn that ended on a slipped tool call, and a
  `--live` (D1) run in which onboarding chooses the port, adds the
  subordinate ids the gate removed beside another user's, and the
  Always-On Service runs from Step 6 to `uninstall` (every step D1 requires
  is recorded), so the gate script's own logic is checked on a host without
  Podman, models or a release build. A simulated path always records a
  failed step and can never be evidence.

CI's "Podman suites (ignored tests)" job runs the Podman tests that are
ignored by default against the ubuntu-24.04 runner's rootless Podman 4.9.3:
`session_trust_podman`, and the eleven `node_modules` mount point tests
(`a_repository_without_node_modules_has_none_left_behind`,
`under_the_hardened_workload_the_mount_point_is_the_persons_until_removal`,
`a_development_containers_node_modules_volume_keeps_the_mount_point`,
`a_container_that_bind_mounts_the_mount_point_itself_keeps_it`,
`a_bind_of_the_mount_point_through_a_link_keeps_it`,
`a_bind_volume_of_the_mount_point_keeps_it`,
`an_overlay_volume_of_the_mount_point_keeps_it`,
`a_tmpfs_on_node_modules_through_a_bind_of_the_repository_keeps_it`,
`mounts_taken_with_volumes_from_keep_the_mount_point`,
`an_overlay_of_a_keep_id_container_keeps_it_and_says_why` and
`dependency_mountpoint::tests::podman_names_what_is_gone_and_the_rest_is_inspected`:
plain, hardened keep-id, a development container, a bind of the mount point
itself, a bind through a link, a bind volume, an overlay volume, a tmpfs
through a bind of the repository, mounts taken with `--volumes-from`, a
keep-id container's overlay volume, and Podman's `no such container` and
`no such volume` answers). Each removal they assert runs while an
unrelated container exists, and each keep of the empty mount point they
assert names why, so a broken `podman container inspect` fails them. The
step must report `11 passed` (as many as it names), so that a renamed test
cannot pass by matching nothing. The egress and workload suites need
prepared local images and are not wired yet.

The "New user (Linux)" job also runs `scripts/release-gate/test-fixtures.sh`
with `GATE_REQUIRE_ALL_TOOLCHAINS=1`, which proves each fixture in
`tests/fixtures/newuser/fixtures.json` fails before the stub's fix and passes
after it with the runner's own toolchains; a missing toolchain fails it. Every
fixture that file lists is in the tree. (A fixture listed with an `owner`
that is missing from the tree would be reported PENDING, and the gate would
fail its runs.) The job then runs `test-release-gate.sh` with that job's
release binary.

The gate refuses to install where `/usr/local/bin` is writable and already
holds an `axocoatl`: the published installer would overwrite another
install.

### The contracts Tier A checks

Tier A fails at the first step where one of these does not hold:

- `axocoatl onboard`'s non-interactive flags and exit codes (0 ready, 1 not
  ready with steps listed, 3 usage), and its writing of `run.models` and
  `server.port`;
- `axocoatl doctor --json` (`{"ready", "sections": {"sandbox", "models",
  "writers", "daemon"}}`, items `{"name", "status", "detail", "fix"}`), the
  document alone on standard output (logs may go to standard error), exit 0
  when ready and 1 when not, and plain doctor's `Ready for:` and `Ready:`
  lines;
- run defaults from `run.models`, admission that refuses an absent check
  program and a reviewer that cannot review with exit 3 before any model call
  (and, for the reviewer, before any Session: `axocoatl session list` must
  not change), and the run's plan line `image … · toolchain … · check … ·
  setup …` (printed as `· note: image …` and under `Notes:`) with the image
  chosen by detection;
- the `node-plain` fixture (onboarding's first-run sample; its
  `stub-fixes.json` entry matches onboarding's `src/paging.js`);
- the installer's `add this line to FILE:` or `add this line to FILE and
  FILE:` output (or a profile change), without which a new shell does not
  find the binary on macOS;
- `scripts/check-copy-paste.mjs`.

## Tier B: macOS hosted runner, every pull request (not required)

`scripts/release-gate/newuser-mac-ci.sh --tarball DIST/axocoatl-vX.Y.Z-<mac target>.tar.gz`
on macos-15 (the aarch64 build) and macos-15-intel (the x86_64 build); job
"New user (macOS plumbing)" in `ci.yml`. A hosted runner cannot run a Podman
machine, so this checks the plumbing a new Mac user meets:

1. A brand-new `HOME` and zsh on the runner's own `PATH`, without
   `~/.local/bin`, `axocoatl`, Podman or Ollama.
2. The README's install command with `/usr/local/bin` unwritable (the
   installer's copy names a locked temporary directory), so it installs into
   `~/.local/bin`; a new `zsh -il` must find it, as in Tier A.
3. `axocoatl doctor --json` against `scripts/release-gate/podman-shim.sh`, a
   stand-in that answers `--version`, `info`, `machine list`, `inspect` and
   `info` as Podman does in each state. The sandbox section must say, for
   each Podman state:

   - not installed: FAIL, fix "Run `axocoatl onboard` (it installs Podman
     after asking), or: `brew install podman`" (the Intel build: "Podman 6
     no longer supports Intel Macs", the Podman 5.8 package);
   - not installed, no Homebrew (aarch64): FAIL, fix names the podman.io
     installer, not onboarding installing Podman;
   - no machine: FAIL, fix runs `podman machine init`;
   - machine stopped: OK: it starts on first use; no FAIL;
   - running, rootless: OK: ready; no FAIL;
   - running, rootful: FAIL, fix sets `--rootful=false`;
   - Podman 6.1.4 (numeric memory, libkrun): no version WARN (doctor lists
     6.1 among the macOS releases it does not warn about); WARN naming the
     libkrun provider as untested; no FAIL.

4. `axocoatl onboard --plan` (nothing changes) must show the consent it would
   ask for: `brew install podman`, the podman.io installer when there is no
   Homebrew, `podman machine init`, or `podman machine start`. It must write
   no config.

   Doctor and onboarding also look for Podman and Homebrew where their
   installers put them (`/opt/homebrew/bin`, `/usr/local/bin`,
   `/opt/podman/bin`), whatever `PATH` says, and the runners have Homebrew
   there. The states without Podman or Homebrew therefore run under
   `sandbox-exec` with a profile that denies reading those programs in those
   directories, as on a Mac that never installed them.
5. Neither command may run a podman command that creates or changes a
   machine.
6. A run's admission starts a stopped machine, as doctor's "it starts on
   first use" says: with the stand-in's machine stopped, `axocoatl dev` and
   a run of the README's task on `node-plain` against the scripted model
   server, both unable to see any other Podman (`sandbox-exec`). After the
   daemon has started, the run must call `podman machine start`, which the
   stand-in answers by running the machine. The stand-in builds and starts
   nothing, so the run itself ends at its first container command; only
   its admission is judged here, and Tier C's `recovery` path runs the
   whole thing on a real machine.

Its evidence (`B-arm`, `B-intel`) is reported in the bundle and is not a
required path.

## Tier C: a real Mac, every release, with a person present

Machines:

- **C1**: an Apple-silicon Mac account that starts without Axocoatl, its
  data or stored secrets, without Podman or a Podman machine, and without
  Ollama or its settings, and that owns the Mac's Homebrew, so that
  onboarding can install Podman and Ollama with it. The public text says
  the gate Mac started that way, and the gate records it before the
  install (see "Required steps"). That is a Mac kept for the gate, reset
  with `--dedicated-machine` (recommended), or the founder's own account
  on his 48 GB Mac reset the same way, as 1.5.0's runs did
  (`GATE_ACCOUNT=erick`): that reset uninstalls his Podman (its machines
  go with it) and Ollama and removes his Axocoatl data and sign-ins, so
  back them up and agree it with him first. A second account on a Mac
  whose Homebrew another account owns cannot serve: Homebrew's Podman and
  Ollama are shared by every account, so it never starts without them.
- **C2**: a 16 GB Apple-silicon Mac. Required only when the copy names 16 GB.

Never use a fresh `HOME` in an account that keeps its Podman and Ollama as
evidence: the Homebrew Ollama service reads that account's real
`~/.ollama`, and Homebrew upgrades would change its Podman and Ollama.

Once per gate account (most of it persists):

1. On a Mac kept for the gate, create its admin account (System Settings,
   admin password), which installs Homebrew.
2. In the account, in a checkout of this repository, after the first
   install: `scripts/release-gate/collect-evidence.sh --init-signer` makes the
   gate's signing key in `~/.axocoatl-gate-signer` and prints its line. Add
   that line to `.axocoatl/gate-keys` in a reviewed commit. Resets never touch
   it.
3. `gh auth login`, and a private throwaway repository named
   `axocoatl-gate-…` (the gate force-resets its `main`) with "Allow GitHub
   Actions to create and approve pull requests" on.
4. Fund an OpenRouter account. Nobody types, pastes or exports its key:
   onboarding signs in to OpenRouter in the browser when it asks for the key
   (the sign-in `axocoatl connect openrouter` runs) and stores the key
   OpenRouter issues in Axocoatl's secret store, never in the configuration.
5. For `--workbench-driver`: turn on Safari's Settings > Developer > Allow
   Remote Automation, and run `safaridriver --enable` once (admin password).
6. On the founder's own Mac: stop his `:11435` test server for the duration.
   An `axocoatl` in a shared directory on every account's `PATH`
   (`/usr/local/bin`) would shadow the gate account's own install, so the
   gate records a failure until it is moved aside for the run.

Each release, the reset removes Axocoatl's data, and with it the sign-ins.
The person signs in again in the browser when onboarding asks: Claude
(the sign-in `axocoatl connect claude-code` runs), ChatGPT (`axocoatl
connect codex`) and OpenRouter (onboarding's own browser sign-in, the one
`axocoatl connect openrouter` runs), as a new user does; the gate records
that each secret was not stored before its path and was stored by its
onboarding, and asks the person whether the sign-in happened in the
browser.

```bash
# In the gate account, in a checkout of the release candidate's commit.
# ~/candidate holds axocoatl-vX.Y.Z-aarch64-apple-darwin.tar.gz and its .sha256
# (from CI or release-artifact.sh); ~/candidate/site is the staged marketing site.
# Add --keep-models to keep Ollama's downloaded models.
GATE_ACCOUNT="$(id -un)" scripts/release-gate/reset-mac-account.sh --dedicated-machine
GATE_ACCOUNT="$(id -un)" scripts/release-gate/newuser-mac.sh --release-dir ~/candidate \
  --site ~/candidate/site --pr-repo OWNER/axocoatl-gate-receipts --workbench-driver
scripts/release-gate/collect-evidence.sh --version X.Y.Z --commit SHA \
  --from release-evidence --ci-run RUN_ID
```

`RUN_ID` is the CI run of that commit on `main` (a push run), whose
"New user (Linux)" legs uploaded the Tier A evidence.

- `reset-mac-account.sh`:
  - It runs only as the gate account (`GATE_ACCOUNT`, default `axo-newuser`).
    It never runs as root, and never as an account that owns `/opt/homebrew`
    or `/usr/local/Homebrew` unless `--dedicated-machine`.
  - It stops the account's running `axocoatl dev` or `serve`. It then removes
    the account's Axocoatl data (including `~/.axocoatl`, the daemon's socket
    directory on macOS), logs, launch agents, Podman machines and container
    storage, `~/.local/bin/axocoatl` and `~/.ollama`.
  - With `--keep-models` it keeps Ollama's downloaded models (the next run
    records "models cached") and removes only Ollama's settings,
    `~/.ollama/server.json`, where onboarding turns cloud models off, and
    onboarding's backups of it (`server.json.bak-*`). The next onboarding
    then finds cloud models on, as a new user's Ollama has them, and turns
    them off again. Ollama reads its settings when it starts: when one of the
    account's Ollama servers is running, the plan says to stop it first
    (`--dedicated-machine` stops and uninstalls it).
  - Only with `--dedicated-machine` does it `brew uninstall podman ollama`,
    which the C1 paths need.
  - It never touches `~/.axocoatl-gate-signer`.
  - It shows the plan and asks for the account name first. `--dry-run` only
    shows the plan; `--print-paths` lists the paths a reset removes (with
    `--keep-models`, the paths that reset removes) and changes nothing.
  - Podman or Ollama installed from their own `.pkg` or app need the admin
    password to remove, and are left in place; the next `local` path then
    fails its first steps.
- `newuser-mac.sh`:
  - It refuses any other account and records the whole session with
    `script(1)`, written to the file as it happens (`-t 0`). Each question
    the person answers, and the answer, is printed into the transcript and
    kept in the path's `answers.txt`; the step's detail says who answered.
  - Before the install, it records that the account has no Axocoatl, its
    data or stored secrets, no Podman or Podman machine (none where Homebrew
    or podman.io install it, none on a new login's `PATH`), and no Ollama or
    its settings (no program or app, no `~/.ollama/server.json`, nothing on
    port 11434; downloaded models may stay).
  - It reads the staged site's commands from its `index.html`. It runs the
    install command exactly as written; the installer may ask on the
    terminal whether to add `~/.local/bin` to `~/.zprofile`, and the person
    answers. It checks that the site shows `axocoatl onboard`,
    `axocoatl doctor` and `axocoatl dev`, and runs them.
  - Onboarding runs with `--start none` (except in the `service` path),
    because its last step's default would start `axocoatl dev` in the
    foreground and print the sign-in link into the transcript. The gate
    starts the daemon itself, with its output kept out of the transcript.
  - It runs, in order, the paths `local`, `claude-code`, `codex`,
    `openrouter`, `service`, `workbench`, `workbench-chromium`, `recovery`
    and `real-repo` (C2: `local` and `openrouter`). Run them in one session
    right after the reset: `local` records the account before the install,
    and each sign-in path checks that its secret was not stored yet. The
    person answers every onboarding consent prompt live, and the transcript
    is the evidence that the prompts read correctly. `--yes` answers
    onboarding with flags; it is for trying the gate. It skips the
    "onboarding was answered live in the terminal" and sign-in steps, so
    the paths that onboard (`local`, `claude-code`, `codex`, `openrouter`
    and `service`) cannot pass with it; `workbench`, `recovery` and
    `real-repo` do not onboard and are not changed by it.
  - Every `onboard --update` after the first finds the gate's own
    `axocoatl dev`, which started before onboarding changes the
    configuration, and asks "The daemon on port … started before … changed,
    so the first run would use the old configuration. Stop it now …?".
    Answer yes (Enter): onboarding stops it, and the gate starts it again
    after onboarding (any daemon on the port that runs an older
    configuration and is not the gate's own fails the path).
  - `local`: onboarding installs Podman and Ollama with Homebrew after
    asking, creates the Podman machine, turns Ollama's cloud models off and
    passes its first-run sample; the gate records each from Homebrew's list,
    `podman machine list` and doctor. Then the README's command on every
    fixture.
  - `claude-code`, `codex`, `openrouter`: `onboard --update --writer …`
    signs in in the browser when it asks (Claude, ChatGPT; on the
    `openrouter` path, the person chooses the browser sign-in, the default,
    and onboarding stores the key OpenRouter issues as the secret
    `openrouter`, never in `config.yaml`).
  - `service`: the Always-On Service as a person meets it. The gate stops
    its own `axocoatl dev` (the service takes the port). `onboard --update`
    runs without `--start`: in Step 6 the person chooses "Always on,
    starting at login" and agrees to install it, and `axocoatl service
    status` must then say installed and running, with its launch agent in
    `~/Library/LaunchAgents`. A second `onboard --update` switches the
    writer between the local models and OpenRouter, so the configuration
    changes while the service runs; its Step 7 asks "The Always-On Service
    started before … changed … Restart it now?": answer yes, and the
    service must answer on the port with the new configuration. Then the
    README's command on `node-deps` runs through it, and `axocoatl service
    status`, `stop` (nothing answers on the port after it) and `uninstall`
    (the launch agent is gone) must each do what they say.
  - `recovery`: `podman machine stop`, then the README's command on
    `node-deps` must pass and leave the machine running (the run's
    admission starts it).
  - Each step's wall time and download (interface counters) and the free
    disk at the start and end are recorded. Doctor must end `Ready: yes`,
    as in Tier A.
- The `real-repo` path times a run on each repository in
  `scripts/release-gate/real-repos.json`, each at its pinned commit:
  `expressjs/express` v5.1.0 (a Node app with 27 dependencies, 16 dev
  dependencies and no lockfile) and this repository at the candidate commit
  (Rust). A verdict of 0, 1 or 2 passes; anything else is an infrastructure
  failure. `--real-repo DIR --real-task TEXT` runs another repository.
- The `claude-code` path's `--keep pr` step resets the throwaway repository to
  the `node-deps` fixture with the docs' strict verify-receipt workflow (the
  one that runs only on Keep as PR branches and keeps `require-receipt` at
  its default, `true`), pinned to the candidate commit, and waits for the
  Action on the pull request. A passing check is not enough (a skipped job
  passes too): the Action's own log must say "Verified the receipt of run"
  with the run the hero command just made. The Action installs the
  Axocoatl release its `version` input names, so before the tag exists it
  proves that the candidate's receipts verify with the published verifier.
- The workbench path asks eight questions, from the sign-in link to a
  commit: the link opens the workbench signed in; Open workspace refuses
  the home folder and accepts the repository; a New session (with 5173 in
  Exposed ports) starts; Team & budget is pre-filled and Apply works; one
  turn asking to fix the paging bug in `src/paginate.js` changes the file;
  Source Control shows "Last turn N" (N of at least 1: the writer may also
  add or change a test) listing `src/paginate.js`; a commit from Source
  Control succeeds. The person answers them in Safari, or, with
  `--workbench-driver`, `scripts/release-gate/safari-workbench.mjs` does in
  Safari itself through `safaridriver` (WebDriver): it clicks and types as
  a person would, sends the prompt "Fix the paging bug in
  src/paginate.js.", and each answer is recorded as the driver's, with that
  prompt and what the workbench showed (its log and a screenshot per
  question are kept). The gate checks the commit's author and that Ctrl+C
  stops `axocoatl dev` with exit 0, and never prints or records the
  sign-in link (the driver reads it on standard input).
- Then, in Safari itself, always answered by the person (the cookie and
  website-data steps need Safari's own profile, not WebDriver's). After
  the driver, whose automation window closes when it finishes, the gate
  opens the sign-in link in an ordinary Safari window and names the
  workspace and the Session (from `GET /api/sessions`) to open there. The
  gate writes a small app into the repository; the person starts it in the
  Session's Terminal (`node gate-preview-app.cjs`; it logs each request's
  `Sec-Fetch-Dest`) and opens `http://localhost:5173` in the Session's
  Browser view. The person confirms that the Preview shows there, and that
  Open full preview is disabled with the line saying why. The gate then
  gives the Preview's address (`http://<session>-p5173.localhost:<port>/`)
  to paste into a new tab, a link on another `*.localhost` address that
  redirects to it (`scripts/release-gate/safari-pages.mjs`), and asks for
  a Command-click on the app's link inside the Preview: each must end on
  Axocoatl's refusal page, and the app's log must show no request for a
  page of its own. Last, a page on another `*.localhost` address sets the
  cookie flood the docs describe (51 cookies of 4,000 bytes and then 9
  smaller ones, `Domain=localhost; Path=/api`). Its own server counts the
  planted cookies Safari sends back to it, and the page shows that count:
  the step needs at least one, so the evidence shows the flood was in
  place. When the workbench's sign-in link is on `localhost`, reloading it
  must show "This browser is signed out", and removing localhost in
  Safari > Settings > Privacy > Manage Website Data must sign it in again
  with the link; when the link is on another host, which `Domain=localhost`
  cookies never reach, the workbench must stay signed in. Either way,
  after the removal the same server's count page must show that Safari
  sends none of the planted cookies back.
- Separately, CI's "Product browser regressions" job runs
  every browser regression on WebKit, Safari's engine
  (`AXO_BROWSER_ENGINE=webkit`, see `axocoatl-server/browser-tests/README.md`),
  and in Chromium on x86_64 and aarch64, all against the release build.
  `release.yml`'s browser gate runs Chromium only, on a debug build. In
  every engine a failure that followed a browser crash
  the suite detected is marked `# browser crash:`; nothing is rerun, so it
  still fails the suite (that README, "Browser crashes"). Run on a Mac with
  `AXO_BROWSER_ENGINE=webkit`, Playwright's
  WebKit (measured with 26.4 and 27.2) accepts `Domain=localhost` cookies from
  a top-level page as Safari on macOS is expected to; the workbench path's
  Safari steps above check it in Safari itself on every release. So there
  the daemon refuses a Preview page that would run
  top-level and the workbench keeps every Preview embedded, and
  the "Preview browser contract" journey asserts that Open full preview has
  no address and says why, that the Preview frame has no `allow-popups`, and
  that the Preview's own `window.open` of its address, a `target=_blank` link
  or form, a Command-click its script makes up and a navigation of the
  workbench's tab open nothing. The journey after it asserts there that no
  Markdown link, other link (click, Command-click, middle click, context
  menu, drag), `window.open` or Terminal (OSC 8) link in the workbench opens a
  Preview's address in a tab, and in Chromium that the same routes open it as
  before. Elsewhere the contract journey opens the full Preview, whose script
  tries `Domain=localhost` cookies (one that is not ASCII, a forged
  `axocoatl-token-<port>` and `-signed-in`, and 40 of 1 KB), and asserts the
  browser refuses them. In every engine it asserts that the workbench's own
  token cookie never reaches a Preview origin, and that the workbench still
  opens, connects and reads the API. `preview-cookies.test.mjs` needs Podman and
  `docker.io/library/node:20-slim`. On a developer machine it is skipped
  without that image. CI's product-browser legs and `release.yml`'s browser
  gate pull the image by digest and set `AXOCOATL_E2E_REQUIRE_NODE_IMAGE=1`,
  so there a missing image fails it. Through the real Preview proxy and a real
  Session app it asserts that:
  - in WebKit on a Mac, a Preview address typed into a tab, a redirect from
    another site to one, and a real Command-click on a Preview link inside the
    Preview's frame and inside the Files view's frame each get the daemon's
    refusal page (403, no script, a link to the workbench); the app receives
    none of those requests and no cookie is set. In Chromium the same
    addresses open the app top-level, as before;
  - in every engine, a signed-in browser's page on the legacy
    `/api/sessions/<id>/proxy/<port>` path gets 403 and a line of text without
    the app being asked, while `fetch()` from the workbench and a program
    with the token still reach the app, and the app's `Set-Cookie` is not
    passed on;
  - the app never receives an `axocoatl-token-*` cookie over HTTP or a
    WebSocket upgrade (outside Safari, from Preview pages opened top-level
    with forged `Domain=localhost` cookies set from a top-level page; in
    Safari, from a page on another `*.localhost` address, since the daemon
    refuses the Preview there);
  - an app's `Set-Cookie` with `Domain=localhost` stays host-only;
  - a forged signed-in cookie gives a signed-out browser the retry page at
    most once per navigation;
  - the workbench shows that Preview in its frame, where in WebKit on a Mac
    it offers no Open full preview, the frame has no `allow-popups`, and the
    Preview's `window.open` of its address returns null and opens nothing.

  `signed-out.test.mjs` asserts in every engine that while `/` serves the
  shell and the API answers 401, the workbench reloads once and then shows
  "This browser is signed out": no further automatic reload in the 3 seconds
  it then waits, nor after a click on the message's Reload button, and no
  reload at all without sessionStorage. It does not pin the one-minute window
  the page's code uses (`ONE_MINUTE`). In WebKit on a Mac it sets, from a
  page on another `*.localhost` address opened top-level (Playwright serves
  it), 60 `Path=/api` cookies for `localhost` (51 of
  4,000 bytes, then 9 from 2,048 down to 8 bytes; the large ones alone leave
  room for the token cookie), and asserts:
  - the token cookie stays stored but API requests get 401;
  - the workbench loads twice, then shows the message, and does not reload in
    the next 5 seconds, nor in the 3 seconds after the sign-in link is opened
    again;
  - the same cookies with `Path=/ws` leave the live connection reconnecting.
- Closed in 1.5.0 for Previews in Safari on macOS (measured in Playwright's
  WebKit), and by the same user-agent rule on iPhone and iPad (not measured):
  a Preview page that ran top-level could set a `Domain=localhost` cookie from
  script that reaches the workbench and every other Preview
  ([Safari shares `Domain=localhost` cookies][safari-cookies]).
  The daemon now refuses, in Safari's engine (by user agent), every request for
  a Preview page that would run top-level, whatever led there (a link, a
  redirect, Command-click inside the Preview, the context menu, a typed
  address), and the legacy proxy path refuses every browser page in every
  engine. The macOS WebKit journeys assert the typed, redirected and
  Command-clicked cases, and the workbench path checks them in Safari itself
  on every release; iPhone and iPad are decided the same way by user agent
  and are not measured.
- Known gap: a page Axocoatl does not serve that runs top-level on a
  `*.localhost` address in Safari on macOS can still set such cookies: a
  development server the person runs, a Session's app opened directly at the
  loopback port it is published on (the workbench never shows that port), or
  a service worker a Preview registered while it ran top-level before 1.5.0
  (not measured). From such a page:
  - Such cookies with `Path=/api`, 51 of 4,000 bytes and then smaller ones
    that fill what is left of the about 200 KB Safari sends, sign that browser
    out of the workbench's API until localhost's data is removed in Safari's
    settings. The workbench then says "This browser is signed out" instead of
    reloading without end.
  - With `Path=/` they sign that browser out of the workbench entirely (the
    sign-in page, not the signed-out message) until the cookies are removed
    and that page is closed; signing in again works until it writes them
    again. No journey asserts this case yet.
  - With `Path=/ws` they keep its live connection from opening.

  The macOS WebKit journeys assert the `Path=/api` and `Path=/ws` cases, with
  the cookies set from a page Playwright serves on another `*.localhost`
  address. Any page in Safari can also sign out a browser that signed in
  seconds earlier and has no workbench page open, by writing about 1,000
  cookies, because WebKit drops its whole cookie jar.

Pass: every required path exits 0 and every receipt verifies; no command
copied from the site fails; doctor ends ready; time to first success and the
total download are recorded and compared with the previous release.

Expected cost per release: about 90 to 150 minutes, mostly unattended; the
model download dominates unless models are cached.

## Tier D: Linux and Windows VMs, every release

- **D1, Ubuntu 24.04 (blocking).** A new Lima VM on the gate Mac, or a
  cloud VM, with a new user made by `adduser` for the gate, logged in over
  SSH. A VM where Podman is already installed cannot show onboarding's apt
  install: use a new one each release.
  - The VM must not be plain: `limactl create --plain` turns off Lima's port
    forwarding, and OpenRouter's browser sign-in calls back to a loopback
    port in the VM (`http://127.0.0.1:<port>/callback`, a port the system
    chooses). Without `--plain`, Lima's guest agent forwards every port the
    VM listens on at 127.0.0.1 to the same port on the Mac, so the sign-in
    page opened in the Mac's browser reaches it. Two of 1.5.0's D1 attempts
    (on a1428e69) used a plain VM and timed out waiting for that callback.
    A third (on 05e930f9) forwarded the callback port with `ssh -L` and also
    timed out after 10 minutes; why was never found. So the gate now checks
    the forwarding before onboarding, and onboarding's log records each
    connection its callback receives (both below). On the Mac:

    ```bash
    limactl create --name=axo-d1 --cpus=4 --memory=8 template:ubuntu-24.04
    limactl start axo-d1
    limactl list axo-d1 --format '{{.Config.Plain}}'   # must print false
    limactl shell axo-d1 -- sudo adduser --gecos '' d1gate
    limactl shell axo-d1 -- sudo usermod -aG sudo d1gate
    limactl shell axo-d1 -- sudo install -d -m 700 -o d1gate -g d1gate /home/d1gate/.ssh
    limactl shell axo-d1 -- sudo install -m 600 -o d1gate -g d1gate /dev/stdin \
      /home/d1gate/.ssh/authorized_keys <~/.lima/_config/user.pub
    ```

  - Log in as that user over SSH, not with `su` or `sudo -iu`: only a login
    gives it the systemd user session the Always-On Service needs (the gate
    records whether `systemctl --user` reaches it):

    ```bash
    ssh -t -i ~/.lima/_config/user -o StrictHostKeyChecking=no \
      -p "$(limactl list axo-d1 --format '{{.SSHLocalPort}}')" d1gate@127.0.0.1
    ```

  - Copy the candidate's `axocoatl-vX.Y.Z-aarch64-unknown-linux-gnu.tar.gz`
    and its `.sha256` into `~/candidate` (for example with `scp -P` and the
    same key), check out this repository at the candidate commit, and run:

  ```bash
  scripts/release-gate/newuser-linux.sh --live \
    --tarball ~/candidate/axocoatl-vX.Y.Z-aarch64-unknown-linux-gnu.tar.gz
  ```

  - `--live` uses no stub and runs in the account's own `HOME`. It refuses
    an account that already has Axocoatl state, and it never removes that
    `HOME`. It records that the account has no Axocoatl and no Podman, and,
    in Lima, whether the guest agent forwards ports (a plain VM's is
    missing).
  - Before onboarding, the gate listens on a free loopback port and prints
    an address on it (`http://127.0.0.1:PORT/` and a random path) to open in
    the browser the person will sign in to OpenRouter with, with the
    `ssh -L` line to run first in a plain or cloud VM. The step passes when
    the gate's listener received that request and the person saw the
    page. A sign-in that later never calls back is then not the
    forwarding.
  - It then removes the account's own range of subordinate ids from
    `/etc/subuid` and `/etc/subgid` with `sudo` (after a backup in its work
    directory, put back if onboarding adds none), so that onboarding must
    add one: the collision case, since another user's range must overlap
    100000-165535, the first one onboarding would otherwise take. Check
    `/etc/subuid` in the VM first: when no other user's range overlaps
    100000-165535, give Lima's own user that range (`sudo usermod
    --add-subuids 100000-165535 --add-subgids 100000-165535 LIMA_USER`).
    The gate records whose range it found.
  - Onboarding runs interactively with `--start none`, and the gate starts
    `axocoatl dev` itself on the port onboarding wrote to `server.port`
    (8080 when it is free). Pass `--port N` to choose one.
  - Answer yes when onboarding asks to install Podman with apt and to add a
    range of subordinate ids (it names one that overlaps no other user's),
    each with `sudo`. The gate records that Podman's package is installed,
    and that the account's new range overlaps no other user's.
  - Choose OpenRouter to write and review: a CPU-only VM cannot run a 30B
    model. Onboarding then installs no Ollama, which it does only when a
    local model is chosen; the gate records that.
  - When onboarding asks how to get the OpenRouter key, choose the browser
    sign-in (the default). Do not run `axocoatl connect openrouter` first:
    `--live` refuses an account that already has Axocoatl state, so the
    sign-in happens inside onboarding. Never type, paste or export the key.
    Open the link onboarding prints in the Mac's browser; Lima forwards the
    callback port. When the page cannot reach `127.0.0.1:<port>` (a plain
    or cloud VM), run on the Mac the `ssh -L PORT:127.0.0.1:PORT …` line
    onboarding prints, with the same `-p` and `-i` as the login, and load
    the page again. Onboarding's log (kept in the evidence under
    `logs/onboarding/`) names each connection its callback received; when
    the sign-in fails, the step's detail quotes those lines (`OpenRouter
    sign-in: …`), or says that none reached the callback.
  - Then the README's command, without its `--model` options so that the
    models onboarding configured are used, runs on every fixture, followed
    by the receipt and record checks. `--command TEXT` runs another command
    instead.
  - Then the Always-On Service, a systemd user unit: the gate stops its
    `axocoatl dev`, and `onboard --update` runs without `--start`; in Step
    6 choose "Always on, starting at login" and agree to install it. A
    second `onboard --update --writer-model …` (by default the first of
    `openrouter:z-ai/glm-4.6` and `openrouter:deepseek/deepseek-chat-v3.1`
    that is neither model already configured; `--service-writer-model`
    chooses another) changes the configuration while the service runs. Its
    Step 7 asks to restart the service: answer yes. Then the README's
    command on `node-deps` runs through the service, and `axocoatl service
    status`, `stop` and `uninstall` must each do what they say. The
    evidence is `D1.json`.
- **D2 Fedora 42 with SELinux enforcing, D3 Debian 12, D4 Ubuntu 22.04:**
  each must pass or refuse with the documented message (`--expect-refusal`
  for 22.04). Reported, not required; their "not yet verified" label is
  dropped only when they pass.
- **D5 WSL2** on a Windows 11 VM with nested virtualization: the site's WSL2
  flow end to end. WSL2 stays labelled "not yet verified" until it passes.

## Restating what was verified

The public statements of what was verified live describe the release's
final live runs, never an earlier build. While those runs are outstanding,
each such place holds a pending marker (the string `pendingMarker` names in
`sites/marketing/scripts/validate.mjs`): the CHANGELOG's Verified section and
its other entries, `README.md`, `llms.txt`, `docs/CLAIMS.md`, the marketing
home, install and changelog pages, and the docs' install, onboarding,
security and Fix/QA/Audit pages. Except in `llms.txt`, which has the marker
alone and points to the CHANGELOG, the earlier text is commented out below
each marker as a template. Restating replaces the marker and its commented
template with the final text: the release-bound check looks for the marker
only, so a template left behind would ship in the page source.

Each sentence of the final text says what one of the required steps above
shows (see "Required steps"), and nothing those steps do not show: the
evidence check refuses a bundle without them, so the text cannot outrun
the runs. Say who answered (the person, or `safari-workbench.mjs`), on
which machine and account the runs were made, and which models were used,
as the evidence records them.

`release.yml`'s marketing job runs `./scripts/verify-marketing-gate.sh
source-bound`, whose release-bound validation refuses every file in that list
that still holds a marker, and the build needs that job; a release-tag
marketing deploy runs the same check. CI on pull requests and
`scripts/preflight.sh` run the portable mode, which does not look for
markers, so a green CI does not mean the markers are gone. The restatement
therefore has to land before the tag. Check it before tagging with:

```bash
./scripts/verify-marketing-gate.sh source-bound /tmp/axocoatl-marketing
```

The evidence check accepts a bundle only for the tagged commit, or an
ancestor that differs from it in evidence files alone (see above), so text
changed after the live runs needs the runs again on the new commit. Write
the restated text, and remove the markers, in the candidate commit before
Tier C and D1 run on it, stating what those runs are to show; if a run fails
or shows something else, correct the text and run again on the corrected
commit.

## Keeping the gate honest

- A fixture changes only with its README, `fixtures.json` and its
  `stub-fixes.json` entry; `stub-model-server.test.mjs` and
  `test-fixtures.sh` fail otherwise.
- Adding a required path means a line in the block above; the checker reads
  it from the tagged commit, so a path cannot be dropped without a reviewed
  change to this file. The same holds for a required step, and a step
  renamed in a gate script without its line in "Required steps" fails
  `evidence.test.mjs`.
- A new gate key is a reviewed change to `.axocoatl/gate-keys`. Remove a key
  when its account or machine is retired; old bundles stay verifiable only
  while their key is listed.
- Who may override is a reviewed change to `.axocoatl/gate-override-actors`.

[safari-cookies]: https://docs.axocoatl.ai/operate/security/#safari-shares-domainlocalhost-cookies
