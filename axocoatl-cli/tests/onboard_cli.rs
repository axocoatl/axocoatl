//! `axocoatl onboard`, `doctor --json` and `dev` as a new user meets them,
//! each in a fresh HOME with its own daemon socket: onboarding refuses to
//! run without a terminal or `--yes` and changes nothing; `--plan` changes
//! nothing; doctor fails without Podman and says so in JSON; `dev` says a
//! port is taken before it says it runs.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

fn axocoatl(home: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_axocoatl"));
    command
        .current_dir(home)
        .env("HOME", home)
        .env_remove("XDG_CONFIG_HOME")
        .env_remove("XDG_DATA_HOME")
        .env_remove("XDG_STATE_HOME")
        .env_remove("AXOCOATL_DATA_DIR")
        .env_remove("CONTAINER_CONNECTION")
        .env_remove("CONTAINER_HOST")
        .env_remove("OPENROUTER_API_KEY")
        .env("AXOCOATL_SOCKET_PATH", "axo.sock")
        .stdin(Stdio::null());
    command
}

fn tree(directory: &Path) -> Vec<(PathBuf, Vec<u8>)> {
    let mut entries = Vec::new();
    for entry in std::fs::read_dir(directory).into_iter().flatten().flatten() {
        let path = entry.path();
        if path.is_dir() {
            entries.push((path.clone(), Vec::new()));
            entries.extend(tree(&path));
        } else {
            entries.push((path.clone(), std::fs::read(&path).unwrap_or_default()));
        }
    }
    entries.sort();
    entries
}

/// Without a terminal and without --yes, onboarding writes nothing and
/// exits 3, pointing to the flags (it used to write an Ollama config and
/// exit 0).
#[test]
fn onboard_without_a_terminal_refuses_and_points_to_the_flags() {
    let home = tempfile::tempdir().unwrap();
    let output = axocoatl(home.path())
        .arg("onboard")
        .env("PATH", "/usr/bin:/bin")
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(3), "{stderr}");
    assert!(
        stderr.contains("axocoatl onboard --yes --writer"),
        "{stderr}"
    );
    assert!(stderr.contains("--plan"), "{stderr}");
    assert!(stderr.contains("Nothing was changed."), "{stderr}");
    assert!(tree(home.path()).is_empty(), "{:?}", tree(home.path()));
}

/// A command line onboard does not take is a usage error (exit 3).
#[test]
fn onboard_usage_errors_exit_3() {
    let home = tempfile::tempdir().unwrap();
    for args in [
        vec!["onboard", "--writer", "gemini"],
        vec!["onboard", "--yes", "--reviewer", "anthropic:opus"],
        vec!["onboard", "--yes", "--pull", "--no-pull"],
        vec![
            "onboard",
            "--yes",
            "--ollama-url",
            "http://192.0.2.10:11434",
        ],
    ] {
        let output = axocoatl(home.path())
            .args(&args)
            .env("PATH", "/usr/bin:/bin")
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(3), "{args:?}");
    }
    assert!(tree(home.path()).is_empty());
}

/// Podman's own state under a HOME: asked for its version or state, Podman
/// may create its settings and (on Linux) storage directories there, and
/// Podman 4 (Ubuntu 24.04's 4.9.3) the lock file of its CNI network
/// configuration, `.config/cni/net.d/cni.lock`.
fn podmans_own(home: &Path, path: &Path) -> bool {
    let relative = path.strip_prefix(home).unwrap();
    relative.starts_with(".config/containers")
        || relative.starts_with(".local/share/containers")
        || [
            ".config",
            ".local",
            ".local/share",
            ".config/cni",
            ".config/cni/net.d",
            ".config/cni/net.d/cni.lock",
        ]
        .iter()
        .any(|own| relative == Path::new(own))
}

/// The regular files under `directory` and their contents, leaving out
/// Podman's own.
fn files(directory: &Path) -> Vec<(PathBuf, Vec<u8>)> {
    tree(directory)
        .into_iter()
        .filter(|(path, _)| path.is_file() && !podmans_own(directory, path))
        .collect()
}

/// `--plan` prints what this computer has and what onboarding would do,
/// and writes no file under HOME. (Podman, asked for its version or state,
/// may create its own settings and storage directories in a HOME that has
/// none.)
#[test]
fn onboard_plan_changes_nothing() {
    let home = tempfile::tempdir().unwrap();
    let before = files(home.path());
    let output = axocoatl(home.path())
        .args([
            "onboard",
            "--plan",
            "--writer",
            "ollama",
            "--ollama-url",
            "http://127.0.0.1:9",
            "--port",
            "8231",
        ])
        .env("PATH", "/usr/bin:/bin")
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(output.status.code(), Some(0), "{stdout}");
    assert!(stdout.contains("This computer: "), "{stdout}");
    assert!(
        stdout.contains("Plan only: nothing was changed."),
        "{stdout}"
    );
    assert!(stdout.contains("would"), "{stdout}");
    assert_eq!(files(home.path()), before);
    for (path, _) in tree(home.path()) {
        assert!(podmans_own(home.path(), &path), "{path:?}");
    }
}

