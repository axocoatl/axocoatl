# Release gate: a brand-new user, from zero

No Axocoatl release is published until a machine that has never seen Axocoatl
goes from the published install command to a first passing, signed run on
every default path, following only the published copy. Every live run before
1.5 reused a machine that was already set up: a running Podman machine,
Ollama with models already pulled, a `/usr/local/bin` the founder can write
to, and `~/.local/bin` already on `PATH`. This gate exists so that cannot
happen again.

The gate has four tiers. Tier A runs in CI on every pull request and again in
the release workflow. Tiers C and D are live runs that a person starts on
real machines; their results are committed as a signed evidence bundle.
`release.yml` refuses to publish without that bundle.

## Required paths

`release.yml` reads this list. Each path is one file of evidence in the
bundle, and each must say `pass: true`.

<!-- release-gate:required-paths:start -->
- `A-x86`: Tier A, CI, ubuntu-24.04 (x86_64), `scripts/release-gate/newuser-linux.sh`
- `A-arm`: Tier A, CI, ubuntu-24.04-arm (aarch64), the same script
- `C1-local`: Tier C, the gate Mac, local models through Ollama (`newuser-mac.sh`, path `local`)
- `C1-claude-code`: Tier C, Claude Code as the writer, `--keep pr` and the verify-receipt Action
- `C1-codex`: Tier C, Codex as the writer, billed to the ChatGPT plan
- `C1-openrouter`: Tier C, writer and reviewer on OpenRouter, and Claude Code with an OpenRouter reviewer
- `C1-workbench-webkit`: Tier C, the workbench in Safari (WebKit), from the sign-in link to a commit
- `D1`: Tier D, Ubuntu 24.04 VM, a new user made with `adduser`, OpenRouter models (`newuser-linux.sh --live`)
- `C2-local` (when the public copy names 16 GB): Tier C on a 16 GB Apple-silicon Mac
- `C2-openrouter` (when the public copy names 16 GB): the same Mac, OpenRouter reviewer
<!-- release-gate:required-paths:end -->

The C2 paths are required as soon as `README.md`, `llms.txt`,
`docs/CLAIMS.md`, the marketing site or the docs site name 16 GB anywhere
(the checker prints each `file:line`). Any mention counts, including "not
tested on 16 GB": describe a memory floor without naming 16 GB, or record C2.

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
  but `release-evidence/` and `.axocoatl/gate-keys`. That is how evidence is
  committed for the commit it tested: the tested commit is not changed, only
  evidence (and, the first time, the gate key) is added on top of it, and the
  tag goes on the evidence commit or a later evidence-only commit.
- `manifest.dsse.json` is a DSSE envelope of `manifest.json`'s exact bytes,
  signed by a key in `.axocoatl/gate-keys` (payload type
  `application/vnd.axocoatl.release-evidence+json`, so a receipt can never
  pass for a manifest or the reverse).
- Every file in the bundle is in the signed manifest with its SHA-256, and
  nothing else is in the bundle (no extra files, no symbolic links).
- The manifest is for this version and commit, and every required path above
  has a file whose `pass` is `true`, whose required steps all passed (a
  skipped required step fails the path) and whose version and commit match.

When it refuses, it names every missing or failing path and why, and the job
fails.

### The override (the only way past a failing gate)

A failing gate can be overridden only by a manual run of the release workflow
on the tag, with the phrase typed exactly:

```bash
gh workflow run release.yml --ref v1.5.0 \
  -f new_user_gate_override='publish v1.5.0 without the new-user gate'
```

- The phrase names the version; any other text, or an override on a tag push,
  still fails.
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
  manifest.dsse.json       manifest.json signed with the gate account's receipt key
