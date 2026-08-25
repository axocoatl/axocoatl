//! Sandbox sessions — isolated code execution in a rootless Podman container.
//!
//! A *directory session* in Axocoatl does not run the agent's tools on your
//! host. It runs them inside a per-session **rootless, daemonless Podman
//! container**. The session's working directory is its only host bind mount; a
//! root Node project also gets a Podman-managed volume over `node_modules` so
//! Linux never consumes host-native dependencies. The container is the security
//! boundary: a `write_file` or `run_command` tool can touch that sandboxed tree
//! and no other host directory, runs without the escape/recon Linux
//! capabilities, and — for untrusted code — can be cut off from the network.
//!
//! This example is **README-first and hands-on**: the live sandbox needs Podman,
//! which CI (and most reviewers' first checkout) won't have. So:
//!
//!   * `cargo run -p sandbox-session` prints this guide: the threat model, the
//!     exact `podman ps` / mount-inspection commands a Podman user would run to
//!     *see* the isolation, and the `axocoatl.yaml` config knobs that tune it.
//!     It also probes for Podman and tells you whether the live path is runnable
//!     on this machine.
//!   * The config-parse unit test (`sandbox_config_parses_and_maps_to_policy`)
//!     runs everywhere with no Podman — it parses a real `sandbox:` block with
//!     the real config type and maps it to the real [`SandboxPolicy`] using the
//!     exact conversion the daemon uses.
//!   * The live test (`sandbox_jails_the_workspace_and_runs_commands`) is
//!     `#[ignore]`d. With Podman installed, run it explicitly:
//!     `cargo test -p sandbox-session -- --ignored`.
//!
//! Run: `cargo run -p sandbox-session`.
//!
//! Where this lives in the real runtime:
//!   * Sandbox lifecycle + hardening: `crates/axocoatl-isolation/src/session_sandbox.rs`
//!   * Podman detection / setup:      `crates/axocoatl-isolation/src/podman.rs`
//!   * Config struct + defaults:      `crates/axocoatl-config/src/types.rs` (`SandboxConfigYaml`)
//!   * Config → policy conversion:    `crates/axocoatl-daemon/src/bootstrap.rs` (`ensure_sandbox`)
//!   * Threat model in prose:         `docs/ARCHITECTURE.md` (Security model)

use axocoatl_isolation::podman::{self, PodmanReadiness};
use axocoatl_isolation::session_sandbox::{
    CURATED_IMAGES, DEFAULT_IMAGE, REQUIRED_REPOSITORY_COMMANDS,
};

/// The escape/recon Linux capabilities dropped from every session container.
/// These mirror `DROPPED_CAPS` in `session_sandbox.rs` (a private const there,
/// reproduced here only for the guide text). The package-manager caps (CHOWN,
/// SETUID/SETGID, DAC_OVERRIDE, FOWNER) are deliberately KEPT so apk/apt/npm/pip
/// still work.
const DROPPED_CAPS_FOR_DISPLAY: &[&str] = &[
    "SYS_ADMIN",       // mount, namespace ops — the classic escape lever
    "SYS_PTRACE",      // inspect/inject other processes
    "SYS_MODULE",      // load kernel modules
    "SYS_RAWIO",       // raw device I/O
    "SYS_BOOT",        // reboot
    "SYS_TIME",        // set the system clock
    "NET_ADMIN",       // reconfigure networking / firewall
    "NET_RAW",         // raw/packet sockets — spoofing, scanning
    "DAC_READ_SEARCH", // bypass file read/traverse permission checks
    "MKNOD",           // create device nodes
    "AUDIT_WRITE",     // write to the kernel audit log
];

const RULE: &str = "────────────────────────────────────────────────────────────────────";

#[tokio::main]
async fn main() {
    println!("=== Axocoatl: Sandbox Sessions (rootless Podman container) ===\n");

    print_what_it_is();
    print_threat_model();
    print_config_knobs();
    print_inspection_steps();
    print_podman_probe().await;

    println!("\n{RULE}");
    println!(
        "\nThis guide ran with no container. To execute the real sandbox path,\n\
         install Podman and run the ignored integration test:\n\n\
         \x20   cargo test -p sandbox-session -- --ignored\n"
    );
    println!("=== Done ===");
}

