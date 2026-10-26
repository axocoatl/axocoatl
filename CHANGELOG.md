# Changelog

All notable changes to Axocoatl are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [1.5.0] - 2026-10-22

### Workbench first run
- **The first screen shows readiness.** `GET /api/readiness` runs `axocoatl doctor`'s
  shared checks (Podman, the Ollama server's version and cloud mode, pulled models, an
  OpenRouter key, the external writers' sign-ins) in doctor's shape. The rail pill
  follows it instead of an always-green "daemon healthy", the home surface lists each
  step left with its commands, and Create session stays blocked while Podman is missing
  or has no machine. It replaces the Ollama-only toast that suggested `llama3.2`.
  `axocoatl dev` and `serve` log a warning at start when native Sessions will refuse
  the configured Ollama server; `/health` names the version.
- **The workbench's readiness checks a loaded model's context.** For each pulled Ollama
  model it reads, without loading it, the context Ollama has it loaded with. Below 16,384
  tokens it fails, with how to give Ollama a 32,768-token context.
- **Team & budget comes pre-filled.** A new Session's Agents (and helpers) start from the
  built-in `fix` loadout's budget: 4 activations, 400 invocations, 4,000,000 tokens, $0 on
  a local Ollama model, expiring in 24 hours. Until a team is applied, Send reads **Set
  budget** and opens Team & budget. Preview refuses a token limit a turn cannot use, for
  a kept slot too, and names the numbers ("Lead reserves 36,864 tokens per model call
  (32,768 context + 4,096 output) ... at least 73,728"); an expiry must be within a year;
  Team & budget errors lose their internal "Provider setup failed: Session dispatch:"
  layers.
- **Source Control's Last turn works for native turns**, from the repository captures
  around each activation, and again after a restart; it says when a turn's files cannot
  be attributed.
- **Commits from the workbench are the person's.** Source Control commits as the Git
  identity the host uses for the repository (as Keep as PR does), dated in the host's time
  zone, refuses without one and in a repository that signs its commits, shows Git's own
  message when a commit fails, and says nothing was committed when nothing was staged.
  Reading Git never initializes a repository: a folder that is not one offers
  **Initialize Git here** (`git init` only), and Ways no longer writes an identity into
  the repository.
- **Broad Workspaces are refused.** Open workspace refuses the file system root, the home
  folder, the folders above it and any folder that contains Axocoatl's own directories
  (`workspace_too_broad`, 409) until you type the folder's name, and never opens
  Axocoatl's own directories (`workspace_protected`). The per-user configuration
  directory is hidden from Sessions like the data root. The picker marks Git
  repositories, and **Remove from list…** (`DELETE /api/workspaces/{id}`) forgets a
  Workspace that has no open Sessions, never touching its folder.
- **A failed Create session leaves nothing behind.** Without a usable Podman, Create
  session saves no Session; a Session that never started preparing is closed and deleted
  without Podman, so Ctrl+C exits cleanly; a failed environment change leaves the Session
  as it was instead of "closing".
- **Create session waits for the Podman check.** The New Session picker keeps Create
  session disabled until its readiness read is back (at most 10 seconds, so a hung check
  never blocks it for good), so it cannot be clicked before a missing Podman blocks it.
- **New session works for a Workspace the Session browser has not read yet** (right after
  the workbench opens, or one opened elsewhere since): the lists are read again and the
  picker opens, instead of "Open a Workspace first".

### Changed
- **Native Sessions run on Ollama 0.40.6 and later; Ollama 0.20.6 is refused.** Native
  execution accepted only an Ollama reporting exactly 0.20.6, while Homebrew and
  ollama.com ship 0.40.6 and Ollama's registry refuses some models (gpt-oss, gemma3) to
  0.20.6 clients. The guarantees native execution relies on were audited again in the
  Ollama 0.40.6 source, where every GGUF model runs in llama.cpp's `llama-server`: a
  prompt longer than the context is refused before generation with `truncate: false`, a
  positive `num_predict` stops generation, and `shift: false` never slides the context.
  Native Sessions now accept `0.40.P` with `P` 6 or later (`AUDITED_OLLAMA_VERSION` is
  0.40.6) and later 0.x minor releases through the contract probes (below), and refuse,
  each with what to do, pre-releases and local builds, `0.0.0`, older releases (upgrade
  Ollama), a new major release and any release on a compiled-in denylist (empty). Ollama
  0.20.6 gets its own message: its engine and model digests changed; install 0.40.6 or
  later, then review Team & budget again. A Team reviewed on Ollama 0.20.6 by Axocoatl
  1.4 says "Ollama was upgraded from 0.20.6 to 0.40.8; review Team & budget again", with
  the version that runs now. It says so in the workbench's "not sent" notice, in
  `POST /api/sessions/{id}/execute` and in Team & budget, without Axocoatl's internal
  "Provider setup failed: Session dispatch: Invalid request for ollama:" layers. This was
  checked with the released Axocoatl 1.4.0, the official Ollama 0.20.6 release and the
  Homebrew Ollama 0.40.8. While 0.20.6 still served, a send and execute gave the 0.20.6
  message. After the upgrade they gave the upgrade message. After Team & budget was
  applied again, the contract probes passed and a turn ran.
- **GGUF only, MLX refused.** On Apple silicon, Ollama 0.40 runs some models with its MLX
  runner, whose limit is the model's own maximum context, not `num_ctx`. A model Ollama
  shows as non-GGUF or MLX, or loads (`/api/ps`) with any runner but `llama-server`, is
  refused. Models with a legacy GGUF manifest (Ollama lists their runner as `ggml`, such
  as `qwen3-coder:30b`) run in `llama-server` too and are accepted.
- The context observation loads the model with `shift: false`, as every inference
  request asks: Ollama 0.40 reloads a model whose context-shift setting differs. The
  observation records the Ollama version it saw (schema 2).
- **Newer Ollama minor releases are accepted through the contract probes.** Ollama ships
  a minor release every few weeks and Homebrew and the app serve only the latest, so a
  later 0.x release (0.41 or later) is no longer refused: a model on it serves only after
  the contract probes pass for that model (they run before a Team or run uses a model,
  and again whenever the server's version changes). `axocoatl doctor` and onboarding warn
  that it is newer than the audited 0.40.6, and a run's receipt names it as
  `ollama 0.41.0 (unaudited, contract probes passed)`. A scheduled workflow
  (`ollama-watch`) runs the same probes against each new Ollama release and release
  candidate as they are published.
- **Upgrading from 1.4.** A Team reviewed on Ollama 0.20.6 cannot run on 1.5. Each of its
  Ollama Agents, and each Ollama helper they delegate to, refuses with the upgrade
  message above. Applying the Team again in Team & budget with nothing changed now
  observes them again on the Ollama that runs now. Preview names each one with the reason
  ("this Team was reviewed on Ollama 0.20.6, which native Sessions no longer accept") and
  shows each such Agent as replaced, with a new conversation. Applied unchanged, a Team
  used to keep its 0.20.6 profiles, so every turn and every delegation kept asking for
  the review. Measured with the released 1.4.0 and a release build of 1.5: after the
  re-apply, the 1.4 Session's next turn delegated to its helper and completed. The
  Upgrade page has an "Upgrading from 1.4" procedure: stop, then back up the data root,
  the configuration and the Workspace repositories; install, then Ollama 0.40.6 or later
  (for the Ollama app: download it again, then quit and reopen it); run
  `axocoatl doctor`; then, in each continued Session's Team & budget, select Edit, then
  Preview and Apply, typing nothing. A daemon test starts on a data root written by the
  released Axocoatl 1.4.0 (`tests/fixtures/data-root-1.4.0`), with its Sessions'
  Workspace folder in place. It checks the root's Sessions, History and run, that each
  Agent's and helper's profile refuses to connect with the 0.20.6 message, and the
  re-apply as Team & budget's Edit drafts it.
- **Going back to 1.4 is the backup.** Axocoatl 1.5 records how each model response of a
  native Session ended (`provider_response`, below) and records a change larger than 512
  KiB by its digest. Axocoatl 1.4.0 reads neither, so it does not start on a data root on
  which 1.5 ran a workbench Session turn or an `axocoatl run`, whatever the size of its
  change: after one 1.5 `axocoatl run fix` that created a one-line file, 1.4.0 stopped
  with "could not recover canonical history for Session '…': … unknown variant
  `provider_response` …", and after a change larger than 512 KiB with "invalid checked
  repository identity". No command converts these records. The way back is the backup
  made before upgrading: start 1.5 on the restored copy once, then 1.4.0, which runs only
  on Ollama 0.20.6. Restoring the data root does not change the repositories: each stays
  as 1.5 left it unless its copy from the same backup is restored too. A large file a 1.5
  run left uncommitted is a change 1.4.0 may not capture either, and its runs there end
  needing attention until the file is committed or moved. This was measured with the
  released 1.4.0 and is described in the Upgrade page's "Data written by 1.5".
- **`axocoatl onboard` is the one setup command.** It detects what the computer has,
  asks who writes the code (Claude Code, Codex, a local Ollama model or OpenRouter), sets
  up the rootless Podman sandbox where it can (below), the writer and a different
  reviewer, writes the user configuration, checks it with doctor's code, and can start
  the daemon and run a first fix on a sample repository
  (`tests/fixtures/newuser/node-plain`). It asks once per
  change to the computer, showing the exact command and its size; Enter is yes only for a
  change whose size is shown and that fits, and no for anything that changes an existing
  setup, except the configuration itself: updating or replacing `config.yaml` (shown
  first and backed up) defaults to yes. It ends ready (exit 0) or with a numbered list of
  the steps left (exit 1); it no longer prints "✓ Axocoatl configured" after a failed
  check. Every command it runs is logged under `~/Library/Logs/Axocoatl` (macOS) or
  `~/.local/state/axocoatl` (Linux).
  - Sandbox: installs Podman where Homebrew (on Apple silicon) or a package manager it
    knows is present (apt, dnf, pacman or zypper with `sudo`; Ubuntu 22.04 and older are
    refused, and elsewhere it names the installer to use), creates and starts the
    machine at `podman machine init --cpus 6 --memory 8192 --disk-size 60` clamped to half
    the computer (never rootful; applehv pinned when Podman would use another provider),
    starts a stopped machine, offers to switch a rootful machine (Podman Desktop's
    default) to rootless (or, keeping it, to create a separate rootless `axocoatl` machine
    for `CONTAINER_CONNECTION`) and to grow one under 4 GiB, refuses Podman older than 4.3
    (Ubuntu 22.04's 3.4.4), adds a range of subordinate ids (Fixed, below), reports
    Landlock, and proves the setup with one throwaway keep-id container. It never creates
    a machine where Podman has a connection defined, or when `CONTAINER_HOST` or
    `CONTAINER_CONNECTION` names one. Every `podman machine` command it runs or prints
    names the machine in use (Podman acts on `podman-machine-default` without a name), and
    nothing is changed through `CONTAINER_HOST` or a connection that is not a machine. A
    Podman installed outside `PATH` is a step left, since the daemon looks on `PATH`.
  - Ollama: installs it (`brew install ollama && brew services start ollama`, or Ollama's
    Linux installer pinned to 0.40.6), upgrades one that is too old, restarts a server
    older than its upgraded CLI, never suggests `brew upgrade` for one that is too new,
    turns cloud models off where the running server reads them (`~/.ollama/server.json`,
    merged and backed up, for the brew service; a systemd drop-in for the Linux service,
    which runs as the user `ollama`; the app's setting) and waits until the server says
    so. Models follow memory: none larger than about 0.65 of it is preselected
    (`gpt-oss:120b` was preselected on 48 GB Macs). Installed models that fit and make a
    pair (a gpt-oss model, or one larger than the writer, to review) come first, at any
    memory size; otherwise 96 GB or more gets the measured `gpt-oss:120b` reviewer, 48 to
    95 GB `gpt-oss:20b`, and below 48 GB onboarding asks first (below; with `--yes`, an
    explicit `--writer ollama` there needs the models named; it is never switched to
    OpenRouter). Pulls show their sizes and the free disk they need, check each exit
    status and show Ollama's error; a model too large for this computer's memory is named
    in the pull question and is not pulled on Enter, nor under `--yes` without `--pull`,
    and its step left names `axocoatl onboard --update --writer ollama` (or the Claude
    Code or Codex writer), or `--replace` while an Agent that differs from what
    onboarding writes runs that model, and with `--yes` a pull that does not fit the disk
    is a step left; each model is loaded once to read its context; below 16384 tokens
    onboarding sets `OLLAMA_CONTEXT_LENGTH=32768` for the service (Homebrew's
    `services/ollama.env`, recognised from the real `brew services --help`, or a systemd
    drop-in) after asking. Writing into an existing `server.json`, service environment
    file or drop-in defaults to no.
  - Below 48 GB with no such pair installed, at a terminal, onboarding first offers
    OpenRouter for both models (the default), Claude Code or Codex as the writer, a local
    writer with OpenRouter reviewing, or local models for both. When Ollama is not
    answering at the default address, it asks whether Ollama already runs at another
    address on this computer. On Linux it passes on the installer's note when Ollama
    found no NVIDIA or AMD GPU.
  - Claude Code and Codex: finds the CLI, states the subscription each needs, builds the
    recipe image and signs in through `axocoatl recipe build` and `axocoatl connect`
    after asking, sets up the reviewer alone on Ollama or OpenRouter, and names Codex's
    credential on the run command instead of `auto`: `codex-chatgpt`, or `codex-openai`
    only when the configuration already names it and that key is stored.
  - OpenRouter: separate writer (default `qwen/qwen3-coder-30b-a3b-instruct`, below) and
    reviewer (default `openai/gpt-oss-120b`) models, the same model refused, the writer's
    output limit 4096 (it was 2048 for every Agent), and the key and both models checked
    before anything is written. The key comes from a browser sign-in: at a terminal with
    no key yet, onboarding offers **Sign in to OpenRouter in your browser (recommended)**
    first, which runs `axocoatl connect openrouter`'s OAuth PKCE flow inline (the models
    checked in OpenRouter's catalog, the sign-in page opened and its address printed, the
    browser's return to a loopback port, which Lima forwards to the host unless the VM is
    plain, and `ssh -L` forwards from a plain or cloud VM), stores the key OpenRouter
    issues as the secret `openrouter` without showing it, and writes
    `api_key_secret: openrouter` and `openrouter_billing: credits`, never the key. A key
    connected before with `axocoatl connect openrouter` (the secret the configuration
    names in `api_key_secret`, or `openrouter` when it names no key), when the secret
    store holds it as doctor checks, is used without asking, at a terminal and under
    `--yes`; one OpenRouter refuses is a step left that connects again. An environment
    variable (`--openrouter-key-env VAR`, `OPENROUTER_API_KEY`, or one named at the
    terminal) and a hidden prompt remain the other ways. Under `--yes` with no key,
    nothing is asked and the step left says to run `axocoatl connect openrouter`, then
    the same `axocoatl onboard` command. Without an OpenRouter key nothing is saved, and
    the step left gives the full `axocoatl onboard` command again: the writer
    (`--writer claude-code`, `--writer codex` or a local `--writer-model`), the reviewer
    and, when one was named, `--openrouter-key-env VAR`.
  - The configuration holds `run.models`, the defaults `axocoatl run` uses for
    `writer_model` and `reviewer_model`, a `# Writer:` comment naming who writes (and
    Codex's credential), and the first free port from 8080 in `server.port`. An existing
    configuration is updated (backed up, shown as a diff with every secret hidden,
    keeping its writer, models and OpenRouter key unless flags name others, with the
    Agents onboarding wrote moved to its models: below), replaced (only the new file is
    shown), or left alone and checked.
  - The first run uses the daemon that runs, waiting for a service Step 6 just started,
    and never starts a second one beside it; it works in a new owner-only temporary
    directory. A daemon that started before the configuration changed (`--update` adding
    OpenRouter, say) would run the old one: onboarding asks to restart the Always-On
    Service when it runs, or to stop the other daemon (`--yes` answers yes), waits for
    it to exit and starts one for the run, or lets the service Step 6 started take the
    port. `/health` now names when a daemon started; one that does not say, from before
    1.5.0, counts as older. `--start dev` runs `axocoatl dev` as its own process.
  - `--yes` with `--writer`, `--reviewer`, `--writer-model`, `--ollama-url`, `--podman`,
    `--machine-cpus`, `--machine-memory`, `--pull`, `--recipe-build`, `--connect`,
    `--openrouter-key-env`, `--port`, `--start`, `--smoke-test`, `--allow-sudo`,
    `--update` and `--replace` runs it unattended; `--plan` prints every action, Steps 6
    and 7 and the Podman checks included, and writes nothing. Without a terminal and
    without `--yes` it exits 3 and changes nothing (it used to write an Ollama
    configuration and exit 0). It shows `axocoatl onboard --yes --writer WRITER`, with
    the writers named in words, and points to `axocoatl onboard --help` for the other
    flags; it used to print a synopsis whose `claude-code|codex|…` pasted as a pipeline.
    `--help` says what `--podman create` does on a Mac and on Linux, and that it refuses
    Ubuntu 22.04. `--install-daemon` is now `--start service`.
- **OpenRouter: onboarding's default writer is `qwen/qwen3-coder-30b-a3b-instruct`.** It
  is the model the local default `qwen3-coder:30b` runs, and two OpenRouter providers
  serve it. On 2026-10-23 the previous default, `qwen/qwen3-coder`, had no healthy
  endpoint for a time, and `z-ai/glm-4.6` and `deepseek/deepseek-chat-v3.1` were
  rate-limited (HTTP 429) at their cheapest endpoints. Measured with Axocoatl that day on
  the onboarding sample and the five new-user fixtures: the writer's change passed the
  fixture's tests in all six runs, and no answer held a text tool call. The reviewer
  (`openai/gpt-oss-120b`) approved the Rust run. In the other five runs its endpoint
  answered HTTP 429 (rate-limited upstream) on the call and its retry, so those runs
  ended needing attention; such a call now moves to another endpoint (Fixed, below).
- **`onboard --update` rewrites the Agents onboarding wrote, with the models chosen and
  this release's prompts.** Before, it kept every Agent and changed only `run.models`. A
  new writer or reviewer therefore left Lead, Scout, the Reviewer and the Assistant on
  the old models, which doctor then still checked. An Agent counts as onboarding's when
  its id and fields are as onboarding writes them, its prompt is one this release or
  1.1.0 to 1.4.0 wrote, and it runs the model onboarding gave it. Such an Agent is now
  written as this release writes it, where it is in the file, and the change is shown in
  Step 4's diff. This happens even when only the port changes. An Agent that differs from
  what onboarding writes (one you edited) stays as it is, and so does one of your own.
  Onboarding names the Agents of its team it kept on other models. Doctor's and
  onboarding's fixes say `axocoatl onboard --update`, and say `--replace` only while such
  a kept Agent runs the model. Tested with the configurations 1.4.0 wrote, byte for byte.
- **`axocoatl doctor` says what is ready.** It reports Sandbox, Models, Writers and
  Daemon, each item OK, WARN or FAIL with its fix, then `Ready for: …` or the steps left,
  ending with `Ready: yes` or `Ready: no`; `--json` prints the same report. A missing
  Podman, a missing machine, a rootful runtime, Podman older than 4.3 and missing
  subordinate ids now fail (they were warnings followed by "All required checks passed"),
  rootless is always probed, and a stopped machine is OK because it starts on first use.
  Doctor, and onboarding's own check, warn when Podman on macOS is a release other than
  6.1, the one Homebrew installs ("has not been tested with Axocoatl on macOS yet (6.1
  is)"), and when Podman on Linux is any release other than 4.9, such as Fedora's 5.x or
  Arch's 6.x ("(4.9 is)"); 1.4.0's doctor and onboarding named no Podman release as
  tested. Each configured Ollama model shows its size against memory and the context
  Ollama loads it with (below 16384 fails); while a daemon runs, a model Ollama already
  holds has its context read from `/api/ps` and is not loaded or probed again, since a
  run may be using it; the Writers section shows built recipe images, stored credential
  names and the `claude`, `codex` and `gh` CLIs, and fails the Claude Code or Codex
  writer a setup chose while its image or sign-in is missing; the Daemon section compares
  the running daemon's version with the CLI's and names the program the Always-On Service
  runs. A daemon of another version, or one that closes the connection on the IPC
  protocol's new version request (`{"type": "version"}`), as a daemon from before 1.5.0
  still running after an upgrade does, is a warning with the restart
  (`axocoatl service restart`, or stop `axocoatl dev` and start it again), where doctor
  said "running (IPC reachable)". Each loaded model shows how much of it is in GPU
  memory. With `sandbox.backend: e2b`, `axocoatl run` still needs Podman. The Rust
  toolchain warning is gone, and a missing configuration points to `axocoatl onboard`. A
  writer path that is not set up and was not chosen is marked with a neutral `·`, and an
  optional writer that is only partly set up (its image without its sign-in, say) with
  `!`, in doctor's summary and onboarding's Step 5, instead of a failure. The fix for a
  configured Ollama model that is too large for this computer, or whose context cannot be
  read, follows the model's role. It names the writer the configuration records
  (`--writer …`), so onboarding neither asks again who writes nor switches it. For a
  reviewer, the fix is OpenRouter for the reviewer
  (`--reviewer openrouter:openai/gpt-oss-120b`) or, unless OpenRouter writes, a smaller
  reviewer. The command is `axocoatl onboard --update`, which moves the Agents onboarding
  wrote to the new model, or, while an Agent that differs from what onboarding writes
  runs the model, `axocoatl onboard --replace` with a smaller model or OpenRouter (with
  an OpenRouter writer, OpenRouter only, keeping the writer's model), since `--update`
  keeps such an Agent as it is. When only a local reviewer's context is too small, the
  fix keeps the local writer. For a configuration that does not say who writes, it says
  to change the model in the file or to write a new per-user configuration (onboarding
  has no `--config`) with `axocoatl onboard --replace --writer WRITER`, with the writers
  named in words (a `claude-code|codex|…` list pasted into a shell is a pipeline that
  replaces the configuration). The default configuration named another way (a relative
  path, a link) counts as the default one, and a missing configuration given with
  `--config` gets a `cp` command that copies the default one there. Without
  `providers.openrouter`, the fix is `axocoatl connect openrouter`, which adds it and
  changes nothing else, and a key that sign-in stored (`api_key_secret`) counts as
  present. That fix, and the one for a key secret that is not stored, add `--config PATH`
  when doctor checks a configuration other than the default one, so the key is stored for
  that configuration. A Claude Code or Codex writer that is not set up names `axocoatl
  recipe build claude-code && axocoatl connect claude-code` or `axocoatl connect codex`,
  which leave the configuration as it is.
- **One Podman preflight and machine size everywhere.** Onboarding, doctor, the daemon's
  start and Session cleanup, and the workbench's readiness report decide about Podman
  the same way: they ask Podman itself first (`podman info`), then why it did not
  answer. `recipe build` and `connect codex` start a
  stopped machine (by its name, or the one `CONTAINER_CONNECTION` names) and name a
  missing Podman or machine with its fix, instead of `os error 2` or `podman build failed
  (exit status: 125)`, and a failed image check says what Podman printed; every hint that
  says to create a machine prints the sized command (`podman machine init --cpus …
  --memory … --disk-size 60 && podman machine start`) instead of Podman's 2 GiB default.
  What to do about a missing Podman is one text, chosen per computer, in the readiness
  checklist, the New Session picker, `axocoatl doctor`, the "podman is required … not
  installed" refusal and the Podman status line: on Apple silicon with Homebrew "Run
  `axocoatl onboard` (it installs Podman after asking), or: `brew install podman`"; on
  Apple silicon without Homebrew, the podman.io installer; on an Intel Mac, the Podman
  5.8 package; on Linux, the distribution's command (`sudo apt-get update && sudo apt-get
  install -y podman`, dnf, pacman, zypper); on Ubuntu 22.04 and older, that its Podman
  is too old. 1.4.0
  printed `brew install podman && podman machine init && podman machine start` on every
  Mac.
- **The daemon follows `CONTAINER_CONNECTION` and `CONTAINER_HOST` as Podman does.** On
  macOS, 1.4.0's daemon read `podman machine list` first: with no machine listed it
  judged the machine missing ("podman is installed but has no VM"), and with none
  running it judged the machine stopped, without asking the Podman those variables
  select (it ran `podman info` only when a listed machine was running). So with Podman
  reached through `CONTAINER_HOST` it started
  as if Podman were not ready and skipped removing its own leftover containers. The
  daemon's start and its Session cleanup now use the preflight above, which, like
  onboarding and doctor, reads `CONTAINER_CONNECTION` before `CONTAINER_HOST`, as Podman
  does (an empty value counts as unset when choosing between them). On Linux a variable
  that is set but empty counts as set, as it does for Podman: Podman then runs as a
  client of its default connection, or of the user's API socket
  (`/run/user/<uid>/podman/podman.sock`) when it has none, instead of the local engine,
  and onboarding, doctor and the daemon say `CONTAINER_HOST= is set, and Podman does not
  answer through it: …` when it does not answer, and treat that Podman as reached
  elsewhere, as for any `CONTAINER_HOST`. On macOS an empty value counts as unset, as it
  does for Podman there. When the Podman a variable selects does not answer, the daemon's
  start fails whenever the data directory holds the record of any Podman Session, closed
  ones and ones it cannot load included, because it cannot then show that their
  containers are gone. It names the variable (`early bootstrap could not determine
  whether deterministic Podman Session containers are absent: CONTAINER_HOST=… is set,
  and Podman does not answer through it: …`): unset the variable, or start the Podman it
  selects. With no such record it starts. A stopped machine that
  `CONTAINER_CONNECTION` names starts on first use, like the default one.
- **`axocoatl dev` and `serve` hold their port before they start.** A taken port fails at
  once with which port and how to choose another, instead of "Axocoatl is running"
  followed by "Address already in use". `GET /health` also returns the daemon's version.
- The README, the site and the docs use `gpt-oss:20b` as the reviewer in their commands
  and configuration examples; `gpt-oss:120b` (65 GB) needs roughly 96 GB of memory or
  more. The measured results still name the gpt-oss 120B reviewer they used.
- **The installer keeps `axocoatl` on PATH for new terminals.** On a stock Mac
  `/usr/local/bin` is not writable, the installer used `~/.local/bin`, which macOS does
  not put on PATH, and the `axocoatl onboard` it printed next failed with "command not
  found". When the install directory is not on PATH, the installer now asks on the
  terminal before adding one marked line to the startup files your shell reads in a new
  terminal and a new login (`~/.zprofile` for zsh on macOS, `~/.zshrc` and `~/.zprofile`
  on Linux; for bash the login file that exists, else `~/.bash_profile` on macOS and
  `~/.profile` on Linux, and on Linux `~/.bashrc` too; `fish_add_path --path` in fish's
  `config.fish`, under `$XDG_CONFIG_HOME` when that is set, which changes only `PATH` and
  saves nothing in fish's universal `fish_user_paths`; `~/.profile` otherwise), and
  prints that line and those files when it cannot ask. The line adds the directory only
  when PATH does not have it. A file gets none when it has the installer's marked line
  (its line right after the comment `# Added by the Axocoatl installer`, which it writes
  with it) or its guarded `case` line, wherever it is in the file, so installing again
  never adds a second one; a marked line you moved under a condition still counts. It
  also gets none when it has, where the installer can tell nothing before it keeps it
  from running, the `export PATH="…/.local/bin:$PATH"` line that 1.4.0 and earlier
  printed for you to add, fish's `fish_add_path` line, or the lines Debian's and
  Ubuntu's `~/.profile` use to add `~/.local/bin`: at the top level, not in a block or a
  function, not after a line that ends in `&&`, `||`, `|` or `\`, not inside a quote, and
  not after `return`, `exit` or an `exec` that runs a command (also in quotes, as
  `'return'` or `"exit"`) or a construct the installer cannot follow. It cannot see
  `set -e` and a failing command, a sourced file that exits, a syntax error, or a later
  line that takes the directory out of PATH again, and it can misread a valid file in
  other ways, so in such a file a line can count although it does not run. A
  subdirectory, a line that removes the directory, a mention in a comment, or such an
  unmarked line behind an OS check do not count, so such a file gets the line too. Next
  to such a `~/.profile`, read directly or by a `~/.bash_profile` that sources it by the
  same rule (`. ~/.profile`, `. "$HOME"/.profile`, its full path, or behind
  `[ -f … ] &&` or an `if [ -f … ]` that ends with `fi` alone on its line or followed
  only by redirections, as in `fi >/dev/null`), only `~/.bashrc` gets a line, one that
  skips login shells. With Debian's and Ubuntu's own startup files, read directly or
  through such a `~/.bash_profile`, a new login, an SSH login (`bash -il`) and a new
  terminal each list `~/.local/bin` once. The line goes at the end of the file; when the
  file can stop before its end in some shells, the installer says so and to move the
  line above that point, instead of saying new terminals find axocoatl: in
  `config.fish`, `status is-login || return` or another return, exit or exec (also after
  assignments); in an sh, bash or zsh file, a `return`, `exit` or `exec` where a command
  starts (also after `builtin`, `command`, `command -p` or assignments, as in
  `SHELL=/usr/bin/fish exec fish`), outside a function or a `( )` subshell, other than
  one that stops only shells that are not interactive (as Ubuntu's, Debian's, Arch's and
  Gentoo's `~/.bashrc` do, also as `return 0` or a one-line `if`) and other than an
  `exec` with only redirections (`exec 2>>log`, `exec {fd}>file`); or a line the
  installer cannot follow. A quoted `#` is not taken for a comment, and `<( )` and
  `>( )` are read as commands, as `$( )` is. `AXOCOATL_MODIFY_PATH=yes` adds it without
  asking and `=no` never asks; end of input at the question (Ctrl-D) counts as no. In a
  GitHub Actions step it adds the directory to `$GITHUB_PATH` instead. The next commands
  are printed with the binary's full path unless the shell already finds it, and an
  earlier `axocoatl` on PATH is named. Without a `podman` command on PATH, the installer
  says what onboarding does about it, on every platform, instead of an `apt-get` line on
  WSL alone: it installs Podman after asking, with Homebrew on an Apple-silicon Mac or
  with apt, dnf, pacman or zypper on Linux; on an Apple-silicon Mac without Homebrew it
  points to the podman.io installer; elsewhere it says what to do. (On a Mac where
  Podman is installed in `/opt/podman/bin`, `/opt/homebrew/bin` or `/usr/local/bin` but
  that directory is not on PATH, onboarding installs nothing and says to add it to
  PATH.) Its last lines say onboarding sets up a writer and a reviewer, and Podman where
  it can.
- **The installer can install a given release from a given place.**
  `AXOCOATL_VERSION=v1.5.0` installs that release instead of the latest one, and
  `AXOCOATL_INSTALL_BASE_URL` (`https://` or `file://`, with `AXOCOATL_VERSION`) names the
  directory holding its archive and `.sha256` file, which is still required; both are
  checked before any download.
- **The installer says what it replaces.** It names the Axocoatl it replaces at its
  install path, including a pre-release such as 1.5.0-rc.1.
  - Installing a newer minor or major release, it says to back up before Axocoatl starts
    again, since the way back is that backup.
  - Installing an older one, it says that this is going back and where the docs say how.
  - It names each running `axocoatl serve` or `axocoatl dev`, run by name or by an
    absolute or relative path, one with a space included. It reads where the path ends
    from ps's argv[0] on macOS and from /proc on Linux, so another program or another
    axocoatl command that names one in its arguments is not taken for a daemon. A daemon
    of the replaced copy runs the new one after a restart; a daemon of another copy, such
    as a build from source, is not changed by the install. For a daemon started by a
    relative path, which copy it runs depends on the folder it was started in, and the
    note says so and how to run the new release.
- The Upgrade page's release installer is `curl -fsSL https://axocoatl.ai/install.sh |
  sh`, as on the install page. The page ran `scripts/install.sh` from the repository's
  `main` branch, whose code may not be released yet. The site serves `scripts/install.sh`
  from the release tag of the CLI version it deploys (`vX.Y.Z`, fetched from the
  repository by `scripts/served-installer.sh`), never the deploying commit's. An
  installer change merged to `main` is therefore served only once a release ships it,
  and a change to `scripts/install.sh` alone no longer deploys the site. A version with
  no tag yet, or a tagged installer that does not parse, is refused. The /install page
  links that served installer.
- **Commands on the home page, /install, the README, `llms.txt` and the docs' first-run
  pages that did not run as written are fixed, and a lint checks their shape.** The
  receipt check is
  `axocoatl receipt verify receipt.json --key "$(axocoatl receipt key)" --repo .` (the
  `--keys trusted-keys` it showed named a file no step created, exit 3); the local-models
  command is one copyable line (its `<br>` made a paste run without the reviewer, then
  `--model: command not found`); the home page's configuration snippet passes
  `axocoatl validate`; and the quickstarts name the loadout that matches the writer chosen
  in onboarding (`fix` for a local or OpenRouter writer, `fix-claude-code` or `fix-codex`
  otherwise).
- **Printed commands name values in capitals.** Many commands Axocoatl printed used
  `<name>`-style placeholders, which a shell reads as redirections when pasted. Examples:
  `axocoatl secret set NAME`, `axocoatl url --config CONFIG`,
  `wsl --set-version DISTRO 2`, `podman volume rm NAME`, `axocoatl recipe build RECIPE`,
  `--model writer=openrouter:VENDOR/MODEL`, `--check "COMMAND"`, `--setup "COMMAND"`,
  `--toolchain LANGUAGE`, and the workbench's `axocoatl receipt verify FILE --keys
  TRUSTED_KEYS --repo CHECKOUT` and `axocoatl loadouts validate FILE`. The paste-safety
  tests now read the CLI's, the server's and the crates' sources and every string in the
  workbench's scripts. They fail on an angle-bracket placeholder in a printed command or
  flag value, including a quoted one of several words.
- **The public pages say what the release gate requires, what is not yet verified and
  what a run needs.** The README, `llms.txt`, the install pages, the security page and
  the marketing home and changelog pages list the paths and steps the 1.5.0 release gate
  requires on the release commit (see Verified, below). They name no memory, models or
  Linux release as checked, since the gate's steps check none of these; its evidence
  records them, and `docs/CLAIMS.md` says so. Intel Macs, Ubuntu 22.04, Debian 12,
  Fedora with SELinux and WSL2 are labelled "not yet verified", the pages say that the
  gate's runs verify local models only on the memory and models its evidence names, and
  the install command says it runs in macOS, Linux or WSL2, not PowerShell. The pages
  name the prerequisites (a Claude Pro or Max plan and the `claude` CLI for Claude Code,
  a reviewer model, Podman), the memory from which onboarding offers the local pair when
  no installed pair fits (48 GB; with less, it first offers OpenRouter for the reviewer
  or both, or Claude Code or Codex as the writer), what gets downloaded (including the
  87 MB embedding model the first daemon start fetches from huggingface.co), how a run
  detects the repository's language and installs its dependencies before the writer
  starts (for `fix-claude-code` and `fix-codex` too), and that Docker is not supported
  (Podman can run alongside it).
- **Claims withdrawn from the site and docs.** The default reviewer, `gpt-oss:20b`, is
  "a different reviewer", not a stronger one: it is smaller than its writer and has not
  been measured. The example run summary on the README and the home page, which 1.4.0
  labelled as an example (a `pages.py` fix whose review asked for changes once), is
  replaced by another example in 1.5.0's format: `axocoatl run fix` on a small Go
  repository, `qwen3-coder:30b` writing and `gpt-oss:20b` reviewing. The pages label it
  as an example of the format `axocoatl run fix` prints, not a measurement. The why page
  no longer says the executable keeps working if the project stops: a new major Ollama
  release needs a newer Axocoatl. `axocoatl run` is "from your shell", not "a shell or a
  CI job", and only `axocoatl run` gets a receipt, not every run. The Codex API-key route
  (`connect codex --api-key`) is out of the quickstart, and 1.5.0 refuses it (below).
- **The verify-receipt workflow in the docs no longer fails every pull request without a
  receipt.** Its example sets `require-receipt: false`, so pull requests from people,
  Dependabot and the one that adds the workflow pass with a notice; a strict variant
  limited to Keep as PR's `axocoatl/` branches is shown separately, as a whole workflow
  file, with a warning. The keys file is created with `mkdir -p .axocoatl` first and
  merged before the workflow.
- **docs.axocoatl.ai pages show `verify-receipt@v1.4.0` as written.** Cloudflare's email
  obfuscation turned it, and `e2e@0.18.0`, into "[email protected]" on the served page.
  Every built docs page now wraps its body in `<!--email_off-->`, which Cloudflare leaves
  alone, and the docs gate refuses a page without it.
- **The fix loadouts' writer cleans up before its final answer.** The writer instructions
  of `fix`, `fix-claude-code` and `fix-codex` now also say to leave no file created only
  for testing or debugging (scratch scripts, test binaries such as `go test -c` output,
  build outputs, reports), to delete them, and to run the project's check command one
  last time before the final answer. On a clean Mac a Go fix run wrote a correct fix but
  left a `test_fix.go` of package `main` beside the package, so `go test ./...` failed
  with "found packages pages and main" and the run ended `checks_failed`. The three
  loadouts' digests change with their text.
- **`axocoatl connect codex --api-key` and `--api-key-only` are refused.** Storing the
  OpenAI API key that OpenAI issues with the browser sign-in was never tried with an
  OpenAI Platform organization. 1.5.0 refuses both flags before anything runs (exit 3,
  nothing run or stored; both are hidden from the help) and points to
  `axocoatl connect codex` and `--param credential=codex-chatgpt`. Onboarding no longer
  asks how to bill Codex: runs bill to the ChatGPT plan. A configuration that names
  `codex-openai` keeps it only while that key is stored; otherwise it moves to
  `codex-chatgpt` and says so. Doctor's fix for a missing `codex-openai` is
  `axocoatl onboard --update --writer codex`. The daemon's and `secret set`'s messages
  for a missing or rejected `codex-openai` lead with the ChatGPT sign-in, then
  `axocoatl secret set codex-openai --from-env VAR` for automation. A key stored as
  `codex-openai` by 1.3.1 to 1.4.x is still used.
- **A Codex run says when it is billed at API rates.** When `credential: auto` takes
  `codex-openai` while `codex-chatgpt` is also stored, the run's note adds two things:
  the run is billed at OpenAI API rates to the key's organization, not to your ChatGPT
  plan, and `--param credential=codex-chatgpt` uses the plan.
- **While a version's films are pending, the site leaves out the showcase page.** The
  page holds nothing but films and showed twelve "recording pending" notes. The
  marketing validation refuses a pending build that has it, and the page returns with
  the recordings.
- The docs say that Keep as PR and the verify-receipt Action are for GitHub only (only
  github.com has been tried), and what to do on another host: `--keep branch`, then
  verify the receipt `--receipt` wrote, since Keep writes no description there. They
  also say that a receipt block over 40,000 characters stays out of the pull request
  description.
- The install page has a section on proxies, VPNs and rate limits.
- When the installer cannot resolve the latest release, its error now names GitHub's
  limit of 60 requests an hour without a token and `AXOCOATL_VERSION`.
- **A refusal is a 422 with its reason.** `POST /api/sessions/{id}/execute` and
  `POST /api/agents/{id}/execute` answer a native provider check's refusal with 422 and
  the check's own words. Before, they answered 500 with Axocoatl's internal error layers.
  They also answer an invalid request with 422; conflicts stay 409 and other failures
  500. Team & budget's preview and apply answer a provider refusal with 422 (they
  answered 400). The workbench's "not sent" notice shows the same words.
- **The daemon logs each HTTP request at debug.** Before, its default log had one line
  per workbench poll. A server error (5xx) is still logged at `warn`.
  `RUST_LOG=info,axocoatl_server=debug` shows the requests, with any sign-in token as
  `REDACTED`.
- Browser journeys now cover Full-page Preview cookies with WebKit's own Linux user
  agent, Source Control's change in Monaco's diff editor, and the skill-automation and
  Remove from list journeys without their readiness races; every WebKit page on Linux
  settles before it reloads or navigates.
- **CI runs every test that needs Podman, the internet or a live Ollama.** CI used to run
  12 of the tests the workspace ignores by default. Now, on every push and pull request:
  - four "Podman and live suites" jobs run every ignored test of axocoatl-isolation (with
    sandbox-session), axocoatl-daemon, axocoatl-cli, and axocoatl-memory and
    axocoatl-session, on rootless Podman 4.9.3, with images pinned by digest and the
    recipe and browser images built from the commit;
  - the execution supervisor suite runs in Alpine and Debian images, as root and not, on
    x86_64 and aarch64;
  - the live Ollama tests run against Ollama 0.40.6 with cloud models off and qwen3:0.6b.

  A suite fails unless every test ran and passed, and CI fails when an ignored test is in
  no suite. Ten tests are left out, each with its reason: four live E2B, two live
  OpenRouter, four helpers. The daemon's live egress test now also covers the ubuntu and
  github presets.
- **The browser regressions run in WebKit on macOS, in CI and in the release workflow.**
  There is a new macOS 26 WebKit leg; without Podman it leaves out the five Podman
  journey files. `release.yml`'s browser gate, which ran Chromium on a debug build, now
  runs CI's four legs on the release build, and a failing leg blocks the release. The
  Linux legs pull `rust:bookworm`, so the coordination reference journey runs. A new
  required gate path, `C1-browser-webkit`, runs every journey in WebKit on the gate Mac
  with Podman against the candidate's macOS archive.
- **A release takes its Tier A evidence only from a green CI run.** `collect-evidence.sh`
  refuses a run whose conclusion is not success, and records the run in each path
  (`ci-run.json`, which is signed). `check-evidence.mjs` and `release.yml` fail a Tier A
  path without that record or from a red run.
- **Release archives carry a build provenance attestation.** Each of the four archives is
  attested and checked with `gh attestation verify`. The install page explains how to
  check one, and says the macOS binaries are signed ad hoc and not notarized, so a
  browser-downloaded archive is quarantined, while the installer's curl download is not.
- **The new-user gate installs real dependencies through every package manager the setup
  supports.** The rust and go fixtures have dependencies (`cargo fetch`,
  `go mod download`). Seven new Tier A fixtures cover pnpm 10, Yarn 1, Yarn 4, Poetry,
  uv, PDM and Hatch. `config.mdx` no longer calls the yarn, pypi, go, github and ubuntu
  presets unverified.
- **The workflows pin the Node 24 releases of their GitHub Actions** (checkout 5.1.0,
  setup-node 6.5.0, setup-go 6.5.0, upload-artifact 7.0.2, download-artifact 8.0.2,
  wrangler-action 4.1.3 with Wrangler 3.90.0 kept). So do the receipts page's examples.
- CI pulls Docker Hub images directly from their origin, preserving digest pins and
  avoiding incomplete cached images.
- The macOS plumbing CI gate's admission step uses a short `HOME` under `/tmp`, so the
  daemon's socket path fits macOS's 104-byte limit.
- **The tests pass under Ubuntu's login umask, whatever Podman connection the shell
  names, and on a loaded computer.** About 730 Rust tests and every browser journey
  failed under umask 002. The Podman tests no longer read `CONTAINER_HOST` or
  `CONTAINER_CONNECTION`. Failed mount point, egress and workload fixtures leave nothing
  behind. The OCR tests, the daemon's helper tests and onboarding's and doctor's tests
  with a fake `podman` no longer fail on a loaded computer, and a test no longer reads a
  half-written pid file.

### Added
- **Runs get the repository's toolchain and dependencies, and a run whose check cannot
  work is refused before any model call.** A loadout with a `detected` check (`fix`,
  `fix-claude-code`, `fix-codex`) now detects the repository's language from its root
  files (`package.json`, `pyproject.toml`/`setup.py`/`setup.cfg`/`requirements*.txt`,
  `Cargo.toml`, `go.mod`; with several, the check command's program chooses, else the
  run is refused naming `--toolchain`) and runs an image of it built from the new
  toolchain recipes `node` (Node 24.21.0), `python` (Python 3.13.16), `rust` (Rust
  1.99.1) and `go` (Go 1.27.2), each an official Debian-based image pinned by its
  multi-architecture index digest plus git and make. `axocoatl run` builds a missing
  image first and prints what it builds and its size, an agent's included (`--no-build`
  refuses, naming `axocoatl recipe build`); an external writer's recipe composes with
  the toolchain into one image (`claude-code python`). Before the first model call a
  setup installs the declared dependencies (`npm ci`, `pnpm` and Yarn 1 or Yarn 2+
  through corepack, `pip install -e '.[test]'` into a virtual environment with the test
  extra or group, Poetry, uv, PDM and Hatch test lists given as requirements, and
  `pytest` whenever the check runs it and nothing installed brings it, `cargo fetch`,
  `go mod download`) as the writer, with only that ecosystem's registries open (a
  per-Session allow recorded with the run as actor, revoked before the writer starts; a
  setup refused 30 connections is stopped). The installed packages persist in a
  per-Workspace cache volume, so a second run installs offline; only the setup writes
  it, and root takes it back read-only before the first model call, so a writer cannot
  change what its own or a later run's checks run. A failed setup ends the run with exit
  code 5 and the installer's last lines. Then a probe runs the check once on the
  untouched tree through the same supervisor as the run's checks: a missing check
  program or test runner, a missing Python module that `python -m` runs or the project
  declares, or a check that runs none of the repository's test files refuses the run
  with exit code 3, naming the image, the check, what is missing and the fixes
  (`--no-probe` skips it). The check is `--check`, else a `Check command:` line of
  `AXOCOATL.md`, else detected; Python repositories get `python3 -m pytest` when they
  use pytest (including plain `def test_` files), else `python3 -m unittest discover`.
  New flags `--toolchain detected|node|python|rust|go|none`, `--image`, `--no-setup`,
  `--no-build`, `--no-probe` and the loadout field `environment.toolchain`; `none` keeps
  the earlier behavior. The run's notes, manifest and receipt
  (`predicate.sandbox.environment`, ignored by 1.4 verifiers) record the toolchain and
  why, the image, the check, the setup command, exit code, duration, registries and its
  connections, and the probe. Rust repositories are supported like the others: from
  Landlock ABI 2 (Linux 5.19) the hardened Session's Landlock domain now allows the
  cross-directory renames cargo and rustc make in every build (Fixed, below). On a
  kernel with only ABI 1 the kernel still refuses them, and the probe ends the run with
  exit code 5, naming "Invalid cross-device link".
- **`axocoatl connect openrouter`: OpenRouter through a browser sign-in, with no key to
  paste.** It runs OpenRouter's OAuth PKCE flow: a loopback server on `127.0.0.1` (a port
  the system picks), OpenRouter's sign-in page opened in the browser and printed
  (`--no-browser` only prints it), the browser's return checked against the sign-in's
  state (any other request to the loopback server is ignored, so a stray or forged one
  cannot end the sign-in), and the code exchanged with a verifier that never leaves the
  computer for a new key. The key is checked with `GET /api/v1/key` (its label, credit
  limit, usage and free-tier status are shown, never the key), stored in the secret store
  as `openrouter`, and the configuration gains `openrouter_billing: credits` and
  `openrouter: {api_key_secret: openrouter}`, every other line kept; on a computer with
  no configuration yet it writes one with only those entries. Models named with
  `--writer` and `--reviewer` are checked in OpenRouter's public catalog before the
  sign-in, and must differ. It asks onboarding's BYOK question first; cancelling in the
  browser, Ctrl-C, or the end of the 10-minute sign-in time (`--sign-in-timeout`; at a
  terminal it first asks whether to keep waiting, below) stores nothing. `--status` shows
  the stored key's label and limits (it sends only an `sk-or-` value); `--remove` deletes
  it and its line in the configuration, keeping the line while Agents still use
  OpenRouter. It never stores into, sends or removes a secret another command owns
  (`claude-code-oauth`, `codex-chatgpt`, `codex-openai`, the receipt key). Onboarding's
  OpenRouter step runs the same sign-in, which leaves signal handling to the process; a
  failure there names the `axocoatl onboard` command to run again.
- **`api_key_secret` for hosted providers.** `providers.<name>.api_key_secret` names a
  secret in the data root's store in place of `api_key`, so the key is not in the
  configuration file; the daemon reads it when it starts and logs, without a value,
  a named secret that is not stored. `axocoatl doctor` checks that it is stored. A
  value there that reads as a key is refused and never repeated in a message, and a
  hosted provider block with neither `api_key` nor `api_key_secret` is refused, as a
  missing `api_key` was before.
- **Contract probes.** When Team & budget, or a loadout run's admission, observes an
  Ollama model's context, Axocoatl sends a few small requests to the person's own server,
  with that context and the settings every call uses.
  - A prompt about four times longer than the context must be refused before any
    generation.
  - A one-token request must stop after one token, with a prompt count inside the
    context.
  - A request for hundreds of lines with an output limit of 64 tokens must stop by 64
    generated tokens. It must report exactly 64 when the limit ended it, and stream no
    more pieces of text than the tokens it reports. Speculative decoding, such as the
    `-mtp` tags, could overshoot or undercount here.
  - For a model that reports tool calling, a request to call the one tool it is offered,
    within 1,024 tokens, must come back as a call of that tool with a call id and a
    string argument, read the way a Session reads an answer. A call to a tool it was not
    offered beside it is allowed, since a Session answers such a call with a tool error.
  - Every probe samples with one fixed seed at the model's own settings. On Ollama
    0.40.8, qwen3:0.6b, qwen3:4b, qwen3.5:0.8b, qwen3-coder:30b and gpt-oss:20b each gave
    the same tool-call answer five runs out of five. Unseeded, they called the tool after
    23 (qwen3-coder:30b) to 309 (qwen3:4b) tokens.
  - A model that fails a probe is refused on that server for every Agent and run, and the
    error names the guarantee.
  - A probe Ollama does not answer refuses nothing: a failed request, an error status, or
    an answer that ends before its final record, as Ollama 0.40.8 gave when a model
    runner had run out of GPU memory. The error says the check did not run.
  - Passes are kept per server, Ollama version, model digest, context and tool calling.
    After an upgrade to another accepted version, the probes run again before it serves
    an existing Team.
  - The probes passed on Ollama 0.40.8 with the models above, with an MTP
    speculative-decoding tag (qwen3.5:0.8b-mtp-q8_0), and with a GGUF model that has no
    template of its own (llama-server's chat template). Ollama's renderer, Go-template
    and Harmony paths were covered too.
  - Images are not probed. Each call reserves the whole context plus its output limit,
    and llama-server counts an image's tokens against the context. On Ollama 0.40.8, a
    64x64 image counted 11 tokens: 4,090 tokens of text were generated from at a
    4,096-token context, and the same text with the image (4,101 tokens) was refused.
- **The Ollama version in the record and the receipt.** Each native Ollama provider
  records the version it checked right before each inference request. A loadout run's
  Outcome keeps them by model (`ollama_versions`), and its receipt names them in the
  native Agent's `program_version` (`ollama 0.40.6`), a field the 1.4.0 verifier already
  reads, and for a native reviewer in the new optional `review.program_version`, which
  the 1.4.0 verifier ignores, so receipts of this release still verify with
  `verify-receipt@v1.4.0`.
- `axocoatl doctor` prints the Ollama version with the version policy's verdict and
  checks each configured model that is pulled the way native admission does
  (`POST /api/show`): a local GGUF completion model, with tool calling for an Agent that
  lists tools.
- **`axocoatl doctor` checks the data directory's ownership record**, read only and
  without a lock, so a running daemon is unaffected: `[ OK ]` when it names this
  directory; `[ OK ]` with a note when the directory was copied or moved and the next
  start records its new location; `[FAIL]` with the reason and what to do when the daemon
  would refuse to start on it.
- **An Uninstall page** (docs, Operate): stop the daemon and remove the service, the
  Podman containers, volumes and images Axocoatl made, the configuration, data and logs,
  the binary and the PATH line the installer added, and optionally the Podman machine,
  the Ollama models and the sign-ins.
- **`scripts/check-copy-paste.mjs`**, run by the docs gate with its tests, extracts every
  shell command and YAML snippet from the README, `llms.txt`, the marketing site and the
  docs' first-run pages, and refuses a `<br>` inside code, a copy button with a newline
  or placeholder, a line that starts with a flag, a dangling backslash, an unbalanced
  quote, an unknown `axocoatl` subcommand, `--keys FILE` that no earlier step creates,
  YAML that does not parse or a configuration snippet `axocoatl validate` would refuse
  (the product browser gate runs it with the binary it builds, which validates each
  configuration and loadout snippet), a GitHub workflow snippet without `on:` or `jobs:`,
  "stronger" next to `gpt-oss:20b`, `gpt-oss:120b` without its 96 GB note, the withdrawn
  claims, and WSL2, Fedora, Debian 12 or Ubuntu 22.04 named on an install page without
  "not yet verified". `--json` lists every block for a gate that runs them verbatim.
- **`axocoatl service status` shows why the service last failed to start.** When the
  service's last start failed, it prints the line the daemon logged
  (`Failed to bootstrap daemon: …`, including a configuration error or a port in use),
  read from the end of `~/Library/Logs/Axocoatl/daemon.log` on macOS or of the user
  journal on Linux. It does so also while launchd or systemd keeps starting a failing
  service and status reads "running: yes". It says when systemd does not load the unit.
  Run live with launchd on macOS and with systemd on Ubuntu 24.04.
- **A clean-machine new-user gate blocks a release unless a logged override is used**
  (`docs/RELEASE_GATE.md`). The release workflow builds nothing until a signed evidence
  bundle for the tagged commit (`release-evidence/<version>/<commit>/`) passes every
  required path: a brand-new user from the published install command to a passing, signed
  `axocoatl run fix` on Linux x86_64 and ARM64 in CI, and live runs on a clean Mac
  account (local models, Claude Code, Codex, OpenRouter, the workbench in Safari) and an
  Ubuntu 24.04 VM. The bundle's manifest is signed with the gate account's receipt key
  (`axocoatl receipt sign-evidence`, hidden) and checked against `.axocoatl/gate-keys`;
  the release workflow also runs the Linux new-user flow again on its own archives. The
  only way past it is a manual run on the tag, started by a login listed in
  `.axocoatl/gate-override-actors`, with a typed override phrase, which is logged. CI
  runs the Linux flow on every pull request against the commit's release archive, with a
  scripted Ollama-compatible model server and fixture repositories in Node, Python, Rust
  and Go (`tests/fixtures/newuser`), plus an Ubuntu 22.04 leg that must refuse cleanly.
  It also checks a new Mac user's install, `doctor` and `onboard --plan` against a Podman
  stand-in in each Podman state on macOS runners (arm64 and Intel). The browser
  regressions now also run on WebKit (Safari's engine) and on aarch64, against the
  release build. CI's browser legs and the release workflow's browser gate now pull
  `node:20-slim` by digest and require the Preview cookie journey, which runs a Session
  app behind the real Preview proxy; a missing image fails it instead of skipping it. The
  evidence check also requires, for each required path, the steps `docs/RELEASE_GATE.md`
  lists as the evidence for each sentence of the Verified section below. A path that
  skipped one or never recorded it fails, naming the step.
  - The Mac run records first that the account has no Axocoatl, its data or stored
    secrets, no Podman and no Ollama. After each setup it records what onboarding
    installed with Homebrew, created and turned off.
  - Each sign-in path records that its secret was not stored before its interactive
    onboarding and was stored by it.
  - New required live paths: the Always-On Service on the Mac (chosen in onboarding's
    Step 6, restarted by Step 7 after `onboard --update` changes the configuration, a fix
    through it, then `service status`, `stop` and `uninstall`), a run that starts a
    stopped Podman machine, and timed runs on two real repositories.
  - The Ubuntu VM run does the same with a systemd user unit. It removes the new user's
    own range of subordinate ids first, so that onboarding must add one beside another
    user's.
  - The workbench path accepts "Last turn N" for any N of at least 1 that lists the
    changed file. Its questions can be answered by a WebDriver driver in Safari
    (`scripts/release-gate/safari-workbench.mjs`), whose answers and prompt are recorded
    as its own.
  - The person also checks, in Safari itself, that a Preview address typed into a tab,
    reached by a redirect or Command-clicked gets Axocoatl's refusal page and never
    reaches the app. They also check what a `Domain=localhost` cookie flood does to the
    workbench and that Manage Website Data removes it.
  - The `--keep pr` step requires the verify-receipt Action's log to say it verified that
    run's receipt.
  - Every fixture run fails the gate when an answer the run ended with, or a file it
    changed, holds tool-call markup a model wrote as text (`<function=`, `</tool_call>`).
  - Every request the gate makes to a daemon or stub is bounded, and a failed request is
    no longer taken for a running daemon.
  - Transcripts are written as they happen, with each question and answer also kept in
    the evidence.
  - Tier B checks against the Podman stand-in that a run's admission starts a stopped
    machine.
- **`axocoatl receipt verify --require-pass`** exits 6 (result `not-passed`, naming the
  verdict) for a receipt that verifies but whose run's verdict is not `pass` (checks
  failed, needs attention, interrupted or error). A bad signature (1), an untrusted key
  (2) and files that differ (4) come first. Without the flag such a receipt still
  verifies with exit 0: it says truthfully what the run did.
- **`axocoatl recipe clear-cache [--repo DIR | --all]`** removes a repository's toolchain
  cache volume, or every one of the data root, instead of `podman volume rm`. A cache
  that a Session's container still uses is left, and the command names that Session and
  exits 5.
- **Provider answer timeouts are settings.** Every provider block (`ollama`,
  `openrouter`, `openai`, `anthropic`, `gemini`, `mistral`) takes
  `first_token_timeout_secs` and `idle_timeout_secs`.
  - `first_token_timeout_secs` runs from the request to the first chunk: 600 s for
    Ollama, 120 s for a hosted provider, 1 to 3600.
  - `idle_timeout_secs` is the longest pause between chunks: 90 s, 1 to 900.
  - Before, every provider waited 120 s for a streamed answer's headers and 90 s between
    chunks, so a CPU-only or busy computer failed a long prompt.
  - A streamed answer still ends 15 minutes after its first chunk at the latest
    (`stream total timeout`). That limit now counts from the first chunk, not from the
    response headers, so a first-token timeout longer than 15 minutes is usable with a
    hosted provider too.
  - Loading an Ollama model to read its context and the contract probes after it now take
    `providers.ollama.first_token_timeout_secs` too, where they had 120 s. This happens
    before a native Session's first call, in `axocoatl doctor`, and in
    `axocoatl onboard`, which reads the setting from your configuration. The timeout is
    600 s unless set, and never less than 120 s.
  - The errors name the setting: `first-token timeout`, `stream idle timeout`,
    `did not finish loading`.
  - Giving up closes the connection. Measured with Ollama 0.40.8, Ollama then abandons
    the answer ("aborting completion request due to client closing the connection") or
    the model's load ("client connection closed before llama-server finished loading,
    aborting load"; the model was not loaded 30 s later).
- **Each container resource cap applies on its own, and what applied is recorded.**
  - Before, Session, browser, service-forwarder, egress-sidecar and SearXNG containers
    asked for memory, CPU and process caps together. One cgroup controller missing from
    rootless Podman's delegation dropped all three, with only a log line.
  - Each container now gets every cap whose controller `podman info` lists, and each kind
    of container logs once at `warn` which caps it runs without.
  - With `sandbox.require_resource_limits: true`, none of these containers starts without
    every cap: the start is refused, naming what is missing.
  - The Session's record (`environment.resource_limits`), a run's notes and its receipt
    (`sandbox.limits`, which `axocoatl receipt verify` prints) say which caps the Session
    container got and which it did not. Measured on a Podman machine (Podman 6.1.4): the
    record matched what `podman container inspect` shows (2 GiB, 2 CPUs, 512 processes).
  - `axocoatl doctor` warns and prints how to delegate the missing controllers, on Linux
    and inside a Podman machine. The workbench's first screen shows a Resource caps
    warning, or a failure that blocks Create session when the caps are required.
- **Sessions on a Linux computer with SELinux.**
  - Where Podman reports SELinux enabled, Session and browser containers run with
    `--security-opt label=disable`, as Toolbox does, instead of relabelling your files.
    Under the default `container_t` label, SELinux refused them the bind-mounted
    repository and the execution supervisor.
  - The user namespace, dropped capabilities, seccomp, `no-new-privileges` and the
    supervisor's Landlock filter stay.
  - Measured with a Linux build on Fedora CoreOS 44, Podman 6.1.4, SELinux enforcing: a
    plain Session, a hardened-workload Session, a Node-dependency Session and the browser
    container (reaching exposed ports, and declared hosts through the egress proxy)
    passed. Their containers ran with `label=disable` and all three caps.
  - A live test, `the_browser_reaches_the_sessions_app_within_its_limits`, starts the
    browser container as a Session's browser tools do and drives Chromium to the
    Session's app. In that Podman machine (kernel 7.2.5), a Linux build passed it with
    `label=disable`, and the browser ran with no process label. The container's cgroup
    held the 2 GiB memory, 512-process and 2-CPU caps, and Chromium ran in it (75
    processes and 202 MiB at peak).
- **Docs: open the workbench from another computer** with an SSH port forward
  (`ssh -N -L 8080:127.0.0.1:8080`).
- **Doctor names what this platform and login cannot do.**
  - A `Platform` line is OK on macOS on Apple silicon and on Ubuntu 24.04, where the
    release gate's runs are made. It warns that Axocoatl has not been verified yet on an
    Intel Mac, another Linux release, WSL2 (keep repositories off `/mnt/c`), SELinux
    enforcing, and local models below 48 GB of memory.
  - Doctor checks `git`, which the daemon runs on every repository. On macOS it checks
    the Command Line Tools first, since `/usr/bin/git` without them opens Apple's
    installer; the fix is `xcode-select --install`.
  - On Linux, doctor checks the systemd user session the Always-On Service needs. A login
    reached with `su -` or `sudo -iu` has none. The fix is to log in directly and, unless
    linger is already on, to run `loginctl enable-linger USER` there to keep the service
    running after logout. For an installed service, doctor also checks linger, read from
    `loginctl` or `/var/lib/systemd/linger`.
  - It warns when `HTTPS_PROXY`, `HTTP_PROXY` or `ALL_PROXY` is set, naming the variable
    and never its value: a Session's network connects directly and its containers do not
    get the variables. Requests to this computer and to Ollama never use a proxy;
    Axocoatl's other requests (hosted model providers, onboarding's OpenRouter checks,
    webhooks and MCP servers on other computers) follow them.
  - It warns that a configured `hooks:` section is checked but never run.
  - On Landlock ABI 1 it says that Rust builds fail with "Invalid cross-device link".
    With `sandbox.backend: e2b` it says that `axocoatl run` still runs in Podman.
  - Onboarding gives the platform and proxy notes at Step 0, and does not offer the
    Always-On Service in a login without a systemd user session.
- **Onboarding checks each OpenRouter model's endpoints before it writes the setup,
  reading them as a run does.**
  - When OpenRouter reports every usable endpoint of a model as unavailable or degraded
    right now, onboarding names each endpoint's status and success rate, and says to try
    again later or to choose another model.
  - When no endpoint fits native Sessions, or the model is not in OpenRouter's catalog,
    it says why and asks for another model.
  - At a terminal another model can be named: Enter takes the first of
    `qwen/qwen3-coder-30b-a3b-instruct`, `z-ai/glm-4.6`, `deepseek/deepseek-chat-v3.1`,
    `qwen/qwen3-coder` and `openai/gpt-oss-120b` that OpenRouter reports healthy. With
    `--yes` it is a step left that names that model, and nothing is saved.
  - Before, onboarding approved such a setup and its first run failed with exit code 5.
    On 2026-10-23, with every `qwen/qwen3-coder` endpoint at status -5 or -2, onboarding
    stopped at Step 3 with that message and wrote nothing.
- **OpenRouter sign-ins can wait longer, and are logged.**
  - At a terminal, when the sign-in time (10 minutes) ends without a sign-in, onboarding
    and `axocoatl connect openrouter` ask whether to keep waiting instead of failing.
    Without a terminal the time stays the limit.
  - Each connection to the sign-in's loopback address is logged with its method, path and
    `Host`, never the code. A connection that sent no request is logged as what it did:
    closed early, sent no HTTP/1 request, sent too much, or sent nothing in 5 seconds.
  - Onboarding writes these lines to its own log. `connect` writes them to
    `connect-openrouter-<time>.log` beside it; the first connection creates that log, and
    a failed sign-in names it. A sign-in that ends with nothing having come back says so.
  - In WSL the sign-in pages (OpenRouter's and Codex's) open with `wslview`, in Windows'
    browser.
- **`axocoatl session record-location`** records a copied or moved data root's new
  location while Axocoatl is stopped and changes nothing else, so Axocoatl 1.1.0 to 1.4.0
  start on a restored backup. Measured with 1.1.2, 1.2.0, 1.3.2 and 1.4.0, each on a
  restored backup holding a Session and on a copy at another path. Starting 1.5.0 on the
  restored root instead converts the journals of Sessions written by 1.2 and earlier,
  which 1.1 and 1.2 then refuse. With `--original-deleted` it also records that the data
  root it was copied from was deleted, so the next start removes the volumes that one's
  Sessions left.

### Fixed

- **The Mac release gate stops dependent work when onboarding fails.** It retains the
  command's failure and stops that path before fixture runs, sign-in attestations or
  resetting the disposable PR repository. Ctrl+C preserves the path's evidence and
  ends the harness instead of starting another path.
- **Release evidence is written when the runner is reached through a path alias.**
  Gate helpers resolve their directory, and the evidence command compares file
  identity before invoking its CLI; macOS `/var` and `/private/var` no longer cause a
  silent successful exit without evidence.
- **Apt refreshes package lists before onboarding installs Podman.** Fresh Ubuntu
  images can have package sources enabled but incomplete cached indexes, so installation
  reported no Podman candidate. Onboarding asks separately before `sudo apt-get update`
  and installation, and stops Podman setup if the refresh is declined or fails. Doctor
  and the workbench's manual install command include the refresh too.
- **Cancelled macOS command I/O closes its pipe ends.** Child pipes are created
  close-on-exec so unrelated programs cannot inherit them. The parent's FIFO ends
  use nonblocking reads and writes that yield to deadlines; cancellation releases
  them immediately, without leaving detached workers or continuing a cancelled write.
- **Snapshot coverage distinguishes submodules from unread nested checkouts.**
  Submodules record their checked-out commit, with the index gitlink as a fallback;
  their contents are not captured. Every other nested checkout is named as not
  covered, even when its entry did not change. Malformed or oversized encoded path
  lists are refused before duplicate names can reduce their apparent size.
- **Signed-out browser fixtures wait for cookie writes before closing their page.**
  Chromium's queued writes can otherwise be lost, making the cookie-flood journeys
  fail before they exercise sign-in behavior.
- **A change whose patch is too large for a repository capture to print no longer stops
  the turn: the capture records it by its digest and size, and reads a repository of tens
  of thousands of files within its time.**
  - A writer left two compiled test programs (4.6 MB each) in the repository root, seen
    in the release gate on the Rust fixture with `qwen3-coder:30b`. The capture after it
    failed with "repository command output is incomplete or exceeds its capture bound;
    partial bytes cannot become file input": it printed the whole patch against `HEAD`,
    every untracked file included, where its output may take 1 MiB (768 KiB around the
    checks). The turn paused, and the required checks and the review never ran. The run
    ended needing attention without saying why: its summary said the check did not run
    and the review did not pass, and the turn's own reason was only "A turn ended needing
    attention".
  - A capture now reads the patch once, as Git writes it, taking its SHA-256 and size as
    it passes. It keeps only what it prints: the whole patch up to 512 KiB, the most the
    host records, and of a larger one its first 12 KiB, so its output stays under 768 KiB
    whatever the change. That prefix is kept only as text to show, never as a patch
    anything applies or reads files from. No patch is too large to capture:
    `axocoatl run fix` with a writer that adds an 80 MiB random file (a 108,117,524-byte
    patch) passes on macOS with Podman, and its checks and review run. What a capture's
    commands write to standard error is kept to its first and last 8 KiB, so Git warning
    once per file cannot fail it.
  - A capture read each file of the repository with six processes, twice. On a 6-CPU
    Podman machine 12,288 tracked files took 55 seconds a capture, so a repository of
    about 40,000 files passed the 180 seconds a capture has. It now reads each batch of
    files with one `sha256sum` and one `stat`: 50,000 tracked files take 12 seconds and
    12,288 new files 16. The output is the same as before, checked side by side under
    dash, bash and busybox. Each of a capture's file listings may take 64 MiB: with
    160,000 committed files whose paths are about 230 bytes long, a capture passed that
    after 15 seconds and failed as "Repository capture failed or its result was
    truncated; the candidate is not verified".
  - A capture that still runs past its 180 seconds says so around the checks and in the
    `receipt_incomplete` warning: "The repository capture did not finish within its 180
    seconds: it reads every file in the repository, so a very large repository or change
    (hundreds of thousands of files), or a busy computer, can take longer". Before, it
    said "repository command did not complete normally: TimedOut; observed
    primary_exit=…".
  - A file named `-` is recorded with its own mode (the capture recorded its standard
    input's, 666) and shown with its content in the patch. A submodule or nested checkout
    no longer costs the files after it in its batch, as many as a thousand, their
    records.
  - The reviewer of a change too large to record is told so and how large its patch is.
    It is given at most 200 of the paths the turn changed in 8 KiB, files `HEAD` has
    first, the rest counted by directory, each name quoted as Git quotes one so none adds
    a line. Then it gets at most 6 KiB of the patch's beginning with the data of binary
    files left out, cut after a whole line to fit its model's context. When the capture
    taken as the turn began listed no paths but recorded its whole patch, the reviewer is
    told which paths the turn changed apart from those that already had uncommitted
    changes. When that capture recorded neither, or `HEAD` moved during the turn, the
    reviewer is given the paths that differ from `HEAD`, told they may include earlier
    changes, and why.
  - Keep, Source Control's Last turn and receipts attribute such a change from the lines
    of the changed paths each capture records, compared on the same `HEAD`, when neither
    the manifest nor the patch fits. A receipt names each file with its digest as before.
    A capture records those lines for at most 10,000 paths, and only when they fit its
    output: a change of more paths (12,288 small files, say) is still checked and
    reviewed, but what it changed cannot be attributed, so its receipt does not cover it.
  - Such a run, passing or not, now carries the new warning `receipt_incomplete` in its
    summary, JUnit file and record. The warning says why: the capture that lists none of
    the paths, with its 10,000-path limit and the 512 KiB of a patch a capture records
    whole. The receipt lists it among its own warnings, and `axocoatl run --receipt` adds
    that the receipt does not cover every path the run changed.
  - A write-scope comparison listing more changed paths than the 512 KiB the host accepts
    is refused by the capture itself, as the host refused it. Turns admitted with an
    earlier release's capture keep it for their checks. A capture around the checks that
    cannot record the repository is named, with its reason, in the checks' readiness in
    the workbench and on the run's Needs attention line.
- **The reviewer is no longer shown a change's binary data, and what it is shown of a
  change fits its model's context.**
  - Since 1.1.0 the reviewer was shown up to 48 KiB of the patch as Git prints it, the
    base85 data of binary files included. One untracked 24 KiB binary file left beside
    the fix made the reviewer's request need 34,344 tokens, where `gpt-oss:20b` with a
    32,768-token context holds 27,852 with its output allowance. The reviewer failed with
    "Current request needs 34344 context tokens, exceeding limit 27852" and the run ended
    `needs_attention` (exit 2) with its checks passed. A 200 KiB binary needed 47,533
    tokens and 400 KB of base64 text 45,115.
  - Each binary file is now named with its size and its data left out, as for a change
    too large to record. The rest of the change is shown whole when it fits three
    quarters of what one request to the reviewer's model may hold beside its output
    allowance (85% of the context), less the rest of the request: the task, the answers,
    the reviewer's instructions and attachments, and the room kept to show a previous
    answer when it is asked again. It is counted as the host counts a request. For
    `axocoatl run fix` with a 32,768-token reviewer and an 8,192-token output allowance
    that is about 10,700 tokens, enough for 30 KiB of Rust.
  - A larger change is shown source first, then generated and dense text (lock files,
    base64, minified code), then binary files, each file whole when it fits and else cut
    at a hunk boundary; a file of one hunk that does not fit, such as a new file, is left
    out. The reviewer is told first that not all of the change is shown and how much is
    not. Each cut says what of its file was left out, and the files left out are listed
    with their bytes. Its prompt says it has not seen that part, should read those files
    before judging them and must not approve code it has not read.
  - The context is the one the reviewer's model was observed with when its team was
    applied. Without one, the change is fitted to 16,384 tokens, the smallest context
    onboarding sets up.
  - A previous answer without a verdict is shown again only when it fits a sixteenth of
    what a request may hold beside its output allowance: 1,228 tokens for a 32,768-token
    reviewer, at most 2,048. A longer one is not shown again, and the reviewer is told
    so.
  - Measured with `gpt-oss:20b` reviewing: `axocoatl run fix` on the Node fixture passes
    beside a 24 KiB binary, a 200 KiB binary, 400 KB of base64 text and an 80 MiB file.
    The 1.5.0 candidate ended `needs_attention` for the first three.
- **Onboarding and doctor read the user's subordinate ids from `/etc/subuid` and
  `/etc/subgid`.** They judged the range only from `podman info`, which reports the user
  namespace Podman's pause process already holds. After a user's range was removed (seen
  in the release gate's CI case), that namespace kept the old range, so doctor said OK
  and exited 0 while every `keep-id` container, which Podman maps from the files, failed
  in crun with "write to `/proc/sys/net/ipv4/ping_group_range`: Invalid argument", and
  onboarding showed only that message. On Linux, when Podman runs on this computer, both
  now read the files (by user name or uid, as Podman does): no range, or fewer than 65536
  ids, is a FAIL ("User ID ranges", so doctor exits 1) whose fix is `sudo usermod
  --add-subuids <first>-<last> --add-subgids <first>-<last> <user> && podman system
  migrate`, with the first 65536 ids from 100000 up that no line of either file uses
  (100000-165535 whenever no line uses those, for example when the files are empty). When
  only one file is short, the finding names that file and usermod adds a range to it
  alone. When the files leave no free range from 100000 up, no range is named: the fix
  says to choose one, with only the short file's `usermod` option. Onboarding asks before
  running it and, under `--yes`, runs it only with `--allow-sudo`. A range in the files
  that Podman has not picked up yet fails with `podman system migrate` alone (no sudo; it
  stops running containers). Rootless Podman maps a range with `newuidmap` and
  `newgidmap`, which it looks up on `PATH` only; doctor now looks there too. When one is
  missing, or is neither setuid nor given file capabilities, `podman info` maps only the
  user's own id (a pause process started before the range was added). Usually, with a
  range in the files, it fails instead: `command required for rootless mode with multiple
  IDs: exec: "newuidmap": executable file not found in $PATH`; once a pause process has
  run, Podman logs the same failure with its quotes escaped and a suggestion to try
  `podman system migrate`, and both forms are recognized. Doctor's Podman line now names
  the package that ships them, where it said to run `podman info`. Onboarding offers it
  after asking, like the Podman install (under `--yes` only with `--allow-sudo`):
  `sudo apt-get install -y uidmap && podman system migrate` on Debian and Ubuntu (Podman
  only recommends `uidmap`). An install does nothing for a package that is installed
  (shadow-utils always is on Fedora and RHEL), even when a helper's file is gone or lost
  its setuid bit or file capabilities. So doctor and onboarding ask the package manager
  (`dpkg-query`, `rpm`, `pacman`) and then offer its reinstall:
  `sudo apt-get install --reinstall -y uidmap`, `sudo dnf reinstall -y shadow-utils`, or
  `sudo pacman -S --noconfirm shadow` on Arch. Such a helper is reported as missing
  though its package is installed, not as not installed. A helper in `/usr/bin` while
  `/usr/bin` is not on `PATH` is reported as that. A copy earlier on `PATH` that the
  package does not own is to be deleted. No install is offered for either. When an
  install or reinstall runs and the helpers still cannot map, onboarding says so and runs
  nothing after it, and neither doctor nor a later onboarding offers the same command
  again (next entry). When the install or reinstall itself fails, the step says why: a
  build dnf no longer has (dnf5's and dnf4's wording; `sudo dnf upgrade -y shadow-utils`
  instead), apt's out-of-date lists (`sudo apt-get update` first), or otherwise what the
  command reported, to fix before running it again. On other distributions the fix says
  what to install or reinstall. The workbench's readiness item, a loadout run refused for
  this, and every command that first makes Podman ready (Session start, `axocoatl run`,
  `recipe build`, `connect codex`, the browser containers and SearXNG) name the packages
  too; the latter said only `podman readiness check did not complete safely` with
  Podman's error. A sandbox check container that still fails on `ping_group_range` names
  the same fix. The files are not judged when ranges come from SSSD (`nsswitch.conf`), or
  when Podman is reached through `CONTAINER_HOST`, `CONTAINER_CONNECTION` or a remote
  connection: that Podman's own `podman info` decides, as before. If it maps one id, the
  fix says to add the range where that Podman runs, then, as that Podman's user there, to
  run `podman system migrate` and then restart its API service (either one alone keeps
  one id mapped). For a macOS machine, that is inside it with `podman machine ssh`, then
  restart the machine. On Linux a `unix://` socket may be another user's Podman on this
  computer; on a Mac it is usually a machine's API socket. Onboarding runs nothing for it
  (no usermod, and no `podman system migrate` here, which the remote client refuses).
- **A reinstall of the id-map helpers that did not help is remembered.**
  - Onboarding may install or reinstall the package of newuidmap/newgidmap. When that ran
    and left them unable to map, it is kept in `onboarding/id-map-attempt.json` in your
    Axocoatl data directory. The same happens when it failed in a way the same command
    cannot get past: dnf cannot reinstall a build no enabled repository has, or apt's
    lists do not have the package.
  - A failure the network or a repository may have caused (for example
    `Curl error (6): Could not resolve host`) is not kept, and the next run offers the
    command again.
  - `axocoatl doctor` and later onboarding runs say it, with its date, and do not offer
    the same command while the helpers are the same files.
  - Onboarding makes that directory, and the data directory above it when it is the first
    to, owner-only whatever the umask. Under `umask 002` (Ubuntu 24.04's default) they
    would otherwise be group-writable, which the daemon refuses.
  - On Arch, doctor reads pacman's file list and pacman.conf's `NoExtract` and
    `NoUpgrade`. Measured with pacman 7.1: `NoExtract` makes a reinstall remove the
    helper and write none, and `NoUpgrade` writes it as `.pacnew`.
  - dpkg's reasons are said once for both helpers ("remove that line" when one
    `path-exclude` line leaves out both). A `HOME` of `/` no longer marks every helper as
    foreign. Commands to run are in backticks.
  - Run in Arch Linux ARM and ubi9 containers.
- **Hardened commands can rename files into other directories.** From Landlock ABI 2
  (Linux 5.19), a Landlock domain that does not handle `REFER` refuses every rename or
  link into another directory. The hardened supervisor's domain handled only creating
  block devices, so cargo and rustc, which rename across directories in every build,
  failed with "Invalid cross-device link" and no Rust check could pass in a loadout run.
  The domain now also handles `REFER` and allows it beneath `/`, so renames work as they
  do outside it; creating block devices stays refused. Landlock ABI 1 cannot handle
  `REFER`, so on Linux 5.13 to 5.18 such renames are still refused, and the probe names
  them. The embedded execution supervisors were rebuilt with this change.
- **One upstream error, or a reviewer model that loops or answers nothing, no longer ends
  a run.**
  - When a required reviewer's model call failed after its response had started (an HTTP
    502 from OpenRouter, seen live), the provider retry did not apply. The review
    recorded "The reviewer did not finish" and the run ended needing attention.
  - When the reviewer's call fails with a transient error (a 429, a 5xx, a timeout or a
    reset connection, the same policy as provider retries), the host now runs that review
    round again, once per round in an epoch, with the same prompt about the same result.
    It first waits for the provider's `Retry-After` (at most 30 seconds), or else 4
    seconds, doubled for each round the turn already ran again, at most 30. A
    `Retry-After` over 30 seconds is not waited for. While it waits, the workbench shows
    the review as under way, and Stop ends the wait.
  - These model failures now run the round again at once, in a fresh conversation:
    - the model kept calling tools and gave no answer even when asked for its final
      answer without tools (the local `gpt-oss:20b` did so on the python-pytest fixture
      in the release gate, after the checks passed);
    - the model gave no answer and called no tool it was offered (`gpt-oss:20b` called a
      `search` tool, which Ollama dropped);
    - the reviewer used every tool round of its activation, or reached its token limit
      for the activation.
  - The run again takes a round, as asking again for a verdict does, so it happens only
    while a round remains. The review ends as before on a second failure, a 400, 401, 402
    or 403, a refusal, a safety stop, a request larger than the model's context, or a
    failure whose cause is not recorded. The review's reason and the run's Needs
    attention line now say why in the reviewer's own words. For a request larger than its
    model takes, that is "its request needed 34,440 tokens of its model's context, more
    than the 27,852 one request may take with its output allowance, …".
  - The run record lists a transient retry as a provider retry of the reviewer with its
    HTTP status, and the Outcome counts it. The control plane shows any other run again
    as `review_rerun` evidence with its cause.
- **A run no longer leaves a `node_modules` the person cannot write in a repository that
  had none.** A Node project's Session container mounts its own dependency volume over
  the repository's `node_modules`. When the repository had none, the OCI runtime made
  that directory in the person's tree as the container's root: on Linux, under rootless
  Podman with the hardened workload every loadout run uses, a subordinate uid with mode
  `drwxr-xr-t`, so the person's own `npm install` or `npm ci` then failed with
  `EACCES: permission denied, mkdir 'node_modules/…'` until they removed it; on macOS it
  was the person's but stayed behind after every run. Axocoatl now makes that directory
  as the person before the container starts and marks it with a random value in the
  extended attribute `user.axocoatl.mountpoint`. It removes it again, if it is still
  empty, still carries that value, and no container Podman lists has a mount on it: when
  it removes a Session's container (at Close, when `axocoatl dev` stops, or, after a
  daemon was killed, when the next daemon removes the containers it left, at that
  daemon's start or at its first Session when the Podman machine was stopped then; if
  Podman cannot list those containers at that point, the directory stays), before it
  starts one, and once a start that mounts it has its container. A repository's own
  `node_modules`, one the person makes in its place (even when ext4 gives it the same
  inode number), and what the person installs into it stay theirs; on a filesystem
  without user extended attributes, Axocoatl compares device, inode and birth time
  instead, which a directory made again at once can match. A container of the person's
  own keeps the directory while that container exists when it bind-mounts the directory,
  mounts an overlay volume of it (`-v …:O`) or a `local` volume made with `o=bind` and
  the directory as its device (a Compose bind volume), or bind-mounts a directory above
  it and mounts a volume, a bind or a tmpfs where the directory is (a development
  container). Axocoatl compares those paths as Podman reports them and, when that settles
  nothing, as they resolve on the host at that moment, on threads of their own and for at
  most 5 seconds each time it checks. When resolving a path fails other than by finding
  nothing there, or does not finish in time (as on a stale NFS, SMB or sshfs mount), the
  directory stays. When Podman cannot say which containers mount it, it stays too.
  Removing a container of the person's own runs nothing, so the directory goes at
  Axocoatl's next such start or removal, and stays if the daemon stops first. Axocoatl
  removes nothing while a container it cannot list could hold the directory: when Docker
  is installed (a `docker` that is not Podman's own, `DOCKER_HOST`, or a Docker socket),
  when Podman lists another connection (every Podman machine on macOS has a rootful one),
  when rootful Podman is installed on Linux (`/run/podman` exists, which the podman
  packages of Fedora and Ubuntu make at every boot, whether or not rootful Podman's API
  is enabled), or when `podman ps --external` lists a container. On a Mac, and on Fedora
  and Ubuntu with their podman packages, the directory therefore stays, empty and the
  person's, and the daemon logs why once per mount point and reason. A container that has
  never started, or that mounts a volume plugin's volume, keeps every such directory
  until it starts or is removed. A container run `--privileged`, with `CAP_SYS_ADMIN`, or
  confined by neither SELinux (`container_t`, enforcing) nor AppArmor
  (`containers-default`, which rootless Podman does not apply) can mount in a user
  namespace of its own (measured with Podman 6.1.4 and `label=disable`, and with rootless
  Podman 4.9.3 on Ubuntu 24.04). Such a container keeps the directory when it takes a
  directory above it, a `--rootfs` counts as such a directory, and the daemon logs which
  container once. When one container has a mount on the directory and another only could,
  the first is the one named. Among the mounts Axocoatl still does not see are those of
  rootful `sudo podman` on a Linux computer without `/run/podman` (a Podman not installed
  from such a package), and of other engines (nerdctl, LXC); a bind through a link
  removed or changed since its container started; a mount that reaches the directory
  through another mount of it; and the tmpfs Podman mounts for an image's `VOLUME` under
  `--image-volume=tmpfs`. Removing the directory breaks such mounts.
- A run that ended at admission (a failed environment or setup) reported "0 recorded
  events" in its network summary; it now counts what the Session's record holds.
- The `sandbox.egress` warning under `sandbox.network: bridge` said `allow` was
  ignored; loadout runs under `network: egress`, as every built-in loadout is, use it,
  and the warning now says so.
- Root's commands in a Session container (its readiness checks and user setup) ran
  programs found on the image's `PATH`, which can name a directory the workload users
  write; they now run with a `PATH` of system directories only.
- On Linux, `axocoatl connect claude-code` and `connect codex` no longer type a line
  break and an end of file into the program's terminal when they stop relaying
  keystrokes (the pseudo-terminal library's writer did this when it was dropped, which
  happens when the relay stops, normally once the program has exited). The terminal
  echoed that line break into the output still being read: an extra blank line after the
  program's last line, or after the Podman error line held back for the message, always
  while a process the program left behind held the terminal and now and then
  otherwise. On macOS the terminal is gone by then.
- **A copied or moved data directory starts again.** The data root's format ownership
  record, `.axocoatl-daemon.lock/format.json`, names the directory it was written in: the
  SHA-256 of its path, the device and inode of the directory, and those of the retained
  lock file `.axocoatl-daemon.lock.v1`. A copy usually has new inodes (an in-place
  `rm -rf` and `cp -a` on ext4 can get the same ones back, and such a restore was not
  refused) and a moved directory a new path, so 1.1.0 through 1.4.0 refused a data root
  that was restored from a cold backup with `cp -a` (as
  [Data and backup](https://docs.axocoatl.ai/operate/data-backup/) says to make one),
  brought to a new Mac by Migration Assistant or Time Machine, or moved: `Failed to
  bootstrap daemon: … failed data-directory format ownership admission: invalid or
  ambiguous format ownership: unsupported, incomplete, or differently owned manifest`.
  An installed service restarted and failed every 10 seconds without end, while
  `axocoatl doctor` passed. When only those identities differ and every other field and
  the shape of the record are exact, the daemon now records the directory's new location
  once it holds all of the root's locks (its external lease, the lock file and the lock on
  the directory itself), keeps the ownership id that every Session's journals are bound
  to, and logs one warning that the data directory was copied or moved. The new record is
  written and synced at `.axocoatl-daemon.lock.rebind` in the data root, then renamed over
  `format.json`, so the record's directory never holds anything but one complete record; a
  start after an interruption removes the leftover file. Every other difference (another
  format, schema, migration or protocol, a missing, nil or non-canonical ownership id,
  identities this version would not write, an extra file, a symbolic link) is still
  refused, and leaves the directory unchanged. The lock path stays a directory, so 1.0.0
  still refuses the root. After a 1.5.0 start has recorded a copied root, 1.4.0 starts on
  it too.
- **Copied and moved data roots.** A data root copied while a 1.0 Session storage upgrade
  was interrupted refused to start. The upgrade now finishes at the new location when it
  had not started converting a Session. A copy taken after a conversion started is
  refused with what to do, and `axocoatl doctor` says which case applies. A data root
  moved to another path now removes, as its own, the Podman volumes its closed and
  deleted Sessions left at the old path. Those volumes carried the old path's runtime
  authority and were never removed. It also removes the SearXNG container a daemon killed
  at the old path left, once it has exited or while no daemon runs at that path. A copy
  (moved to another volume, or a backup restored at another path) never takes its
  original's volumes for its own, even though they are named after the same Sessions, and
  even when its first start was cut short. `axocoatl doctor` counts those volumes. Once
  the original is deleted, `axocoatl session record-location --original-deleted` lets the
  copy's next start remove them as a moved root's. The docs show how to remove the
  volumes of a data directory that was deleted.
- **A Session keeps working in a repository restored or copied at the same path.** After a
  restart, the first turn sent to an existing Session checks that its repository is the
  one its last turn recorded, by path and by the folder's device and inode. A repository
  restored from a backup, or brought to a new Mac at the same path, is a new inode, so
  every later turn of every Session in it was refused with `repository reattachment
  changed its durable Session, Workspace, environment, backend, runtime, path, or inode`.
  The folder's inode may now differ; its path, the Session, the Workspace, the
  environment and the runtime must still be the same. A Session whose folder is gone or
  moved is still refused, naming the folder, as in 1.3.2.
- **Safari opens the workbench from a bookmark at `127.0.0.1` or `localhost`.**
  - Since the per-daemon token (1.1.2) the workbench moved `127.0.0.1` to `localhost`, a
    different site. Safari leaves the SameSite=Strict sign-in cookie off a navigation
    redirected from another site, and off its reloads, so Safari showed "Sign in to
    Axocoatl" there while Chromium opened the workbench.
  - The workbench is now served at `127.0.0.1` (see Security), and a `localhost` bookmark
    is moved there.
  - A sign-in also sets `axocoatl-token-<port>-signed-in=1` (HttpOnly, SameSite=Lax, no
    secret, never a credential).
  - When a navigation you started yourself (`Sec-Fetch-Site: none`) reaches `/` with that
    cookie but without the token cookie, the daemon answers with a page that opens the
    same address again from `127.0.0.1` itself, which sends the token cookie. That page
    clears the signed-in cookie, so this happens at most once.
  - A link from another site still gets the sign-in page.
- **Preview WebSockets stay open beside a cookie that is not ASCII.** The Preview proxy's
  WebSocket handshake to the app could not carry such a cookie and failed. So in Safari,
  one Preview page that set `junk=é; Domain=localhost` stopped the live reload of every
  Session's Preview. The handshake now forwards only the cookies that are visible ASCII;
  plain HTTP requests still forward every cookie but Axocoatl's.
- **The workbench no longer reloads itself without end when its API answers 401 but `/`
  does not.**
  - The page replaced itself with `/` on every API 401, because `/` then serves the
    sign-in page.
  - In Safari on macOS a page can make `/` keep serving the workbench while the API
    answers 401. Safari sends at most about 200 KB of cookies with a request: longest
    path first and, among cookies of one path, those set in a later second first, then by
    name. It skips each one that no longer fits.
  - So cookies that a page on another `127.0.0.1` port sets with `Path=/api`, 51 of 4,000
    bytes and then smaller ones that fill what is left, leave the sign-in cookie off API
    requests. The 51 large ones alone still leave room for it.
  - The page then reloaded without end: 167 times in 20 seconds in Playwright WebKit
    26.4.
  - It now reloads at most once a minute, and only when it can record that in the tab's
    session storage. A journey with a fake clock pins the minute.
  - Otherwise it shows "This browser is signed out" over the page, with how to sign in
    again: close the page that set the cookies, remove this browser's website data for
    `127.0.0.1` (in Safari > Settings > Privacy > Manage Website Data it is one record
    whose removal removes those cookies, measured with the system's WebKit on macOS
    26.6), and open the sign-in link again.
  - The message now closes any open modal dialog and keeps another from opening over it.
    Before, it sat hidden under one.
  - The same cookies with `Path=/ws` left the live connection reconnecting, with nothing
    else said. After three failed attempts in a row the workbench now asks `/ws` with an
    ordinary request, and that request's 401 shows the same message.
  - With `Path=/` the cookies keep the sign-in cookie off `/` as well. The sign-in page
    now names that cause and the same remedy.
- **A theme change while the file editor loads no longer takes its suggestions away.**
  The vendored Monaco sets `window.monaco` before the chunk that registers the services
  of suggestions, code actions, code lenses, inlay hints and drop into editor has
  loaded, and the workbench applied a theme change through it. A change at that moment
  (the system turning dark, or the theme control) while a page opened its first file
  started Monaco's services without them, and every editor of the page lacked those
  features until the page was reloaded. The workbench now gives Monaco a theme only
  once Monaco has loaded; Monaco takes the page's theme when it finishes.
- **A workbench terminal closed while xterm.js still had work queued no longer throws.**
  - xterm.js 5.3.0 queues viewport work it never cancels: a timer after the terminal
    opens, and a frame after `reset()`. That work reads the renderer that `dispose()`
    removed.
  - A terminal disposed with that work pending threw "undefined is not an object
    (evaluating 'this._renderer.value.dimensions')" in WebKit as an uncaught page error.
    After a cross-tab Close this turned CI's WebKit leg red (push run 39911558815).
  - The workbench now detaches the terminal at once, and disposes it after its written
    output is parsed, a timer turn and two frames.
  - A journey destroys a real terminal right after it opens, right after its socket
    drops, and mid-resize. It failed with the old code in Chromium and WebKit and passes
    with the new.
- **Knowledge offers New note and Import Markdown only once it has read the notes.** A
  click before the first read came back threw in WebKit and showed nothing.
- **Workbench and loadout turns on Ollama no longer end "done" when Qwen3-Coder writes a
  tool call as text.** `qwen3-coder:30b` on Ollama 0.40.8 sometimes writes a call without
  its opening `<tool_call>` tag, and Ollama returns it as the answer, so the turn ended
  "done" without running it. Both Ollama providers, native and OpenAI-compatible, now
  read such calls the same way: a run of complete calls in Qwen3-Coder's own
  `<function=…>` format that ends the answer runs when it starts at the first line that
  begins with a call tag outside a code fence or an HTML block (`<pre>`, `<!--`,
  `<script>` …), every call names a tool the turn offered and its values fit that tool's
  parameters (types, required parameters, `enum`). An unfenced example of that format
  that ends an answer therefore runs. Markup in a fence, an HTML block or indented code,
  a call tag after other text in inline code or mentioned in a sentence, markup inside
  another call's value, and markup that names only tools the turn did not offer stay
  text. An answer that calls an offered tool in a way that cannot run is refused: a line
  that starts a call but no runnable run (prose after the calls, a malformed block, an
  unoffered tool next to an offered one, values that do not fit, an answer cut at its
  output limit inside a call), a call block written after other text on its line
  (`Looking.<function=glob>` and its parameters), or a call written as JSON that ends the
  answer. Nothing from it runs; the model is asked once more (below), and when that
  answer cannot run either, the activation fails with the reason, Ollama's done reason
  and the number of tools offered. The workbench says "The model wrote a tool call as
  answer text that Axocoatl cannot run", the turn needs attention instead of ending
  "done" with nothing run, and the Outcome of `axocoatl run` lists it as
  `unrunnable_tool_call`, not as a provider failure. Text from a line that starts a call
  appears only when the response completes, and a `<tool_call>` that ends the prose stays
  hidden until the next line shows whether it opens a call, also when Ollama streams it
  token by token; a call tag mentioned inside a line no longer holds back the rest of the
  answer. When a response also has a call Ollama returned as a tool call, only those
  calls run and the markup stays text. Nothing is read from the text when the request set
  stop sequences or asked for JSON. Numbers in such calls are typed only from exact JSON
  literals (`0755` stays text, as does an integer beyond 64 bits).
- **No JSON in an Ollama answer runs as a tool call.** The OpenAI-compatible Ollama
  provider ran `{"name": …, "arguments": …}` objects and `<tool_call>{…}</tool_call>`
  blocks found anywhere in an answer, a fenced example too, and the actor ran any
  `{"tool": {…}}` object in an answer from either Ollama provider. The actor's ids for
  those (`Axo000000`, …) started again in every response, so a second such call in a
  native Session made every later request fail ("invalid or duplicated native history
  tool call"). A JSON call to an offered tool that ends the answer (the whole answer, its
  last lines, or a `<tool_call>` block; its arguments an object or a string holding one)
  is now refused as above; JSON in a code fence or followed by more text stays the
  answer. A call read from text gets an id that is unique in the Session
  (`call_text_<16 hex digits>_<n>`), also after a round was dropped to fit the context.
  This changes what Qwen2.5-Coder can do: `qwen2.5-coder:14b` wrote its calls as a bare
  JSON object in Axocoatl's tests in July 2026, which 1.0.0 through 1.4.0 ran through the
  OpenAI-compatible provider; 1.5.0 refuses them. Onboarding no longer suggests it, nor a
  model whose Ollama `capabilities` lack `tools`, and says why; `axocoatl doctor` warns
  about it for an Agent that lists tools; the docs' examples use `qwen3-coder:30b`.
- **A native Ollama response that cannot be used is asked again once.** A completed
  response with no answer and no tool call (`gpt-oss:20b` can end one with only
  reasoning, and an Ollama runner that ran out of GPU memory answers so too), a call to a
  tool nobody offered under a name that cannot be answered as a tool error (Harmony's
  `browser.search`), or a tool call written as text that cannot run (now also an answer
  that ends with a bare `<tool_call>`) is refused, with its usage settled. The model is
  then asked once more within the activation, with a note that says what the response
  held, as a call admitted and budgeted on its own and recorded as a provider retry; the
  note is not kept. A second such response, or one the budget cannot ask again, fails the
  activation: a text call as `unrunnable_tool_call`, as before, anything else as the new
  `no_answer` ("The model gave no answer and no tool call Axocoatl could run"), not as a
  provider failure. A required reviewer's round still runs again once after that.
- **OpenRouter refuses a Qwen tool call returned as answer text.** A complete standalone
  function/parameter envelope naming an offered tool, with no native tool call, now
  uses the same one-retry path with usage and cost settled. A repeated malformed
  response stops as `unrunnable_tool_call`. The text is never executed and no tool id
  is invented; quoted examples and responses with native calls keep their meaning.
  The regression replays the response captured during service onboarding and exercises
  both recovery to a real native call and refusal after a second malformed response.
- **An Ollama model that another program unloads while Axocoatl reads its context is
  loaded again.** To read a native Ollama model's context, Axocoatl loads it and reads
  `/api/ps`. When another program on the same Ollama loaded a model in between, Ollama
  unloaded this one, and a run was refused (exit code 3) or Team & budget's Apply failed
  with "the exact local model was not listed". The model is now loaded again, three loads
  in all. Past that the refusal says "Ollama unloaded MODEL each of the 3 times Axocoatl
  loaded it to read its context", names what `/api/ps` lists instead (or that nothing is
  loaded, with the keep-alive setting) and what to do, and a run's admission classes it
  as infrastructure (exit code 5).
- A run of a loadout whose writer changed no file of the repository has the new warning
  `run_changed_nothing` ("The run changed no file of the repository: its writer (writer)
  made no change that the result keeps"), so a check that failed is seen to have run on
  the repository as it was. It never changes the verdict.
- **How each model response ended is recorded.** Every completed model response of a
  native Session activation adds a `provider_response` event to the activation's stream:
  how it ended (`stop`, `tool_use`, `max_tokens`, `content_filter` or `error`), how many
  tools its request offered and how many calls it returned. History, its JSON and
  Markdown export, the run's record bundle and the workbench's observed route show it ("1
  tool offered · 1 call"). Axocoatl 1.4.0 does not read these records: see "Going back to
  1.4 is the backup" under Changed.
- Native Ollama requests no longer send an empty `stop` list, which replaced the stop
  strings of the model's Modelfile (`qwen3-coder:30b` has `<|im_start|>`, `<|im_end|>`
  and `<|endoftext|>`).
- **The default Lead prompt asks for tool calls first.** After a sentence that announces
  a call, `qwen3-coder:30b` samples a plain `<` about as often as its `<tool_call>` token
  and then writes the call as text. In 96 replays of a workbench Lead's first request on
  Ollama 0.40.8, the numbered prompt that onboarding and `axocoatl init` wrote led to a
  call written as text 12 times; the new prompt ("Answer only with tool calls until the
  change is made and checked; then write a short summary of what you changed. You own
  this change: …", every earlier step kept) did so none of the times. Onboarding,
  `axocoatl init`, the example configurations and the docs use it, and
  `axocoatl onboard --update` writes it into a Lead that onboarding wrote; a Lead you
  edited keeps its prompt.
- The opt-in live Ollama tools test (`AXOCOATL_TEST_OLLAMA_TOOLS=1`) accepts a model that
  calls the tool more than once (`qwen3-coder:30b` did in 1 of 4 runs). It checks for a
  call with the requested value, one model call per tool round plus the answer, and the
  answer.
- **Requests to this computer never go through a proxy.** reqwest, the HTTP library of
  every Axocoatl client (rmcp's for MCP servers over HTTP too), sends requests through
  `HTTP_PROXY`, `HTTPS_PROXY` and `ALL_PROXY`, even to `127.0.0.1` and `localhost`,
  unless `NO_PROXY` names the host. So with `HTTP_PROXY` set, `axocoatl run` and
  `connect codex --refresh` sent their requests to the local daemon, with its API token,
  to the proxy in plain text, and failed, and so did doctor's and onboarding's daemon
  checks, the Ollama probes and model listing, the OpenAI-compatible Ollama provider,
  the workbench's Preview proxy, and a provider, webhook or MCP server at a `localhost`
  address (such as LM Studio's `base_url`). When a proxy variable is set, `axocoatl` now
  adds `localhost`, `127.0.0.0/8` and `::1` to `NO_PROXY` and `no_proxy` for itself,
  after what they list, before anything else runs; the daemon client, the health checks,
  the Ollama clients and the Preview proxy also ignore the proxy variables themselves.
  Requests to Ollama never use a proxy, wherever it runs; requests to other hosts still
  follow the variables. Sessions and runs do not support a proxy: the egress broker
  connects directly.
- **`axocoatl connect codex` says when its sign-in time ran out on Podman 6.1.** For a
  few seconds after the login container's `--timeout`, Podman 6.1.4 still lists the
  container while `container inspect` no longer finds it. The command said
  `Codex's auth.json could not be read from the login container (exit status: 125)` and
  exited 5. It now waits up to 10 seconds for Podman to remove the container, then says
  the sign-in was not completed within the time, with exit 2. After a confirmed
  timeout, Podman's `container has already been removed` teardown message is hidden
  too; an early container removal or another error remains visible.
- **A run stops its Session's containers when it ends.** A finished run, a run that ended
  at its environment and a run whose driver failed left the Session's container and
  egress sidecar running until the daemon stopped. An error after admission made the
  Session left the Session open. Now the runtime stops at the end of every run, before
  the network summary, the receipt and the Outcome. The Session stays open for Keep, the
  record bundle and the workbench, which starts the container again when it needs it. A
  stop that fails is the warning `session_runtime_not_stopped`, and a run refused after
  its Session was made closes that Session.
- **Ctrl-C while the daemon is still admitting a run no longer leaves its Session
  running.** Interrupting `axocoatl run` during the Session's setup or the check probe
  dropped the admission with the request. The Session's container and egress sidecar then
  ran until the daemon stopped, no run was recorded, and a retry with the same request id
  was refused as "being admitted" until a restart. The daemon now finishes the admission
  without its client: a refused run is cleaned up, and an admitted one stops before its
  first turn as interrupted, with its containers stopped. `axocoatl run` exits at once
  with code 6 and says what the daemon does. Checked on Podman by interrupting a run
  during its setup and during its check probe. A run whose record was made before a later
  admission write failed now ends in the record as failed, instead of being listed as
  preparing until a restart.
- **`axocoatl run` checks a run before it builds a toolchain image.** A run that could
  not start, such as `fix-claude-code` without `axocoatl connect claude-code`, first
  built its image (the claude-code and Python image is 894 MB) and was then refused with
  exit code 3. The command now asks the daemon's dry run first, without loading a model,
  and refuses such a run with nothing built.
- **A run said "model identity or endpoint availability differs" when OpenRouter reported
  every endpoint of the model down.** On 2026-10-23 `qwen/qwen3-coder` had no healthy
  endpoint (status -5 and -2, 0 to 93% of requests succeeding over 30 minutes), and runs
  failed with that message. An endpoint that fits but has a status other than 0 is now
  told apart. When every such endpoint is unavailable or degraded, the run stops before
  any model call (exit code 5). It names each endpoint's status and success rate, says
  this is OpenRouter's status and not the setup, and says to try later or choose another
  model. An endpoint under another model id is said as that. A degraded endpoint is never
  used, even when no endpoint is healthy: it fails part of its calls, some part way and
  after they cost credits. When a Session's endpoint is reported degraded later, its
  calls move to another endpoint (below), where the Session said "review a new profile".
- **OpenRouter key problems at a terminal no longer end onboarding's Step 3.** If
  OpenRouter refuses a key you connected, or a stored secret holds no OpenRouter key,
  onboarding now offers its own browser sign-in instead. Before, it left a step telling
  you to run `axocoatl connect openrouter` and onboarding again. When the sign-in
  replaces a key written in the configuration, the backup leaves that key out (onboarding
  says so), and the file's comment says the key is in the secret store. `--plan` no
  longer makes the secret store's directory owner-only while it reads it.
- **Below 48 GB, a Mac whose Ollama answered at first but was not ready after setup** was
  offered the built-in pair "not tested at this memory size", and told to pull models it
  already had. It is now offered the pair it has.
- **Long questions and menus no longer leave stale lines.** Questions wider than the
  terminal, in onboarding and in `axocoatl connect openrouter`, no longer print their
  first line twice. A menu item wider than the terminal no longer leaves stale lines when
  the menu redraws: each item is listed whole above a menu cut to the terminal's width,
  and the answer is shown whole.
- **A Podman machine Axocoatl starts outlives what started it.** Onboarding,
  `axocoatl dev`, runs, `recipe build`, `connect codex` and the Always-On Service run
  `podman machine start` in a session of its own. A Ctrl-C, a closed terminal, or
  stopping or restarting the service no longer kills the VM. On macOS, vfkit and gvproxy
  kept the starter's process group, and launchd ends a stopped job's group. Before a
  start, Axocoatl removes the API socket a killed VM left behind when nothing listens on
  it. That socket made the next `podman machine start` fail with exit 125 ("bind: address
  already in use"). Onboarding says when it removed one. A daemon that shuts down while
  it starts the machine no longer ends the start half-way.
- **`axocoatl service` start, stop and restart.** On macOS, `service start` failed on a
  loaded agent ("Bootstrap failed: 5: Input/output error"). Choosing the Always-On
  Service at onboarding Step 6 while it ran therefore ended not ready. `service start`
  now leaves a running service running and loads again one that is loaded but does not
  run. `service stop` and `service uninstall` return once the daemon has exited. New
  `axocoatl service restart` loads the installed definition (on systemd it runs
  `restart`). Step 6 installs the service and restarts it, asking first when one runs.
  Doctor, `axocoatl run`, the installer, onboarding and the troubleshooting pages give
  `axocoatl service restart` where they said to run `service stop` and `service start`.
- **`service start` and `service restart` say whether the daemon started.** They said
  "started" and exited 0 while the daemon failed at bootstrap and launchd or systemd
  started it again and again. They now wait up to 60 seconds for the daemon to say it
  started. They exit 1 with the line it logged when its start fails (a data directory it
  refuses, an invalid configuration, a port or the IPC socket in use), or when it exits
  without saying why. Onboarding's Step 6 shows that line. On Linux they first clear
  systemd's count of recent failed starts, which refused a start after five within 10
  seconds, so a restart right after fixing the cause starts the daemon. Run live with
  launchd on macOS 26.6 and with systemd 255 on Ubuntu 24.04.
- **A stopped service gets time to end its Sessions.** launchd killed the daemon 5
  seconds after asking it to stop, because the LaunchAgent set no `ExitTimeOut`. The
  LaunchAgent now sets 60 seconds, the most launchd allows: on macOS 26.6 a larger value
  is listed and applied as 60. systemd gives 90 seconds, its default. A daemon killed
  after that leaves Session containers, which its next start removes.
- **The Always-On Service starts on Linux.** The systemd unit quoted its
  `WorkingDirectory=`, which systemd refuses ("path is not absolute"), so the service
  never started (Ubuntu 24.04, systemd 255). `service start` still said it had, because
  `systemctl enable --now` exits 0 when the start fails. Now the path is written as
  systemd reads it, and `service start` fails when systemd cannot start the unit. Install
  refuses a configuration directory whose path ends with a backslash, which would join
  the next unit line to it. `service stop` leaves a unit that does not run as it is. A
  unit written by an earlier release needs `axocoatl service install` and
  `axocoatl service restart` once.
- **The first run does not use an older daemon.** Every release before 1.5.0 answers
  `/health` without a version. Onboarding's first-run handoff took such a daemon for
  another program and used it, so a service or `axocoatl dev` still running an older
  axocoatl got the first run. Onboarding now asks to restart the service, or to stop that
  daemon. It first installs the service again from this axocoatl when the service runs
  another program. It uses the daemon only once it answers as this version and started
  after the configuration changed. When the daemon's process is not visible, it waits on
  the port. It waits 10 seconds for a daemon's `/health` (it was 3), so a slow daemon on
  a loaded computer is not taken for another program.
- **A socket path too long for the daemon says so, with the fix.** The daemon's IPC
  socket is at `~/.axocoatl/run/axocoatl.sock`, or at
  `$XDG_RUNTIME_DIR/axocoatl/axocoatl.sock`. When that path is longer than a Unix socket
  path may be (103 bytes on macOS, 107 on Linux; a long home directory does this),
  `axocoatl dev` and `serve` stopped with "path must be shorter than SUN_LEN" and
  "Another Axocoatl daemon may already be running." Now:
  - They say the path is too long, give its length and the limit, and say to set
    `AXOCOATL_SOCKET_PATH` to a shorter path (or use a shorter home directory or
    `XDG_RUNTIME_DIR`).
  - They make no directory for that path.
  - They say another daemon runs only when one answers on the socket.
  - `axocoatl doctor` fails its Daemon line with the same reason instead of saying to
    start a daemon.
  - Onboarding starts no first-run daemon and installs no Always-On Service that could
    not listen. When a first-run daemon stops at once, the step left quotes the line it
    logged when that line is a failed bootstrap or a failed reservation of the IPC
    socket.
  - `axocoatl service install` records an absolute `AXOCOATL_SOCKET_PATH`, and warns when
    the service's daemon could not listen. `axocoatl service status` shows that failure
    as the last start failure.
- **A slow Podman no longer stops the daemon's start.** Before it starts, the daemon asks
  Podman which Session containers an earlier start left, and it still does not start
  without an answer.
  - Before: a `podman ps` slower than its 10 s bound (seen in a loaded Ubuntu VM) ended
    the start with "early bootstrap could not prove containers exposing control-plane
    root '…' absent: Tool execution timed out after 10s".
  - Now: each Podman command keeps its bound. A command that does not answer within it is
    asked again, for up to 3 minutes in all. Any other Podman error still stops the start
    at once.
  - When Podman never answers, the start says how long it waited, which command did not
    answer, that Podman may still be starting or be held by another Podman command, and
    to check that `podman ps` answers.
  - Measured: Podman 4.9.3 (Ubuntu 24.04) held `podman ps` for 69 s while another
    `podman rm` deleted a container with 600,000 files. Podman 6.1.4 in a Mac's Podman
    machine answered within 4.4 s while an image loaded and containers came and went.
  - A Podman readiness probe that times out now says "Podman did not answer `podman info`
    within 15 s" instead of "Tool execution timed out after 15s".
- **Tool-call numbers reach the tool exactly.** serde_json's default float parser landed
  1 ULP off for about a third of random f64 values. `float_roundtrip` is now on. A native
  Ollama test of 2,048 values (576 changed before) and a property test pass. The embedded
  execution supervisors were rebuilt with it; three builds in different directories gave
  byte-identical payloads (`docs/EXEC_SUPERVISOR_BUILD.md`).
- **The installer works with the curl snap.** On Ubuntu 24.04 without curl, the shell
  suggests a snap (curl 8.22.0). With it, the installer failed with "client returned
  ERROR on write", because the snap has its own `/tmp`. The installer now writes what
  curl downloads itself. Measured in an Ubuntu 24.04 VM with only the snap.
- **An expired budget says how to renew it, and Edit renews it.** Every budget has an
  expiry. Team & budget suggests 24 hours ahead when it fills in an Agent, and a run's
  Team continued in the workbench expires when the run's `wall_clock` ends (60 minutes
  for `fix`). A request after that used to fail with "approved Session grant expired
  before Begin; review the team budget", and Team & budget then refused the Team as it
  was ("NAME's budget expiry has passed; choose a time in the future") until a new time
  was typed for each Agent. Now:
  - The request is refused before any model is asked and says "NAME's budget expired
    before this request could start; open Team & budget and select Edit: it fills in a
    new expiry. Then Preview and Apply".
  - Team & budget says when the budget expired. Edit fills in a new expiry, 24 hours from
    then, and says so. Preview shows it beside the approved one before Apply approves it.
    The limits and the Agents' conversations are kept.

  After an upgrade from 1.4 the budget has usually passed, so this message comes before
  the 0.20.6 one. The daemon's refusal of a passed expiry names the Agent, where it said
  "Enter an explicit budget expiry for every Agent". A daemon test lets a budget expire,
  checks the refusal and that no model is asked, renews it as Edit does, and runs the
  next turn. A browser journey renews an expired budget through Edit, Preview and Apply,
  in Chromium and WebKit.
- **A Team whose model, Ollama or OpenRouter endpoint changed is observed again when
  applied.** A Team keeps the provider profile each Agent and helper was reviewed with.
  Its turns used to fail, and kept failing after an unchanged Apply:
  - "observed local model identity changed; resolve a new execution profile" or "observed
    local model capabilities changed": the Ollama model was pulled again (another
    digest), is listed under another name, or reports other tool-calling or thinking
    support.
  - "configured Ollama endpoint differs from the admitted native profile":
    `providers.ollama.base_url` names another Ollama.
  - "the retained OpenRouter endpoint, capabilities, price ceiling or token limits
    changed; review a new profile": OpenRouter changed the endpoint (a price, a limit,
    its parameters) or removed it.

  Each refusal now says what changed and what to do:
  - "MODEL was pulled again since this Team was reviewed (digest …, now …); review Team &
    budget again: applying the Team observes the model again". A model listed under
    another name or reporting other capabilities is said the same way.
  - "providers.ollama.base_url names URL, but this Team was reviewed with the Ollama at
    URL; review Team & budget again: applying the Team observes the model at the endpoint
    configured now (or set providers.ollama.base_url back and restart Axocoatl)".
  - "OpenRouter lists other terms for MODEL's endpoint TAG than when this Team was
    reviewed (input price $1/M, now $2/M); review Team & budget again: applying the Team
    observes MODEL's endpoints again". Also when OpenRouter no longer lists the endpoint,
    or it can no longer be used.

  An unchanged Apply observes such an Agent or helper again, and Preview gives the reason
  (`observed_again` in `POST /api/sessions/{id}/team/preview`). An OpenRouter Agent
  continues on the cheapest endpoint that fits its budget. To find out, Preview reads
  only the provider's metadata: nothing is loaded or sent to a model, and a provider that
  has not answered within 15 seconds is not taken for a change. An OpenRouter endpoint
  that is only unavailable right now, or a model that no endpoint can serve, is reported
  as before and not observed again.

  Of the changes during an Agent's work, only a change in vision support now says "Ollama
  reports other capabilities for MODEL than when this Agent's work started; send the
  request again"; a change in tool calling or thinking gets the refusal above. All three
  used to say "local model capabilities changed; reconnect the execution profile". Daemon
  tests restart the daemon on another Ollama endpoint, pull the model again, and change
  an OpenRouter endpoint's price, then check each refusal, the re-apply and the next
  turn.
- **A Session's next request after a restart no longer holds its Workspace when it cannot
  start.** After a daemon restart, a Session whose last turn had ended held its Workspace
  from its next request until that turn ended. When the request then failed to start, for
  example on a provider refusal such as the 0.20.6 message, every later `axocoatl run`
  and other Session in that repository was refused as busy until the Session was closed
  or the daemon restarted. The Session is now between turns, as after a turn that ended
  in the running daemon. A daemon test restarts the daemon and checks this.
- **A rate-limited or failing OpenRouter endpoint no longer ends the call while another
  endpoint of the model can take it.** Native OpenRouter Sessions pinned the cheapest
  endpoint that fits an Agent and retried a failure once after 2 s. On 2026-10-23, 5 of 6
  runs of the default OpenRouter setup ended needing attention because the reviewer's
  endpoint (`openai/gpt-oss-120b` on Near AI) answered HTTP 429 "rate-limited upstream"
  to the call and its retry. OpenRouter's catalog, read about an hour later, listed 20
  other endpoints of the model as healthy.
  - Team & budget retains, cheapest first, up to three more healthy endpoints of the
    model that can take over each call: at least the selected endpoint's context and
    prompt limit, the same output limit and reasoning setting, and the smallest call
    within the approved budget. Each activation adds, as it opens, every other endpoint
    OpenRouter lists healthy then that passes the same checks.
  - A call an endpoint fails before producing anything is sent at once to the next
    endpoint that can carry it and that the grant can pay for: an upstream 429, a 408 or
    5xx, a timeout or closed connection, a 429, 408 or 5xx reported inside an answer that
    had produced nothing, an endpoint OpenRouter reports degraded now, or one whose
    contract changed. The moved call is a new call reserved at that endpoint's own
    prices, and that endpoint is checked against the catalog before the call is sent.
    Each request still pins one endpoint with OpenRouter's fallbacks off, and an answer
    that names another provider is refused.
  - Budgets are never raised. A move the grant cannot pay for is not made, and the call
    falls back to the retry on its own endpoint. A later call the grant cannot pay for on
    the endpoint calls moved to goes to the endpoint that reserves least.
  - OpenRouter's limit on the account (a 429 that names no upstream) is waited out on the
    same endpoint. When no endpoint is left, the Session waits once for the longest
    `Retry-After` the endpoints that failed the call stated (at most 30 s; 2 s without
    one). It then tries them again, starting on the last one. An endpoint that asked to
    wait longer than 30 s is not sent to again for that call.
  - When every endpoint fails, the error says that OpenRouter reports every endpoint of
    the model that the Agent may use as rate-limited or unavailable right now. It names
    each endpoint with its failure, says this is a provider outage and not the setup, and
    says to try again later or choose another model. It never quotes OpenRouter's raw
    JSON, its hint to add a provider key (BYOK, which native Sessions refuse), or an
    error page that is not OpenRouter's JSON.
  - The run record shows each move as a provider retry that names both endpoints. The
    run's notes, the Outcome's new `openrouter_endpoints` and the receipt's
    `program_version` (`openrouter endpoint deepinfra/bf16 (DeepInfra)`) name the
    endpoints that answered each model and the calls moved away from each.
  - Profiles retained by 1.4.0 and by earlier 1.5.0 builds load byte for byte, and their
    activations fail over too.
  - Tested with the real provider and Session boundary against a stub OpenRouter server:
    a move at once on a 429; every endpoint limited, in refusals and inside answers; the
    account limit; a fallback whose price changed; a fallback answering as another
    provider; a move the grant cannot pay for; the longest `Retry-After` of a round; and
    an endpoint asking for 60 s.
- **OpenRouter usage is exact when OpenRouter reports it.** Every measured OpenRouter run
  said "cost unknown (reserved up to $X) ... (known subtotal: some usage was not
  reported)", including runs whose answers all completed. In the records of the
  2026-10-23 runs, the writer's usage was complete and the reviewer's was unknown. Each
  call OpenRouter refused with HTTP 429 had kept its whole reservation as unknown spend.
  - A call OpenRouter refused with an HTTP status before generating anything, or one
    never sent, now settles at zero tokens and $0 and gives its reservation back.
    OpenRouter bills the tokens a provider processed, and such a call processed none.
  - The provider states this where the failure happens. An error inside an answer is
    never taken as free, whatever its message says, since the endpoint may already have
    billed reasoning.
  - Where usage truly was not reported, the usage line names the Agent, the generation
    and how many of its calls (`known subtotal: usage or cost not reported for
    required-review (generation 1): 1 of 4 model calls`). The Outcome's new
    `usage.unreported`, the workbench's run view and the JUnit report say the same. One
    retry now reads "1 provider retry".
- The daemon no longer keeps, for its whole life, the Ollama versions and OpenRouter
  endpoints that served each Session. They are dropped when the Session is closed or
  deleted.
- **The fix for an MLX model works.** Every refusal of a model that Ollama runs with MLX,
  or that is not GGUF, now says `ollama rm MODEL; ollama pull --runner llamacpp MODEL`,
  when ollama.com has a GGUF variant. The `;` lets the pull run if a previous attempt
  already removed the model. The fix warns that removal happens even if the pull fails:
  a model you created yourself is removed for good. It also offers choosing another
  model. This is in doctor, Team & budget, a run's admission and onboarding's list of
  installed models. On Ollama 0.40.8 with qwen3.5:0.8b, a pull with the undocumented
  `--runner llamacpp` flag gave the GGUF variant on a store without the model. With the
  MLX variant installed, the same pull pulled nothing and the tag stayed MLX. After
  `ollama rm` it gave the GGUF variant, which doctor accepted and whose contract probes
  passed. Onboarding, when its own pull brings an MLX variant, now removes it and pulls
  the GGUF one after asking; its repair used to leave the MLX variant in place. Doctor
  and run admission no longer suggest a GGUF pull for an embedding, image-generation or
  cloud model.
- **Git sends its egress proxy credential with its first request**
  (`GIT_HTTP_PROXY_AUTHMETHOD=basic`). Before, each Git connection was first refused with
  `no_credential` and recorded as a refusal.
- **The e2e recipe image builds with Ubuntu 24.04's Podman.** Podman 4.9.3's builder read
  the recipe's heredocs as instructions ("Unknown instruction: SET"). The recipe now
  writes its package files with printf; the lockfile and steps are unchanged. Its image
  name changes, so an e2e image built by 1.4 is built again once.
- **The Ollama watch reports on every release again.** On 22 and 23 October, Ollama's
  installer started no server because it found no systemd, so the watch could not report
  on 0.40.9-rc1 and 0.40.8. The watch now starts `ollama serve` itself, and fails when no
  server of the version under test answers.

### Security
- **Claude Code token capture handles reused terminal cells.** A redraw that reused
  ordinary cells in the token's prefix, body, wrapped rows or suffix could leave text
  unmasked or capture an incomplete token. The filter now reconstructs and masks those
  cells. It also detects uncertain redraws of captured credential cells, keeps the
  affected characters hidden and refuses capture with exit 2 before verification or
  storage, even with `--no-verify`, instead of choosing the earlier token.
- **The route to Ollama on this computer refuses Ollama 0.40's new endpoints.** Its
  exact-path allowlist already refused them; the tests now name them: `/api/codex/…` (a
  proxy that forwards to api.openai.com and chatgpt.com, checks only that the caller is
  on loopback, and is not gated by Ollama's cloud-disabled mode; native Sessions never
  call it), `/api/balance`, `/api/usage`, `/api/experimental/model-recommendations`,
  `/v1/systemone` and `/v1/responses/compact`.
- `GET`/`HEAD /v1/models/<model>` has no body, and a cloud-enabled Ollama proxies it to
  ollama.com for a cloud model: the route now refuses a cloud model named in that path
  (percent-decoded), as it refuses one in a body.
- A local copy of a cloud model (`ollama cp gpt-oss:20b-cloud mygpt`) keeps its cloud
  host under a name no request check can recognize. A Session now opens the route only
  when Ollama on its port reports its cloud features disabled (`GET /api/status`), and
  `axocoatl network reload` refuses to turn the route on otherwise. The route also asks
  `GET /api/status` before every request it sends: it asked only when a Session's network
  opened or reloaded, so cloud features turned on later let such a copy reach ollama.com
  while the Session stayed open. While Ollama does not report its cloud features off, the
  route refuses the request with `ollama_cloud_not_off` (403, or 502 when Ollama does not
  answer), and nothing is sent. Checked from inside a Podman container with a stand-in
  Ollama whose status flips, and against Ollama 0.40.8 with its cloud features off and
  on.
- **`axocoatl run --repo` keeps to the Workspace scope rules.** A run's Session mounts
  its repository read-write, and run admission did not check the folder. So `--repo ~`,
  `--repo /` or a folder inside Axocoatl's data or configuration directory was accepted.
  Admission now refuses a repository inside one of Axocoatl's own directories
  (`workspace_protected`). It also refuses the file system root, the home folder, the
  folders above it and a folder that contains Axocoatl's directories
  (`workspace_too_broad`), unless you opened that folder with Open workspace… and
  confirmed it. Both are usage problems (exit code 3), for runs and dry runs alike,
  reported before anything is created.
- **A broad folder's confirmation is recorded on its Workspace.** Being registered as a
  Workspace counted as the confirmation, but 1.4 registered broad folders without asking:
  Open workspace… had no check, and `axocoatl run --repo ~` registered the home folder.
  Open workspace… now records the confirmation (`broad_confirmed_at`), and new Sessions
  and runs on a broad folder need it. A broad folder that 1.4 registered is refused with
  `workspace_too_broad` until you open it again with Open workspace… and confirm it.
- Dependencies move to zip 9 (calamine 0.37) for RUSTSEC-2026-0367, -0368 and -0369, flaws
  in zip's `extract()`. candle-core 0.8, used for local embeddings, still depends on zip
  1.1.4; it opens `.npz` and `.pth` archives entry by entry and never calls `extract()`,
  and Axocoatl loads its embedding model from safetensors, so `.cargo/audit.toml` records
  those three IDs as not reachable until candle-core moves to zip 9 (candle-core 0.11.0,
  the newest, still requires zip 8.6). A test in axocoatl-memory fails if any file but
  `neural.rs` uses Candle or names its `.npz` or `.pth` readers.
- rustls-webpki moves to 0.103.16 for RUSTSEC-2026-0382 (certificate verification could
  accept distrusted intermediates), and webpki-roots and webpki-root-certs to 1.0.10 for
  RUSTSEC-2026-0381 and -0383 (bundled roots trusted distrusted emSign intermediates).
- The workbench's vendored markdown-it moves to 14.3.3 for GHSA-vfgh-jcmw-g37q and
  GHSA-gfr2-rv44-r23c, two quadratic-time paths when rendering Agent output.
- The workbench's vendored Monaco editor is rebuilt with DOMPurify 3.4.17 for
  GHSA-cg2g-xh64-v4q7, GHSA-mrv2-3pqr-j5jr and GHSA-c6gr-qvv4-5v3f. Monaco's sanitizer
  sets none of the affected `SAFE_FOR_TEMPLATES`, `IN_PLACE` or XHTML parser options.
  The Monaco build record lists all three, and the 35 npm advisories of the Monaco
  source tree's build tooling (1 low, 13 moderate, 21 high, on 2026-10-23), none of
  whose code is in the 151 shipped files.
- The documentation site's build moves to vite 8.3.4 for GHSA-587h-7wmj-h5pm (its
  development server's editor launch on Windows). Nothing of the site's build ships in
  Axocoatl.
- **The workbench is served at `127.0.0.1`, which no `Domain=localhost` cookie reaches.**
  - Safari on macOS lets a page on a `*.localhost` address that runs in a tab of its own
    set a cookie for `Domain=localhost` from script. Safari then sends it to `localhost`
    on every port, where the workbench was.
  - Enough such cookies with `Path=/api`, `Path=/` or `Path=/ws` signed Safari out of the
    workbench's API, or out of the whole workbench, or kept its live connection from
    opening. A forged "signed in" cookie sent each navigation through the retry page.
  - The sign-in link now names `127.0.0.1`: `axocoatl url`, and what `axocoatl dev` and
    `axocoatl serve` print.
  - With the local API token, the workbench opened at `localhost` or `[::1]` answers
    `307` to `127.0.0.1` on the same port, query included. From `localhost` the redirect
    also removes this daemon's two sign-in cookies there.
  - A cookie for a domain never matches an IP address. Measured in Playwright's Chromium
    and WebKit on macOS: the same floods set for `Domain=localhost`, with each path, left
    the workbench signed in, its API answering and its live connection open.
  - Previews keep their `<session>-p<port>.localhost` origins.
  - A loopback listener needs `127.0.0.1` and does not start without it.
  - With the local API token, `axocoatl dev`, `axocoatl serve` and the server refuse a
    `server.host` on another IPv4 loopback address, such as `127.0.0.2`, where the link
    would reach nothing.
  - With configured credentials or `allow_unauthenticated` there is no sign-in cookie and
    no redirect, so a proxy on the same host may send `Host: localhost`.
- **A Session's app can no longer receive the workbench's sign-in cookie.**
  - Podman published Session ports on `127.0.0.1`, which `localhost` reaches too, and
    cookies ignore ports.
  - With no click, a Preview's code could navigate its own frame to its app's published
    port, or nest a frame there, and the browser sent the app the workbench's
    SameSite=Strict token cookie. Chromium did so for the Preview's own frame; WebKit did
    so for either, because it compares only with the top-level page. Measured in
    Playwright 1.64.1's Chromium and WebKit on macOS.
  - Such a frame could also set cookies the workbench receives.
  - Session ports, the egress Preview container's included, are now published on `[::1]`
    on macOS and on `127.0.0.2` on Linux.
  - Measured: rootless and rootful Podman 6.1.4 on macOS, and Podman 4.9.3 on Ubuntu
    24.04 (rootless with slirp4netns and with pasta, and rootful), served the app there
    and nothing on `127.0.0.1`.
  - A journey asserts in Chromium and WebKit that both kinds of frame get none of the
    workbench's cookies. Its WebKit run fails with ports on `127.0.0.1`.
  - Where the computer cannot listen on that address, Axocoatl publishes on `127.0.0.1`
    and logs a warning.
- **The first start of 1.5.0 replaces the local API token once.**
  - Sign-in links before 1.5.0 named `localhost`. A browser that signed in then may still
    hold the token in a cookie for `localhost` for up to 30 days, and every server on a
    `localhost` port receives that cookie, including a Session's app on macOS.
  - The daemon now writes the SHA-256 of the token it issued to `local-api-token.issued`,
    and replaces, once, a token that file does not name.
  - Open the new sign-in link (`axocoatl url`); every browser needs it after this upgrade
    anyway. Give scripts that kept the old token the new one.
- **In Safari a Preview never runs top-level.** Safari on macOS lets a page on a
  `*.localhost` address that runs in a tab of its own set a `Domain=localhost` cookie
  from script, and then sends it to `localhost` on every port, where the workbench was
  before this release, and to every other Session's Preview. Chromium and WebKit on Linux
  refuse such a cookie, and Safari gives an embedded Preview no cookies at all. Before, a
  Preview did run top-level in Safari: from **Open full preview**, from a window its code
  opened when you clicked in it, and from its address opened in a tab. Now the daemon
  answers every request for a Preview page that Safari would show in a tab or window of
  its own with `403` and a short page that says why and links to the workbench, and never
  asks the app. That holds whatever led there: a link, a redirect, Command-click on a
  link inside the Preview (Safari opens a new tab whatever the frame's sandbox says), the
  context menu, or an address typed or pasted. The page runs no script and sets no
  cookie. The Preview in the workbench's pane (`Sec-Fetch-Dest: iframe`) works as before.
  - The daemon recognizes Safari's engine by its user agent: `AppleWebKit` on a Mac
    without a Chromium or Firefox token, and every browser on iPhone and iPad. A Safari
    older than 16.4 does not say whether a page is for a frame or a tab, so it gets the
    refusal page for the embedded Preview too.
  - The workbench says so before you try. It offers no **Open full preview**: the
    control stays disabled, and a line under the Preview's address says why. The Preview
    frame may not open windows (no `allow-popups`), so the Preview's own `window.open`,
    `target="_blank"` links and forms open nothing. No link in the workbench opens a
    `*.localhost` address in a tab of its own: a Markdown link to one in the conversation
    loses its address, and clicks, middle clicks, the context menu and dragging on any
    other such link are refused, as are `window.open` (the file editor's links) and a
    link a program prints in the Terminal.
  - Other browsers keep **Open full preview**, popups and their links.

  Measured in Playwright WebKit 27.2 on macOS: a typed Preview address, a redirect from
  another site to one, and a real Command-click on a Preview link inside the Preview's
  frame and inside the Files view's frame each got the refusal page, and no cookie was
  set. With Playwright's iPhone and iPad user agents, in Chromium and WebKit, a typed
  Preview address got the refusal page and a framed one was served. Not measured in
  Safari itself, on an iPhone or iPad, or in Safari before 16.4.

  A page Axocoatl does not serve that runs top-level on a `*.localhost` address in Safari
  can still set such cookies:
  - a development server you run yourself;
  - a Session's app opened directly at its published port through a `*.localhost` name
    (on macOS its ports are on `[::1]`; the workbench never shows them);
  - a service worker a Preview registered while it ran top-level before this release.
    Measured once in Playwright's WebKit: it answers a later top-level navigation without
    the server. Removing all website data, as History > Clear History does, removes it
    (measured with the system's WebKit on macOS 26.6).

  Those cookies reach other apps on `localhost` addresses. They never reach the workbench
  at `127.0.0.1`, and they reach a Session's Preview only in a tab of its own, which the
  daemon refuses in Safari.

  A page on another `127.0.0.1` port, such as a development server you run there, still
  sets cookies the workbench receives, in any browser ([Security: Pages on other 127.0.0.1
  ports](https://docs.axocoatl.ai/operate/security/#pages-on-other-127001-ports);
  measured in Playwright WebKit on macOS):
  - It cannot sign in, or read the workbench's HttpOnly cookies.
  - In Safari, 51 cookies of 4,000 bytes with `Path=/api`, then smaller ones that fill
    what is left, sign that browser out of the API.
  - The same cookies with `Path=/` sign it out of the whole workbench until the link is
    opened again.
  - With `Path=/ws` they keep the live connection from opening.
  - The workbench and its sign-in page then say what to do: close that page, remove
    `127.0.0.1`'s website data (Safari > Settings > Privacy > Manage Website Data, where
    it is one record), and open the sign-in link again.
  - Like any page open in Safari, it can sign out a browser that signed in a few seconds
    earlier and has no workbench page open, by writing about 1,000 cookies at once.
- **The legacy Preview path no longer serves a browser a page, and forwards no app
  cookie.** `/api/sessions/{id}/proxy/{port}` reaches a Session's app from the
  workbench's own origin, behind its sign-in. A link to it, which the workbench's Safari
  guard does not refuse because it is not a `*.localhost` address, opened the app's page
  there. The page ran with an opaque origin (the path sends
  `Content-Security-Policy: sandbox` without `allow-same-origin`), so its script could
  not use the workbench's API. But the app's `Set-Cookie` headers were stored for
  `localhost`, then the workbench's host, `Path=/api` included, and the app's redirect to
  its own Preview address opened that Preview top-level, in Safari too. Measured in
  Playwright 1.64.1's Chromium and WebKit on macOS. In every browser the path now answers
  only requests the browser marks `Sec-Fetch-Dest: empty` that are not navigations
  (`fetch()`, `XMLHttpRequest`, `EventSource`, `navigator.sendBeacon`) and programs that
  send no Fetch Metadata. A page, frame, embed, object, fenced frame, script, style
  sheet, image, worker or anything else a browser would show, run or embed there gets
  `403` and a line of text, and the app is not asked. The path forwards none of the app's
  `Set-Cookie` headers.
- **A cookie that is not ASCII no longer signs Safari out of the workbench.** The daemon
  read each `Cookie` header as text and skipped one that was not visible ASCII, token
  cookie included. In Safari on macOS any top-level Preview page could set
  `junk=é; Domain=localhost`, which Safari sent to the workbench, then at `localhost`, as
  the byte 0xE9. The API then answered 401 and the workbench showed "Sign in to
  Axocoatl". Opening a fresh sign-in link did not help while the cookie stayed. The
  daemon now reads the header as bytes, one cookie at a time. Neither such a cookie nor a
  forged `axocoatl-token-<port>` beside the real one, in either order, hides the token
  cookie.
- **A Preview app's server can no longer set a cookie for `localhost` through the Preview
  proxies.** They remove the `Domain` attribute from an app's `Set-Cookie` headers, so
  every browser keeps those cookies host-only on that Preview's origin. Before, Safari on
  macOS accepted `Domain=localhost` from a top-level Preview's server and sent the cookie
  to the workbench, then at `localhost`, and to every other Preview. A cookie whose
  `Domain` named another site, which browsers refused, is now kept host-only.
- The documentation site's build moves to zod 4.6.7 for GHSA-mrhx-c7xc-m3w5 (memory
  exhaustion validating oversized constrained collections), and to astro 7.4.1,
  postcss-selector-parser 7.1.7, postcss-nested 7.0.2 and @expressive-code/core 0.44.3,
  which also clear GHSA-mpr7-6m5w-76rw (CSS injection in View Transitions, which the
  site does not use) and GHSA-rj75-hqrm-r3gf (quadratic selector parsing). Nothing of
  the site's build ships in Axocoatl.
- tokio moves to 1.53.3 for RUSTSEC-2026-0386 (`AsyncFd`'s safe constructors violate I/O
  safety; Axocoatl does not call them). With tokio 1.53.1, `cargo audit -D warnings`
  failed.

### Verified
- **The release gate requires the release commit to go from the published install command
  to passing, signed runs** (`docs/RELEASE_GATE.md`), starting without Axocoatl, on
  GitHub's Ubuntu runners, on a Mac and on Linux. Its evidence check refuses the release
  until the bundle under `release-evidence/1.5.0/`, signed by a key in
  `.axocoatl/gate-keys`, holds every required path with each step the gate lists for it,
  passed. The only way past it is an override that only the GitHub logins listed in
  `.axocoatl/gate-override-actors` can start, and the release workflow logs it. Every run
  on a fixture or a real repository, the `--keep pr` run and the workbench's turn also
  fail their path when an answer they accepted, or a line they added to a file, holds
  tool-call markup a model wrote as text (`<function=`, `</tool_call>`). Each path's
  evidence holds its transcript, written as it happens, with each question, its answer
  and who gave it; each step's time and downloads; the system, its memory and the
  account; the versions of Podman, Ollama and Axocoatl it ran; and each run's receipt and
  record, which name its models. The gate's steps check neither the Mac's memory, nor
  which models onboarding chose, nor the Linux release, so this section names none of
  them.
  - On GitHub's ubuntu-24.04 (x86_64) and ubuntu-24.04-arm runners, in a push run of
    `ci.yml` on the release commit, starting without `axocoatl` on PATH:
    - the README's install command, against the release archive just built, installs that
      archive's binary, and a new shell finds `axocoatl`;
    - with Podman already on the runner, `axocoatl onboard --yes --writer ollama` with no
      terminal, against a scripted model server standing in for Ollama 0.40.6 with cloud
      models off, runs no `sudo` and no package manager and pulls nothing;
    - `axocoatl doctor` says ready, with no failure in its sandbox, models, writers or
      daemon sections, and `axocoatl dev` serves on its configured port;
    - the README's `axocoatl run fix` command passes on every fixture in
      `tests/fixtures/newuser` (on `node-plain`, the command that fixture's own README
      gives), each receipt and record verified;
    - a run whose check program is absent exits 3, refused before any model call, a run
      with the reviewer `anthropic:x` exits 3, and the refused run leaves no Session,
      container or model call;
    - onboarding without Podman exits 1, doctor fails a missing range of subordinate ids
      with the `usermod` fix, and the copy-paste lint passes.

    The release workflow runs these steps again on its own archives.
  - On a Mac, on an account that starts without Axocoatl, its data or stored secrets,
    without Podman or a Podman machine, and without Ollama or its settings, with a person
    answering onboarding live in the terminal. The published install command installs
    Axocoatl, and a new shell finds it. Onboarding installs Podman and Ollama with
    Homebrew, creates the Podman machine, which runs, turns Ollama's cloud models off and
    ends ready with a passing first run. `axocoatl doctor` says ready, and the README's
    command passes on every fixture, each receipt and record verified.
  - Claude Code, with no sign-in stored before: onboarding, answered live, signs it in
    through the browser, and `axocoatl doctor` says ready. The home page's
    `fix-claude-code` command passes on `node-deps` and `python-pytest`. With `--keep pr`
    it opens a pull request, and the docs' verify-receipt Action verifies the pull
    request's receipt. A run with `anthropic:opus` as the reviewer is refused with exit 3
    and leaves no Session.
  - Codex, with no sign-in stored before: onboarding, answered live, says its runs bill
    to the ChatGPT plan and signs in with ChatGPT in the browser, and `axocoatl doctor`
    says ready. The site's `fix-codex` command passes on `node-deps` and `python-pytest`.
  - OpenRouter, with no key stored before: onboarding, answered live, connects it through
    the browser sign-in, leaves no key in `config.yaml` and asks for different writer and
    reviewer models, and `axocoatl doctor` says ready. `axocoatl run fix` passes on
    `node-deps` and `python-pytest` with the OpenRouter models, and `fix-claude-code`
    with the OpenRouter reviewer passes on `node-deps`.
  - The Always-On Service: onboarding, answered live, installs and starts it (Step 6) and
    restarts it after the configuration changed (Step 7). A fix through it passes, its
    receipt and record verified. Then `axocoatl service status` says it is installed and
    running, `service stop` leaves it not running, and `service uninstall` removes it and
    its unit.
  - With the Podman machine stopped (`podman machine stop`), a run starts it and passes,
    its receipt and record verified. A timed run on every pinned real repository ends
    with a verdict, its receipt verified.
  - The workbench in Safari: the sign-in link opens it; Open workspace refuses the home
    folder and accepts the repository; a new Session starts; Team & budget comes
    pre-filled and Apply works; one turn edits a file, and Source Control lists the
    turn's change; a commit from the workbench succeeds, authored by the account's Git
    identity; and Ctrl+C stops `axocoatl dev` with exit 0. The person, or a WebDriver
    driver in Safari (`scripts/release-gate/safari-workbench.mjs`), answers the steps
    from the sign-in link to the commit. The person then checks in Safari that the
    Preview shows the Session's app in the workbench; that Open full preview is disabled
    and the workbench says why; that a Preview address typed into a tab, reached by a
    redirect from another site or Command-clicked inside the Preview gets Axocoatl's
    refusal page; that a `Domain=localhost` cookie flood from another page leaves the
    workbench as the docs say; and that Manage Website Data removes those cookies.
  - On Linux, on an account that starts without Axocoatl, its data or stored secrets, or
    Podman, whose own subordinate ids the gate removes first while another user's range
    overlaps 100000-165535. Onboarding, answered live, installs Podman with apt, adds a
    range of subordinate ids that no other user's overlaps and connects OpenRouter
    through the browser sign-in, and it does not install Ollama. `axocoatl doctor` says
    ready, with no failure in its sandbox, models, writers or daemon sections, and
    `axocoatl dev` serves on its configured port. The README's command passes on every
    fixture, each receipt and record verified. The login has a systemd user session, and
    in it the Always-On Service goes through the same steps as on the Mac, from
    onboarding's Step 6 to `service uninstall`.

## [1.4.0] - 2026-10-14

### Added
- **Signed receipts of loadout runs.** When a run finishes, whatever its verdict, the
  daemon signs a receipt: an in-toto Statement v1 (predicate type
  `https://axocoatl.ai/receipt/v1`) in a DSSE envelope, signed with Ed25519 over DSSE's
  pre-authentication encoding of the statement's exact bytes. Its subjects are the files
  the run changed that exist after it, each with the SHA-256 of its content (of its
  target, for a symbolic link). Its predicate holds the run (ids, times, the signing
  Axocoatl's version, the SHA-256 of the task), the repository (base commit, paths dirty
  before the run, the `origin` remote as `https://host/path` without credentials), the
  loadout (id, version, kind, digest), each Agent (role, runtime, pinned program version,
  provider and model, the name of an external writer's credential), the sandbox (backend,
  network, `hardened` workload, and the last network policy the Session's record holds
  for each scope: revision, digest and the rendered allowlist hosts and routes), the
  network summary, each required check (state, exit code, timeout, command and the tree it
  ran on), the review (model, verdict, rounds, number of findings), each adjudication's
  finding id and decision, the verdict, exit code, attention reasons, what was not
  covered, warning codes, usage, the files and the signer's public key. `files.source`
  says where the digests come from: `judged_capture` (the repository capture of the tree
  the required checks ran on), `after_capture` (the last writer's After capture, when no
  check judged a tree), else `workspace_at_finish` (read on the host when the run
  finished, without following symbolic links). Every repository capture now records,
  whatever the repository's size, the manifest line (mode, kind, SHA-256) of each path
  that differs from `HEAD` in the tree it captured, besides its whole manifest up to
  8 KiB; the record is bounded by the changed paths (up to 10,000, while the capture's
  output with its patch stays within 768 KiB), and only a capture past that bound, or one
  a check group admitted with 1.3's capture command makes, leaves the source
  `workspace_at_finish`. A path changed back to what it is in `HEAD` is read when the
  run finishes and named in `files.read_at_finish`. Removed paths, symbolic links, and
  what cannot be attributed (a turn not in the Session's history, Git's own files, a
  directory) are listed apart, the last making `files.complete` false. The receipt is written as
  `loadout-runs/<run id>/receipt.dsse.json` (`0600`) just before the Outcome, so a
  finished run's receipt is settled; runs that end at admission, after a driver error or
  at a daemon restart are signed too. A receipt that cannot be signed adds the warning
  `receipt_unavailable` with the reason and never changes the verdict or the exit code.
- **A receipt signing key per data root.** One Ed25519 key, made with `ring` the first
  time it is needed and stored with the secret store as `receipt-signing-key` (`0600`,
  replaced atomically under the secrets lock), as a tagged one-line JSON value holding the
  PKCS#8 v2 document. Only the public key is ever shown. The egress broker refuses to send
  that value, and any credential named `receipt-signing-key`, whatever name or file a
  route uses for it, and the secret store never offers it as a route's credential.
- **`axocoatl receipt show|verify|key`.** `show FILE|-` prints what a receipt says, says
  that it is not verified, and with `--json` prints the statement. `verify FILE|-` checks
  the signature against `--key LINE` and `--keys FILE` (one line per key, `#` comments),
  then with `--repo DIR` each covered file's SHA-256 and each removed path in a checkout
  (without following symbolic links), and with `--record BUNDLE` that the bundle verifies
  and carries this receipt for its run; `--json` prints one document. Exit codes: 0 valid
  and trusted (and matching), 1 malformed or a signature that does not verify, 2 a valid
  signature by a key not given as trusted (also with none given), 3 usage, 4 a file or the
  record that does not match. Both read a DSSE envelope or text holding the pull request
  block, from a file or standard input, and need no daemon, Podman or network. `key`
  prints this data root's public key as one trusted-keys line, `ed25519 <base64>
  axocoatl@<host> <first 16 hex of the key id>`, making the key when there is none;
  `--rotate` replaces it, and receipts the old key signed still verify with its line.
- **`axocoatl run --receipt FILE`** writes the run's receipt when it finishes, whatever the
  verdict, atomically like `--junit` and `--record`, and says which key signed it, how
  many files it covers and their source. A run that was never admitted has nothing to
  sign, and a run without a receipt keeps its exit code; both are said on standard error.
- **`GET /api/runs/{run_id}/receipt` and `GET /api/receipts/key`.** The first serves the
  envelope exactly as written (`application/vnd.dsse.envelope.v1+json`; 404 with the
  reason before the run finishes or when it has none), the second the public key, its key
  id and its trusted-keys line, making the key when there is none. A run's status gains
  `receipt`: the signing key's id, the files covered and removed, their source, whether
  every changed path is covered, and the statement's SHA-256.
- **Record bundles of schema `axocoatl.record-bundle/2`**, whose last section, `receipt`,
  is the run's receipt. `axocoatl record verify` accepts `/2` and 1.3's `/1`.
- **Keep as PR commits only what the receipt covers.** With a receipt, each path Keep is
  about to commit must have the content the receipt gives it and each removed path must
  be absent; a path edited after the checks judged it, or one the receipt does not cover,
  is refused, naming it, and a receipt that cannot be read or is another run's is refused
  too. The commit gains the trailers `Axocoatl-Run: <run id>` and `Axocoatl-Receipt:
  sha256:<statement SHA-256>`. The pull request body gains a **Receipt** section before
  the record (signer key, verdict, checks, review, writer, files covered, how to verify)
  and ends with the envelope, base64, in an `<!-- axocoatl-receipt:v1 … -->` block that the
  body's length cut never reaches. A run without a receipt is kept as before, and the body
  and Keep's warnings say it carries none. Keep's response names the receipt.
- **The receipt in the app.** The Run outcome panel shows the key that signed the run's
  receipt, the files it covers and their source, the statement's SHA-256 and **Download
  receipt**, or why the run has none. **Settings → Loadouts → Receipts → Signing key**
  shows the public key line with a Copy button and what a receipt does and does not
  prove.
- **The `verify-receipt` GitHub Action.** `axocoatl/axocoatl/actions/verify-receipt` installs the pinned
  Axocoatl release for a Linux x86_64 or ARM64 runner, checked against the release's
  `.sha256` file and an optional `sha256` input; reads the receipt from the pull
  request's description; reads the trusted keys from the base commit's
  `.axocoatl/receipt-keys` (`keys-file`), never from the head, so a pull request cannot
  add its own key; runs `axocoatl receipt verify` on the checked-out head; and requires
  the receipt to cover every file the pull request changes (`allow-uncovered-files: true`
  turns that into warnings). It fails a pull request without a receipt unless
  `require-receipt` is `false`, and a run that did not pass unless `require-pass` is
  `false`, with an annotation naming what failed, and writes a job summary.
  `scripts/test-verify-receipt-action.sh`, part of the native gate, runs it against a
  stand-in for GitHub's API and real signed receipts; `scripts/run-actionlint.sh` checks
  the action's scripts with the pinned ShellCheck and the workflow contract checks its
  steps.
- **Built-in loadouts `fix-claude-code` and `fix-codex`: Claude Code or Codex through
  Axocoatl with one command.** After `axocoatl recipe build claude-code` (or `codex`)
  and `axocoatl connect claude-code` (or `connect codex`, or `connect codex --api-key`),
  `axocoatl run fix-claude-code --task "…" --model reviewer=<provider:model>` (or
  `fix-codex`) runs the built-in `fix` with the pinned Claude Code 2.1.292 or Codex
  0.160.1 as the writer, inside the Session container from its recipe image: the same
  required check (the repository's check command or `--check`, 3 minutes), the same
  required review by the reviewer's model for up to 3 rounds, the same adjudications,
  `network: egress` and the `hardened` workload. The writer's model is a parameter with
  a default, `anthropic:sonnet` (an alias Claude Code 2.1.292 resolves to
  `claude-sonnet-5-5`) or `openai:gpt-6.1-sol` (the first model of Codex 0.160.1's
  bundled catalog, in Axocoatl's price table); `--model writer=` replaces it. The
  reviewer is required and has no default. The writer's budget is 4 activations, 20,000,000
  tokens and $10, with 400 invocations for Claude Code and 200 for Codex, which has no
  spending stop of its own; the reviewer's is `fix`'s. The tokens hold one call of every
  pinned model (1,000,000 for a 1M-window Claude model, 400,000 for Codex's), and leave
  room for a program that reports its cached context on every request. These numbers are
  choices, not measurements. Axocoatl's Podman tests run both built-ins end to end with
  their default models from the recipe images against fake model APIs, with a reviewer
  that asks for one change: Claude Code's requests named `claude-sonnet-5-5`, and Codex
  ran `gpt-6.1-sol` in its code mode.
- **A Codex writer's `credential` can be `auto` or a parameter.** `fix-codex` declares
  `credential` as a `text` parameter with the default `auto`: when the run is admitted
  it takes `codex-openai` when it is stored (as a secret or a configured credential),
  else `codex-chatgpt`, and with neither it is refused before anything starts (exit code
  3), saying to run `axocoatl connect codex` or `axocoatl connect codex --api-key`.
  `--param credential=codex-openai` or `codex-chatgpt` chooses one. A user loadout can
  write `credential: auto`, or `{param: name}`, too; a Claude Code writer's `auto` is
  `claude-code-oauth`. Every external writer's run records the credential its route
  sent: the run manifest's `credentials` (applied again, not chosen anew, after a daemon
  restart), a `note` event that `axocoatl run` prints as the run starts, and the
  Outcome's notes (summary, JUnit `run` case, Run outcome panel, record bundle); the
  retained definition names it too.
- `axocoatl loadouts show` ends with the run command as Settings shows it,
  `--model ROLE=provider:model` for a `ROLE_model` parameter, leaving out parameters
  with a default. A loadout's graph shows a parameter's default and an external
  writer's credential.

### Changed
- Repository captures record the digest of every path that differs from `HEAD`, which
  signed receipts use. Axocoatl 1.3 does not know that field: once a 1.4 daemon has
  recorded a capture in a data root, a 1.3 daemon does not start on it (`unknown field
  changed_manifest`). Going back to 1.3 is a restore of the cold backup made before
  upgrading ([Upgrade](https://docs.axocoatl.ai/operate/upgrade/#data-written-by-14)).
- **A user loadout that declares a built-in id no longer leaves the choice to
  Axocoatl.** Such a file was listed with an error while a run of the id silently ran
  the built-in. Now, while a file declares `fix`, `fix-claude-code`, `fix-codex`, `qa`
  or `audit`, a run of that id is refused (exit code 3) naming the file, the built-in's
  row in `axocoatl loadouts list`, `GET /api/loadouts` and Settings carries the warning
  `user_loadout_uses_builtin_id`, and the file's row says how to keep it (another id,
  such as `my-fix-claude-code`) or use the built-in (remove the file). The 1.3 external
  agents documentation showed a `fix-claude-code` loadout to write yourself; that id is
  now built in, and the messages say so.

### Fixed
- **A run's Outcome names the tree each check ran on.** `checks[].candidate_sha256` was
  always empty: a check's own control-plane view names no tree. Each check that ran now
  names the tree the readiness review that judged it records.
- **A command line `axocoatl run`, `loadouts`, `record` or `receipt` refuses exits 3**,
  the usage code they document, not clap's 2, which each of them uses for another
  outcome (needs attention, a record that does not verify, an untrusted signer).

## [1.3.2] - 2026-10-14

### Security
- **Routes to model APIs refuse requests that make the provider reach other hosts.**
  Routes checked a request's method, path and query and passed its body unread, so a
  request a route allowed could ask the model provider to fetch a URL, search the web,
  connect to an MCP server or run a tool on its own servers, and the provider then
  reached hosts the Session's egress policy does not list, with whatever the request
  put in the URL or the search. Claude Code 2.1.292's WebSearch tool sends such a
  request itself (`POST /v1/messages` offering Anthropic's `web_search_20250305`), and
  the route Axocoatl adds for a Claude Code writer forwarded it; Codex 0.160.1, as
  Axocoatl ran it, offered OpenAI's hosted `web_search` with live access on every
  request. A route to `api.anthropic.com`, `api.openai.com`, `chatgpt.com`,
  `openrouter.ai`, `generativelanguage.googleapis.com`, `api.mistral.ai`, `api.x.ai` or
  `ai-gateway.vercel.sh`, which includes the routes Axocoatl adds for external writers,
  and any route with `provider_fetch: refuse`, now
  reads each request body whole (at most 64 MiB; a larger one gets `413`) before it adds
  the credential or records the request, and refuses with `403 provider_fetch_refused`
  a body that offers a tool of a type the client does not run (tools pass only by
  client-run type: Anthropic custom, `bash_`, `text_editor_`, `computer_` and
  `memory_` tools; OpenAI `function`, `custom`, `local_shell`, `apply_patch`, local
  `shell`, client-executed `tool_search` and a `namespace` whose tools all pass; Chat
  Completions `function` and `custom`;
  Gemini function declarations; so xAI's `x_search` and `code_execution`, Mistral's
  `image_generation`, `document_library` and `connector`, and the Vercel AI Gateway's
  `vercel:*_search` and `vercel:browserbase_fetch` are refused with the rest), names
  MCP servers (`mcp_servers`), turns on an OpenRouter plugin, `web_search_options` or
  xAI Live Search (`search_parameters` with a mode other than `off`), passes Vercel AI
  Gateway `providerOptions` other than `gateway` through to the provider, asks for
  content from a URL (an Anthropic `source` of type `url`, an `image_url`,
  `video_url`, `audio_url` or `document_url` that is not a `data:` URL, `file_url`, a
  `file_data` URL, an `input_audio` URL, a Gemini file URI outside its Files API),
  names an `:online`, search, deep research or Perplexity model or one of the Vercel AI
  Gateway's search providers, or uses an OpenAI stored prompt, anywhere outside
  schemas, metadata and tool-call arguments. It also refuses a body it cannot read: not
  JSON, a key repeated in one object, more than 512 levels deep, compressed, or sent to
  an endpoint other than Messages, Responses, Chat Completions and Gemini's
  `generateContent`, and any body to an `agents` or `conversations` endpoint (Mistral's
  run the tools of an agent stored with Mistral). The answer names what was
  refused and its JSON path and never repeats a value from the body; the refusal is in
  the Session's network record with that reason, and nothing reaches the provider. An
  allowed body is sent byte for byte. What a request asks for in its headers or through
  settings stored with the provider account (an OpenRouter account's default plugins, a
  preset, a Gemini cached content, Vercel AI Gateway routing rules) is not seen. See
  [Requests to model APIs](https://docs.axocoatl.ai/configure/credentials/#requests-to-model-apis).
- **The route to Ollama on this computer carries only inference and model listing.**
  `sandbox.egress.host_ollama` allowed every request, and Ollama 0.20.6 reaches other
  hosts for several: `/api/pull` and `/api/create` fetch from whatever registry the model
  name names, `/api/experimental/web_search`, `/api/experimental/web_fetch` and the
  account endpoints call `ollama.com`, and with its cloud features on (the default) any
  request for a model whose tag is `cloud` or ends in `-cloud` goes to `ollama.com`,
  prompt included, without a pull. The route now allows only `GET` or `HEAD` of `/`,
  `/api/version`, `/api/tags`, `/api/ps`, `/api/status`, `/v1/models` and
  `/v1/models/<model>`, and `POST` of `/api/show`, `/api/chat`, `/api/generate`,
  `/api/embed`, `/api/embeddings`, `/v1/chat/completions`, `/v1/completions`,
  `/v1/embeddings`, `/v1/responses` and `/v1/messages`; anything else gets
  `403 route_denied`. It reads each body and refuses a cloud model with
  `403 provider_fetch_refused`, and checks the compatible endpoints as it checks a hosted
  API. Both are recorded like any refusal. Pull models on this computer with
  `ollama pull`.
- Update `rustls` in `Cargo.lock` to 0.23.46 for RUSTSEC-2026-0346 (`Debug` output of
  client session values shows TLS secrets), RUSTSEC-2026-0347 (buffered application data
  sent to the outer server after an ECH rejection), RUSTSEC-2026-0348 (brotli certificate
  decompression can allocate up to 1 GiB before authentication) and RUSTSEC-2026-0349 (an
  unauthenticated ServerHello can downgrade later connections to classical key exchange).

### Added
- **`provider_fetch` on a route** (`allow` or `refuse`). Unset, it is `refuse` on a
  route to a model API host and off elsewhere. `allow` sends bodies unread, and
  validation warns about it on a model API host; `refuse` turns the check on for a
  route to another host, such as a gateway serving one of these APIs. A Session's
  policy records the setting of every route to a model API and of every route that
  sets it. **Session network** explains the new refusals, `provider_fetch_refused` and
  `request_incomplete` (a body that did not arrive whole on a route that checks bodies).

### Changed
- **External writers run without their provider's hosted tools.** While the route that
  serves the program's model API refuses provider fetches, the default, Claude Code
  2.1.292 runs with `--disallowedTools WebSearch` and Codex 0.160.1 with
  `-c web_search="disabled"` and `--disable image_generation`, so neither sends a
  request the route would refuse: Codex's offer only its own function and custom tools
  and a `tool_search` it runs itself, and Claude Code's WebSearch tool, which would ask
  Anthropic to search, answers the model `No such tool available: WebSearch`. In the
  Podman test the pinned Claude Code, led to search, finished its task and the run
  passed with no request refused. A loadout route for the program's API host with
  `provider_fetch: allow` starts the program with these tools as it has them.
- **A configured or loadout route to a model API refuses more.** Besides the constructs
  above, a request with a body to an endpoint other than the model endpoints (a file
  upload, a batch, embeddings) and a compressed body are refused. Set
  `provider_fetch: allow` on a route that needs them.
- **An e2e check whose `e2e.config.ts` uses `gateway()` from `ai` is refused by default
  on a route to `ai-gateway.vercel.sh`.** That client, which `e2e init` writes by
  default, sends the AI SDK's own gateway protocol (`POST /v4/ai/language-model` with
  `ai` 7.0.139), which Vercel documents only through the AI SDK, not as an HTTP API, so
  the route cannot check it. The route
  refuses a body to `/v<N>/ai/…` on that host with `403 ai_sdk_gateway_refused`, recorded
  with that reason, and its hint and **Session network** name the fix; admission warns
  `e2e_ai_sdk_gateway_refused` for an e2e check whose `model_route` is that host and
  checks bodies, and still runs it. Build the model with the gateway's OpenAI-compatible
  API instead: `createOpenAICompatible` from `@ai-sdk/openai-compatible` 3.x with
  `baseURL: process.env.AXOCOATL_E2E_BASE_URL`, `apiKey: process.env.AXOCOATL_E2E_API_KEY`
  and `chatModel(process.env.AXOCOATL_E2E_MODEL)`, and allow `POST /v1/chat/completions`
  on the route. Run through e2e 0.18.0's own model adapter in the e2e recipe image, its
  judgments and tool calls pass the check. `provider_fetch: allow` on the route keeps
  `gateway()` working and sends every body unread. See
  [The Vercel AI Gateway](https://docs.axocoatl.ai/configure/e2e-check/#the-vercel-ai-gateway).

### Fixed
- Keep as PR pushes to a private HTTPS remote with Apple's git. Apple's git sets its
  `osxkeychain` credential helper in a configuration file of its installation, which Keep
  left out with the system file, so the push could not authenticate (`could not read
  Username for 'https://github.com'`). Keep now passes on entries from a configuration
  file of the git installation outside the repository, and still never the repository's.
- **A Session whose folder is gone no longer blocks other Sessions and runs, and can be
  deleted.** When the folder a Session works in was deleted, moved away, or replaced by
  a file or a symbolic link, every request that looked at that Session's attempt state
  failed with `opening Workspace attempt metadata: No such file or directory`: each later
  `axocoatl run` on the moved repository (`POST /api/runs`, `400`), a new Session in its
  Workspace, Close and Delete of the Session itself (`500`), and daemon shutdown. Such a
  Session is now skipped by those checks, and the daemon log says so once for that Session
  and folder. Close and Delete succeed, change only Axocoatl's own records, and log that
  the folder was already gone. A turn, the runtime, Files, Git, Terminal, Reopen,
  a change of the environment and Ways are refused with a message that names the folder
  and says to restore it or delete the Session. The folder is never created, and a
  symbolic link in its place is never followed; a daemon whose Session folder became a
  link also starts again.
- **A repository moved to another path keeps its Workspace, and a run there works in the
  new path.** After `mv A B`, opening B or `axocoatl run --repo B` returned A's Workspace
  with its old path, so the new run's Session worked in A, a missing folder or another
  directory made at A since. The Workspace now moves to B, durably, when A is no longer
  a directory (deleted, a file, or a link); a name nobody chose follows the folder's.
  When A is a directory again, B gets a new Workspace and A's stays at A. Sessions made
  in A keep A: while their Workspace is at B they are refused with both paths and the
  choice to move the folder back or delete the Session, and A is never used for them. A
  moved directory is recognized by device, inode and birth time while the daemon runs;
  after a restart, B gets a new Workspace.

## [1.3.1] - 2026-10-13

### Added
- **`axocoatl connect codex`: Codex with your ChatGPT plan, without handling a key.** The
  command runs the official `codex login` (or `codex login --device-auth` with
  `--device-auth`) with file storage forced and a private `CODEX_HOME`, never your
  `~/.codex`. You approve the sign-in in the browser or enter the code Codex shows. By
  default nothing but Podman is needed on your computer: the pinned Codex 0.160.1 runs in
  a throwaway container of the `codex` recipe image (the one `axocoatl recipe build codex`
  recorded, by image id, built and recorded first when there is none), through the
  configured Podman connection, as a non-root user with a read-only root, no
  capabilities, no host mount, no log, a `0700` tmpfs as `CODEX_HOME`, none of your
  environment (not even the proxy variables Podman would copy in), and Podman's default
  bridge network (not Axocoatl's egress routes) to reach `auth.openai.com` without a
  proxy. Podman is the first `podman` on `PATH`. The browser sign-in publishes one port,
  `127.0.0.1:1455`, on your computer's loopback only, to Codex's login server in the
  container; when that port is in use the command stops before starting anything and
  suggests `--device-auth`, which publishes none. Codex cannot open a browser on your
  computer from the container, so Axocoatl opens the address it prints, once, with
  `open` on macOS or `xdg-open` elsewhere (the first on `PATH`; the address as the only
  argument, no shell) and prints `Opened the sign-in page in your browser.` It opens only
  an address shown on screen that is exactly `https://auth.openai.com/oauth/authorize?…`,
  or with `--device-auth` exactly `https://auth.openai.com/codex/device`, and writes it
  to no file or log. `--no-browser` opens nothing, and without a working opener nothing
  is opened either: you open the printed address yourself. With `--codex PATH`, that
  Codex CLI on your computer runs instead (and opens the browser sign-in itself), with a
  temporary `0700` `CODEX_HOME` in the data root (given to Codex as an absolute path,
  also when the data root is relative) as its working directory, and without Codex's
  other credential and endpoint variables. Either way its output reaches
  your terminal through the same masking relay as `connect claude-code`, which here also
  hides OpenAI API keys and JWTs. Axocoatl reads the sign-in from the `auth.json` Codex
  writes, never from the screen (from the container with `podman exec`, straight into
  memory, never to disk), stores the access token, refresh token and ChatGPT account id
  as the secret `codex-chatgpt` (`0600`, replaced atomically, as `axocoatl secret set`
  stores one), and then removes the container, or overwrites every file of the temporary
  home with zeros and removes it, also on an error or a signal (the `podman` commands
  that start, read and remove the container run in a process group of their own and
  ignore SIGINT, SIGQUIT and SIGHUP, so a Ctrl-C typed after Codex exits does not stop
  them, not even while one is being started); Podman removes a login container
  when the sign-in time has passed even if Axocoatl is killed: 30 minutes, or 1 to 60
  with `--sign-in-timeout MINUTES` (Podman's `--timeout`; not with `--codex`). The
  command says when the sign-in starts how long you have (`You have 30 minutes to finish
  signing in.`), and when the time runs out first it says
  ``The Codex sign-in was not completed within 30 minutes; nothing was stored. Run
  `axocoatl connect codex` again.`` (both with the time chosen) instead of saying that
  `auth.json` could not be read from the container, and without the error Podman may
  print about the `podman exec` session the stopped container took with it
  (`Error: retrieving exec session …: no such container`). Exit codes: 0 stored, 2 no
  ChatGPT sign-in (also when the sign-in time ran out), 3 usage (also no Podman on
  `PATH`, port 1455 in use, or a command line it does not take), 5 failure, 6
  interrupted.
- **`axocoatl connect codex --api-key`: Codex's OpenAI API key from the browser sign-in,
  never typed or pasted.** After the browser sign-in, Codex 0.160.1 exchanges the id
  token it obtained for an OpenAI API key at `auth.openai.com` (an OAuth token exchange,
  `requested_token=openai-api-key`) and writes it as `OPENAI_API_KEY` in `auth.json`;
  when the exchange fails it writes none and the sign-in still succeeds, and the
  device-code sign-in never exchanges. With `--api-key`, `connect codex` runs the same
  browser sign-in, reads that key from the same `auth.json` in memory, checks it has the
  form Codex writes (`sk-`, then letters, digits, `-` and `_`, 20 to 1024 characters),
  and stores it as the secret `codex-openai` (a Codex writer's default credential;
  `0600`, replaced atomically, under the secret store's lock) beside the ChatGPT sign-in
  as `codex-chatgpt`; `--api-key-only` stores only the key. Axocoatl never shows, logs
  or writes the key anywhere else, and Codex's own copy goes with the login container
  (or the temporary home). Requests with `codex-openai` are billed at OpenAI API
  rates to the key's OpenAI Platform organization, not to the ChatGPT plan. When
  `auth.json` holds no key, or a value that is not one, nothing is stored as
  `codex-openai` and the command says so plainly (OpenAI issued no API key with this
  sign-in; API-key mode needs an OpenAI Platform organization, and with one the request
  for the key may have failed, so run it again; a `codex-openai` stored before is left as
  it was; ChatGPT mode works with `codex-chatgpt`) and exits with the new code 8; with
  `--api-key` the ChatGPT sign-in is still stored. `--api-key` or `--api-key-only` with
  `--device-auth` is refused before anything runs (exit code 3) with why, and so is a
  value on the command line (after `--api-key`, or `--api-key=VALUE`), whose message
  never repeats it; nor does the message for a key given to another flag, as an argument
  `connect` does not take, as the target or as `--codex` or `--url`. With `--api-key`,
  the store must have room for both secrets before `codex login` runs, so a sign-in is
  never made and then half stored.
- **`axocoatl connect codex --status` and `--refresh`.** `--status` reads the stored
  sign-in from the data root, without the daemon, and shows whether one is stored, its
  ChatGPT account id by its last four characters, when its access token expires (its JWT
  `exp`) and when its tokens were last refreshed or signed in, never a token (exit codes:
  0 stored, 2 none, 5 it cannot be read or is not a sign-in). `--refresh` asks the
  running daemon, found and authenticated the way `axocoatl run` finds it (`--url`,
  `AXOCOATL_URL` or the configured address; `AXOCOATL_TOKEN` or the local API token), to
  refresh the sign-in now, through the new `POST /api/connect/codex/refresh`. The daemon
  refreshes with the broker that refreshes the sign-in before a request (the same request
  to OpenAI, rotation, atomic write under the secret store's lock and remembered refusal,
  one refresh at a time with the route's), even when it is not due, and records the
  refresh and how it ended in its log without a value. The command prints when the new
  access token expires and when it was refreshed, never a token. Exit codes: 0
  refreshed, 1 OpenAI refused the refresh (sign in again; asked again, the daemon does not
  send that refresh token again), 2 no sign-in stored, 3 usage, 4 the refresh could not
  be completed (the token endpoint could not be reached, did not answer in time, or gave
  an answer that is neither new tokens nor a refusal; the stored sign-in is unchanged),
  5 any other failure, 7 no daemon answers. The route answers only an authenticated
  request from a loopback address: `403` from a daemon that accepts unauthenticated API
  requests (`server.auth.allow_unauthenticated`) and for a request from any other
  address (an IPv4 loopback peer that a dual-stack listener reports as
  `::ffff:127.0.0.1` counts as loopback).
- **A Codex writer can run with that sign-in: `credential: codex-chatgpt`.** A loadout
  Agent's new `credential` field names the stored credential its route sends: for
  `runtime: codex`, `codex-openai` (the default, an OpenAI API key, to `api.openai.com`) or
  `codex-chatgpt` (to `chatgpt.com`); for `claude-code`, only `claude-code-oauth`. A run
  whose credential is not stored is refused before it starts with how to store it, as for
  the API key. Codex then runs with its HTTP provider aimed at the ChatGPT backend
  (`https://chatgpt.com/backend-api/codex`) and a placeholder in
  `CODEX_CHATGPT_ACCESS_TOKEN`; the route allows only
  `POST /backend-api/codex/responses` and adds the access token and the
  `ChatGPT-Account-ID` header on your computer. The tokens never enter the Session
  container, and nothing there can reach `auth.openai.com`.
- **The daemon keeps that sign-in current.** Before a request, when the access token
  expires within five minutes (or, without an expiry, was obtained more than eight days
  ago), and after a `401` for the current token, the daemon refreshes it at OpenAI's token
  endpoint the way Codex 0.160.1 does, stores the rotated tokens atomically, remembers a
  final refusal instead of retrying it, and never logs a token. It writes the rotated
  tokens under the secret store's lock after reading the file again: a sign-in that
  `axocoatl connect codex` stored meanwhile is kept, and one that `axocoatl secret remove`
  removed meanwhile stays removed (the refreshed token goes with that one request only).
  Rotated tokens that cannot be written are kept in memory, used and written again on
  the next request, so the refresh token OpenAI already rotated is not sent again. The
  `401` itself still reaches Codex; the run's not-covered reason says to run again, then
  to connect again. When the daemon has no current token, the route's refusal says to
  connect again and the reason is in the daemon's log, not in the answer to the
  container.
- **The sign-in goes to `chatgpt.com` only.** The daemon sends the access token and
  account id only on a route to `chatgpt.com` whose credential is that secret: the one
  Axocoatl adds, or one that a loadout or the configuration lists for that host, with the
  requests its rules allow. A route to any other host whose credential is that secret
  reads it as a plain credential, which refuses the stored sign-in, and answers `503`.
  The refresh token is sent only to OpenAI's token endpoint.
- **A verification-only build that sends one stale access token, to test the refresh
  after a `401` against OpenAI's service.** `axocoatl-daemon` and `axocoatl-cli` have a
  cargo feature `live-verify`, off by default. Only a binary built with it
  (`cargo build --release -p axocoatl-cli --features live-verify`) reads
  `AXOCOATL_VERIFY_STALE_ACCESS_ONCE` as the daemon starts. With `1`, the daemon logs at
  `WARN` that the switch is active, and the first request of the process to
  `chatgpt.com` carries a fixed invalid access token (an unsigned JWT that expired in
  1970) instead of the stored one; when the upstream answers it `401`, the daemon
  refreshes the stored sign-in once (OpenAI rotates its tokens), and the next request
  carries the refreshed token. The `401` reaches Codex as any other does. The switch
  fires once per process and logs at `WARN` when it fires. No release binary has it: the
  release workflow's build job, `scripts/verify-native-gate.sh` and
  `scripts/verify-cross-linux-gate.sh` run the new `scripts/check-no-live-verify.sh` on
  the binaries they build, and `scripts/check-workflow-contracts.rb` refuses a manifest
  other than those two that names the feature, a default feature set that is not empty,
  and a workflow, file under `scripts/` or shell script that names the feature or runs
  any cargo command other than `check`, `clippy` or `doc` with `--all-features`
  (`scripts/test-workflow-contracts.sh`, run by CI and `scripts/preflight.sh`, checks
  that it does). CI's `cargo check` and `cargo clippy --all-features` type-check and
  lint the feature without building a binary. A
  Podman test runs the pinned Codex 0.160.1 with the switch against a fake ChatGPT
  backend and token endpoint: its first model call is answered `401`, the daemon
  refreshes once, Codex sends the call again with the refreshed token, and the run
  passes.

### Changed
- A command line `axocoatl connect …` does not take (an unknown flag, a value out of
  bounds) exits with the `connect` commands' usage code 3 instead of clap's 2, which
  `connect` uses for "no token printed" (`claude-code`) and "no sign-in" (`codex`).
- `axocoatl secret set codex-chatgpt` warns when the value is not a sign-in
  `connect codex` stored, and a missing `codex-chatgpt` names `axocoatl connect codex`.
- A missing or rejected `codex-openai` now names `axocoatl connect codex --api-key`
  instead of `axocoatl secret set codex-openai`, and `secret set codex-openai` from a
  terminal and `connect codex` without one point to it; `secret set --from-env` is
  described only for automation without a browser. The documentation no longer shows
  storing a key from a file you write.
- `axocoatl secret set`, `axocoatl secret remove` and the `connect` commands store and
  remove a secret while holding an exclusive lock on the data root's `secrets` directory
  (waiting up to five seconds for another holder), the lock the daemon takes to write a
  refreshed ChatGPT sign-in.
- **The embedded execution supervisors build reproducibly.** Panic locations embed the
  paths of the registry crates under the Cargo home, so a supervisor built with the Cargo
  home at `/usr/local/cargo` was 64 bytes larger than one built at `/cargo-home`, and its
  bytes depended on where the build's Cargo home was. `scripts/build-exec-supervisor.sh`
  now remaps the Cargo home to `/cargo-home`, as it remaps the checkout to
  `/axocoatl-source`, and refuses a payload that still contains the checkout, the Cargo
  registry or the target build directory it used; the build manifest (schema 3) records
  both remaps. Both embedded supervisors are rebuilt this way and re-pinned: five builds
  with different Cargo homes, checkout paths and target directories gave byte-identical
  payloads and manifests.
- `scripts/test-exec-supervisor-runtime.sh` builds Alpine 3.20 and Debian bookworm test
  images for linux/amd64 and linux/arm64, as root and as a non-root user, and runs the
  supervisor's protocol and process-lifecycle tests in each, checking that every image
  receives the embedded payload of its architecture.

### Fixed
- **A read-only Agent's shell has a temporary directory of its own wherever the
  repository is.** Its commands run under a write restriction that drops every writable
  root containing the repository, so with the repository under `/tmp` the shell could not
  write `/tmp`: outside a hardened container the scratch home Axocoatl makes for each of
  its commands (under `$TMPDIR`, or `/tmp`) could not be made and the command did not run
  (exit 125), and no temporary file could be written under `/tmp`. The execution
  supervisor now makes a scratch directory for every restricted command and gives it as
  `TMPDIR`: `0700`, owned by the user the command runs as, in the first of `/tmp`,
  `/var/tmp` and `/dev/shm` that does not lie inside the repository (judged by name and
  by where links lead), writable to the command whatever its restriction's writable
  roots, and removed when the command tree has ended, also when the command left
  directories in it (up to 256 levels deep) without rights for its owner; a command that
  may write the directory holding it (such as `/tmp` when the repository is elsewhere) can
  move it aside first, and what it moves stays, as any file it writes there would. `/tmp`
  itself stays read-only to the command when it contains the repository (a program that
  writes a fixed path there instead of `$TMPDIR` still fails), and the repository stays
  read-only wherever it is. A
  read-only helper's shell in a hardened container already had such a directory in
  `/tmp` (its `HOME` and `TMPDIR`); it now gets it the same way, so a Workspace that is
  `/tmp` itself puts it in `/var/tmp` instead of refusing the launch. A supervisor that
  can make no such directory refuses to launch the command (`scratch directory
  unavailable`). Both embedded supervisors are rebuilt and re-pinned.

### Security
- **YAML is read without `unsafe-libyaml`.** RUSTSEC-2024-0449 marks `unsafe-libyaml`
  0.2.11 unmaintained: a machine translation of libyaml into unsafe Rust whose repository
  is archived, so `cargo audit -D warnings` failed. It came in through `serde_yaml` 0.9
  (itself deprecated), which read every configuration, loadout, egress route and workflow
  graph. Axocoatl now parses YAML with `granit-parser` 1.3, a YAML 1.2 parser that
  forbids unsafe code and limits how deeply collections nest (its dependencies
  `arraydeque` and `smallvec` do use unsafe code), and reads the parser's events into Rust
  values with its own serde layer, `axocoatl_config::yaml`, which follows `serde_yaml`
  0.9's rules. Neither `serde_yaml` nor `unsafe-libyaml` is in `Cargo.lock` any more, and
  `cargo audit -D warnings` passes with no new exception. The advisory suggests
  `serde-saphyr`; it was not used because its 1.2 and 1.3 releases need Rust 1.89 (Axocoatl
  supports 1.88), and 1.1 read an empty `key:` into a text field as an error instead of
  `""`, `017` as the number 17 where any type fits, and a quoted `"5"` into a number
  field as 5. `noyalib` had a resource-limit advisory (RUSTSEC-2026-0333) on 2026-10-06,
  `serde_yml` is unsound (RUSTSEC-2025-0068), and `serde_norway` and `serde_yaml_ng` still
  use a translation of libyaml.

  YAML files written for earlier versions read to the same values, or are refused
  with the line and column where they would read differently; none reads to a
  different value except where an anchor is defined twice (below). A differential test
  reads 2,281 documents again and compares 5,566 results with what `serde_yaml` made of
  them when it was replaced, and the positions of 27,896 nodes in the 2,252 valid ones:
  every YAML file in the repository, every `yaml` block of the documentation, every
  document a full workspace test run handed to the parser, and a table of YAML 1.1/1.2
  pitfalls. A second one compares 6,241 more documents with what `serde_yaml` 0.9.34
  made of them, recorded outside the workspace: 2,831 edge cases of block scalars, tags,
  anchors, byte order marks, line breaks, tabs, flow collections, documents and nesting,
  and 3,410 generated documents. `yes`, `no`, `on` and `off` are still strings, `017` is
  still the string `017` and not octal, `0o17` is 15, `1_000` and `190:20:30` are
  strings, `~`, `null` and an empty value are null, a text field still takes `017`,
  `true` or `~` as written and an empty `key:` as `""`, a quoted number is not a number,
  `<<` is still an ordinary key (no merge), and more than one document in a file is
  still refused. To keep that, what libyaml did and `granit-parser` does not is kept: a
  NEL (U+0085) is a line break, and so are a LINE SEPARATOR (U+2028) and a PARAGRAPH
  SEPARATOR (U+2029) outside a value; a continuation line of a multi-line quoted string
  may be indented less than YAML 1.2 asks, also with a tab, however many such lines a
  file has; a lone `-` in `[...]` or `{...}` (`args: [--input, -]`) is the string `-`;
  a literal or folded block that ends the file inside its last line has no final line
  break, and one followed by a blank last line indented less than the block keeps it;
  and an empty value is the empty string. Files with CR or mixed line endings read as
  before. Error positions are libyaml's, in the file as written. What differs:
  - Refused now, with the line and column, where `serde_yaml` read a value: a LINE or
    PARAGRAPH SEPARATOR inside a value (write `\L` or `\P` in a double-quoted string);
    a byte order mark (U+FEFF) at the start of a value, or at the start of a line once
    the document's content has begun, or a second one at the start of the file; an anchor or alias name with a character
    other than a letter, digit, `-` or `_` (libyaml read `&a: v` as an anchor on an
    empty key); text starting with `?` or `:` inside `[...]` or `{...}` (libyaml took
    `[?a]` and `{k: :a}` as a key and a value); a comment whose `#` follows the text
    before it without a blank (`[a]#c`, `[a,# c`, `|#c`); `- ?`; and more than 255 block sequences or mappings
    inside one another, or more than 255 flow ones, anywhere in a file (before, nesting
    deeper than 128 levels was refused when read and accepted only inside a field that
    is ignored).
  - Read differently: an alias after an anchor defined again and then a new anchor.
    `serde_yaml` numbered anchors by how many names it had seen, so in
    `a: &x 1`, `b: &x 2`, `c: &y 3`, `d: *x` it read `d` as 3; it is 2 now, as YAML
    says.
  - Read now, valid YAML 1.2 that libyaml refused: a file holding only `...`; an empty
    key or value (`: value`, `- :`, `[:]`, `[a:]`); a tag followed directly by `]` or
    `}` (`[!x]`); a tab as separation, on a blank or comment line, or ending the file
    after a block; a byte order mark before `---` or a directive, or after `...`;
    `%YAML 1.3` and unknown directives, which are ignored; and a non-ASCII implicit key
    of up to 1,024 characters (libyaml counted its limit in bytes).
  - A file that does not parse is reported in `granit-parser`'s words, such as `while
    parsing a block mapping, did not find expected key at line 3 column 2`, still with
    the file, line and column. Every error with a position now shows it, also at line 1,
    column 1, where `serde_yaml` left it out, and a second document in a file is reported
    at its line and column. An unknown alias is reported as `unknown anchor at line …
    column …`, as before.
  - Large files read fast: in a release build on an Apple silicon Mac, a document of
    160,000 tagged values on one line read in 0.15 seconds, a configuration with 100,000
    keys in 0.1 seconds, and one with 20,000 quoted strings whose continuation lines are
    indented too little in 0.09 seconds.
  - `WorkflowGraph::to_yaml` indents a sequence under its key and escapes the NEL, LINE
    SEPARATOR and PARAGRAPH SEPARATOR characters. Workflow YAML written before 1.3.1 reads
    back to the same graph, and so does YAML written now; `axocoatl-graph` now depends on
    `axocoatl-config` for it.
- Update `quinn-proto` in `Cargo.lock` to 0.11.18 for RUSTSEC-2026-0338,
  RUSTSEC-2026-0340 and RUSTSEC-2026-0343 (a remote peer or a sender's own datagrams
  can make it panic). It is listed in the lockfile through `reqwest`'s optional HTTP/3
  support, which no Axocoatl crate turns on, so no Axocoatl build compiled it.

### Measured
- On 2026-10-12 both rebuilt supervisors, with each restricted command's own scratch
  directory, passed all eight protocol and process-lifecycle tests in the Alpine and
  Debian images of their architecture, as root and as a non-root user: the aarch64 one on
  a Linux 7.1 aarch64 kernel with rootless Podman (32 of 32), and the x86_64 one on an
  x86_64 Linux 6.12 kernel (Alpine's, with Landlock, in a QEMU virtual machine emulating
  an x86_64 computer on an Apple silicon Mac) with rootless Podman and with Podman as root
  (64 of 64). The read-only test protects the Workspace and then a repository under the
  container's `/tmp`.
- On 2026-10-11 a `fix` run of the pinned Codex 0.160.1 (`gpt-6-luna`) with a ChatGPT
  plan passed through Axocoatl 1.3.1-rc2 against OpenAI's service: Codex's 6 requests to
  `chatgpt.com/backend-api/codex/responses` went through the route with the sign-in the
  daemon added and were answered `200`, the container held only the placeholder, and no
  token-shaped string appeared in the container, the daemon's log, the record, the JUnit
  report or the output. The run cost $0.0014. OpenAI recommends API keys for automation
  and documents no process other than Codex refreshing a Codex sign-in; check your plan's
  terms.

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
  findings and coverage, with anything not covered as a failure and the run's notes in
  the verdict's `<system-out>`. `--record` writes the
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
  such as a Codex writer's on a model without a price, and "$X (includes cost computed
  from reported tokens at list prices)" when part of it is computed from a program's
  reported tokens (`usage.cost_computed`), such as a Codex writer's. Ctrl-C stops the run and exits 6. A run needs its
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
  validate`, and a run whose budget cannot hold one call of its model is refused before
  anything is created, naming the budget and the minimum (exit code 3): an Ollama model's
  loaded context plus the output bound; for an OpenRouter model, read from OpenRouter's
  public catalog, the 4,096-token prompt allowance a call reserves at least (within the
  model's `context_length`) plus the output bound and the reasoning allowance of its
  effort, never the whole context window; for an external writer's model in Axocoatl's
  pinned table, the most one call of the program can use (400,000 tokens for Codex
  0.160.1's models, the model's window for Claude Code's). For Claude Code the table
  pins each Claude API model that has not retired (from Anthropic's model overview,
  model pages and deprecations page), Claude Mythos Preview aside, whose window they do
  not state, and Claude Code 2.1.292's aliases as the model each runs, read from the
  model catalog bundled in that build: `opus` (`claude-opus-5-5`), `sonnet`
  (`claude-sonnet-5-5`), `haiku` (`claude-haiku-4-5`), `fable` and `best`
  (`claude-fable-5-1`; `best` runs `claude-opus-5-5` for an account without Fable) and
  `opusplan` (`claude-sonnet-5-5`, `claude-opus-5-5` in plan mode), and `[1m]` on a
  model or alias whose window is already 1M tokens. An external writer whose model the
  table does not list (a retired or newer model, `default`, whose model is the
  account's, or `[1m]` on a 200K-token model) is not checked, and the run warns
  (`external_model_not_pinned`).
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
  validation, the API, Settings, Team and budget, the run output and the record, through
  whichever provider each runs it: a reviewer on `openrouter:openai/gpt-5.5` runs a Codex
  writer's `openai:gpt-5.5`, and one on `openrouter:anthropic/claude-haiku-4.5` a Claude
  Code writer's `anthropic:claude-haiku-4-5` (case, `.` against `-`, a snapshot date,
  `-latest` and an OpenRouter `:variant` are ignored). A Claude model is compared by its
  family and version, so Claude Code's aliases (`anthropic:haiku` is
  `claude-haiku-4-5`; `best` and `opusplan` count as the model they run by default), a
  `[1m]` suffix and the older order of a Claude id (`claude-3-5-haiku`) are the same
  model as OpenRouter's `anthropic/…` id, either way round. Like
  the qa and audit blocks, `ADJUDICATIONS` is read after a heading in any case and with
  or without Markdown marks, fenced or not, between XML-style tags
  (`<ADJUDICATIONS>` … `</ADJUDICATIONS>`), or as an answer that is only the JSON; a
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
  parallel with a fresh context, and an integrator merges their findings. No Agent of
  the audit has a shell: its workers, planner and integrator have `read_file`,
  `list_dir`, `grep` and `glob` (in the last pre-release test two workers with `bash` spent 39
  minutes on 80 and 81 calls printing "final audit confirmation" scripts). Coverage is
  the host's, never the workers' word. After the plan the host lists the repository's
  files (`git ls-files --cached --others --exclude-standard` in a Git work tree, else a
  walk that skips what `glob` skips; regular files only, at most 20,000) and gives each
  to the first area whose paths match it; a file no area's paths match goes to the area
  whose paths share the most directories with it, else to an area `rest` the host makes
  for such files. A planned area left without files is not run. The plan as executed
  and the assignment are in the run's progress (`assigned`) and the record, and the
  files given to `rest` or by directory are notes. An area whose files need more reads
  than one worker activation makes within its budget is split, before any worker runs,
  into numbered sub-areas (`src-1`, `src-2`, …) of consecutive files, each with its own
  worker, and the split is a note: the reads are one per default `read_file` window of
  each text file, and one activation makes its `invocations` less a fifth (at least 4)
  held back, at two a read, at most 200 (about 120 for the built-in budget). One turn
  runs at most 16 workers whose reads add up to at most 200; more run in further turns,
  since a turn records only so many tool calls. Each worker's instructions name its
  files (past 8 KiB of names, the directories they are in with counts, all to be
  read). From the `read_file` calls its Session recorded, a file is read when the bytes
  the succeeded reads of it returned, in its area worker's activations that answered,
  cover every byte of it, whatever its size: each read covers what its result reports,
  from its `offset` for its `returned_bytes`, and a partial read of a small file leaves
  it unread. Empty files and binary files (a NUL byte in the first 8,000 bytes) need no
  read; a file over 256 KiB is not read, a note. `grep`, `glob` and `list_dir` (and
  `bash`, in a loadout of your own that gives a worker one) read nothing for coverage. When a worker's turn ends with files of its area unread,
  the host runs a follow-up turn with a fresh activation of every such worker naming
  exactly its unread files ("read these files and report additional findings in the
  same format"), and its findings join the area's report; follow-ups go on while each
  one reads something of its files that no earlier read returned, so a worker whose
  follow-up read nothing new gets no other, and the wall clock bounds them. A worker
  without a result gets one too, unless the wall clock, a person's stop or a 400-403
  refusal ended it. Each file still unread after them, or when the wall clock or a stop
  ends them, is not covered, listed by file with why; so is a worker without a result,
  findings beyond a report's 200 left out of it, and an area whose record of tool calls
  cannot be read. Each area's coverage is a `coverage` progress line. What a worker
  lists as `NOT_REACHED` is a note: the host's coverage decides. A worker whose answer
  has no readable `FINDINGS` block gets up to 2 re-asks after the follow-ups, each a
  turn (`audit_reask`) with a fresh read-only activation of that worker offered no
  tools, whose instructions quote its answer and ask for exactly a `FINDINGS` JSON array
  in the documented format (`[]` when it describes none); like follow-ups they are
  bounded by the budget and the wall clock and recorded (`applying_team` and `reask`
  progress lines, the Outcome's turns), and nothing a re-ask does counts as reading.
  Coverage and findings are separate: an area whose files were all read but whose
  findings stay unreadable is not "not covered"; it is listed in the Outcome's
  `unreadable_findings` with every answer the host could not read, and the run needs
  attention with "Findings unreadable for <area>" (in the summary, a `findings_unreadable`
  failure of the JUnit `findings` suite, and the Run outcome panel). The integrator
  still receives such an answer as text. An
  invalid plan, or a planner without an answer (a provider failure, say), gets one
  retry, which quotes the error after an invalid plan; a second attempt without a plan,
  or a repository the host cannot list, leaves the whole scope not covered, and the
  attention line says "The whole scope was not covered". An integrator without an
  answer gets one retry turn (not after the wall clock, a stop or a 400-403 refusal),
  and an integrator whose `FINDINGS` block cannot be read gets up to 2 re-asks, each a
  fresh activation offered no tools that is sent the integrate request again with the
  unreadable answer quoted. When its answer is still unreadable, or it still has no
  result for a provider's reason, the host merges the findings itself: the union of the
  area findings, duplicates by file, line and normalized title removed, each keeping its
  area, with the note "findings merged by the host: …"; that needs no attention (in
  measured runs the merge step never added a finding). When integration has no result
  for another reason (the wall clock, a stop), the workers' findings are reported
  unmerged and `audit integration` is listed as not covered; the attention line counts
  only areas and says "The integration was not read; the area findings are reported
  unmerged". A finding's location is `path:line` only when the line is a number: a
  location such as `ingest/feed.go:line_number`, which a re-ask once wrote from prose
  that named no line, keeps the file alone, with the finding's `line` `null` in the
  Outcome, and the summary and JUnit show the file only. The host checks every finding
  it reports against the files it listed, never against what a model says: a finding at
  a file the repository does not have (looked up after a leading `./` and the
  repository's place in front of it, an absolute path or its directory's name, are
  removed) is not reported; it becomes the note "finding at a path that does not exist:
  <path> (<title>)" (in pre-release testing a planner invented `notify/rotate_keys.py` and the
  notify worker's finding at it reached the Outcome). A finding whose line is beyond the
  end of its file, as the host counts the file's lines, keeps its file with `line`
  `null` and a note. After the integration, and after the host's own merge, findings at
  the same file and the same or a nearby line (at most 2 lines apart, or both without a
  line) whose titles share more than half of their words (filler such as "potential" or
  "function" left out) are reported once, keeping the one with a line, then the longer
  detail, then a severity, with a note that counts and names the ones removed (in
  pre-release testing the integrator kept the billing and rest workers' "Integer Overflow in
  page_count" at `billing/pagination.py:8` twice). A stopped audit
  starts no further turn, lists the areas not started, the files no follow-up read and
  the integration not run as not covered (`stopped`; with no area report to merge the
  integration is skipped and not listed), reports the workers' findings
  unmerged, and keeps in its Outcome the turns, not-covered entries, findings and usage
  observed until the stop, with usage marked incomplete when it is. A worker's
  `FINDINGS` and `NOT_REACHED` keys are read in any case, and every block (`AREAS`,
  `FINDINGS`, `NOT_REACHED`, `ADJUDICATIONS`, `COVERAGE`) is also read between
  XML-style tags, such as `<FINDINGS>` … `</FINDINGS>`. Notes are in the run's
  progress, the record, the Outcome's `notes`, the summary, the Run outcome panel and
  the JUnit verdict's `<system-out>`. The attention line counts areas, not entries. Its
  documentation states the trade-off measured with Claude Code subagents, not through
  Axocoatl: more recall at lower precision and about three times the tokens on an audit
  larger than one context, as a sensitivity analysis.
- **`read_file` reads a long file to its end, in windows that fit the model.**
  `read_file` takes an optional `offset` (bytes, 0 by default) and `limit` (1 to 65,536
  bytes) and returns a window from there; when more follows, the result says
  `truncated: true` and gives `next_offset`, where the next read starts. In a native
  Session a read without `limit` returns the calling Agent's window: 64 KiB, or a
  quarter of its model's context counted at one token per byte when that is smaller
  (8 KiB for a 32,768-token context), from the provider's live context (an Ollama
  model's loaded one); the tool's description states it, and a larger `limit` reads
  that window. The window travels as a hex dump, so `returned_bytes` and `next_offset`
  count the file's own bytes even when it is not UTF-8: only `content` is decoded,
  with U+FFFD for bytes that are not UTF-8 and `invalid_utf8: true`, and a window that
  would end inside a character ends before it. `read_file` now needs `od` and `tail`
  in the Session image, as Debian, Ubuntu, Alpine and macOS have; a Session's readiness
  check now requires both and installs them with Axocoatl's other repository commands.
- **External agents.** A loadout's writer can be the Claude Code CLI
  (`runtime: claude-code`) or the Codex CLI (`runtime: codex`), run inside the Session
  container as the non-root writer user under `--harden`, admitted, granted and captured
  like a native writer. Its model traffic goes through a route whose credential the
  daemon adds on the host, so the container holds only a placeholder; the Session's
  certificate authority is trusted through `NODE_EXTRA_CA_CERTS` and `SSL_CERT_FILE`.
  Every model call is in the network record, and the program's JSON output is parsed into
  the activation's evidence and answer. The writer's model names the provider whose API
  its program calls (`anthropic:` for Claude Code, `openai:` for Codex); any other is
  refused by validation and by a run (exit code 3), so the model a run records is the one
  that ran. Spend is bounded by an up-front reservation, the route's request count and
  the program's own usage report, not per call. Claude Code reports its cost. Codex
  reports tokens but no cost: its cost is computed from its reported uncached input,
  cached input, cache writes and output at the configuration's `pricing` entry for its
  model, else at the list price Axocoatl pins for each model Codex 0.160.1 lists for API
  use (OpenAI's Standard prices of 2026-10-08), and the run settles to it, marked as
  computed in the work log and the Outcome. A Codex model with neither has no known cost,
  and its activation reserves only an equal share of the cost left. A Claude Code
  writer's model may be one of Claude Code's aliases (`haiku`, `sonnet`, `opus`, …),
  which admission checks as the model the alias runs. Each route
  allows only the program's model calls, since every allowed request gets the
  credential; Claude Code's requests for its account's policy limits and remote
  settings are refused and recorded, and it runs without them. A refused request's 403
  hint names the route Axocoatl added for the writer, and one on a loadout's own route
  names the loadout's route, never a `sandbox.egress.routes` entry. When the model API
  answers a program's call through its route with `401`, the not-covered reason says
  first that the stored credential (such as `claude-code-oauth`) was rejected and how to
  store it again: `axocoatl connect claude-code` for Claude Code's, otherwise
  `axocoatl secret set <name>`, piping in only the token.
- **`axocoatl connect claude-code`.** Connects Claude Code without anyone seeing, copying
  or typing its token: it runs `claude setup-token` in a pseudo-terminal the size of your
  terminal, resized with it on `SIGWINCH` (and at least 40 columns by 24 rows, the
  smallest in which Claude Code draws the whole token), with your terminal in raw mode
  (restored on exit, error, signal or panic) and your keystrokes relayed until it exits,
  so what you type while the token is checked stays for your shell. Its output reaches
  your terminal through a filter that keeps a model of the terminal and shows every cell
  of every `sk-ant-…` token as the next character of `[token hidden by axocoatl]` (then a
  blank), so the screen keeps Claude Code's layout. Claude Code's Ink renderer wraps the
  token at its box's width and places each row by cursor movement, rewriting only the
  cells that change; the filter reads the token from the cells at any width, through any
  escape sequence (colors, hyperlinks, cursor movement, an OSC or DCS string splitting
  `sk-ant-`), a border or padding, and writes split anywhere. Clipboard writes (OSC 52,
  and its kitty, iTerm2, tmux and screen forms) and any escape sequence holding a token
  are dropped. It captures exactly one `sk-ant-oat01-…` token in memory (refusing none or
  several; the next line printed after the token, or text after a blank on its last row,
  is not part of it), checks it with Anthropic over HTTPS (`GET /v1/models`, free;
  skipped with `--no-verify`; a `401`/`403` or an unreachable API stores nothing), and
  stores it as `claude-code-oauth` (or `--secret NAME`) through the secret store. It
  checks before the sign-in that the store can take the token (a link or a directory
  where the secret goes, or a store already holding 64 secrets, is refused before
  `claude setup-token` runs, so no token is made and then lost). The token never enters
  an argv, a child's environment, a log, a temporary file or a message; every buffer
  that held it, the `Authorization` header's included, is overwritten when freed, and
  the HTTP client keeps no connection open after the check (its own copies of the
  header are freed with the request). Exit codes 0 stored, 1 rejected, 2 no single
  token, 3 usage, 4 not checked, 5 store failure, 6 interrupted.
- **`axocoatl secret set|list|remove`.** Stores a route credential read from standard
  input (a pipe or a file; a terminal, where the value would show, is refused) or, with
  `--from-env VAR`, from an environment variable (unset or empty is refused), as an
  owner-only file under `secrets/` in the data root. It is never printed, logged or recorded.
  No message repeats an argument that may be a value passed where a name belongs:
  `--from-env "$TOKEN"` or `--from-env VAR=value` (only a variable's name, a letter or
  `_` then letters, digits or `_`, is accepted), `secret set NAME "$TOKEN"`, and a secret
  name that is not valid (`secret set "$TOKEN"`, `connect claude-code --secret "$TOKEN"`)
  are each refused with exit code 3 and a fixed message. A loadout route's
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
- **An Agent whose calls keep showing nothing new is asked for its answer.** Besides
  three rounds that only print and four rounds of the same calls with the same results,
  a native Agent's next request now goes without tools and asks for its final answer
  when 8 of its last 10 calls with the same tool and arguments, or 8 of its last 10
  `bash` commands with the same pattern (the program and its first option, such as
  `python3 -c`), showed nothing new, wherever they fall in the activation. A result
  shows nothing new when at most one in four of its lines is one that no earlier result
  of the activation showed and that the call's own arguments do not contain, so a script
  that prints its own text shows nothing new. An edit or a write, or a command that
  printed nothing, starts these counts again; polling a terminal never counts. The
  request says why ("you repeated the same call (bash `python3 -c …`) 8 times without
  new results"), and a model that calls a tool instead of answering ends the activation
  as failed with that reason, which a run's follow-ups and re-asks then handle like any
  other worker without a result. A read-only Agent (an empty write scope, as every audit
  Agent has) is also watched across kinds, since in pre-release testing audit workers made 60
  to 113 `grep` calls with different arguments over files they had already read: when
  12 of its last 15 calls of any tool and arguments showed nothing new, its next request
  asks once for its final answer and still offers tools, and when 8 more calls show
  nothing new after that, the next request goes without tools like the rule above
  ("after the host asked for your answer, 8 more of your tool calls showed nothing
  new"). Replayed over the 78 recorded audit activations of two pre-release test rounds that made tool
  calls, this would have asked 10 of them for their answer and ended 5 of those, and
  none of the 10 reported a planted defect whose line it first saw only after that
  request. In a later live test of three audits it asked 3 of their 22 activations,
  workers after 27 to 38 calls, and each answered without another call.
- **A cut `grep` result says what it left out.** `grep` returns up to 64 KiB of whole
  matching lines; a result it cut now also gives `returned_matches`, `total_matches`,
  `total_bytes`, `omitted_matches` and `omitted_bytes` (the same search counted in the
  Session's container, its output not brought back) and a `message` saying how many
  matching lines and bytes were left out and to narrow the search with a path (a
  directory or one file) or a more specific pattern.
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
- **A stopped read-only Agent is stopped, not blocked.** A Stop skips an activation's
  After repository capture, so a read-only Agent with `bash` (a Scout, a Reviewer, or
  an audit worker with a shell) that was stopped failed with "its repository captures
  cannot establish which files it changed … any change is kept for review" (class
  `capture_unavailable`), and a run listed it as `blocked`. A read-only Agent cannot change a file's
  contents, so captures it could not complete are no longer a reason to fail it; a
  change its complete captures do show still is. A stopped read-only activation now
  fails as stopped, and a writer stopped before its After capture fails with "it was
  stopped before the host could capture which files it changed …", classed `stopped`
  with any change kept for review.
- **Read-only Agents read a repository only its owner may enter.** Read-only Agents,
  every Agent of an audit and the required reviewer among them, run as `helper_user`,
  which read only what the file modes let any user read: on a Linux host a repository
  made by `mktemp -d` or under `umask 077` (mode `0700`) made every read of theirs fail
  with `Permission denied`, so an audit reported each area as examined by nobody. The
  execution supervisor now gives every command of a read-only helper a view of the
  Workspace: it starts as root (`podman exec --user 0`) and launches the command as
  `helper_user` with `CAP_DAC_READ_SEARCH` as its only capability (in every set and the
  bounding set, ambient so its children keep it, with no-new-privileges and locked
  secure bits), so it reads and lists Workspace files whatever their modes, in a
  Landlock domain that also handles opening files to read or execute them, listing
  directories and every write right. Beneath the Workspace it may read, list and
  execute; elsewhere only what any user may read when the command starts, judged entry
  by entry in the system directories (`/usr`, `/bin`, `/sbin`, `/lib*`, `/opt`, `/etc`,
  `/proc/sys`, `/sys/devices/system/cpu`), plus a few devices and the kernel's global
  `/proc` files. It reads nothing in the writer's home, `/home`, `/root`, `/run`,
  `/tmp`, `/var/tmp` or `/dev/shm`, nor `/etc/shadow` or any process's `/proc` entries
  (its own included; `ps` and `pgrep` do not work in a helper), and it still cannot read
  a writer process's environment or signal it. It writes almost nowhere: its file tools
  (`read_file`, `grep`, `glob`, `list_dir`) may only open `/dev/null` for writing and
  get no scratch directory, and their seccomp filter also refuses changing a file's
  mode, owner or times; its shell writes only beneath a temporary directory of its own
  (its `HOME` and `TMPDIR`, removed when the command ends) and to `/dev/null`,
  `/dev/zero`, `/dev/tty` and `/dev/urandom`, so a program that writes a fixed path in
  `/tmp`, `/var/tmp` or `/dev/shm` instead of `$TMPDIR` fails there. Neither can write,
  truncate, remove or link anything in the Workspace or in a directory any user may
  change inside another user's private one, which the capability takes it through. Its
  seccomp filter also refuses `AF_UNIX` sockets (connected socket pairs, stream and
  sequenced-packet, stay allowed: child processes' pipes and Rust's process spawning,
  and so `cargo check`, use them), `inotify` and `fanotify` watches and setting or
  removing extended attributes, which the capability would otherwise reach through
  directories it could not enter before, and System V IPC and POSIX message queues,
  which no Landlock rule covers and every user of the container shares; so an app's own
  Unix socket, abstract or not, and the egress proxy's identity socket are out of its
  reach, and its file tools, whose filter also refuses opening any socket, do not reach
  the proxy over TCP either. The command's standard
  pipes are its own, so it can write to `/dev/stdout` and `/dev/stderr`. A
  hardened Session container now keeps `CAP_DAC_READ_SEARCH` for root (Podman does not
  grant it by default; root already reads everything through `CAP_DAC_OVERRIDE`). What
  a helper may read outside the Workspace is judged when each command starts, and
  Landlock does not cover passing through a directory, so a helper can still `stat` a
  path it names in a directory it could not enter before, without listing or opening
  it; the Sandboxes page lists these limits. Rootless
  Podman cannot give the helper an idmapped view of a bind-mounted Workspace instead:
  `mount_setattr(MOUNT_ATTR_IDMAP)` needs `CAP_SYS_ADMIN` over the Workspace's
  filesystem, which a rootless container never has. The run's check before its first
  turn stays, and now launches its probe exactly as the helper's commands are: it ends
  the run with exit code 5 only when that view cannot enter or list the repository (one
  another user owns) or the container or kernel cannot give it, naming why (Landlock
  ABI 3, Linux 6.2 or later; or a hardened Session container started by an earlier build,
  which lacks the capability: restart the Session). The walk of the system directories
  takes tens of milliseconds per helper command in common images (about 40 ms for
  Rust's 25,000 entries). Both embedded execution supervisors are rebuilt from this
  source and re-pinned.
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
- **Lifecycle actions no longer wait for another Session's work.** Closing (or
  deleting) an idle Session while another Session's turn held their Workspace waited
  for the Workspace and failed after 60 seconds with a `500` that blamed the Session
  being closed, and creating a Session on that Workspace (`POST /api/sessions`,
  `POST /api/workspaces/{id}/sessions`) answered only when the turn ended, which for a
  turn that needs attention could be never. Close and Delete also waited up to 60
  seconds when the holder was not a turn: the Session's own Ways checks, which they did
  not interrupt, or another operation that took the Workspace after their first check.
  Every lifecycle action (creating, closing, deleting or reopening a Session, changing
  or repairing its environment, confirming its runtime cleanup) and the start of
  several Ways now takes the Workspace before it changes anything and never waits for
  another Session's work: while another Session's turn, file or Git change, Ways,
  creation or run admission holds it, the action is refused at once with `409` and
  `"code": "workspace_busy"`, naming that Session, run and turn or that operation, and
  nothing changes; try again once it has ended. Close and Delete wait only for their own
  Session's work, which they stop: its turn, until its running command reaches a safe
  point (under a second when measured; at most 30 seconds), and its running Ways or Ways
  checks, which they interrupt, or its runtime while it is starting (at most 15
  seconds each), then they are refused the same way. A second Close, Delete or
  environment change of a Session while one is in progress is refused at once. Other
  requests a person waits on (attachment, file and Git changes, the Ways reads, checks,
  judge and Keep) are refused at once while a turn holds the Workspace and wait at most
  10 seconds for another operation, then are refused, naming it. A turn that cannot
  start because the Workspace is held is busy too, and a loadout run then exits 7, not 5.
- **Close removes a Session's runtime volumes, and the daemon removes the ones left
  behind.** Closing a Session left its `axo-egr-`, `axo-egi-`, `axo-svc-` and `axo-ca-`
  Podman volumes behind, hundreds after many runs. Close now removes them: their sockets
  and trust files are filled again when the Session starts. The Node dependency volume
  (`axo-ses-<id>-node-modules`) still stays until the Session is deleted, so Reopen
  reuses the installed dependencies. Volumes that Sessions closed by 1.2, a daemon that
  stopped before Close, or a removed data root left behind are removed when the daemon
  starts (or, when Podman is not running then, before the first local Session starts):
  each carries the runtime authority label of the data root whose daemon made it, and a
  daemon removes only its own whose Session is closed, deleted or unknown to its data
  root. It keeps an open Session's and never touches another daemon's. A Node dependency
  volume now carries that label too, and the daemon also removes its own whose Session
  is deleted or unknown to its data root (a removed data root, or a cleanup that was cut
  short, leaves one), keeping an open or closed Session's. One made before 1.3.0 has no
  label: it is removed only when its Session is one its data root created and deleted
  (the data root still holds that Session's native History), so a Session another data
  root made is never touched. The log says how many it removed and kept, and `axocoatl
  doctor` shows it ("Session volumes: the daemon removed …"). The browser test harness
  deletes its fixture daemon's Sessions when a test ends, so the browser gate leaves no
  container or volume behind.
- **`GET /api/sessions/{id}/export` exports native Sessions.** Without
  `history_version` it answered `400` for a Session whose History holds native
  execution, such as every loadout run's. It now exports such a Session in the
  versioned form, as JSON or Markdown, like the record bundle and the other History
  reads; `GET /api/session-turns/search` with a `session_id` does the same. An unknown
  Session is a `404`, not a `400`.

### Security
- Hardened commands' seccomp filter also refuses `open_by_handle_at`, which opens a
  file by its handle without checking the directories above it; a read-only helper's
  filter also refuses `AF_UNIX` sockets other than connected socket pairs, `inotify`
  and `fanotify` watches, extended attribute changes, System V IPC and POSIX message
  queues (before, a helper could read or change a writer's IPC object whose mode let
  any user, and leave its own behind), and its file tools' filter also refuses changing
  a file's mode, owner or times and opening any socket (before, only their fixed
  commands kept them from connecting to the egress proxy's loopback relay, which
  refuses a connection without a credential; see Fixed). The supervisor refuses a
  helper's launch whose Workspace is a symbolic link or not a directory, and one
  without `--harden`.
- A read-only helper's file tools in a hardened Session container write nothing
  (before, only the file modes limited what they could write), and its shell writes
  only in a temporary directory of its own and to four devices instead of anywhere in
  `/tmp`, `/var/tmp` and `/dev` (see Fixed).
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
