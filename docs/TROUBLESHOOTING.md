# Troubleshooting

Run `axocoatl doctor` first — it diagnoses most of the issues below
automatically with fix hints.

## Install

**`axocoatl: command not found` after install.sh**
The binary went to `~/.local/bin`, which isn't on your PATH. If you let the installer
add it to your shell's startup files, open a new terminal. Otherwise add the line it
printed to the files it named (`~/.zprofile` for zsh on macOS; for bash on Linux
`~/.bashrc` and `~/.profile`, or `~/.bash_profile` when you have one, and `~/.bashrc`
alone when `~/.profile` adds `~/.local/bin` already, as Ubuntu's does); this sets it for
the current shell only:
```sh
export PATH="$HOME/.local/bin:$PATH"
```

**`cargo install axocoatl-cli` fails to compile**
Needs Rust 1.88+. Update with `rustup update stable`. Or use the prebuilt
binary: `curl -fsSL https://axocoatl.ai/install.sh | sh`.

## Ollama

**`Ollama not reachable at http://localhost:11434`**
Start it: `ollama serve &`. Verify: `curl http://localhost:11434/api/tags`.

**`Model 'llama3.2' not pulled`**
`ollama pull llama3.2`. Confirm with `ollama list`.

**`Ollama at http://localhost:11434 has cloud models enabled; native Sessions refuse it`**
Native Sessions use only an Ollama server that reports its cloud features
disabled. `axocoatl onboard` turns them off where the running server reads them,
after asking, and waits until the server says so. By hand: for the Homebrew service
or an `ollama serve` you run, add `{"disable_ollama_cloud": true}` to
`~/.ollama/server.json` of the user that runs it and restart it
(`brew services restart ollama`); for the macOS app, turn off Cloud in its Settings;
for the Linux service, which runs as the user `ollama`, add
`Environment="OLLAMA_NO_CLOUD=1"` with `sudo systemctl edit ollama` and run
`sudo systemctl restart ollama`. `curl -s http://localhost:11434/api/status` must
then show `"disabled":true`. You can instead run a second server, for example
`OLLAMA_NO_CLOUD=1 OLLAMA_HOST=127.0.0.1:11436 ollama serve`, and set
`providers.ollama.base_url` to it.

**Ollama version refused**
Native Sessions accept Ollama 0.40.6 and later (the audited version is 0.40.6). Later
0.40 releases are accepted; a later 0.x release (0.41 or later) is accepted once the
contract probes pass on each model, with a warning in `axocoatl doctor`. Ollama 0.20.6
and older, pre-releases, development builds and a new major release are refused, and
`axocoatl doctor` names the fix for how Ollama runs here: an upgrade
(`brew upgrade ollama && brew services restart ollama`, or the pinned Linux installer
`curl -fsSL https://ollama.com/install.sh | OLLAMA_VERSION=0.40.6 sh`), a restart when
the `ollama` on PATH is newer than the server that answers, or 0.40.6 in place of a
version that is too new. A Team reviewed on Ollama 0.20.6 asks to be reviewed again
("Ollama was upgraded from 0.20.6 to 0.40.6; review Team & budget again"), in its
turns and in each helper its lead delegates to: in Team & budget, select Edit, then
Preview and Apply with nothing changed. Its Ollama Agents and their Ollama helpers are
observed again, and the Agents start new conversations. With the Ollama app on macOS,
download it again from https://ollama.com/download, then quit and reopen it so that the
new version answers.

**A daemon older than this CLI**
After an upgrade, a daemon started before it (the Always-On Service or an
`axocoatl dev` terminal) runs the old binary until it restarts. `axocoatl doctor`
names its version, or says it is older than Axocoatl 1.5.0 when it does not answer
the version request. Restart it: `axocoatl service restart`,
or stop `axocoatl dev` and start it again.

**Axocoatl 1.4.0 does not start after 1.5 used its data**
"could not recover canonical history for Session …" with "unknown variant
`provider_response`" or "invalid checked repository identity": Axocoatl 1.5 ran a
workbench turn or an `axocoatl run` in this data root (it records how each model
response ended) or recorded a change larger than 512 KiB, which 1.4 cannot read, and no
command converts it. Go back to 1.4 with the cold backup made
before the upgrade: restore it, start Axocoatl 1.5 on it once and stop it (1.4.0 refuses
a restored copy until then), then start 1.4.0, which runs native Ollama Sessions only on
Ollama 0.20.6. Work done with 1.5 is not in that backup. A large file a 1.5 run left
uncommitted in a repository (1 MiB, measured) makes 1.4.0's runs there end needing
attention ("repository command output is incomplete or exceeds its capture bound"):
commit or move it first. The Upgrade page's "Data written by 1.5" has the steps.

**A model loaded with a 4096-token context**
Ollama gives models 4096 tokens on computers with less than about 23 GiB of graphics
memory, and a fix run needs at least 16384. `axocoatl doctor` fails such a model;
`axocoatl onboard` offers to set `OLLAMA_CONTEXT_LENGTH=32768` for the Ollama service
(`~/.homebrew/services/ollama.env` for the Homebrew service, a systemd drop-in on
Linux) and restart it.

**Model refused as not GGUF, or run with MLX**
Native Sessions run only GGUF models, which Ollama runs in llama.cpp's `llama-server`.
On Apple silicon Ollama pulls the MLX variant of some models, and MLX does not hold
`num_ctx` as a hard limit. `ollama rm MODEL && ollama pull --runner llamacpp MODEL`
replaces it with the GGUF variant (`--runner` is an undocumented Ollama flag, and a pull
with it leaves a variant already installed in place, hence the `ollama rm`);
`qwen3-coder:30b` and `gpt-oss:20b` are GGUF.

**Model refused by a contract probe**
When a Team is reviewed, Axocoatl checks on your server that a prompt longer than the
model's context is refused before generation, that a call stops at its output limit and
reports the tokens it generated, and, for a model with tool calling, that a request to
call the one tool it is offered comes back as a call of that tool. The error names the
check that failed; native Sessions and runs refuse that model on that Ollama for every
Agent, so use another model or Ollama release.

## Config

**`Config invalid` / parse errors**
`axocoatl validate` checks the current user's configuration and prints the
exact field and a suggestion. For an explicit project-local configuration, run
`axocoatl validate /absolute/path/to/axocoatl.yaml` instead.
Common causes: missing `name` on an agent, `per_call > per_execution`,
duplicate agent IDs, unresolved `${ENV_VAR}` (export it in the process
environment that starts Axocoatl).

**API provider key warnings**
`axocoatl onboard` asks for hosted-provider keys with a masked prompt. A value
entered there is stored literally in the current user's owner-only
`config.yaml`; leaving the prompt blank keeps the `${OPENAI_API_KEY}`-style
placeholder instead. In that case, export the variable in the process that
starts Axocoatl. Axocoatl does not create or load a `.env` file.

The default user files are:

- macOS: `~/Library/Application Support/Axocoatl/config.yaml` and
  `~/Library/Application Support/Axocoatl/data`
- Linux/WSL: `${XDG_CONFIG_HOME:-$HOME/.config}/axocoatl/config.yaml` and
  `${XDG_DATA_HOME:-$HOME/.local/share}/axocoatl`

Normal commands use those paths regardless of the current directory. A local
`axocoatl.yaml` is used only when you pass it explicitly with `--config` (or as
the positional argument to `validate`).

If you deliberately use an environment placeholder, export it before starting
the foreground daemon:

```sh
export OPENAI_API_KEY=replace-me
axocoatl dev
```

## Runtime

**A Session says `awaiting_approval`**
Open **Review setup**. Approve only the exact proposed command, or clear it and
record an explicit no-setup decision. A detected `npm ci` is never approved by
the daemon's devcontainer default. When
`sandbox.allow_post_create_command: true`, only the exact unedited
`devcontainer.json` command starts checked for an unreviewed Session; an
explicit checked or unchecked Session decision wins.

**A Session environment failed before Files or Terminal opened**
Read the retained environment error and bounded setup output. Files, Git,
Terminal, Preview, new turns, and Ways actions that start work or inspect a
live checkout deliberately fail closed until the Session is Ready. Durable
History, completed Attempt evidence, and Keep/Discard recovery remain
available. Correct the image or exact setup command and choose **Rebuild
environment**; repository tools never fall back to direct host file access.

**Podman is missing, has no VM, or has a stopped VM**
Axocoatl never installs host software or creates a Podman VM during Session
startup. Follow the printed installation or `podman machine init` command. It
may start a VM that already exists but is stopped.

**The Always-On Service cannot find Podman that works in your shell**
A service starts with the service manager's minimal `PATH`. `axocoatl service
install` records the directory of the `podman` your shell finds, plus
`CONTAINER_CONNECTION` and `CONTAINER_HOST` when set. Reinstall from a shell
where `podman` works, then run `axocoatl service restart`.
On macOS the service log is `~/Library/Logs/Axocoatl/daemon.log`.

**A `network: none` Session cannot become Ready**
The selected image may be missing Git or another repository command Axocoatl
requires. With no container network, distro-aware provisioning cannot download
it. Use an image that already contains the required commands; an image outside
the curated list also requires `sandbox.allow_untrusted_images: true`. Podman
may still pull the selected image as a host operation; that is separate from
Session container egress.

**E2B rejects the Session image**
E2B uses the daemon-global `sandbox.e2b.template`; it cannot honor a
per-Session or devcontainer OCI image. Clear the Session image or switch the
daemon backend to Podman. If the E2B template lacks required repository
commands, rebuild the template itself before retrying.

**E2B creation remains blocked with an exact creation token**
Restore provider access so Axocoatl can reconcile the retained token. If that
is impossible, delete every sandbox whose metadata contains
`axocoatl_creation_token=<exact token>`, then use the exact-token manual
confirmation in **Review setup**. Confirmation only releases the durable
record; it does not contact or delete E2B.

**`This Agent reached its token limit for this activation (M tokens; N needed)`**
(before 1.1.0: `Token budget exceeded: used N, budget M`)
Working as designed. Before each provider call, Axocoatl reserves the locally
estimated input plus a bounded completion. With `overflow_policy: abort`, a call
that cannot fit is not sent. When what is left cannot pay for another tool round
and an answer, the Agent's next request goes without tools and asks for its
final answer, so this error means even that answer did not fit. A
provider-reported overrun also stops the current turn, although those remote
tokens may already have been incurred. Switch to `warn` to continue past the
guard, or raise `per_call` / `per_execution`. Core memory and tool schemas count
toward the input estimate. The guard is not an absolute provider billing cap.

**`The Session budget for this Agent is used up: ...`**
The Session grant (Team & budget) has no room for the Agent's next model call:
its tokens, model and tool calls, or spending limit, as the message says. The
Agent asks for its final answer without tools before this happens when it can;
the message means even that call could not be admitted. Raise the limit, then
continue, or finish with the partial result.

**`actor is likely terminated` after a budget abort**
Expected: `abort` policy terminates the agent. Restart it
(`axocoatl agents restart <id>`) or use `warn`.

**A multi-Agent turn is refused on a 1.0 data root**
Expected. A data root that still uses the 1.0 format runs one Agent per Session
turn; single-Agent Sessions and a request targeted at one Agent keep working. To
run a team, stop Axocoatl, make a cold backup, and run
`axocoatl session upgrade --confirm`. The upgrade converts 1.0 Sessions,
including multi-Agent Sessions, to native Sessions.

**Workflow times out (300s)**
A slow/unreachable provider, or an agent never completing. Check the daemon
logs (`axocoatl dev` prints them) and `axocoatl agents status`.

## Daemon / IPC

**A command says "requires a running daemon"**
Session subcommands and `agents restart` need the persistent daemon. Start
`axocoatl dev` or `axocoatl serve`; both expose IPC and HTTP. One-shot commands
such as `chat` and workflow execution can fall back to an in-process daemon.

**`axocoatl chat` connects in-process instead of via IPC**
No daemon is running at the stable per-user socket, or the configured
`AXOCOATL_SOCKET_PATH` differs between processes. Start `axocoatl dev` or
`axocoatl serve`; startup removes only a stale socket and refuses to replace a
live daemon or a non-socket path.

## MCP

**`mcp servers` is empty**
Add an `mcp_servers:` section to the config. `stdio` servers need `command`;
`streamable_http` servers need `url`. A failing server logs a warning at
bootstrap but never aborts the daemon — check the logs.

## Still stuck?

Run with verbose logs: `RUST_LOG=debug axocoatl dev`. File an issue at
https://github.com/axocoatl/axocoatl/issues with the output of
`axocoatl doctor`.