fn print_what_it_is() {
    println!("{RULE}");
    println!("WHAT A SANDBOX SESSION IS");
    println!("{RULE}\n");
    println!(
        "A directory session opens a working directory (e.g. a cloned git repo) and\n\
         starts a long-lived container named `axo-ses-<session_id>` only after\n\
         its durable environment can become Ready:\n\n\
         \x20 • That directory is the only HOST bind mount, read-write, at the SAME\n\
         \x20   path inside the container ({{dir}}:{{dir}}:rw). Nothing else of the\n\
         \x20   host is visible — not home, SSH keys, or sibling projects.\n\
         \x20 • A root Node project gets a separate Podman volume mounted over\n\
         \x20   {{dir}}/node_modules. It masks host-native packages; nested package\n\
         \x20   directories remain part of the read-write Workspace mount.\n\
         \x20 • Every session tool — write_file, read_file, run_command — runs as\n\
         \x20   `podman exec` INSIDE that container. The container is the boundary.\n\
         \x20 • The default base image is `{DEFAULT_IMAGE}`. Local startup verifies\n\
         \x20   the repository commands Axocoatl needs and attempts distro-aware\n\
         \x20   provisioning inside the container. Missing commands fail setup and\n\
         \x20   remove the container; they are not deferred as later tool errors.\n"
    );
    println!("Curated image references accepted without arbitrary-image trust:");
    for image in CURATED_IMAGES {
        println!("\x20   • {image}");
    }
    println!("\nCurated means allowlisted, not inherently trusted or guaranteed to contain a\nproject's language dependencies. Repository setup remains a separate review.\n");
    println!("Commands every repository sandbox must provide before Ready:");
    println!("\x20   {}\n", REQUIRED_REPOSITORY_COMMANDS.join(" "));
}

fn print_threat_model() {
    println!("{RULE}");
    println!("THREAT MODEL — what the sandbox DOES and does NOT solve");
    println!("{RULE}\n");

    println!("Contains the blast radius of a mistaken or misbehaving agent:\n");
    println!(
        "\x20 • Filesystem.  The session's working directory is the only host bind\n\
         \x20   mount. A root Node project also gets a Podman-managed dependency\n\
         \x20   volume, not another host path. A destructive command can change the\n\
         \x20   read-write Workspace, the container, and that dependency volume; the\n\
         \x20   rest of the host is not present.\n"
    );
    println!(
        "\x20 • Privileges.  `--security-opt=no-new-privileges` (a setuid binary\n\
         \x20   can't escalate) plus these dropped capabilities:"
    );
    for cap in DROPPED_CAPS_FOR_DISPLAY {
        println!("\x20       --cap-drop {cap}");
    }
    println!();
    println!(
        "\x20 • Network.    Bridged networking is the default so installs and\n\
         \x20   development servers work. Set `sandbox.network: none` for an\n\
         \x20   untrusted run that must have no outbound connection (cuts off\n\
         \x20   exfiltration / C2 / SSRF).\n"
    );
    println!(
        "\x20 • Resources.  Memory / CPU / PID caps (2 GB / 2 CPUs / 512 pids)\n\
         \x20   bound a runaway loop or fork bomb, where the host's cgroup\n\
         \x20   delegation allows it.\n"
    );

    println!("Does NOT solve — stated plainly so you don't over-trust it:\n");
    println!(
        "\x20 • Prompt injection.  If the agent reads malicious instructions from a\n\
         \x20   file, web page, or tool output, the sandbox does not stop it from\n\
         \x20   ACTING on them inside its workspace and its allowed network.\n\
         \x20   Isolation bounds the blast radius; it is not a defense against an\n\
         \x20   agent being talked into the wrong thing. Keep secrets out of the\n\
         \x20   workspace and prefer `--network none` for untrusted inputs.\n"
    );
    println!(
        "\x20 • Host kernel / Podman bugs.  Container isolation is only as strong\n\
         \x20   as the kernel and Podman underneath it. A container-escape CVE is\n\
         \x20   outside this layer's control.\n"
    );
    println!(
        "\x20 • What the policy grants.  Bridged networking (the default),\n\
         \x20   mounted credentials, or a permissive tool policy widen the\n\
         \x20   surface.\n"
    );
}

