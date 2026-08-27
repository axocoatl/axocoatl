# Troubleshooting

Run `axocoatl doctor` first — it diagnoses most of the issues below
automatically with fix hints.

## Install

**`axocoatl: command not found` after install.sh**
The binary went to `~/.local/bin`, which isn't on your PATH. Add:
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

**`Token budget exceeded: used N, budget M`**
Working as designed. Before each provider call, Axocoatl reserves the locally
estimated input plus a bounded completion. With `overflow_policy: abort`, a call
that cannot fit is not sent. A provider-reported overrun also stops the current
turn, although those remote tokens may already have been incurred. Switch to
`warn` to continue past the guard, or raise `per_call` / `per_execution`. Core
memory and tool schemas count toward the input estimate. The guard is not an
absolute provider billing cap.

**`actor is likely terminated` after a budget abort**
Expected: `abort` policy terminates the agent. Restart it
(`axocoatl agents restart <id>`) or use `warn`.

**Workflow agents run in parallel instead of cascading**
Ensure downstream agents declare `depends_on: [<upstream>]` and the workflow
sets a correct `entry_point`. Entry agents must have `depends_on: []`.

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
