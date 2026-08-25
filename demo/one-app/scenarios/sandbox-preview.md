# Sandbox, Terminal, and Preview

This supporting film isolates the runtime boundary that ordinary coding work
uses without repeating the full single-agent story.

## Claim

A Session owns a rootless Podman container with the authorized workspace
bind-mounted read-write at the same path, a real interactive Terminal inside
that container, an explicitly published development port, and a same-Session
Preview of the process running from the current checkout.

## Do not claim

- This local container is not a microVM.
- The demo is not air-gapped: its configured network is `bridge`, the Preview
  port is published, and Ollama/MCP run on the host side of the product.
- Resource limits are not guaranteed because the demo configuration sets
  `require_resource_limits: false`.
- A read-write workspace mount means intentional in-sandbox changes are visible
  on the host checkout.

## Start or reset

Complete the verified repair in [`single-agent.md`](single-agent.md), then keep
that Northstar root, Ready Session, and uncommitted `lib/orders.js` change
open. This supporting film deliberately starts from the same accepted green
checkout rather than resetting to the original defect. Keep the
project-detected base image `localhost/axocoatl-one-app-demo:latest` and
exposed port `8765`.

## Browser actions

1. Open Terminal. Run this identity probe:

   ```bash
   printf 'hostname: '; hostname
   printf 'working directory: '; pwd
   printf 'node: '; node --version
   sed -n '1,3p' /etc/os-release
   git status --short
   ```

2. Run the repository contract:

   ```bash
   npm run check
   ```

   The repaired checkout must show all six checks passing. This agrees with the
   completed Turn and proves the Terminal is reading the current Session
   checkout, not a staged result from another root.
3. Open a second Terminal and start the fixture server:

   ```bash
   npm run demo
   ```

4. Open Preview at `http://localhost:8765`.
5. Show the Northstar storefront and its `$0.00 · Ready` state.
6. Return briefly to the server Terminal so the running process and Preview are
   visibly connected, then end in Preview.

## Visible proof

- Terminal reports a container hostname, Alpine-based image, Node runtime, and
  the exact authorized workspace path.
- The repository check observes the six-test green state produced by the
  accepted repair.
- A second terminal owns the live development process.
- Preview displays the application served through the Session's published
  port, not a static marketing mockup.

## Durable, filesystem, and runtime evidence

```bash
export AXO_DEMO_URL='http://127.0.0.1:18080'
export AXO_DEMO_ROOT='/private/tmp/axocoatl-one-app-showcase'
curl -sS "$AXO_DEMO_URL/api/sessions"
```

Copy the Session id, then inspect its API record and deterministic container:

```bash
export AXO_SESSION_ID='ses-paste-the-id-here'
curl -sS "$AXO_DEMO_URL/api/sessions" | grep -F "$AXO_SESSION_ID"
podman inspect --format '{{.Name}} {{.ImageName}} {{index .Config.Labels "io.axocoatl.runtime-authority"}} {{json .Mounts}} {{json .NetworkSettings.Ports}}' \
  "axo-ses-$AXO_SESSION_ID"
git -C "$AXO_DEMO_ROOT/workspace" status --short
curl -sS http://127.0.0.1:8765/ | sed -n '1,20p'
```

The container inspection proves the data-root runtime-authority label, mount,
and published port independently of the browser. The host Git status must show
only the already accepted `lib/orders.js` repair; this supporting runtime tour
must introduce no additional checkout change.

## Recording beats

1. Start with the Session header, then open Terminal.
2. Run the compact identity probe and hold on container/image/workspace facts.
3. Show all six repository checks passing.
4. Start `npm run demo` in a second Terminal.
5. Open Preview and end on the real fixture served from the Session checkout.

Target 25–35 seconds after editing. Keep shell output large enough to read; the
container evidence should not be reduced to an unexplained terminal flash.

## Cleanup

1. Stop `npm run demo` in its Terminal and capture the container inspection.
2. Close the Session from **All sessions** so Axocoatl removes its owned
   sandbox, then stop `start.sh` with Ctrl-C.
3. Confirm that logical port `8765` is listed in the Session configuration.
   Axocoatl assigns a distinct loopback transport per Session, so host port
   `8765` does not need to be free. Reset only after the shared
   single-agent/sandbox evidence is captured; never remove an unidentified
   `axo-ses-*` container as a shortcut.

## Known constraints

- File tools are confined to the Session working-directory subtree, while the
  Terminal runs as a process in the Session container. This film proves the
  configured container/mount boundary, not a formal hostile-code security
  audit.
- Preview resolves the configured logical port through this exact Session's
  dynamic loopback mapping. A missing mapping is a startup failure rather than
  a silently degraded Session.
- Project `postCreateCommand` scripts are disabled by the demo policy and are
  not run automatically.
- The E2B backend is a separate optional isolation path and is not used here.
