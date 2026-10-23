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

Playwright's WebKit sends Safari's Mac user agent on Linux too
(`Macintosh; Intel Mac OS X 10_15_7`, while `navigator.platform` names
Linux), from every page Playwright or a page opens. The daemon knows Safari
by its user agent alone (`src/preview_document.rs`), and the workbench
decides as the daemon does, so on both systems the WebKit leg gets what
Safari gets: a Preview stays embedded and a top-level Preview page is
refused. A tab that WebKit on Linux opens on its own (Control-click) is the
exception: its first request carries WebKit's own `X11; Linux` user agent,
and the daemon serves it as WebKit on Linux. Whether the browser keeps a
`Domain=localhost` cookie is the engine's own: WebKit on macOS keeps it,
WebKit on Linux refuses it, and the journeys assert each.

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

The cause is the libsoup that Playwright's Linux WebKit bundles (it never
loads the system's): WebKit 26.4 in Playwright 1.60 bundles libsoup 3.6.5,
and libsoup 3.6.6 fixes a heap use-after-free in its session. These tests
pin Playwright 1.64.1 (WebKit 27.2, libsoup 3.6.6). Under the same stress in
an Ubuntu 24.04 VM (aarch64), 1.60's WebKit crashed 26 times in 1,212
iterations, with 20 failures, and 1.64.1's crashed 0 times in 1,000.

Measured with Playwright 1.60's WebKit on Ubuntu 24.04, the journeys of
`local-api-token.test.mjs` hit this when they reloaded or left a
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

The page itself cancelling what the workbench is still loading can do it
too. `workbench.test.mjs`'s "cross-tab Close and Delete" journey sent a
runtime transition and remounted the cockpit as soon as it was ready, which
cancels the requests the cockpit still has in flight (about three were in
flight each time, and its steps cancelled 40 of them in 100 repetitions).
CI's WebKit leg failed on it at 27025d43 with `Can't find variable: S`. In
the VM the crash is rare: the journey alone failed once in 210 runs, in the
same second as a network process crash, and 900 repetitions of its steps
crashed the network process once, 8 ms after the transition
(`malloc_consolidate(): unaligned fastbin chunk detected`), and then failed
the same way. So that journey, and the rail journey before each of its
reloads, first wait with `settle()` (from `openSession`) until the workbench
has had no request in flight for half a second; the transition then cancels
none. `trackRequests` and `waitForNoRequestsInFlight` in
`support/daemon.mjs` do the waiting, for `local-api-token.test.mjs`'s
`waitForSettledWorkbench` too. Closing a context (200 times) or reloading
the page (220 times) while the workbench loaded did not crash it in the VM.

So the harness says when a context lost its sign-in, under the test, as a
`# browser crash:` line with the time and the request that was refused, and
reruns a file whose failures all followed such a crash ("Browser crashes"
below). A failure that follows from it (the shell's reload on a 401 reaching
the sign-in page, then `Can't find variable: S`) is then not taken for a
product failure, and can be matched with the crash in the browser's stderr
(`DEBUG=pw:browser`) or the system's crash log.

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
silently leave it outside the gate. Naming files runs only those
(`npm run test:no-build -- tests/signed-out.test.mjs`).

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

## Browser crashes

A browser can crash under a journey without the product doing anything
wrong, and the journey then fails far from the cause. Playwright 1.60's
WebKit on Linux does this now and then: its network process dies of heap
corruption and takes every context's cookies with it (above), or a web
process dies under a live page, whose next action fails with "Target page,
context or browser has been closed". CI's WebKit leg failed this way in 3 of
8 runs, each time in a different journey. A Chromium renderer or browser
that crashes is not a product failure either.

So `support/browser-crash.mjs` watches every browser a journey launches, in
every engine, and prints what it saw under the test it happened in, as a
`# browser crash:` line with the time:

