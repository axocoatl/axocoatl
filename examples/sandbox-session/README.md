# Sandbox sessions — isolated code execution in a rootless Podman container

A *directory session* in Axocoatl does not run the agent's tools on your host.
It runs them inside a per-session **rootless, daemonless Podman container** with
the session's working directory as its only host bind mount. For a root Node
project, a separate Podman volume is mounted over the checkout's
`node_modules`, so host-native packages are not imported into Linux. The
container is the security boundary: `write_file` / `read_file` /
`run_command` reach that sandboxed working tree and no other host directory,
run without the escape/recon Linux capabilities, and — for untrusted code —
can be cut off from the network entirely.

This example is **README-first**. The live sandbox needs Podman, which CI (and
most reviewers' first checkout) won't have. So it ships as a runnable guide plus
a CI-safe config test, with the real container path behind an `#[ignore]`d test.

```
cargo run -p sandbox-session
```

That prints the threat model, the exact `podman ps` / mount-inspection commands,
the `axocoatl.yaml` config knobs, and a probe telling you whether the live path
is runnable on *this* machine (it uses the runtime's own Podman detection).

## What the live path needs

Podman — native on Linux/WSL, a managed VM on macOS/Windows:

```
# Linux / WSL
sudo apt-get install -y podman          # or dnf / pacman / zypper

# macOS
brew install podman && podman machine init && podman machine start

# Windows
winget install RedHat.Podman            # then: podman machine init && podman machine start
```

With Podman ready, run the ignored integration test — it starts a real
container, runs a command inside it, writes a file from inside, and proves the
write lands in the bind-mounted workspace. It also proves that a root Node
project's dependency volume masks host `node_modules` and that the host `$HOME`
is *not* visible inside:

```
cargo test -p sandbox-session -- --ignored
```

## Threat model

Stated plainly so you know exactly what to trust it for.

**What the sandbox contains — the blast radius of a mistaken or misbehaving
agent:**

- **Filesystem.** The session's working directory is the only host bind mount
  (`{dir}:{dir}:rw`). Nothing else of the host is visible — not your home
  directory, SSH keys, or sibling projects. A root Node project additionally
  receives a Podman-managed volume at `{dir}/node_modules`; that volume masks
  the host dependency tree rather than exposing another host path. A
  destructive command (`rm -rf`, a bad `git reset`) can still change the
  read-write Workspace and anything in the container or dependency volume.
- **Privileges.** `--security-opt=no-new-privileges` (a setuid binary can't
  escalate) plus dropped escape/recon capabilities: `SYS_ADMIN`, `SYS_PTRACE`,
  `SYS_MODULE`, `SYS_RAWIO`, `SYS_BOOT`, `SYS_TIME`, `NET_ADMIN`, `NET_RAW`,
  `DAC_READ_SEARCH`, `MKNOD`, `AUDIT_WRITE`. The package-manager caps (`CHOWN`,
  `SETUID`/`SETGID`, `DAC_OVERRIDE`, `FOWNER`) are deliberately kept so
  `apk`/`apt`/`npm`/`pip` still work.
- **Network.** Bridged networking is the default so installs and development
  servers work. Set `sandbox.network: none` for an untrusted run that must have
  no outbound connection; that also disables network-dependent setup and tools.
- **Resources.** Memory / CPU / PID caps (2 GB / 2 CPUs / 512 pids) bound a
  runaway loop or fork bomb, where the host's cgroup delegation allows it.

**What it does NOT solve — and we won't pretend otherwise:**

- **Prompt injection.** If the agent reads malicious instructions from a file, a
  web page, or tool output, the sandbox does not stop it from *acting* on them
  inside its workspace and its allowed network. Isolation bounds the blast
  radius; it is not a defense against an agent being talked into the wrong
  thing. Keep secrets out of the workspace and prefer `--network none` for
  untrusted inputs.
- **Host kernel / Podman bugs.** Container isolation is only as strong as the
  host kernel and Podman underneath it. A kernel-level container-escape CVE is
  outside this layer's control.
- **What you explicitly grant.** Bridged networking, mounted credentials, or a
  permissive tool policy widen the surface — by your choice.

## Inspect the isolation yourself

With Podman installed and a session open (or the ignored test running), a
container named `axo-ses-<session_id>` is live. Verify each claim:

```sh
# 1. See the live session container (idling on `sleep infinity`):
podman ps --filter name=axo-ses-
#   CONTAINER ID  IMAGE                            COMMAND         ... NAMES
#   a1b2c3d4e5f6  docker.io/library/alpine:3.20   sleep infinity  ... axo-ses-demo

# 2. Confirm the workspace is the only HOST bind mount — no home dir, no keys:
podman inspect axo-ses-demo --format '{{json .Mounts}}'
#   [{"Type":"bind","Source":"/path/to/workspace",
#     "Destination":"/path/to/workspace","RW":true, ...},
#    {"Type":"volume","Destination":"/path/to/workspace/node_modules", ...}]
# The volume entry appears only for a root Node project and is not a host bind.

# 3. Confirm the dropped capabilities and no-new-privileges:
podman inspect axo-ses-demo \
    --format '{{.HostConfig.CapDrop}} | {{.HostConfig.SecurityOpt}}'
#   [SYS_ADMIN SYS_PTRACE ... AUDIT_WRITE] | [no-new-privileges]

# 4. With network: none, prove outbound is blocked from INSIDE:
podman exec axo-ses-demo wget -qO- https://example.com
#   wget: bad address 'example.com'        # DNS/connect fails — good

# 5. Prove the host filesystem is NOT reachable from inside:
podman exec axo-ses-demo ls /  # the workspace path is present; host $HOME is absent
```

## Config knobs — the `sandbox:` block in `axocoatl.yaml`

The local backend and trust controls default to the conservative setting, so a
new repository cannot approve its own setup command or choose an arbitrary
image without an operator or per-Session decision:

```yaml
sandbox:
  backend: podman

  # Default the exact devcontainer postCreateCommand to approved for an
  # unreviewed compatibility Session. A reviewed per-Session decision wins.
  allow_post_create_command: false

  # Honor a repo/UI-specified base image other than the trusted default.
  # Off by default — an attacker-chosen image is attacker-controlled code.
  allow_untrusted_images: false

  # "bridge" (default: outbound + published ports) or "none"
  # (no network at all — blocks exfiltration for untrusted code,
  #  but also package installs and dev servers).
  network: bridge

  # Refuse to start if memory/CPU/pid limits can't be applied, instead of
  # silently running uncapped. Off by default because some hosts
  # (rootless podman on WSL2) can't delegate cgroups.
  require_resource_limits: false
```

These parse into `SandboxConfigYaml`. The daemon resolves the post-create flag
into the Session's exact durable setup decision first, then deliberately keeps
the generic container `SandboxPolicy.allow_post_create` hook disabled so there
is no second implicit execution path. The CI-safe unit test
(`sandbox_config_parses_and_maps_to_policy`) parses a real `sandbox:` block with
the real config type and asserts that mapping — that's the contract a reviewer
checks without ever needing Podman.

The exact curated references are `docker.io/library/alpine:3.20`,
`docker.io/library/debian:bookworm-slim`, `docker.io/library/ubuntu:24.04`,
`docker.io/library/python:3.12-slim`, `docker.io/library/node:20-slim`, and
`docker.io/library/rust:bookworm`. They are accepted without enabling arbitrary
images. Curated means allowlisted, not pre-provisioned or inherently safe:
local startup verifies the repository commands Files, Git, and Agent tools
need, attempts distro-aware provisioning inside the container, and removes the
container if it still cannot become Ready. Podman itself is never installed and
a missing Podman VM is never created by Session startup; an existing stopped VM
may be started.

With `sandbox.network: none`, the image must already contain the required
repository commands because in-container provisioning cannot download them.
Podman may still pull that image through the host; image acquisition is not
Session container egress.

Repository setup is a separate consent boundary. A lockfile proposal such as
`npm ci` starts unchecked. `allow_post_create_command: true` applies only to the
exact `devcontainer.json` command on an unreviewed Session; a checked or
unchecked reviewed decision is authoritative, and editing the command clears
that default.

## Tests

```
cargo test -p sandbox-session                 # CI-safe: config-parse tests only
cargo test -p sandbox-session -- --ignored    # full live sandbox path (needs Podman)
```

- `sandbox_config_parses_and_maps_to_policy` — parses the `sandbox:` YAML,
  proves the operator setup default remains separate from the generic
  container hook, and maps the remaining runtime policy fields. **Runs in CI.**
- `omitted_sandbox_block_is_secure_by_default` — an absent block yields the
  secure defaults and bridge networking. **Runs in CI.**
- `e2b_backend_and_template_parse_at_daemon_scope` — proves the remote backend
  and template live in daemon-global sandbox configuration rather than a
  per-Session OCI image field. **Runs in CI.**
- `sandbox_jails_the_workspace_and_runs_commands` — starts a real container,
  runs a command, writes a Workspace file that appears on the host, proves the
  root Node dependency volume does not import host `node_modules`, and checks
  the host `$HOME` is not visible inside. **`#[ignore]`d — needs Podman.**

## Where this lives in the real runtime

- Sandbox lifecycle + hardening:
  [`crates/axocoatl-isolation/src/session_sandbox.rs`](../../crates/axocoatl-isolation/src/session_sandbox.rs)
- Podman detection / setup:
  [`crates/axocoatl-isolation/src/podman.rs`](../../crates/axocoatl-isolation/src/podman.rs)
- Config struct + secure defaults: `SandboxConfigYaml` in
  [`crates/axocoatl-config/src/types.rs`](../../crates/axocoatl-config/src/types.rs)
- Config → policy conversion: `ensure_sandbox` in
  `crates/axocoatl-daemon/src/bootstrap.rs`
- Threat model in prose: [`docs/ARCHITECTURE.md`](../../docs/ARCHITECTURE.md) (Security model)
