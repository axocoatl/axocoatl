# Axocoatl local verification guide

This is a bounded smoke-test checklist for the current workbench and runtime.
It deliberately does not promise coverage of every feature or provider. The
browser app at `/` is the product seam; library unit tests are supporting
evidence, not a substitute for exercising that journey.

## Prerequisites

- Rust 1.88 or newer.
- Ollama with `llama3.2` available for the local-provider checks. Native Sessions
  require an audited local Ollama profile; see the
  [provider guide](https://docs.axocoatl.ai/configure/providers/) for the enforced boundary.
- Podman for directory sessions and parallel attempts.
- A disposable Git repository for any test that lets an agent change files.

Start Ollama in a separate terminal if it is not already running:

```bash
OLLAMA_NO_CLOUD=1 ollama serve
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

Use a new child path inside a temporary directory. Axocoatl must create the data root
itself to select native Session storage; an already-existing empty directory retains
the compatibility format. The starter config also performs its one-time legacy
workflow-to-Automation seed:

```bash
export AXOCOATL_TEST_DATA="$(mktemp -d)/data"
echo "$AXOCOATL_TEST_DATA"
AXOCOATL_DATA_DIR="$AXOCOATL_TEST_DATA" \
  cargo run -p axocoatl-cli -- dev -c axocoatl.example.yaml
```

Expect the daemon to expose IPC and the browser app at
`http://127.0.0.1:8080`, and to print a `Sign in:` link. The API requires the
per-daemon token from the data directory. In another terminal, set
`AXOCOATL_TEST_DATA` to the path the `echo` printed, then run the checks there.
Later steps use this second terminal too:

```bash
export AXOCOATL_TEST_DATA='/path/printed/by/echo'
curl -fsS http://127.0.0.1:8080/health
curl -fsS http://127.0.0.1:8080/health/ready
# curl reads the Authorization header from stdin, keeping the token out of argv.
axo_api() { printf 'Authorization: Bearer %s\n' "$(cat "$AXOCOATL_TEST_DATA/local-api-token")" | curl -fsS -H @- "$@"; }
axo_api http://127.0.0.1:8080/api/agents
axo_api http://127.0.0.1:8080/api/automations
```

The starter config defines the default team (Lead, Scout, Reviewer) and two Ollama
Agents for its legacy `hello-world` record, which should appear as a manual record in
the canonical Automation store on this first boot.
If you reuse an existing data directory, its `automations.json` remains
authoritative and YAML is not imported again.

## 3. Exercise the one-app session loop

Open the `Sign in:` link the daemon printed (or run
`AXOCOATL_DATA_DIR="$AXOCOATL_TEST_DATA" cargo run -p axocoatl-cli -- url -c axocoatl.example.yaml`)
and use a disposable Git repository.

1. Choose **Open workspace…**, authorize the repository, and confirm its Workspace name.
2. With that Workspace selected, create a Session with the `researcher` agent.
3. Review the exact runtime and setup proposal. Leave a detected command such
   as `npm ci` unchecked unless you intend to run it, or explicitly choose no
   setup. Wait for the durable environment state to become **Ready**.
4. Before Ready, confirm Conversation Send, Files, Source Control, Terminal,
   Preview, and Ways fail closed rather than reading or executing on the host.
5. Open **Team & budget**, edit the selected Agent, set an explicit maximum output,
   activation/invocation/token/cost limits and expiry, then preview and apply the
   configuration. Confirm Send is refused before approval. Ask for a small,
   verifiable file change after approval.
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
   agent/model selections where available. For native Ways, configure explicit
   History storage limits and review each attempt's budget before starting.
2. In the Ways inspector, observe running, completed, failed, blocked, or
   interrupted states without losing the Session conversation.
3. Open the focused **Attempts** review and run **Checks** after every attempt
   is terminal.
4. Compare **Outcome** and **Route**, then use **Judge** if configured.
5. Choose **Keep this one** only for a passing, non-empty result.
6. Confirm the selected delta is in the primary working tree, no commit was
   created, and the unresolved attempt set is cleaned up.
7. In native History, reopen the retained decision after cleanup. Check its
   bounded Outcome, Route, diff, Checks, usage and human choice; export it and
   attach it as context. Unavailable or truncated evidence must remain labeled.

Also test **Discard** before Keep begins. Starting a second attempt set or a
normal session turn while one is unresolved should return a lifecycle conflict,
not silently replace it. Native decisions that keep nothing remain in History;
deleting the retained record is a separate explicit action.

## 5. Exercise the canonical Automation path

The Settings page and `/api/automations` read the same `AutomationStore` used by
the live dispatcher. Run the seeded manual record from **Settings →
Automations**, or use the compatibility route:

```bash
axo_api -X POST \
  http://127.0.0.1:8080/api/workflows/hello-world/execute \
  -H 'Content-Type: application/json' \
  -d '{"input":"Explain ownership in two stages."}'
```

Expect the explicit DAG executor to run the researcher node before the
summarizer node.

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
Session environment, and wait for Ready. For native storage, also review and apply
**Team & budget** in the browser. Only then execute and close it:

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

Some sandbox tests require Podman, prepared images, the internet or a live
Ollama and are ignored by default. CI runs them through
`scripts/test-podman-suites.sh`, which you can run too: `list GROUP` shows a
group's tests without Podman, `prepare GROUP` pulls and builds the images they
need, and `run GROUP` runs them (groups `isolation`, `daemon`, `cli`, `other`
and `ollama`; see the script's header). Webhook tests bind
a local port; if an execution sandbox blocks that bind, rerun the same test in
an environment that permits it rather than treating the denial as a product
failure.

The tests pass under any umask (the native gate runs them under 002,
Ubuntu's login default) and whatever `CONTAINER_HOST` or `CONTAINER_CONNECTION`
your shell sets, with one exception:
`bootstrap::tests::installed_format_admits_checked_cleanup_without_mutating_legacy_history`
runs the daemon's startup check in the test process, and, as the daemon does,
it refuses a connection one of those variables names when Podman does not
answer through it. Unset them, or start that connection.

For focused runtime work, useful package gates include:

```bash
cargo test -p axocoatl-daemon
cargo test -p axocoatl-server
cargo test -p axocoatl-isolation
```

## 9. Verification-only build: a stale ChatGPT access token

The daemon refreshes a Codex ChatGPT sign-in after a `401`. To exercise that path
against OpenAI's service instead of a fake, build a verification binary with the
`live-verify` cargo feature (off by default; never use it for a release):

```bash
cargo build --release -p axocoatl-cli --features live-verify
```

Start that binary's daemon with `AXOCOATL_VERIFY_STALE_ACCESS_ONCE=1` in its
environment. The daemon logs at `WARN` that the verification switch is active. The
first request of the process to `chatgpt.com` (a Codex writer's first model call with
`credential: codex-chatgpt`) then carries a fixed invalid access token instead of the
stored one, and the daemon logs that at `WARN` too. When the upstream answers `401`,
the daemon logs `WARN` that the `401` to the invalid token arrived, refreshes the
stored sign-in once at OpenAI's token endpoint (`refreshed the ChatGPT sign-in` at
`INFO`; OpenAI rotates the tokens, which are written back to `secrets/codex-chatgpt`),
and the next request carries the refreshed token. In Axocoatl's Podman test
(`the_pinned_codex_retries_a_401_after_the_stale_access_switch_and_the_run_passes`),
the pinned Codex 0.160.1 sends the call again after the `401` and the run passes.
The switch fires once per process. Without the variable, or with any value but `1`, it
stays off; a binary built without the feature never reads the variable.

Release builds must not have the feature: `./scripts/check-no-live-verify.sh BINARY`
refuses a binary that does, and the native, cross-Linux and release gates run it.

```bash
cargo test -p axocoatl-daemon --features live-verify --test live_verify_switch
CONTAINER_CONNECTION=<connection> cargo test -p axocoatl-daemon --lib \
  turn_tests::the_pinned_codex_retries -- --ignored
```

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
