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
  adds the `axocoatl-token-<port>` cookie and the secret-free
  `axocoatl-token-<port>-signed-in` cookie for `localhost` and `127.0.0.1`,
  exactly as the sign-in link would. In WebKit a journey that opens `/` at
  `127.0.0.1` then passes the daemon's one same-origin retry page (Safari
  leaves the Strict cookie off the redirect to `localhost`), so wait for the
  workbench (`waitForFunction`, a locator) rather than reading the page right
  after `goto`;
- Node `WebSocket` clients pass the header explicitly with `runtime.token`.

Two helpers keep journeys engine-neutral. `settleTransitions(page)` waits two
frames and then until no CSS transition runs, before a test reads computed
colors. `isWebKitLoadDiagnostic(error)` recognizes the console lines WebKit
reports as page errors for a load it refused or cancelled, or for a
cross-origin frame read that returned null; an unhandled rejection or an
uncaught `SecurityError` is reported separately and still fails a journey.

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
Chromium-family executable.

## WebKit (Safari)

`AXO_BROWSER_ENGINE=webkit` runs every test on WebKit, the engine Safari
uses, with no change to the tests: `support/browser-engine.mjs` (which
`npm test` loads with `--import` when the variable is set) makes the tests'
`chromium` import WebKit and drops the Chromium-only launch options
(`executablePath`, `args`, `channel`, `chromiumSandbox`).
`tests/browser-engine.test.mjs` checks the switch without a browser.

```bash
npx playwright install webkit
AXO_BROWSER_ENGINE=webkit npm test
```

`scripts/verify-product-browser-gate.sh` takes the same variable, installs
that engine, and with `AXO_BROWSER_PROFILE=release` drives the release
build. CI runs the release build in Chromium on x86_64 and aarch64 and in
WebKit on x86_64. Set `AXOCOATL_E2E_BINARY` to select a binary other
than `target/debug/axocoatl`; `npm run test:no-build` deliberately skips the
Cargo build and uses that selected or default binary as-is.

On Linux, Playwright's WebKit loads everything through one network process
that uses the system's libsoup, and that process sometimes crashes when a
reload or a navigation cancels requests that are still in flight: libsoup
logs `soup_session_feature_request_queued: assertion
'SOUP_IS_SESSION_FEATURE (feature)' failed` or glibc logs
`malloc_consolidate(): unaligned fastbin chunk detected`, and the process dies
of SIGSEGV or SIGABRT. WebKit starts a new one without telling Playwright. A
load that was under way fails with "WebKit encountered an internal error" or
never ends, and the cookies of every context, which only the old process
held, are gone, so a journey that had signed in goes on signed out.

Measured with Playwright 1.60's WebKit on Ubuntu 24.04 (libsoup 3.4.4), the
journeys of `local-api-token.test.mjs` hit this when they reloaded or left a
workbench that had just connected and was still loading: the loopback IP
journey failed in 6 of 200 runs and the "once signed in" journey in 12 of
440 (it then served its other website with `page.route`, which makes
Playwright intercept every request of the page), each time with a crash of
the network process, and CI's WebKit leg failed once on each journey with
the same symptoms. Waiting (with Playwright's `networkidle`) until the
workbench had no request in flight before each reload or navigation away,
the same journeys passed 200 of 200 and 100 of 100 runs with no crash. So
these journeys call `waitForSettledWorkbench` first, and serve another
website from a real server on `[::1]`, a site that is neither `127.0.0.1`
nor `localhost`, instead of from a route. The crash is WebKit's and can
still happen elsewhere: a journey that fails in WebKit on Linux with
"internal error" or with its sign-in cookie gone may be this crash.

Journeys a new user takes first (the folder picker and Workspace scope, Source
Control's commit identity, Team and budget's suggested limits, the readiness
checklist) run once per browser engine from `browserEngines()`: Chromium always,
and WebKit, the Safari engine and the default browser on a Mac, when Playwright's
WebKit is installed (`npx playwright install webkit`). With
`AXOCOATL_E2E_WEBKIT=1` a missing WebKit fails instead of being skipped; the
gate script sets it when run with `AXO_BROWSER_ENGINE=webkit`, which also
installs WebKit and runs every journey on WebKit, not only these per-engine
ones. A journey that needs the daemon's own `HOME` (the folder picker starts
there) passes `home: true` to `launchTestDaemon`, which gives the daemon a
fixture home folder and keeps Podman's real configuration through the XDG
directories.

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
`AXOCOATL_REFERENCE_RUNTIME_IMAGE`. The first-turn journey
(`native-turn-git.test.mjs`) runs one native turn on Podman against a stub
Ollama server, in `docker.io/library/rust:bookworm` (or the curated image with
Git that `AXOCOATL_NATIVE_TURN_IMAGE` names); it is skipped, not pulled, when
that image is not on the machine. The Preview cookie journey
(`preview-cookies.test.mjs`) runs a real Session app in the curated
`docker.io/library/node:20-slim` behind the real Preview proxy, with
`sandbox.network: bridge` so the Session's ports are published on loopback;
it too is skipped, not pulled, when that image is not on the machine, unless
`AXOCOATL_E2E_REQUIRE_NODE_IMAGE=1`, which makes a missing image a failure.
The product-browser legs of `ci.yml` and the release gate in `release.yml`
pull that image, pinned to its index by digest, tag it
`docker.io/library/node:20-slim` and set that variable, so there a missing
image fails the journey instead of skipping it; the Session then runs on the
runner's rootless Podman. A cold embedding cache can require downloading
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
- Open workspace marks Git repositories, refuses the home folder until its
  name is typed, and Remove from list forgets a Workspace without touching its
  folder, refusing one with open Sessions.
