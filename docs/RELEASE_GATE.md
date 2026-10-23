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

A bundle may hold other paths too (the Ubuntu 22.04 refusal leg, Tier B,
optional Tier C paths). They are reported, and a failing one is a warning,
not a refusal.

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
  skipped required step fails the path) and whose version and commit match.
- Every path CI records (Tier A and Tier B) carries `<path>/ci-run.json`,
  which `collect-evidence.sh --ci-run` writes from the GitHub run it
  downloaded the path from: a push run of `.github/workflows/ci.yml` on the
  bundle's commit, in the repository whose code it ran, that completed with
  the conclusion `success`. A path from a red run, or one collected with
  `--from` instead of from its run, fails: CI is green on the commit before a
  tag. `collect-evidence.sh` refuses a run that has not succeeded before it
  downloads anything.

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
  The list ships with comments only, so nobody can override until the
  founder adds his own login in a reviewed commit. The workflow contract
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
- `attachments`, the files in its folder: the full transcript, each step's
  log, the install command as run, and for each fixture the exact command,
  its output, the receipt, the JUnit and the record bundle.

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
6. `axocoatl dev` must serve on the configured port.
7. On each fixture in `tests/fixtures/newuser/` (`node-plain`, `node-deps`,
   `python-pytest`, `rust`, `go`), the README's `axocoatl run fix` command
   exactly as written, plus `--junit`, `--record` and `--receipt` when it
   lacks them, must exit 0. `node-plain`, onboarding's first-run sample, has
   another bug than the README's task describes, so it runs the command its
   own README gives (`"command": "fixture-readme"` in `fixtures.json`). Each
   run's plan line must name the image the fixture needs (and the setup,
   for `node-deps` and `python-pytest`). The documented
   `axocoatl receipt verify receipt.json --key "$(axocoatl receipt key)" --repo .`
   and `axocoatl record verify` must exit 0, and the receipt must match the
   record.
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

The ubuntu-22.04 leg (`--expect-refusal "Podman 3.4.4"`) checks only that
onboarding and doctor refuse cleanly and name Podman 3.4.4.

CI uploads each leg's evidence as the artifact `newuser-<label>`; collect it
with `collect-evidence.sh --ci-run <run id>`. The gate step runs whenever the
archive was built, even when the job's self-checks before it failed, so CI
shows how far the product gets.

Supporting checks, run in CI's workflow-contracts job:

- `node --test scripts/release-gate/*.test.mjs`: the stub, the fixture table,
  the command extraction, the evidence format, the release decision and the
  override list.
- `scripts/release-gate/test-release-gate.sh`. It checks:
  - the scripts' safety refusals;
  - the installer step, with an installer whose `/usr/local/bin` is a locked
    temporary directory;
  - Tier B's Podman stand-in;
  - `collect-evidence.sh`: a key made with `--init-signer`, kept through a
    simulated reset, signs a bundle that `check-evidence.mjs` then passes;
    CI runs that are not the canonical push run are refused, and
    credential-shaped content is refused.

  It also runs `newuser-linux.sh` end to end with `GATE_SIMULATION=1`,
  against a stand-in `axocoatl` (`scripts/release-gate/testdata/fake-axocoatl.mjs`,
  which follows the shared contract) and the real stub. That covers Tier A
  and a `--live` (D1) run in which onboarding chooses the port, so the gate
  script's own logic is checked on a host without Podman, models or a release
  build. A simulated path always records a failed step and can never be
  evidence.