```

A path file (`axocoatl.release-evidence.path/1`) holds:

- `pass`, true only when every required step passed;
- `steps`, each with `name`, `status` (`pass`, `fail` or `skip`), `required`,
  `exit_code`, `seconds`, `bytes_downloaded` and a `detail`;
- `host` (OS, version, kernel, architecture, memory, CPUs, user);
- `tools` (Podman, Ollama, git, Node and the `axocoatl --version` it ran);
- `attachments`, the files in its folder: the full transcript, each step's
  log, and for each fixture the exact command, its output, the receipt, the
  JUnit and the record bundle.

The manifest is signed with the gate account's Axocoatl receipt key, by the
product itself: `axocoatl receipt sign-evidence manifest.json` (a hidden
subcommand; it signs only a manifest whose `schema` is
`axocoatl.release-evidence/1`, and only with a key that already exists). The
receipts inside are the product's own receipts, so the bundle shows the
product's signing working on the machine that ran the gate.

`scripts/release-gate/collect-evidence.sh` assembles a bundle: it copies path
evidence from the Tier C and D runs (`--from`) and downloads the Tier A
artifacts of a CI run (`--ci-run`), refuses evidence of another version or
commit, refuses anything that looks like a credential, writes the manifest,
signs it, verifies the result and compares each path's wall time and download
with the previous release's (it prints REGRESSION above 20%).

## Tier A: CI, every pull request and every release

`scripts/release-gate/newuser-linux.sh --tarball DIST/axocoatl-vX.Y.Z-<target>.tar.gz`
on a GitHub-hosted runner (ubuntu-24.04 and ubuntu-24.04-arm; job
"New user (Linux)" in `ci.yml`, "Release gate — new user (Linux)" in
`release.yml`):

1. A fresh `HOME` and a `PATH` without `~/.local/bin` or any `axocoatl`.
2. The published installer (`scripts/install.sh`, which the site serves
   byte for byte), piped to `sh` as `curl -fsSL https://axocoatl.ai/install.sh | sh`
   does. A `curl` shim serves its two GitHub requests from the just-built
   tarball through the real curl (`file://` URLs) and refuses any other URL.
   The gate checks where it installed, that the binary is the tarball's, and
   that a new login shell finds it (or that the installer printed the line
   that puts it on `PATH`).
3. A scripted model server (`scripts/release-gate/stub-model-server.mjs`):
   an Ollama 0.40.6 on 127.0.0.1 with cloud models off, `qwen3-coder:30b`
   and `gpt-oss:20b` "installed", Axocoatl's two contract probes answered as
   Ollama answers them, and an OpenAI-compatible endpoint. Its writer reads
   each fixture's buggy file and fixes it with `edit_file`
   (`scripts/release-gate/stub-fixes.json`); its reviewer approves.
4. `axocoatl onboard --yes --writer ollama --ollama-url <stub> --writer-model
   ollama:qwen3-coder:30b --reviewer ollama:gpt-oss:20b --no-pull --start none
   --no-smoke-test --port <free>` with no terminal, `sudo` and package
   managers replaced by shims that fail and log. It must exit 0, run no
   `sudo`, pull nothing, and write `run.models` and the port.
5. `axocoatl doctor --json` must say `"ready": true` with no `fail` item in
   any section, and `axocoatl doctor` must exit 0.
6. `axocoatl dev` must serve on the configured port.
7. On each fixture in `tests/fixtures/newuser/` (`node-plain`, `node-deps`,
   `python-pytest`, `rust`, `go`), the README's `axocoatl run fix` command
   exactly as written, plus `--junit`, `--record` and `--receipt` when it
   lacks them, must exit 0; its start line must name the image the fixture
   needs (and the setup, for `node-deps` and `python-pytest`); the documented
   `axocoatl receipt verify receipt.json --key "$(axocoatl receipt key)" --repo .`
   and `axocoatl record verify` must exit 0, and the receipt must match the
   record.
8. Negative cases: a repository whose check program is absent exits 3 within
   15 s with no model call; `--model reviewer=anthropic:x` exits 3 with no
   Session, container or model call; onboarding without Podman on `PATH`
   prints the install command, exits 1 and runs no `sudo`; with
   `--system-changes` (CI only), an `/etc/subuid` without the user's range
   makes doctor FAIL with the `usermod --add-subuids` fix (the file is
   restored afterwards).
9. `scripts/check-copy-paste.mjs`, when the tree has it.
10. Ctrl+C stops `axocoatl dev` with exit 0 (recorded, not required).

The ubuntu-22.04 leg (`--expect-refusal "Podman 3.4.4"`) checks only that
onboarding and doctor refuse cleanly and name Podman 3.4.4.

CI uploads each leg's evidence as the artifact `newuser-<label>`; collect it
with `collect-evidence.sh --ci-run <run id>`.