- Source Control names the person a commit is by, disables Commit with the
  reason when no Git identity is set, shows a refused commit as Not committed
  with Git's message, offers Initialize Git here in a folder that is not a
  repository, and says when the last turn's files are not known.
- Team and budget opens a new Session with the suggested limits filled in,
  applies them without typing, and shows Preview's refusal of a token limit a
  turn cannot use.
- A first native turn on the suggested limits edits a file, Source Control's
  Last turn marks it before and after a daemon restart, and a commit right
  after `git config user.name …` in the folder is by that identity, with
  nothing of Axocoatl's written to the repository's configuration.
- The first screen lists what the first Session still needs, with each fix's
  commands, the rail pill opens the same checklist, and Create session stays
  blocked while Podman has no machine.
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
  policy and reaches nothing, **Open full preview** opens the exact virtual URL
  with no opener, and host-only cookies work top-level and reach neither the
  workbench nor another Session/port origin. The top-level page also sets, for
  `Domain=localhost`, a cookie that is not ASCII, a forged
  `axocoatl-token-<port>` and `-signed-in` cookie and 40 cookies of 1 KB. A
  loopback cookie echo (`support/cookie-echo.mjs`) reads the Cookie headers the
  browser sends (cookies ignore ports): the workbench's own token cookie
  reaches the workbench and never a Preview origin, in every engine. Chromium
  and WebKit on Linux refuse the `Domain=localhost` cookies; in WebKit on macOS
  (Safari) the journey asserts the known behaviour instead, that every one of
  them, the forged `axocoatl-token-*` cookies included, reaches the workbench
  and every other Preview origin (`operate/security.mdx`). In every engine the
  workbench page still reads the API, and a new workbench page opens, connects
  and reads the API.
- `preview-cookies.test.mjs` does the same through the real Preview proxy and a
  real Session app on Podman, after signing in with the real link. It asserts
  that no request or WebSocket upgrade the app receives on either Preview port
  carries an `axocoatl-token-*` cookie, while Safari sends the forged ones to
  those origins and the app gets the other forged cookies (an upgrade only the
  visible-ASCII ones, and its WebSocket opens); that the app's own
  `Set-Cookie` with `Domain=localhost` stays host-only on its Preview origin
  (the proxy removes `Domain`); that the workbench reloads, connects and reads
  the API; and that a browser that never signed in, holding a forged
  `-signed-in` cookie, gets the retry page then the sign-in page on each
  navigation in Safari (the sign-in page at once elsewhere), never a loop.
- `signed-out.test.mjs`, in every engine: when `/` serves the shell but every
  API request answers 401 (Playwright answers them), the shell reloads once
  and then shows the signed-out message (a modal `alertdialog` over an inert
  page, which takes focus, stays after Escape, and names `axocoatl url`,
  `axocoatl dev` and Safari's Manage Website Data), and its Reload button
  reloads once more without another automatic
  reload; when the sign-in cookie is gone, one reload reaches the sign-in
  page; without `sessionStorage` it never reloads and shows the message.
  Against the real daemon, a full-page Preview origin (served by Playwright)
  sets 51 cookies of 4,000 bytes and smaller ones for `localhost` with
  `Path=/api`, which fill WebKit's Cookie header cap: in WebKit on macOS
  (Safari) the token cookie is still stored but left off API requests, which
  answer 401, and the workbench loads twice and then shows the message (a
  shell that reloads on every 401 loaded 167 documents in 20 seconds here);
  opening the sign-in link again shows it again without a reload; clearing
  the cookies and signing in again restores the workbench. The same cookies
  with `Path=/ws` keep the live connection from opening (the status bar says
  it is reconnecting) while the API answers. Chromium and WebKit on Linux
  refuse the cookies, and the journey asserts the workbench works.
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
by a real loopback upstream. `preview-cookies.test.mjs` is the one journey
here whose Preview requests reach a live sandbox app through the proxy; the
rest of the live sandbox/dev-server transport remains part of the separate full
Session/Attempt acceptance journey.

Apart from the first-turn and Preview cookie journeys, which start a Session
on Podman when their image is present, the default suite does not approve
`npm ci`, start Podman, execute a successful Agent turn, or mutate a project
repository. The opt-in compatibility reference journey
executes controlled work inside its disposable runtime.
