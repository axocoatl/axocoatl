//! The Session trust volume on a real Podman.
//!
//! Ignored by default. Run with a Podman connection you own:
//!
//! ```text
//! CONTAINER_CONNECTION=axocoatl-ci-pr74 \
//!   cargo test -p axocoatl-isolation --test session_trust_podman -- --ignored --test-threads=1
//! ```
//!
//! Every object it creates carries `io.axocoatl.test=<AXOCOATL_TEST_LABEL or
//! session-trust-<pid>>` and is removed by that label. It runs
//! `docker.io/library/alpine:3.20` with `--pull=never`.

use std::process::Command;

use axocoatl_isolation::session_trust::{
    copy_trust_files, populate_trust_volume, remove_trust_volume, trust_mount_arg,
    trust_volume_name, TrustFile, TrustVolumeSpec, TRUST_MOUNT_DIR,
};

const IMAGE: &str = "docker.io/library/alpine:3.20";

fn label() -> String {
    let value = std::env::var("AXOCOATL_TEST_LABEL")
        .unwrap_or_else(|_| format!("session-trust-{}", std::process::id()));
    format!("io.axocoatl.test={value}")
}

fn podman(args: &[&str]) -> (bool, String) {
    let output = Command::new("podman").args(args).output().unwrap();
    let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&output.stderr));
    (output.status.success(), text)
}

/// Removes every container and volume carrying the test label.
struct Cleanup(String);

impl Drop for Cleanup {
    fn drop(&mut self) {
        let filter = format!("label={}", self.0);
        let (_, containers) = podman(&["ps", "-a", "-q", "--filter", &filter]);
        for id in containers.split_whitespace() {
            podman(&["rm", "-f", "-t", "0", id]);
        }
        let (_, volumes) = podman(&["volume", "ls", "-q", "--filter", &filter]);
        for name in volumes.split_whitespace() {
            podman(&["volume", "rm", "-f", name]);
        }
    }
}

fn files(version: &str) -> Vec<TrustFile> {
    vec![
        TrustFile {
            name: "bundle.pem".into(),
            contents: format!(
                "-----BEGIN CERTIFICATE-----\nroots-{version}\n-----END CERTIFICATE-----\n"
            )
            .repeat(50)
            .into_bytes(),
        },
        TrustFile {
            name: "session-ca.pem".into(),
            contents: format!(
                "-----BEGIN CERTIFICATE-----\nca-{version}\n-----END CERTIFICATE-----\n"
            )
            .into_bytes(),
        },
    ]
}

#[tokio::test]
#[ignore = "needs Podman: CONTAINER_CONNECTION=axocoatl-ci-pr74"]
async fn the_trust_volume_is_imported_and_mounted_read_only() {
    let label = label();
    let _cleanup = Cleanup(label.clone());
    let session = format!("trust-test-{}", std::process::id());
    let spec = TrustVolumeSpec {
        session_id: session.clone(),
        runtime_authority: None,
        labels: vec![label.clone()],
    };
    populate_trust_volume(&spec, &files("one")).await.unwrap();
    let (ok, labels) = podman(&[
        "volume",
        "inspect",
        "--format",
        "{{json .Labels}}",
        &trust_volume_name(&session),
    ]);
    assert!(ok, "{labels}");
    assert!(
        labels.contains("\"io.axocoatl.role\":\"trust\""),
        "{labels}"
    );

    let script = format!(
        "cat {dir}/bundle.pem | head -2; stat -c '%a %u %g' {dir}/bundle.pem {dir}/session-ca.pem; \
         if touch {dir}/x 2>/dev/null; then echo WRITABLE; else echo READONLY; fi",
        dir = TRUST_MOUNT_DIR
    );
    let run = |script: &str| {
        podman(&[
            "run",
            "--rm",
            "--pull=never",
            "--network",
            "none",
            "--label",
            &label,
            "--mount",
            &trust_mount_arg(&session),
            IMAGE,
            "sh",
            "-c",
            script,
        ])
    };
    let (ok, output) = run(&script);
    assert!(ok, "{output}");
    assert!(output.contains("roots-one"), "{output}");
    assert_eq!(output.matches("644 0 0").count(), 2, "{output}");
    assert!(output.contains("READONLY"), "{output}");

    // Importing again replaces the files (a reload adds routes to the CA's
    // bundle without a new volume).
    populate_trust_volume(&spec, &files("two")).await.unwrap();
    let (ok, output) = run(&format!("cat {TRUST_MOUNT_DIR}/session-ca.pem"));
    assert!(
        ok && output.contains("ca-two") && !output.contains("ca-one"),
        "{output}"
    );

    remove_trust_volume(&session).await.unwrap();
    let (exists, _) = podman(&["volume", "exists", &trust_volume_name(&session)]);
    assert!(!exists);
}

#[tokio::test]
#[ignore = "needs Podman: CONTAINER_CONNECTION=axocoatl-ci-pr74"]
async fn the_fallback_copies_the_files_into_a_running_container() {
    let label = label();
    let _cleanup = Cleanup(label.clone());
    let name = format!("axo-trust-cp-{}", std::process::id());
    let (ok, output) = podman(&[
        "run",
        "-d",
        "--pull=never",
        "--network",
        "none",
        "--name",
        &name,
        "--label",
        &label,
        IMAGE,
        "sh",
        "-c",
        &format!("mkdir -p {TRUST_MOUNT_DIR} && sleep 300"),
    ]);
    assert!(ok, "{output}");
    let mut ready = false;
    for _ in 0..50 {
        if podman(&["exec", &name, "test", "-d", TRUST_MOUNT_DIR]).0 {
            ready = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert!(ready, "the container never made {TRUST_MOUNT_DIR}");
    copy_trust_files(&name, &files("copied")).await.unwrap();
    let (ok, output) = podman(&[
        "exec",
        &name,
        "sh",
        "-c",
        &format!(
            "cat {TRUST_MOUNT_DIR}/session-ca.pem; stat -c '%a %u' {TRUST_MOUNT_DIR}/bundle.pem"
        ),
    ]);
    assert!(ok, "{output}");
    assert!(output.contains("ca-copied"), "{output}");
    assert!(output.contains("644 0"), "{output}");
    assert!(copy_trust_files("-bad", &files("x")).await.is_err());
}
