//! Real TLS clients in containers accept the certificates a Session's
//! authority issues, through the trust files and variables Axocoatl gives
//! containers: OpenSSL's strict verification, Python's `ssl` with
//! `VERIFY_X509_STRICT` (the default from Python 3.13), curl with
//! `CURL_CA_BUNDLE`, and Node with `NODE_EXTRA_CA_CERTS`. Each refuses a
//! certificate for another host.
//!
//! Ignored by default; everything runs inside containers with
//! `--network none`, over their own loopback:
//!
//! ```text
//! CONTAINER_CONNECTION=axocoatl-ci-pr74 \
//!   cargo test -p axocoatl-daemon --test egress_broker_clients_podman -- --ignored
//! ```
//!
//! Uses `docker.io/library/rust:bookworm` (Python 3.11, OpenSSL 3, curl) and
//! `docker.io/library/node:22-bookworm-slim` with `--pull=never`. Containers
//! carry `io.axocoatl.test=<AXOCOATL_TEST_LABEL or broker-clients-<pid>>`
//! and are run with `--rm`.

use std::path::Path;
use std::process::Command;

use axocoatl_daemon::egress_broker::{SessionCa, TrustMaterial};
use base64::Engine as _;

const TOOLS_IMAGE: &str = "docker.io/library/rust:bookworm";
const NODE_IMAGE: &str = "docker.io/library/node:22-bookworm-slim";

fn pem(label: &str, der: &[u8]) -> String {
    let encoded = base64::engine::general_purpose::STANDARD.encode(der);
    let lines: Vec<&str> = encoded
        .as_bytes()
        .chunks(64)
        .map(|line| std::str::from_utf8(line).unwrap())
        .collect();
    format!(
        "-----BEGIN {label}-----\n{}\n-----END {label}-----\n",
        lines.join("\n")
    )
}

fn label() -> String {
    let value = std::env::var("AXOCOATL_TEST_LABEL")
        .unwrap_or_else(|_| format!("broker-clients-{}", std::process::id()));
    format!("io.axocoatl.test={value}")
}

fn run(image: &str, dir: &Path, script: &str) -> (bool, String) {
    let output = Command::new("podman")
        .args([
            "run",
            "--rm",
            "--pull=never",
            "--network",
            "none",
            "--label",
            &label(),
            "-v",
            &format!("{}:/t:ro", dir.display()),
            image,
            "sh",
            "-c",
            script,
        ])
        .output()
        .unwrap();
    let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&output.stderr));
    (output.status.success(), text)
}

const PYTHON: &str = r#"
import socket, ssl, threading
server = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
server.load_cert_chain("/t/leaf.pem", "/t/leaf.key")
listener = socket.create_server(("127.0.0.1", 0))
port = listener.getsockname()[1]
def serve():
    while True:
        conn, _ = listener.accept()
        try:
            with server.wrap_socket(conn, server_side=True) as tls:
                tls.sendall(b"hello")
        except Exception:
            pass
threading.Thread(target=serve, daemon=True).start()
def connect(name):
    client = ssl.create_default_context(cafile="/t/bundle.pem")
    client.verify_flags |= ssl.VERIFY_X509_STRICT
    with socket.create_connection(("127.0.0.1", port)) as raw:
        with client.wrap_socket(raw, server_hostname=name) as tls:
            return tls.recv(5)
assert connect("api.test") == b"hello"
print("python strict: ok")
try:
    connect("other.test")
    print("python other host: ACCEPTED")
except ssl.SSLCertVerificationError as error:
    print("python other host: refused", error.verify_message)
"#;

const NODE: &str = r#"
const tls = require("tls"); const fs = require("fs");
const server = tls.createServer({cert: fs.readFileSync("/t/leaf.pem"), key: fs.readFileSync("/t/leaf.key")},
  (socket) => socket.end("hello"));
server.listen(0, "127.0.0.1", () => {
  const port = server.address().port;
  const connect = (name, done) => {
    const socket = tls.connect({host: "127.0.0.1", port, servername: name}, () => {
      socket.on("data", (data) => { done(null, data.toString()); socket.end(); });
    });
    socket.on("error", (error) => done(error));
  };
  connect("api.test", (error, data) => {
    console.log(error ? "node: " + error.code : "node: " + data);
    connect("other.test", (error) => {
      console.log("node other host: " + (error ? "refused " + error.code : "ACCEPTED"));
      server.close();
    });
  });
});
"#;

#[test]
#[ignore = "needs Podman: CONTAINER_CONNECTION=axocoatl-ci-pr74"]
fn container_clients_trust_the_session_authority_for_its_hosts_only() {
    let ca = SessionCa::new("ses-clients").unwrap();
    let (leaf, key) = ca.leaf("api.test").unwrap();
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("leaf.pem"), pem("CERTIFICATE", &leaf)).unwrap();
    std::fs::write(
        dir.path().join("leaf.key"),
        pem("PRIVATE KEY", key.secret_der()),
    )
    .unwrap();
    // No host roots: the bundle is then the Session authority alone, which
    // is what a container sees for a route host.
    let material = TrustMaterial::with_roots(&ca, &[]);
    for file in material.files() {
        std::fs::write(dir.path().join(&file.name), &file.contents).unwrap();
    }
    std::fs::write(dir.path().join("check.py"), PYTHON).unwrap();
    std::fs::write(dir.path().join("check.js"), NODE).unwrap();

    let script = "set -e
openssl verify -x509_strict -purpose sslserver -CAfile /t/session-ca.pem /t/leaf.pem
openssl verify -x509_strict -purpose sslserver -verify_hostname other.test -CAfile /t/session-ca.pem /t/leaf.pem && echo 'openssl other host: ACCEPTED' || echo 'openssl other host: refused'
python3 /t/check.py
openssl s_server -quiet -cert /t/leaf.pem -key /t/leaf.key -accept 127.0.0.1:8443 -www >/dev/null 2>&1 &
sleep 1
CURL_CA_BUNDLE=/t/bundle.pem curl -sS -o /dev/null -w 'curl: %{http_code}\\n' --resolve api.test:8443:127.0.0.1 https://api.test:8443/
CURL_CA_BUNDLE=/t/bundle.pem curl -sS -o /dev/null --resolve other.test:8443:127.0.0.1 https://other.test:8443/ && echo 'curl other host: ACCEPTED' || echo 'curl other host: refused'
";
    let (ok, output) = run(TOOLS_IMAGE, dir.path(), script);
    assert!(ok, "{output}");
    assert!(output.contains("/t/leaf.pem: OK"), "{output}");
    assert!(output.contains("openssl other host: refused"), "{output}");
    assert!(output.contains("python strict: ok"), "{output}");
    assert!(output.contains("python other host: refused"), "{output}");
    assert!(output.contains("curl: 200"), "{output}");
    assert!(output.contains("curl other host: refused"), "{output}");

    let (ok, output) = run(
        NODE_IMAGE,
        dir.path(),
        "NODE_EXTRA_CA_CERTS=/t/session-ca.pem node /t/check.js",
    );
    assert!(ok, "{output}");
    assert!(output.contains("node: hello"), "{output}");
    assert!(
        output.contains("node other host: refused ERR_TLS_CERT_ALTNAME_INVALID"),
        "{output}"
    );
}