- a page whose web process crashed (Playwright's page `crash` event), unless
  the journey was closing that page, its context or its browser;
- a browser that disconnected although the journey did not close it;
- a context or page the browser has not created after 30 seconds, which then
  fails the journey's `newContext` or `newPage`. Creating one runs no
  product code, and Playwright's WebKit never finishes a page whose web
  process ends while it is being created, so the journey would otherwise
  wait until CI cancels the job;
- a context that no longer holds the sign-in cookie it was given
  (`authorizeContext`, so `newAuthorizedContext`) or signed in with
  (`/?token=`), although the journey did not clear it with
  `context.clearCookies`. Axocoatl never removes that cookie. A context is
  checked when a daemon answers it 401, when the journey closes it, and when
  the test ends, and the test's lines are printed after those checks have
  finished;
- a failure with "Target page, context or browser has been closed", "Target
  crashed", "Page crashed" or "WebKit encountered an internal error", but only
  with one of the above in the same test, or on a browser that disconnected
  earlier in the file. Alone, those errors prove nothing.

`npm test` (`support/run-tests.mjs`) reads each failure and the lines under
it (`support/crash-report.mjs`, a second node:test reporter beside the usual
one). By default it reruns nothing: every failure fails the suite, with its
crash lines printed under it, in CI and in the release gates alike. A page
'crash' can also be the product's own page running the renderer out of
memory now and then, which a rerun would hide. While triaging, set
`AXO_BROWSER_CRASH_RERUNS=N` (up to 20), and it:

- runs a test file once more when every failure in it has a
  `# browser crash:` line. The rerun must pass entirely;
- never reruns a file with a failure that has none; that failure fails the
  suite. A failure of the file itself (a hook) has none, though a daemon left
  running that only sets the exit code is not seen when another test of the
  file failed;
- reruns at most N files in a suite. More than that fails the suite and
  reruns none;
- prints both runs and then a summary: the first run's counts, each failure
  with its crash lines, the rerun's counts, and `Browser regressions: PASS`
  or `FAIL`. On GitHub Actions each rerun is also an annotation: a warning
  when the rerun passed, an error when it did not.

Detection is on for every engine. With reruns on, a crash in the same test as
a real failure gets the file rerun, and a failure that happens every time
then fails the rerun and so the suite; an intermittent one can pass.
A crash the harness does not see (the network process of a browser none of
whose contexts holds a sign-in cookie, for example) is a plain failure.
`tests/browser-crash.test.mjs` checks these rules, and runs a lost sign-in
and a killed browser in a child `node --test`.

Measured in an Ubuntu 24.04 VM (aarch64) with Playwright 1.60's WebKit and a
release build, by signalling the run's own browser processes a fraction of a
second into a journey: killing `WPENetworkProcess` (SIGSEGV in 10 runs,
SIGKILL in 4) or `WPEWebProcess` (SIGKILL in 18 runs) was detected each time,
and the file was run once more and passed. In 7 of the web process runs the
process ended while a page was being created, and `newPage` failed after 30
seconds instead of waiting forever. A SIGSEGV sent to `WPEWebProcess` with
`kill` did not end it (4 runs), so those runs used SIGKILL. A deliberately broken
assertion failed the suite with no rerun. With a crash in the same test the
file was rerun and the assertion failed the rerun; with a crash in another
test of the file, the file was not rerun.

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
- A theme change while Monaco is still loading leaves the editor its
  suggestions, code actions, code lenses, inlay hints and drop into editor,
  and the editor takes the page's theme. The vendored Monaco sets
  `window.monaco` before the chunk that registers those features' services
  has loaded, and any call into it then starts Monaco's services without
  them; the journey holds that chunk to make the gap certain. The shell and
  the journeys reach Monaco through `axMonacoIfLoaded()`, which is null until
  Monaco has loaded.
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
  blocked while Podman has no machine. Create session waits for the New
  Session picker's own readiness read (a read that fails blocks nothing; the
  daemon refuses a Session it cannot prepare), and typing a Session name
  never enables a Create session that readiness or anything else holds.
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
  access. A legacy-proxy address pasted into a tab, signed in, gets the real
  daemon's 403 and a line of text in every engine, without the app being
  asked.
- Opening the workbench at a loopback IP proves the product's canonical
  `127.0.0.1` → `localhost` document redirect. Preview cookie coverage checks
  that an ordinary embedded write is unavailable under modern third-party
  policy and reaches nothing. In Chromium, **Open full
  preview** opens the exact virtual URL with no opener, and host-only cookies
  work top-level and reach neither the workbench nor another Session/port
  origin. The top-level page also tries, for `Domain=localhost`, a cookie that
  is not ASCII, a forged `axocoatl-token-<port>` and `-signed-in` cookie and
  40 cookies of 1 KB, which Chromium refuses. Safari would accept them
  (`operate/security.mdx`), so there the daemon refuses a Preview page that
  would run top-level (`src/preview_document.rs`) and the workbench keeps the
  Preview embedded (`static/ui/preview-embedding.js`); WebKit gets the same on
  macOS and Linux, because it sends Safari's user agent on both.
  This journey's Preview is served by Playwright, so the daemon never sees
  its requests; in WebKit it asserts
  that Open full preview has no address and says why, that the frame has no
  `allow-popups`, and that the Preview's own `window.open` of its address, a
  `target=_blank` link or form, a Command-click its script makes up and a
  navigation of the workbench's tab open nothing. A loopback cookie echo
  (`support/cookie-echo.mjs`) reads the Cookie headers the browser sends
  (cookies ignore ports): the workbench's own token cookie reaches the
  workbench and never a Preview origin, and no `Domain=localhost` cookie
  reaches anything, in every engine. In every engine the workbench page still
  reads the API, and a new workbench page opens, connects and reads the API.
