# Embedded workbench browser regressions

These tests combine real workbench and HTTP-route journeys with focused browser
component fixtures served from the actual `axocoatl` binary. A few focused seam
tests extract existing handlers from that binary's `/` page. Component checks
complement the visible workbench journeys; they do not replace them.

The harness builds `axocoatl-cli`, starts a fresh daemon on a free loopback
port, assigns it a unique data directory and IPC socket, creates two temporary
project folders, and seeds named Workspaces and AwaitingApproval Sessions
through the public HTTP API. Chromium then drives the visible `/` product
surface. When a test file ends, `runtime.stop()` deletes every Session of its
fixture daemon through the API (`DELETE /api/sessions/{id}?force=true`, which
removes a Session's containers, runtime volumes and dependency volume), stops the
daemon, and then removes, by exact name, any container or volume still named after
one of those Sessions; nothing is matched by prefix, so another daemon's
containers and volumes are never touched. The temporary daemon, socket, data, and
projects are then removed; an already-running Axocoatl daemon is not touched. So
the suite leaves nothing behind on Podman, including the egress volumes of the
loadout runs. Set `AXOCOATL_E2E_TEARDOWN_LOG` to a file to have each teardown append
one JSON line with the fixture's Session ids and anything it had to remove by name.

## Local API token

Each daemon requires its per-daemon local API token, which it writes to
`local-api-token` in its data directory. After the daemon is healthy, the
harness reads that file and registers the daemon for the test process:

- seeding and the runtime's helpers send `Authorization: Bearer <token>`;
- Node-side `fetch` calls to a registered daemon origin get the same header
  unless the caller set a credential, and the `/` redirect from `127.0.0.1` to
  `localhost` is followed with the header kept;
- `newAuthorizedContext(browser, options)` (or `authorizeContext(context)`)
  adds the `axocoatl-token-<port>` cookie for `localhost` and `127.0.0.1`,
  exactly as the sign-in link would;
- Node `WebSocket` clients pass the header explicitly with `runtime.token`.

`runtime.fetchWithoutToken`, `runtime.signInUrl`, and a plain
`browser.newContext()` exercise the signed-out paths; `local-api-token.test.mjs`
covers the real sign-in link. The daemon runs in its temporary directory with a
relative IPC socket path, so a long `TMPDIR` cannot exceed the Unix socket path
limit.

## Run

```bash
cd axocoatl-server/browser-tests
npm ci
npx playwright install chromium
npm test
```

CI installs the matching Chromium plus its Linux runtime dependencies with:

```bash
npx --no-install playwright install --with-deps chromium
```

On macOS the harness prefers the installed Google Chrome binary, so a browser
download is not required. Set `PLAYWRIGHT_CHROMIUM_EXECUTABLE` to test another
Chromium-family executable. Set `AXOCOATL_E2E_BINARY` to select a binary other
than `target/debug/axocoatl`; `npm run test:no-build` deliberately skips the
Cargo build and uses that selected or default binary as-is.

`npm test` builds the current `axocoatl-cli` package before opening the embedded
page. The no-build command still discovers every `tests/*.test.mjs` file in
sorted order and fails when the suite is empty, so adding a test file cannot
silently leave it outside the gate.

The runner creates one disposable embedding-model cache for the run. A healthy
fixture publishes only files matching the daemon's pinned sizes and SHA-256
hashes. Later legacy fixtures copy those model files into their own data roots;
Session state and authority are never shared. Native first-install fixtures
retain a genuinely absent data root and perform their own cold bootstrap.
The runner allows up to five minutes for that cold startup. Individual test-file
runs retain the shorter default unless `AXOCOATL_E2E_STARTUP_TIMEOUT_MS` is set
(maximum `300000`). `AXOCOATL_E2E_MODEL_CACHE` can point to an existing directory
containing the three pinned model files; an externally supplied cache is not
removed by the runner.

## CI gate

The `Product browser regressions` job in `.github/workflows/ci.yml` uses Node
22, the lockfile-pinned Playwright release, and its matching headless Chromium
on an explicit Ubuntu runner. It rebuilds `axocoatl-cli`, then runs this suite
against that binary. The main Rust job also verifies that the lockfile, runner,
and product regression file are present in the checkout, preventing an omitted
untracked suite from producing a green build.

The default suite needs no running Podman service, Ollama model, provider
credential, or external application server. The optional compatibility reference
journey requires a real Podman runtime image selected with
`AXOCOATL_REFERENCE_RUNTIME_IMAGE`. A cold embedding cache can require downloading
the pinned local embedding model. The harness starts only a fresh loopback Axocoatl daemon; its
Sessions remain at environment review, while controlled browser responses mock
the Preview and failure paths called out below.

## Covered product contracts

- The left rail collapses and expands from its visible control and
  `Ctrl/Cmd+\\`, and the explicit preference survives reload plus compact ↔
  wide resizing.
- The Workspace selector exposes every named Workspace with its canonical path
  as secondary identity, while the rail only lists Sessions owned by the
  selected Workspace.
- A Session awaiting setup approval shows the exact command, disables Send and
  Explore several ways, leaves runtime-backed components inert—the file tree
  is unbound and the same-Session editor is suspended so drafts survive—and
  routes Enter, Terminal, Files, Preview, and
  Source Control plus `Ctrl/Cmd+P` to the environment review dialog without
  reaching execution, tree, file read/write, Git, Preview, or terminal mutation
  APIs.
- Quick Open terminates directory-only and cyclic traversal at explicit entry,
  directory, and depth limits, and abandons a build when its Session changes.
- Source Control attributes a normal turn's absolute-path tool write to **Last
  turn** after normalizing it to the repository-relative changed path.
- Failed setup evidence renders the failed command, exit code, and bounded
  stdout/stderr as text in the active banner; environment review preserves the
  same durable evidence as text-only UI.
- A runtime that was durably Ready before daemon restart is reconciled to
  Failed when lazy reconstruction fails. The same exact-Session refresh covers
  auto-terminal, task polling, primary-turn, Files, Source Control, and Preview
  failure signals and immediately unbinds runtime-backed controls.
- A Closed Session with an unresolved Attempt set mounts as a read-only
  recovery view without an implicit Reopen or runtime task poll. Its transcript,
  restored results, Compare, Finish Keep, and Finish cleanup remain reachable;
  only the explicit banner action can reactivate the Session runtime.
- Preview runs on a dedicated per-Session, per-port virtual Host. Controlled
  responses at that browser origin prove the browser-side contract for ES
  modules, root-relative assets, fetch/XHR, native forms, local storage,
  identity-encoded responses, and Vite-style WebSockets.
- Complementary Rust route integration uses real loopback HTTP and WebSocket
  servers to exercise the transport itself: method/query/body and application
  headers, bounded HTML bridge injection, binary streaming, virtual Host and
  Origin forwarding, subprotocol negotiation, and bidirectional frames.
- Preview application code cannot read the parent DOM, forge picker messages
  from another frame/origin, or reach workbench routes through its virtual Host.
  Under the suite's default empty CORS policy it also cannot read the main
  control API, issue blind control mutations, or write across Preview origins;
  an operator-supplied CORS allowlist deliberately changes cross-origin control
  access. Direct legacy-proxy documents remain response-sandboxed.
- Opening the workbench at a loopback IP proves the product's canonical
  `127.0.0.1` → `localhost` document redirect. Preview cookie coverage checks
  that an ordinary embedded write is unavailable under modern third-party
  policy, **Open full preview** opens the exact virtual URL with no opener,
  host-only cookies work top-level, and neither that cookie nor an attempted
  `Domain=localhost` parent cookie reaches a sibling Session/port origin.
- Manual E2B cleanup is high-friction for both retained identities: the dialog
  requires the exact runtime ID or `axocoatl_creation_token`, begins unchecked,
  sends no request before affirmation, posts the exact confirmation payload,
  closes, and reconciles the active cockpit after success.
- When operator policy pre-approves an exact devcontainer post-create command,
  the creation UI explains and checks that default while keeping consent
  reversible. Unchecking creates an AwaitingApproval Session without executing
  the command; package-lock `npm ci` suggestions remain unchecked.

The policy and stale-runtime cases use persisted Session records and real daemon
API transitions, while focused crawler/turn/poll/Preview failure signals use
controlled browser responses around the real embedded page. Preview HTTP and
WebSocket fixtures are intercepted at the virtual browser origin; this suite
does not claim those fixture bytes traversed the Rust transport proxy. Server
tests separately cover strict Host parsing and origin rejection, body/header
classification and limits, and mode-aware credential/Host forwarding observed
by a real loopback upstream. A live sandbox/dev-server transport remains part of
the separate full Session/Attempt acceptance journey.

The default suite does not approve `npm ci`, start Podman, execute a successful
Agent turn, or mutate a project repository. The opt-in compatibility reference journey
executes controlled work inside its disposable runtime.