CI's "Podman suites (ignored tests)" job runs the Podman tests that are
ignored by default against the ubuntu-24.04 runner's rootless Podman 4.9.3:
`session_trust_podman`, and the nine `node_modules` mount point tests
(`a_repository_without_node_modules_has_none_left_behind`,
`under_the_hardened_workload_the_mount_point_is_the_persons_until_removal`,
`a_development_containers_node_modules_volume_keeps_the_mount_point`,
`a_container_that_bind_mounts_the_mount_point_itself_keeps_it`,
`a_bind_of_the_mount_point_through_a_link_keeps_it`,
`a_bind_volume_of_the_mount_point_keeps_it`,
`an_overlay_volume_of_the_mount_point_keeps_it`,
`a_tmpfs_on_node_modules_through_a_bind_of_the_repository_keeps_it` and
`dependency_mountpoint::tests::podman_names_what_is_gone_and_the_rest_is_inspected`:
plain, hardened keep-id, a development container, a bind of the mount point
itself, a bind through a link, a bind volume, an overlay volume, a tmpfs
through a bind of the repository, and Podman's `no such container` and `no
such volume` answers). Each removal they assert runs while an unrelated
container exists, and each keep of the empty mount point they assert names
why, so a broken `podman container inspect` fails them. The step must report `9 passed`, so that a
renamed test cannot pass by matching nothing. The egress and workload suites need prepared local images and are
not wired yet.

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
   `info` as Podman does in each state. The sandbox section must say:

   | Podman state | doctor |
   |---|---|
   | not installed | FAIL, fix "Run `axocoatl onboard` (it installs Podman after asking), or: `brew install podman`" (the Intel build: "Podman 6 no longer supports Intel Macs", the Podman 5.8 package) |
   | not installed, no Homebrew (aarch64) | FAIL, fix names the podman.io installer, not onboarding installing Podman |
   | no machine | FAIL, fix runs `podman machine init` |
   | machine stopped | OK: it starts on first use; no FAIL |
   | running, rootless | OK: ready; no FAIL |
   | running, rootful | FAIL, fix sets `--rootful=false` |
   | Podman 6.1.4 (numeric memory, libkrun) | no version WARN (doctor lists 6.1 among the macOS releases it does not warn about); WARN naming the libkrun provider as untested; no FAIL |

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

Its evidence (`B-arm`, `B-intel`) is reported in the bundle and is not a
required path. Not built yet: a run's admission calling `podman machine
start` for a stopped machine (it needs a daemon and a run against the
stand-in).

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
5. On the founder's Mac (Option B): stop the founder's own Ollama
   (`brew services stop ollama`), Podman machine and `:11435` test server for
   the duration, with the founder's agreement; port 11434 and memory are
   shared. An `axocoatl` in a shared directory on every account's `PATH`
   (`/usr/local/bin`) would shadow the gate account's own install, so the
   gate records a failure until it is moved aside for the run.

Each release, the reset removes Axocoatl's data, and with it the sign-ins.
The person signs in again in the browser when onboarding asks: Claude
(`axocoatl connect claude-code`), ChatGPT (`axocoatl connect codex`) and
OpenRouter (onboarding's own browser sign-in, the one `axocoatl connect
openrouter` runs), as a new user does.

```bash
# In the gate account, in a checkout of the release candidate's commit.
# ~/candidate holds axocoatl-vX.Y.Z-aarch64-apple-darwin.tar.gz and its .sha256
# (from CI or release-artifact.sh); ~/candidate/site is the staged marketing site.
# Add --keep-models to keep Ollama's downloaded models, --dedicated-machine on a
# gate-only Mac.
scripts/release-gate/reset-mac-account.sh
scripts/release-gate/newuser-mac.sh --release-dir ~/candidate --site ~/candidate/site \
  --pr-repo OWNER/axocoatl-gate-receipts
scripts/release-gate/collect-evidence.sh --version X.Y.Z --commit SHA \
  --from release-evidence --ci-run RUN_ID
```

`RUN_ID` is the CI run of that commit on `main` (a push run), whose
"New user (Linux)" legs uploaded the Tier A evidence. It must have
succeeded: `collect-evidence.sh` refuses a run whose conclusion is not
`success`.

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
  - Only with `--dedicated-machine` does it `brew uninstall podman ollama`.
  - It never touches `~/.axocoatl-gate-signer`.
  - It shows the plan and asks for the account name first. `--dry-run` only
    shows the plan; `--print-paths` lists the paths a reset removes (with
    `--keep-models`, the paths that reset removes) and changes nothing.
  - Podman or Ollama installed from their own `.pkg` or app need the admin
    password to remove, and are left in place.
- `newuser-mac.sh`:
  - It refuses any other account and records the whole session with
    `script(1)`.
  - It reads the staged site's commands from its `index.html`. It runs the
    install command exactly as written; the installer may ask on the
    terminal whether to add `~/.local/bin` to `~/.zprofile`, and the person
    answers. It checks that the site shows `axocoatl onboard`,
    `axocoatl doctor` and `axocoatl dev`, and runs them.
  - Onboarding runs with `--start none`, because its last step's default
    would start `axocoatl dev` in the foreground and print the sign-in link
    into the transcript. The gate starts the daemon itself, with its output
    kept out of the transcript.
  - It runs, in order, the paths `local`, `claude-code`, `codex`,
    `openrouter` and `workbench` (C2: `local` and `openrouter`). The person
    answers every onboarding consent prompt live, and the transcript is the
    evidence that the prompts read correctly. On the `openrouter` path,
    onboarding asks how to get the key; the person chooses the browser
    sign-in (the default), and onboarding stores the key OpenRouter issues as
    the secret `openrouter`. A `--yes` rerun is allowed for later commits of
    the same release; its OpenRouter path signs in first with `axocoatl
    connect openrouter`, and onboarding then uses that key without asking.
  - Optional paths: `workbench-chromium`, `recovery` (`podman machine stop`,
    then a run must start the machine) and `real-repo`.
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
  the `node-deps` fixture with the docs' verify-receipt workflow, pinned to
  the candidate commit, and waits for the Action on the pull request. The
  Action installs the Axocoatl release its `version` input names, so before
  the tag exists it proves that the candidate's receipts verify with the
  published verifier.
- The workbench path is answered by the person in Safari item by item, from
  the sign-in link to a commit. The gate checks the commit's author and that
  Ctrl+C stops `axocoatl dev` with exit 0, and never prints or records the
  sign-in link. Separately, CI's "Product browser regressions" job runs
  every browser regression on WebKit, Safari's engine
  (`AXO_BROWSER_ENGINE=webkit`, see `axocoatl-server/browser-tests/README.md`),
  and in Chromium on x86_64 and aarch64, all against the release build.
  `release.yml`'s browser gate still runs Chromium only, until the WebKit leg
  has passed in CI. In every engine a failure that followed a browser crash
  the suite detected is marked `# browser crash:`; nothing is rerun, so it
  still fails the suite (that README, "Browser crashes"). Run on a Mac with
  `AXO_BROWSER_ENGINE=webkit`, Playwright's
  WebKit (measured with 26.4 and 27.2) accepts `Domain=localhost` cookies from
  a top-level page as Safari on macOS is expected to; live Safari has not
  confirmed it yet. So there the daemon refuses a Preview page that would run
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
  ([Safari shares `Domain=localhost` cookies](https://docs.axocoatl.ai/operate/security/#safari-shares-domainlocalhost-cookies)).
  The daemon now refuses, in Safari's engine (by user agent), every request for
  a Preview page that would run top-level, whatever led there (a link, a
  redirect, Command-click inside the Preview, the context menu, a typed
  address), and the legacy proxy path refuses every browser page in every
  engine. The macOS WebKit journeys assert the typed, redirected and
  Command-clicked cases; iPhone and iPad are decided the same way by user
  agent and are not measured, nor is Safari itself.
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

- **D1, Ubuntu 24.04 (blocking).**
  - Set up a Lima VM (`limactl create template://ubuntu-24.04`, about 1 GB)
    or a cloud VM, with a new user made by `adduser` for the gate.
  - As that user, copy the candidate's
    `axocoatl-vX.Y.Z-<linux target>.tar.gz` and its `.sha256` into the VM,
    check out this repository at the candidate commit, and run:

  ```bash
  scripts/release-gate/newuser-linux.sh --live --tarball ~/candidate/axocoatl-vX.Y.Z-aarch64-unknown-linux-gnu.tar.gz
  ```

  - `--live` uses no stub and runs in the account's own `HOME`. It refuses
    an account that already has Axocoatl state, and it never removes that
    `HOME`.
  - Onboarding runs interactively with `--start none`, and the gate starts
    `axocoatl dev` itself on the port onboarding wrote to `server.port`
    (8080 when it is free). Pass `--port N` to choose one.
  - Answer yes when onboarding asks to install Podman with apt and to add a
    subuid range if one is missing, each with `sudo`.
  - Choose OpenRouter to write and review: a CPU-only VM cannot run a 30B
    model. Onboarding then installs no Ollama, which it does only when a
    local model is chosen.
  - When onboarding asks how to get the OpenRouter key, choose the browser
    sign-in (the default). Do not run `axocoatl connect openrouter` first:
    `--live` refuses an account that already has Axocoatl state, so the
    sign-in happens inside onboarding. Never type, paste or export the key.
    The sign-in's callback is a loopback port in the VM: open the link
    onboarding prints in a browser that can reach it. Lima forwards a guest's
    loopback ports to the host by default; for a cloud VM, forward the port
    first (`ssh -L PORT:127.0.0.1:PORT`; onboarding prints that command when
    the VM has no `xdg-open`).
  - Then the README's command, without its `--model` options so that the
    models onboarding configured are used, runs on every fixture, followed
    by the receipt and record checks. `--command TEXT` runs another command
    instead. The evidence is `D1.json`.
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
  change to this file.
- A new gate key is a reviewed change to `.axocoatl/gate-keys`. Remove a key
  when its account or machine is retired; old bundles stay verifiable only
  while their key is listed.
- Who may override is a reviewed change to `.axocoatl/gate-override-actors`.