/// Without Podman doctor fails, names the install, and its JSON has the
/// documented shape.
#[test]
fn doctor_json_fails_without_podman() {
    let home = tempfile::tempdir().unwrap();
    let empty = tempfile::tempdir().unwrap();
    let config = home.path().join("axocoatl.yaml");
    std::fs::write(
        &config,
        "agents: []\nserver:\n  port: 8233\n  host: \"127.0.0.1\"\n",
    )
    .unwrap();
    let output = axocoatl(home.path())
        .args(["doctor", "--json", "--config"])
        .arg(&config)
        .env("PATH", empty.path())
        .env("AXOCOATL_DATA_DIR", home.path().join("data"))
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    let report: serde_json::Value = serde_json::from_slice(&output.stdout)
        .unwrap_or_else(|error| panic!("{error}: {}", String::from_utf8_lossy(&output.stdout)));
    assert_eq!(report["ready"], false);
    for section in ["sandbox", "models", "writers", "daemon"] {
        assert!(report["sections"][section].is_array(), "{section}");
    }
    let podman = &report["sections"]["sandbox"][0];
    assert_eq!(podman["name"], "Podman");
    assert_eq!(podman["status"], "fail");
    assert!(podman["detail"]
        .as_str()
        .unwrap()
        .contains("needed for every Session and every axocoatl run"));
    assert!(podman["fix"].as_str().unwrap().contains("axocoatl onboard"));
    assert!(!report["steps_left"].as_array().unwrap().is_empty());
}

/// Without a configuration, doctor says to run onboard.
#[test]
fn doctor_without_a_configuration_points_to_onboard() {
    let home = tempfile::tempdir().unwrap();
    let output = axocoatl(home.path())
        .args(["doctor", "--config"])
        .arg(home.path().join("missing.yaml"))
        .env("PATH", "/usr/bin:/bin")
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(output.status.code(), Some(1), "{stdout}");
    assert!(
        stdout.contains("[FAIL] Config missing: no configuration at"),
        "{stdout}"
    );
    // Without a configuration there is no port to look at.
    assert!(!stdout.contains("Port"), "{stdout}");
    assert!(stdout.contains("Run `axocoatl onboard`"), "{stdout}");
    assert!(stdout.ends_with("Ready: no\n"), "{stdout}");
}

/// `dev` holds its port before it starts or prints that it runs: a taken
/// port fails at once with which port and how to change it.
#[test]
fn dev_says_a_port_is_taken_before_it_says_it_runs() {
    let home = tempfile::tempdir().unwrap();
    let holder = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = holder.local_addr().unwrap().port();
    let config = home.path().join("axocoatl.yaml");
    std::fs::write(
        &config,
        format!("agents: []\nserver:\n  port: {port}\n  host: \"127.0.0.1\"\n"),
    )
    .unwrap();
    let started = std::time::Instant::now();
    let output = axocoatl(home.path())
        .args(["dev", "--config"])
        .arg(&config)
        .env("PATH", "/usr/bin:/bin")
        .env("AXOCOATL_DATA_DIR", home.path().join("data"))
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(1), "{stdout}\n{stderr}");
    assert!(
        stderr.contains(&format!("Port {port} on 127.0.0.1 is already in use")),
        "{stderr}"
    );
    assert!(
        stderr.contains("axocoatl onboard --update --port"),
        "{stderr}"
    );
    assert!(!stdout.contains("Axocoatl is running"), "{stdout}");
    assert!(!stdout.contains("Axocoatl dev mode"), "{stdout}");
    assert!(started.elapsed() < std::time::Duration::from_secs(30));
    drop(holder);
}

/// `recipe build` without Podman says what is missing and how to set it
/// up, instead of `podman build`'s "No such file or directory".
#[test]
fn recipe_build_without_podman_names_the_setup() {
    let home = tempfile::tempdir().unwrap();
    let empty = tempfile::tempdir().unwrap();
    let output = axocoatl(home.path())
        .args(["recipe", "build", "claude-code", "--config"])
        .arg(home.path().join("config.yaml"))
        .env("PATH", empty.path())
        .env("AXOCOATL_DATA_DIR", home.path().join("data"))
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(5), "{stderr}");
    assert!(stderr.contains("podman is required"), "{stderr}");
    assert!(stderr.contains("axocoatl onboard"), "{stderr}");
    assert!(!stderr.contains("os error 2"), "{stderr}");
}

/// After a reboot the machine is stopped: `recipe build` starts it before
/// it builds (a shim Podman records the order and builds nothing).
#[cfg(target_os = "macos")]
#[test]
fn recipe_build_starts_a_stopped_machine_first() {
    use std::os::unix::fs::PermissionsExt;
    let home = tempfile::tempdir().unwrap();
    let bin = home.path().join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let podman = bin.join("podman");
    std::fs::write(
        &podman,
        format!(
            r#"#!/bin/sh
D="{bin}"
echo "$*" >> "$D/calls"
case "$*" in
  "--version") echo "podman version 5.8.2" ;;
  "info --format json") if [ -f "$D/running" ]; then echo '{{"host":{{}}}}'; else exit 125; fi ;;
  "machine list --format json") if [ -f "$D/running" ]; then r=true; else r=false; fi; echo "[{{\"Name\":\"podman-machine-default\",\"Running\":$r}}]" ;;
  "machine start") touch "$D/running" ;;
  *) echo "shim: no $1" >&2; exit 125 ;;
esac
"#,
            bin = bin.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&podman, std::fs::Permissions::from_mode(0o755)).unwrap();
    let output = axocoatl(home.path())
        .args(["recipe", "build", "claude-code", "--config"])
        .arg(home.path().join("config.yaml"))
        .env("PATH", format!("{}:/usr/bin:/bin", bin.display()))
        .env("AXOCOATL_DATA_DIR", home.path().join("data"))
        .output()
        .unwrap();
    let calls = std::fs::read_to_string(bin.join("calls")).unwrap();
    let start = calls.find("machine start").expect(&calls);
    let build = calls.find("build ").expect(&calls);
    assert!(start < build, "{calls}");
    // The shim builds nothing, so the build itself fails.
    assert_eq!(output.status.code(), Some(5));
}