- The journey after it tries every other route by which the workbench could
  put a Preview's address in a tab of its own: a Markdown link in the
  conversation, a web-search citation (click, Command-click, middle click,
  context menu, drag), `window.open` (how the file editor opens a link), and an
  OSC 8 link a program prints in the Terminal (an `<ax-terminal>` whose
  WebSocket Playwright answers). In WebKit, whose user agent is Safari's on
  macOS and Linux alike (the journey asserts it), each is refused, opens
  nothing, and the workbench says why. In Chromium nothing is refused: a click,
  `window.open` and the Terminal link open the address as before (the
  Terminal after xterm.js's own prompt), and whatever a Command-click or middle
  click opens, as that engine decides, is that address. Where no Chrome is
  installed (Linux, CI), this journey runs Chromium in its new headless mode
  (`channel: 'chromium'`): Playwright's Chromium headless shell
  (156.0.8078.4) crashes the renderer of a tab a middle click opens in the
  background for another site, and the crashed tab then fails the journey's
  next call that reaches every page (`routeWebSocket`, "Target crashed").
- `preview-embedding.test.mjs` checks, without a browser, which browsers keep
  a Preview embedded (Safari's engine on macOS, iOS and iPadOS, by
  `navigator.vendor`, then the platform or the user agent, so Playwright's
  WebKit on Linux too; not Chromium-based browsers, Firefox, or WebKit on
  Linux with its own `X11; Linux` user agent) and which addresses the
  workbench then refuses to open in a tab (any subdomain of `localhost`).
- `preview-cookies.test.mjs` works through the real Preview proxy and a real
  Session app on Podman, after signing in with the real link. In Chromium
  it opens Preview addresses top-level itself, as someone who opens
  one in a tab does, and sets the same cookies there; in WebKit (taken for
  Safari on macOS and Linux) it
  sets them from a page on another `*.localhost` address (the cookie echo,
  which Axocoatl does not serve), because the daemon refuses the Preview. It
  asserts that no request or WebSocket upgrade the app receives on either
  Preview port carries an `axocoatl-token-*` cookie (outside Safari the app
  gets its other cookies, an upgrade only the visible-ASCII ones, and its
  WebSocket opens; in WebKit the app receives no top-level request at all,
  though WebKit on macOS sends the forged cookies to those origins); that the app's
  own `Set-Cookie` with `Domain=localhost` stays host-only on its Preview
  origin (the proxy removes `Domain`); that the workbench reloads, connects
  and reads the API; and that a browser that never signed in, holding a
  forged `-signed-in` cookie, gets the retry page then the sign-in page on
  each navigation in WebKit on macOS (the sign-in page at once elsewhere), never a
  loop. Its third journey shows the real Preview in the workbench: in WebKit
  Open full preview has no address, the frame has no
  `allow-popups`, and the Preview's `window.open` of its address returns null
  and opens nothing. A real Command-click (Control-click on Linux) on a link
  inside the Preview's frame, and inside the Files view's frame for an HTML
  file, opens a tab: in WebKit on macOS the daemon's refusal page (403, no
  script, a link to the workbench), with no request to the app and no cookie
  set; elsewhere the app, whose script ran. In WebKit on Linux the first
  request of a tab the browser opens on its own carries WebKit's own
  `X11; Linux` user agent (Playwright gives Safari's only to the pages it or
  a page opens), so the daemon serves it as it serves any WebKit on Linux,
  and the journey asserts that user agent. Playwright's WebKit reports no
  request for a tab opened that way, so the tab's page is asserted. In every
  engine the app received no token cookie and the workbench stays signed in.
  Its last journey opens a Preview address typed into a tab and one reached
  by a redirect from another site: in WebKit both get the refusal
  page and the app none of the requests; in Chromium the app. In every engine a
  page on the legacy `/api/sessions/<id>/proxy/<port>` path, signed in, gets
  403 and a line of text and the app is not asked, while `fetch()` from the
  workbench and a program with the token still reach it and its
  `Set-Cookie` is not passed on.
- `signed-out.test.mjs`, in every engine: when `/` serves the shell but every
  API request answers 401 (Playwright answers them), the shell reloads once
  and then shows the signed-out message (a modal `alertdialog` over an inert
  page, which takes focus, stays after Escape, and names `axocoatl url`,
  `axocoatl dev` and Safari's Manage Website Data), and its Reload button
  reloads once more without another automatic
  reload; when the sign-in cookie is gone, one reload reaches the sign-in
  page; without `sessionStorage` it never reloads and shows the message.
  Against the real daemon, a page on another `*.localhost` address opened
  top-level (served by Playwright; in Safari the daemon never serves a Preview
  that way) sets 51 cookies of 4,000 bytes and smaller ones for `localhost` with
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
