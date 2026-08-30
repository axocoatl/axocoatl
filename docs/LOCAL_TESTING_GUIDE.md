# Axocoatl local verification guide

This is a bounded smoke-test checklist for the current workbench and runtime.
It deliberately does not promise coverage of every feature or provider. The
browser app at `/` is the product seam; library unit tests are supporting
evidence, not a substitute for exercising that journey.

## Prerequisites

- Rust 1.88 or newer.
- Ollama with `llama3.2` available for the local-provider checks.
- Podman for directory sessions and parallel attempts.
- A disposable Git repository for any test that lets an agent change files.

Start Ollama in a separate terminal if it is not already running:

```bash
ollama serve
ollama pull llama3.2
```

Run the remaining commands from the Axocoatl repository root.

## 1. Build and validate the starter config

```bash
cargo build -p axocoatl-cli
cargo run -p axocoatl-cli -- validate axocoatl.example.yaml
```

Expect validation to succeed. Do not assert an exact repository-wide test count
or binary size; both change as the workspace evolves.

## 2. Start a clean local runtime

Use a fresh data directory so the starter config can perform its one-time legacy
workflow-to-Automation seed:

```bash
export AXOCOATL_TEST_DATA="$(mktemp -d)"
AXOCOATL_DATA_DIR="$AXOCOATL_TEST_DATA" \
  cargo run -p axocoatl-cli -- dev -c axocoatl.example.yaml
```

Expect the daemon to expose IPC and the browser app at
`http://localhost:8080`. In another terminal:

```bash
curl -fsS http://localhost:8080/health
curl -fsS http://localhost:8080/health/ready
curl -fsS http://localhost:8080/api/agents
curl -fsS http://localhost:8080/api/automations
```

The starter config has two Ollama agents. Its legacy `hello-world` record should
appear as a manual record in the canonical Automation store on this first boot.
If you reuse an existing data directory, its `automations.json` remains
authoritative and YAML is not imported again.

## 3. Exercise the one-app session loop

Open `http://localhost:8080` and use a disposable Git repository.

1. Choose **Open workspace…**, authorize the repository, and confirm its Workspace name.
2. With that Workspace selected, create a Session with the `researcher` agent.
3. Review the exact runtime and setup proposal. Leave a detected command such
   as `npm ci` unchecked unless you intend to run it, or explicitly choose no
   setup. Wait for the durable environment state to become **Ready**.
4. Before Ready, confirm Conversation Send, Files, Source Control, Terminal,
   Preview, and Ways fail closed rather than reading or executing on the host.
5. Ask for a small, verifiable file change.
6. Confirm Conversation remains in the main area. From the Session header or
   **More**, open Files/editor, Preview, Source Control, and Agent graph as focused
   tools; Terminal stays in its bottom dock.
7. Run the repository's real check command from the workbench.
8. Inspect the resulting git diff. The app must not commit automatically.
9. Reload the page and resume the same Session. Verify the transcript and
   workspace identity remain attached to it.

For UI changes, also inspect light, dark, narrow, and reduced-motion states and
check the browser console. A DOM-only check is not visual verification.

## 4. Exercise several attempts

Parallel attempts currently require a single autonomous-Agent Session on the local Podman
backend and a Git repository. A coordinator or worker cannot be selected as a Way. Ways use
their configured primary provider/model without rate-limit fallback so the retained identity
and cost cannot describe a route that did not run.

1. From the session, choose **Explore several ways**. Use the contextual **Ways**
   inspector to configure at least two attempts with deliberately different
   agent/model selections where available.
2. In the Ways inspector, observe running, completed, failed, blocked, or
   interrupted states without losing the Session conversation.
3. Open the focused **Attempts** review and run **Checks** after every attempt
   is terminal.
4. Compare **Outcome** and **Route**, then use **Judge** if configured.
5. Choose **Keep this one** only for a passing, non-empty result.
6. Confirm the selected delta is in the primary working tree, no commit was
   created, and the unresolved attempt set is cleaned up.

Also test **Discard** before Keep begins. Starting a second attempt set or a
normal session turn while one is unresolved should return a lifecycle conflict,
not silently replace it.

## 5. Exercise the canonical Automation path

