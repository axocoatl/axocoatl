# Sandbox sessions — isolated code execution in a rootless Podman container

A *directory session* in Axocoatl does not run the agent's tools on your host.
It runs them inside a per-session **rootless, daemonless Podman container** with
only the session's working directory bind-mounted in. The container is the
security boundary: `write_file` / `read_file` / `run_command` reach that one
directory and nothing else of the host, run without the escape/recon Linux
capabilities, and — for untrusted code — can be cut off from the network
entirely.

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
write lands in the bind-mounted workspace (and that the host `$HOME` is *not*
visible inside):

```
cargo test -p sandbox-session -- --ignored
```

## Threat model

Stated plainly so you know exactly what to trust it for.

**What the sandbox contains — the blast radius of a mistaken or misbehaving
agent:**

- **Filesystem.** Only the session's working directory is bind-mounted
  (`{dir}:{dir}:rw`). Nothing else of the host is visible — not your home
  directory, SSH keys, or sibling projects. A destructive command (`rm -rf`, a
  bad `git reset`) can only reach that one directory.
- **Privileges.** `--security-opt=no-new-privileges` (a setuid binary can't
  escalate) plus dropped escape/recon capabilities: `SYS_ADMIN`, `SYS_PTRACE`,
  `SYS_MODULE`, `SYS_RAWIO`, `SYS_BOOT`, `SYS_TIME`, `NET_ADMIN`, `NET_RAW`,
  `DAC_READ_SEARCH`, `MKNOD`, `AUDIT_WRITE`. The package-manager caps (`CHOWN`,
  `SETUID`/`SETGID`, `DAC_OVERRIDE`, `FOWNER`) are deliberately kept so
  `apk`/`apt`/`npm`/`pip` still work.
- **Network.** Untrusted runs can start with `--network none` — no outbound
  connections at all (cuts off exfiltration / C2 / SSRF). Bridged networking is
  opt-in, per policy.
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

# 2. Confirm ONLY the workspace is mounted — no home dir, no keys:
podman inspect axo-ses-demo --format '{{json .Mounts}}'
#   [{"Type":"bind","Source":"/path/to/workspace",
#     "Destination":"/path/to/workspace","RW":true, ...}]

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

All four default to the **secure** setting, so merely opening a hostile repo can
neither run its setup scripts nor pull an attacker-chosen image:

```yaml
sandbox:
  # Run a repo's devcontainer postCreateCommand on session open.
  # Off by default — otherwise opening a hostile repo is RCE.
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

These parse into `SandboxConfigYaml` and convert to the runtime `SandboxPolicy`
exactly as the daemon does. The CI-safe unit test
(`sandbox_config_parses_and_maps_to_policy`) parses a real `sandbox:` block with
the real config type and asserts that mapping — that's the contract a reviewer
checks without ever needing Podman.

## Tests

```
cargo test -p sandbox-session                 # CI-safe: config-parse tests only
cargo test -p sandbox-session -- --ignored    # full live sandbox path (needs Podman)
```

- `sandbox_config_parses_and_maps_to_policy` — parses the `sandbox:` YAML and
  maps it to `SandboxPolicy` with the daemon's exact conversion. **Runs in CI.**
- `omitted_sandbox_block_is_secure_by_default` — an absent block yields the
  secure defaults and bridge networking. **Runs in CI.**
- `sandbox_jails_the_workspace_and_runs_commands` — starts a real container,
  runs a command, writes a file that appears on the host, and checks the host
  `$HOME` is not visible inside. **`#[ignore]`d — needs Podman.**

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