Supporting checks, run in CI's workflow-contracts job:
`node --test scripts/release-gate/*.test.mjs` (the stub, the fixture table,
the command extraction, the evidence format and the release decision) and
`scripts/release-gate/test-release-gate.sh`. The latter checks the scripts'
safety refusals, the installer step (with an installer whose
`/usr/local/bin` is a locked temporary directory), and a bundle signed by the
real CLI when one is built; and it runs `newuser-linux.sh` end to end with
`GATE_SIMULATION=1` against a stand-in `axocoatl`
(`scripts/release-gate/testdata/fake-axocoatl.mjs`, which follows the shared
contract) and the real stub, so the gate script's own logic is tested on any
host before the product parts exist. A simulated path always records a
failed step and can never be evidence. The "New user (Linux)" job also runs
`scripts/release-gate/test-fixtures.sh`, which proves each fixture fails
before the stub's fix and passes after it with the runner's own toolchains,
and `test-release-gate.sh` with that job's release binary.

The gate refuses to install where `/usr/local/bin` is writable and already
holds an `axocoatl`: the published installer would overwrite another
install.

### What Tier A depends on

The gate was written against the shared 1.5.0 contract while these were being
built; until they land, Tier A fails at the first step that needs them:

- `axocoatl onboard`'s non-interactive flags and exit codes (0 ready, 1 not
  ready with steps listed, 3 usage), and its writing of `run.models` and
  `server.port`;
- `axocoatl doctor --json` (`{"ready", "sections": {"sandbox", "models",
  "writers", "daemon"}}`, items `{"name", "status", "detail", "fix"}`), the
  document alone on standard output (logs may go to standard error), exit 0
  when ready and 1 when not;
- run defaults from `run.models`, admission that refuses an absent check
  program and a reviewer that cannot review with exit 3 before any model call
  (and, for the reviewer, before any Session: `axocoatl session list` must
  not change), and the run's `image: … · check: … · setup: …` start line
  with the image chosen by detection;