The Settings page and `/api/automations` read the same `AutomationStore` used by
the live dispatcher. Run the seeded manual record from **Settings →
Automations**, or use the compatibility route:

```bash
curl -fsS -X POST \
  http://localhost:8080/api/workflows/hello-world/execute \
  -H 'Content-Type: application/json' \
  -d '{"input":"Explain ownership in two stages."}'
```

Expect the explicit DAG executor to run the researcher node before the
summarizer node. This is not a lattice-threshold activation loop.

Create a net-new Automation with **+ Automation** in Settings. Choose its ID,
name, starter Agent, and manual, interval, event, or Skill trigger; Settings
persists a valid Input → Agent starter graph through `POST /api/automations`
and opens it for editing. Verify the running daemon sees the new record and
subsequent edits without a restart. The HTTP endpoint remains available for
programmatic creation, and legacy YAML can seed the initial store when no
canonical store file exists. Scheduled Automations accept fixed intervals such
as `30s`, `5m`, `2h`, and `1d`; cron expressions are not supported.

For a top-level Interrupt, stop the daemon while the run is parked, restart it,
and verify the prompt reappears and resumes without replaying completed nodes.
This recovery boundary does not reconstruct an arbitrary provider/tool call
that was in flight, and an Interrupt inside a nested Subgraph remains
process-local.

## 6. Exercise directory-session CLI compatibility

With the daemon running, use a disposable repository path:

```bash
cargo run -p axocoatl-cli -- session new /absolute/path/to/repository --agent researcher
cargo run -p axocoatl-cli -- session list
```

`session new` does not silently approve detected project setup. If its output
prints a proposed command as **not run**, open the browser, review that exact
Session environment, and wait for Ready. Only then execute and close it:

```bash
cargo run -p axocoatl-cli -- session exec <session-id> "Inspect the repository and report its checks."
cargo run -p axocoatl-cli -- session close <session-id>
```

`axocoatl chat` is a legacy agent-global console. Its accepted `--session` value
does not currently select or restore an isolated workspace Session; use the
browser app or `session` subcommands when session identity matters.

## 7. Verify `serve` exposes HTTP and IPC

Stop `dev`, then start the same clean configuration in service mode:

```bash
AXOCOATL_DATA_DIR="$AXOCOATL_TEST_DATA" \
  cargo run -p axocoatl-cli -- serve -c axocoatl.example.yaml
```

Expect this mode to expose both HTTP/browser and IPC against its daemon state. Recheck
`/health`, list Automations, and run a directory-session CLI read. `serve` is not
HTTP-only.

## 8. Repository verification gates

Before pushing any PR, run the complete repository gate with the CI-pinned
runtimes and tools documented in `CONTRIBUTING.md`:

```bash
./scripts/preflight.sh
```

This includes workflow, film, site, browser, cross-Linux, installer, release,
MSRV, Rust, and package checks. The focused commands below are useful during
iteration, but they do not replace a green preflight before a PR.

Some sandbox tests require Podman and are ignored by default. Webhook tests bind
a local port; if an execution sandbox blocks that bind, rerun the same test in
an environment that permits it rather than treating the denial as a product
failure.

For focused runtime work, useful package gates include:

```bash
cargo test -p axocoatl-daemon
cargo test -p axocoatl-server
cargo test -p axocoatl-coordination
cargo test -p axocoatl-isolation
```

The coordination package tests its reusable event/signal, HTN, and auction
primitives. Passing those tests does not prove that the product daemon uses the
standalone example activation loops.

## Quick reference

| Action | Command |
|---|---|
| Validate starter | `cargo run -p axocoatl-cli -- validate axocoatl.example.yaml` |
| Dev (IPC + HTTP) | `cargo run -p axocoatl-cli -- dev -c axocoatl.example.yaml` |
| Serve (IPC + HTTP) | `cargo run -p axocoatl-cli -- serve -c axocoatl.example.yaml` |
| List sessions | `cargo run -p axocoatl-cli -- session list` |
| List manual Automations | `cargo run -p axocoatl-cli -- workflow list -c axocoatl.example.yaml` |
| Build release CLI | `cargo build -p axocoatl-cli --release` |
| Full test suite | `cargo test --workspace` |