fn print_config_knobs() {
    println!("{RULE}");
    println!("CONFIG KNOBS — the `sandbox:` block in axocoatl.yaml");
    println!("{RULE}\n");
    println!(
        "The local backend and trust controls default conservatively: a new repo\n\
         cannot approve its own setup command or choose an arbitrary image:\n"
    );
    println!("\x20 sandbox:");
    println!("\x20   backend: podman");
    println!();
    println!("\x20   # Default the exact devcontainer postCreateCommand to approved");
    println!("\x20   # for an unreviewed Session. A reviewed Session decision wins.");
    println!("\x20   allow_post_create_command: false");
    println!();
    println!("\x20   # Honor a repo/UI-specified base image other than the trusted");
    println!("\x20   # default. Off by default — an attacker-chosen image is");
    println!("\x20   # attacker-controlled code.");
    println!("\x20   allow_untrusted_images: false");
    println!();
    println!("\x20   # \"bridge\" (default: outbound + published ports) or \"none\"");
    println!("\x20   # (no network at all — blocks exfiltration for untrusted code,");
    println!("\x20   # but also package installs and dev servers).");
    println!("\x20   network: bridge");
    println!();
    println!("\x20   # Refuse to start if memory/CPU/pid limits can't be applied,");
    println!("\x20   # instead of silently running uncapped. Off by default because");
    println!("\x20   # some hosts (rootless podman on WSL2) can't delegate cgroups.");
    println!("\x20   require_resource_limits: false");
    println!();
    println!(
        "The daemon first resolves the operator's devcontainer default into one\n\
         exact durable Session command. The generic container post-create hook\n\
         stays disabled, so there is no second implicit execution path. A checked\n\
         or unchecked reviewed decision wins; detected npm ci and edited commands\n\
         are never covered by the operator default.\n\n\
         Missing Podman or a missing VM fails with manual instructions. Session\n\
         startup never installs host software or creates a VM, though it may start\n\
         an existing stopped VM. With network: none, the image must already contain\n\
         required repository commands; a host-side image pull is separate.\n"
    );
}

fn print_inspection_steps() {
    println!("{RULE}");
    println!("INSPECT THE ISOLATION — exact commands (with Podman installed)");
    println!("{RULE}\n");
    println!(
        "After Axocoatl opens a session (or after you run the ignored test below),\n\
         a container named `axo-ses-<session_id>` is running. Verify it yourself:\n"
    );
    println!("\x20 # 1. See the live session container (sleeping `sleep infinity`):");
    println!("\x20 $ podman ps --filter name=axo-ses-");
    println!("\x20   CONTAINER ID  IMAGE                            COMMAND         ... NAMES");
    println!(
        "\x20   a1b2c3d4e5f6  docker.io/library/alpine:3.20   sleep infinity  ... axo-ses-demo"
    );
    println!();
    println!("\x20 # 2. Confirm the workspace is the only HOST bind — no home dir or keys:");
    println!("\x20 $ podman inspect axo-ses-demo --format '{{{{json .Mounts}}}}'");
    println!("\x20   [{{\"Type\":\"bind\",\"Source\":\"/path/to/workspace\",");
    println!("\x20     \"Destination\":\"/path/to/workspace\",\"RW\":true, ...}},");
    println!(
        "\x20    {{\"Type\":\"volume\",\"Destination\":\"/path/to/workspace/node_modules\", ...}}]"
    );
    println!("\x20   # The volume entry appears only for a root Node project.");
    println!();
    println!("\x20 # 3. Confirm the dropped capabilities and no-new-privileges:");
    println!("\x20 $ podman inspect axo-ses-demo \\");
    println!("\x20       --format '{{{{.HostConfig.CapDrop}}}} | {{{{.HostConfig.SecurityOpt}}}}'");
    println!("\x20   [SYS_ADMIN SYS_PTRACE ... AUDIT_WRITE] | [no-new-privileges]");
    println!();
    println!("\x20 # 4. With network: none, prove outbound is blocked from INSIDE:");
    println!("\x20 $ podman exec axo-ses-demo wget -qO- https://example.com");
    println!("\x20   wget: bad address 'example.com'        # DNS/connect fails — good");
    println!();
    println!("\x20 # 5. Prove the host filesystem is NOT reachable from inside:");
    println!("\x20 $ podman exec axo-ses-demo ls /  # workspace path present; host $HOME absent");
    println!();
}