- the `node-plain` fixture (onboarding's first-run sample) and its entry in
  `stub-fixes.json` (a provisional entry is there; the stub's tests fail until
  it matches the real fixture);
- `scripts/check-copy-paste.mjs`.

## Tier B: macOS hosted runner (not built yet)

Planned: install on the stock runner `PATH` with `/usr/local/bin` not
writable, `axocoatl onboard --plan`, and `axocoatl doctor --json` against
Podman shims for each state (not installed, no machine, stopped, rootful,
Podman 6 JSON, Intel). It is not a required path; Tier C covers macOS live.

## Tier C: a real Mac, every release, with a person present

Machines:

- **C1**: a dedicated Apple-silicon Mac (recommended), or a new macOS admin
  account `axo-newuser` on the founder's 48 GB Mac.
- **C2**: a 16 GB Apple-silicon Mac. Required only when the copy names 16 GB.

Never use a fresh `HOME` in the founder's own account as evidence: the
Homebrew Ollama service reads the founder's real `~/.ollama`, and Homebrew
upgrades would change the founder's own Podman and Ollama.

Once per gate account (most of it persists):

1. Create the account (System Settings, admin password).
2. In the account: `axocoatl receipt key` after the first install, and add
   the line it prints to `.axocoatl/gate-keys` in a reviewed commit.
3. Sign in to Claude (`claude setup-token` through `axocoatl connect
   claude-code`) and to ChatGPT (`axocoatl connect codex`) in the browser
   when onboarding asks; fund an OpenRouter key and either connect it in the
   browser or export `OPENROUTER_API_KEY` yourself in the gate Terminal.
4. `gh auth login`, and a private throwaway repository named
   `axocoatl-gate-…` (the gate force-resets its `main`) with "Allow GitHub
   Actions to create and approve pull requests" on.
5. On the founder's Mac (Option B): stop the founder's own Ollama
   (`brew services stop ollama`), Podman machine and `:11435` test server for
   the duration, with the founder's agreement; port 11434 and memory are
   shared. An `axocoatl` in a shared directory on every account's `PATH`
   (`/usr/local/bin`) would shadow the gate account's own install, so the
   gate records a failure until it is moved aside for the run.

Each release:

```bash
# In the gate account, in a checkout of the release candidate's commit.
# ~/candidate holds axocoatl-vX.Y.Z-aarch64-apple-darwin.tar.gz and its .sha256
# (from CI or release-artifact.sh); ~/candidate/site is the staged marketing site.
# Add --keep-models to keep ~/.ollama, --dedicated-machine on a gate-only Mac.
scripts/release-gate/reset-mac-account.sh
scripts/release-gate/newuser-mac.sh --release-dir ~/candidate --site ~/candidate/site \
  --pr-repo OWNER/axocoatl-gate-receipts
scripts/release-gate/collect-evidence.sh --version X.Y.Z --commit SHA \
  --from release-evidence --ci-run RUN_ID
```

`RUN_ID` is the CI run of that commit on `main`, whose "New user (Linux)" legs
uploaded the Tier A evidence.

- `reset-mac-account.sh` runs only as the gate account (`GATE_ACCOUNT`,
  default `axo-newuser`), never as root and never as an account that owns
  `/opt/homebrew` or `/usr/local/Homebrew` unless `--dedicated-machine`. It
  removes the account's Axocoatl data, logs, launch agents, Podman machines
  and container storage, `~/.local/bin/axocoatl` and, unless
  `--keep-models`, `~/.ollama` (the next run records "models cached"). Only
  with `--dedicated-machine` does it `brew uninstall podman ollama`. It shows
  the plan and asks for the account name first; `--dry-run` shows it only.
  Podman or Ollama installed from their own `.pkg` or app need the admin
  password to remove and are left in place.
- `newuser-mac.sh` refuses any other account, records the whole session with
  `script(1)`, installs with the staged site's own command, and runs, in
  order, the paths `local`, `claude-code`, `codex`, `openrouter` and
  `workbench` (C2: `local`, `openrouter`). The person answers every
  onboarding consent prompt live; the transcript is the evidence that the
  prompts read correctly. A `--yes` rerun is allowed for later commits of the
  same release. Optional paths: `workbench-chromium`, `recovery` (`podman
  machine stop`, then a run must start the machine) and `real-repo`
  (`--real-repo DIR --real-task TEXT`, a timed run on a real repository).
  Each step's wall time and download (interface counters) and the free disk
  at the start and end are recorded.
- The `claude-code` path's `--keep pr` step resets the throwaway repository to
  the `node-deps` fixture with the docs' verify-receipt workflow, pinned to
  the candidate commit, and waits for the Action on the pull request. The
  Action installs the Axocoatl release its `version` input names, so before
  the tag exists it proves that the candidate's receipts verify with the
  published verifier.
- The workbench path is answered by the person in Safari (the WebKit engine)
  item by item, from the sign-in link to a commit; the gate checks the
  commit's author and that Ctrl+C stops `axocoatl dev` with exit 0. The
  sign-in link is never printed or recorded.

Pass: every required path exits 0 and every receipt verifies; no command
copied from the site fails; doctor ends ready; time to first success and the
total download are recorded and compared with the previous release.

Expected cost per release: about 90 to 150 minutes, mostly unattended; the
model download dominates unless models are cached.

## Tier D: Linux and Windows VMs, every release

- **D1, Ubuntu 24.04 (blocking).** A Lima VM (`limactl create
  template://ubuntu-24.04`, about 1 GB) or a cloud VM, with a new user made
  by `adduser`. As that user, copy the candidate's
  `axocoatl-vX.Y.Z-<linux target>.tar.gz` and its `.sha256` into the VM, check
  out this repository at the candidate commit, export `OPENROUTER_API_KEY`
  yourself (a CPU-only VM cannot run a 30B model), and run:

  ```bash
  scripts/release-gate/newuser-linux.sh --live --tarball ~/candidate/axocoatl-vX.Y.Z-aarch64-unknown-linux-gnu.tar.gz
  ```

  `--live` uses no stub: onboarding runs interactively (answer yes when it
  asks to install Podman with apt, add a subuid range if one is missing, and
  install Ollama with its official installer and the systemd cloud-off
  drop-in, each with `sudo`; choose OpenRouter models), then the README's task with no `--model` (so the models
  onboarding configured) on every fixture, and the receipt and record checks.
  `--command TEXT` runs another command instead. Its evidence is `D1.json`.
- **D2 Fedora 42 with SELinux enforcing, D3 Debian 12, D4 Ubuntu 22.04:**
  each must pass or refuse with the documented message (`--expect-refusal`
  for 22.04). Reported, not required; their "not yet verified" label is
  dropped only when they pass.
- **D5 WSL2** on a Windows 11 VM with nested virtualization: the site's WSL2
  flow end to end. WSL2 stays labelled "not yet verified" until it passes.

## Keeping the gate honest

- A fixture changes only with its README, `fixtures.json` and its
  `stub-fixes.json` entry; `stub-model-server.test.mjs` and
  `test-fixtures.sh` fail otherwise.
- Adding a required path means a line in the block above; the checker reads
  it from the tagged commit, so a path cannot be dropped without a reviewed
  change to this file.
- A new gate key is a reviewed change to `.axocoatl/gate-keys`. Remove a key
  when its account or machine is retired; old bundles stay verifiable only
  while their key is listed.