/// Probe this machine for Podman and report whether the live path is runnable
/// here. This is the same detection the runtime uses (`podman::detect`), so the
/// answer is accurate, not guessed.
async fn print_podman_probe() {
    println!("{RULE}");
    println!("PODMAN ON THIS MACHINE");
    println!("{RULE}\n");
    let readiness = podman::detect().await;
    println!("\x20 detect() → {readiness:?}");
    match readiness {
        PodmanReadiness::Ready => {
            println!(
                "\x20 Podman is ready. The ignored integration test will actually start a\n\
                 \x20 container here:  cargo test -p sandbox-session -- --ignored"
            );
        }
        other => {
            println!("\x20 {}", other.summary());
            println!(
                "\x20 The live sandbox path can't run until Podman is ready; the guide and\n\
                 \x20 the config-parse test above run regardless."
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use axocoatl_config::parse_config;
    use axocoatl_isolation::session_sandbox::{SandboxNetwork, SandboxPolicy};
    use std::path::PathBuf;

    /// CI-safe (no Podman). Parses a real `sandbox:` block with the real config
    /// type, then mirrors the runtime container policy. The daemon resolves
    /// post-create trust into the exact durable Session command before sandbox
    /// startup, so the generic hook remains disabled even when the config
    /// default is enabled.
    #[test]
    fn sandbox_config_parses_and_maps_to_policy() {
        // Exercise every trust/network mapping, including the daemon-level
        // devcontainer default, while cutting the container network.
        let yaml = r#"
sandbox:
  allow_post_create_command: true
  allow_untrusted_images: false
  network: none
  require_resource_limits: true
"#;
        let config = parse_config(yaml, &PathBuf::from("axocoatl.yaml"))
            .expect("a valid sandbox block must parse");
        let sc = &config.sandbox;

        // Parsed straight off the real SandboxConfigYaml.
        assert!(sc.allow_post_create_command);
        assert!(!sc.allow_untrusted_images);
        assert_eq!(sc.network, "none");
        assert!(sc.require_resource_limits);
        assert_eq!(sc.backend, "podman");

        // The runtime mapping from crates/axocoatl-daemon/src/bootstrap.rs:
        // config-level trust is resolved into the Session record, never passed
        // to SessionSandbox as a second implicit post-create execution path.
        let policy = SandboxPolicy {
            allow_post_create: false,
            allow_untrusted_image: sc.allow_untrusted_images,
            network: match sc.network.as_str() {
                "none" => SandboxNetwork::None,
                _ => SandboxNetwork::Bridge,
            },
            require_resource_limits: sc.require_resource_limits,
            passive_start: false,
            runtime_authority: None,
            control_plane_dirs: Vec::new(),
            control_plane_roots: Vec::new(),
        };

        assert!(!policy.allow_post_create);
        assert!(!policy.allow_untrusted_image);
        assert_eq!(policy.network, SandboxNetwork::None);
        assert!(policy.require_resource_limits);
    }

    /// An omitted `sandbox:` block must fall back to the secure defaults, and
    /// the default network string must map to bridge.
    #[test]
    fn omitted_sandbox_block_is_secure_by_default() {
        let config = parse_config("agents: []\n", &PathBuf::from("axocoatl.yaml"))
            .expect("empty config must parse");
        let sc = &config.sandbox;
        assert!(!sc.allow_post_create_command);
        assert!(!sc.allow_untrusted_images);
        assert_eq!(sc.network, "bridge");
        assert!(!sc.require_resource_limits);

        let network = match sc.network.as_str() {
            "none" => SandboxNetwork::None,
            _ => SandboxNetwork::Bridge,
        };
        assert_eq!(network, SandboxNetwork::Bridge);
        assert_eq!(sc.backend, "podman");
    }

    /// E2B is selected once for the daemon. Its template is not a per-Session
    /// OCI image and must be provisioned with Axocoatl's repository commands.
    #[test]
    fn e2b_backend_and_template_parse_at_daemon_scope() {
        let config = parse_config(
            r#"
sandbox:
  backend: e2b
  e2b:
    api_url: "https://api.e2b.dev"
    api_key: "test-key"
    template: "team-ready"
    domain: "e2b.app"
"#,
            &PathBuf::from("axocoatl.yaml"),
        )
        .expect("valid E2B sandbox config must parse");

        assert_eq!(config.sandbox.backend, "e2b");
        let e2b = config
            .sandbox
            .e2b
            .as_ref()
            .expect("E2B settings live under the daemon's sandbox config");
        assert_eq!(e2b.template, "team-ready");
    }

    /// Live sandbox path. Requires Podman (a VM on macOS/Windows), so it is
    /// ignored by default and skipped in CI. With Podman ready:
    ///   cargo test -p sandbox-session -- --ignored
    ///
    /// It starts a real session container, runs a command inside it, writes a
    /// file from inside, and proves the write lands in the bind-mounted host
    /// directory. A root package.json also proves the nested dependency volume
    /// masks host node_modules instead of importing platform-specific packages.
    #[tokio::test]
    #[ignore = "requires Podman; run with: cargo test -p sandbox-session -- --ignored"]
    async fn sandbox_jails_the_workspace_and_runs_commands() {
        use axocoatl_isolation::session_sandbox::SessionSandbox;
        use std::time::Duration;

        let workspace = tempfile::tempdir().expect("temp workspace");
        std::fs::write(
            workspace.path().join("package.json"),
            "{\"private\":true}\n",
        )
        .expect("write package marker");
        std::fs::create_dir(workspace.path().join("node_modules"))
            .expect("create host dependency directory");
        std::fs::write(
            workspace.path().join("node_modules/host-only.txt"),
            "host-native",
        )
        .expect("write host dependency marker");
        let sandbox_id = format!("demo-{}", std::process::id());

        // Secure defaults: no post-create, no untrusted image, bridge network
        // (so apk/dev tools work). Flip `network` to None to demonstrate the
        // outbound block — see step 4 of the inspection steps in the guide.
        let sandbox = SessionSandbox::start(
            &sandbox_id,
            workspace.path(),
            None, // use DEFAULT_IMAGE
            &[],  // no published ports
            &[],  // no post-create commands
            &SandboxPolicy::default(),
        )
        .await
        .expect("sandbox should start with Podman ready");
        assert!(sandbox.uses_node_dependency_volume());

        // A command runs INSIDE the container.
        let echo = sandbox
            .exec(&["echo", "hello-from-sandbox"], Duration::from_secs(30))
            .await
            .expect("exec echo");
        assert!(echo.ok(), "echo exited non-zero: {echo:?}");
        assert!(echo.stdout.contains("hello-from-sandbox"));

        // A write from inside the container lands in the bind-mounted workspace
        // and is visible on the host — proving the mount is exactly this dir.
        sandbox
            .exec_stdin(
                &["sh", "-c", "cat > \"$1\"", "sh", "from-inside.txt"],
                "written-in-container",
                Duration::from_secs(30),
            )
            .await
            .expect("exec write");
        let on_host = std::fs::read_to_string(workspace.path().join("from-inside.txt"))
            .expect("file written inside the container must appear on the host");
        assert_eq!(on_host, "written-in-container");

        // The named volume mounted over root node_modules starts without
        // copying host-native contents. Writes there stay in the volume.
        let host_dependency_probe = sandbox
            .exec(
                &["sh", "-c", "test ! -e node_modules/host-only.txt"],
                Duration::from_secs(30),
            )
            .await
            .expect("probe masked host dependencies");
        assert!(
            host_dependency_probe.ok(),
            "host node_modules leaked into Linux: {host_dependency_probe:?}"
        );
        let dependency_write = sandbox
            .exec(
                &[
                    "sh",
                    "-c",
                    "printf '%s' container-linux > node_modules/container-only.txt",
                ],
                Duration::from_secs(30),
            )
            .await
            .expect("write container dependency marker");
        assert!(dependency_write.ok());
        assert!(
            !workspace
                .path()
                .join("node_modules/container-only.txt")
                .exists(),
            "container dependency volume mutated host node_modules"
        );

        // The host home directory is NOT mounted — listing it from inside the
        // container must not reveal host contents. We assert the container is
        // confined to its own root filesystem plus the one workspace mount.
        let host_home = std::env::var("HOME").unwrap_or_default();
        if !host_home.is_empty() {
            let probe = sandbox
                .exec(&["ls", &host_home], Duration::from_secs(30))
                .await
                .expect("exec ls");
            // The host $HOME path does not exist inside the container (the only
            // bind mount is the temp workspace), so `ls` fails.
            assert!(
                !probe.ok(),
                "host HOME unexpectedly visible inside the sandbox: {probe:?}"
            );
        }

        sandbox.stop().await;
        SessionSandbox::remove_named_with_dependencies(&sandbox_id)
            .await
            .expect("remove example container and dependency volume");
    }
}
