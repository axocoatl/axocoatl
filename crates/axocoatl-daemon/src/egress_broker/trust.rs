//! What containers need to trust a Session's certificate authority.
//!
//! Two files go into the Session's trust volume (see
//! [`axocoatl_isolation::session_trust`]), mounted read-only at
//! `/etc/axocoatl/ca`:
//!
//! - `bundle.pem`: this computer's trusted roots (`rustls-native-certs`)
//!   followed by the Session's authority. Tools that read one bundle in
//!   place of the system's (OpenSSL, curl, Python, pip, Git, Cargo) get it
//!   through [`BUNDLE_ENV`], so hosts reached through plain `allow` tunnels
//!   still verify against the usual roots.
//! - `session-ca.pem`: the Session's authority alone, for tools that add
//!   certificates to their built-in roots ([`SESSION_CA_ENV`]: Node, Deno).

use axocoatl_isolation::session_trust::{TrustFile, TRUST_MOUNT_DIR};
use rustls::pki_types::CertificateDer;

use super::ca::SessionCa;
use super::x509;

/// The bundle of this computer's roots plus the Session's authority.
pub const BUNDLE_FILE: &str = "bundle.pem";
/// The Session's authority alone.
pub const SESSION_CA_FILE: &str = "session-ca.pem";

/// Variables set to the bundle's path inside containers.
pub const BUNDLE_ENV: [&str; 6] = [
    "SSL_CERT_FILE",
    "CURL_CA_BUNDLE",
    "REQUESTS_CA_BUNDLE",
    "PIP_CERT",
    "GIT_SSL_CAINFO",
    "CARGO_HTTP_CAINFO",
];
/// Variables set to the Session authority's path inside containers.
pub const SESSION_CA_ENV: [&str; 2] = ["NODE_EXTRA_CA_CERTS", "DENO_CERT"];

/// The bundle's path inside containers.
pub fn bundle_path() -> String {
    format!("{TRUST_MOUNT_DIR}/{BUNDLE_FILE}")
}

/// The Session authority's path inside containers.
pub fn session_ca_path() -> String {
    format!("{TRUST_MOUNT_DIR}/{SESSION_CA_FILE}")
}

/// The environment of D7: every [`BUNDLE_ENV`] variable set to the bundle
/// and every [`SESSION_CA_ENV`] variable set to the authority.
pub fn trust_env() -> Vec<(String, String)> {
    BUNDLE_ENV
        .iter()
        .map(|name| (name.to_string(), bundle_path()))
        .chain(
            SESSION_CA_ENV
                .iter()
                .map(|name| (name.to_string(), session_ca_path())),
        )
        .collect()
}

/// The trust files for one Session.
#[derive(Debug, Clone)]
pub struct TrustMaterial {
    pub bundle_pem: String,
    pub session_ca_pem: String,
    /// How many of this computer's roots the bundle holds.
    pub host_roots: usize,
    /// Roots that could not be loaded, for a warning.
    pub host_root_errors: Vec<String>,
}

impl TrustMaterial {
    /// The bundle with this computer's roots, read now.
    pub fn new(ca: &SessionCa) -> Self {
        let loaded = rustls_native_certs::load_native_certs();
        let mut material = Self::with_roots(ca, &loaded.certs);
        material.host_root_errors = loaded.errors.iter().map(ToString::to_string).collect();
        material
    }

    /// The bundle with the given roots.
    pub fn with_roots(ca: &SessionCa, roots: &[CertificateDer<'_>]) -> Self {
        let session_ca_pem = ca.pem();
        let mut bundle_pem = String::new();
        for root in roots {
            bundle_pem.push_str(&x509::pem("CERTIFICATE", root));
        }
        bundle_pem.push_str(&session_ca_pem);
        Self {
            bundle_pem,
            session_ca_pem,
            host_roots: roots.len(),
            host_root_errors: Vec::new(),
        }
    }

    /// The files for the trust volume.
    pub fn files(&self) -> Vec<TrustFile> {
        vec![
            TrustFile {
                name: BUNDLE_FILE.into(),
                contents: self.bundle_pem.clone().into_bytes(),
            },
            TrustFile {
                name: SESSION_CA_FILE.into(),
                contents: self.session_ca_pem.clone().into_bytes(),
            },
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_env_names_every_tool_and_routes_cannot_reuse_them() {
        let env = trust_env();
        assert_eq!(env.len(), 8);
        for (name, value) in &env {
            assert!(
                axocoatl_config::egress_routes::is_reserved_env(name),
                "{name} must be reserved from env_placeholders"
            );
            if SESSION_CA_ENV.contains(&name.as_str()) {
                assert_eq!(value, "/etc/axocoatl/ca/session-ca.pem");
            } else {
                assert_eq!(value, "/etc/axocoatl/ca/bundle.pem");
            }
        }
    }

    #[test]
    fn the_bundle_holds_the_host_roots_then_the_session_authority() {
        let ca = SessionCa::new("ses-trust").unwrap();
        let material = TrustMaterial::new(&ca);
        assert!(
            material.host_roots > 0,
            "no host roots loaded: {:?}",
            material.host_root_errors
        );
        let blocks = x509::parse_pem_certificates(&material.bundle_pem).unwrap();
        assert_eq!(blocks.len(), material.host_roots + 1);
        assert_eq!(blocks.last().unwrap(), ca.der().as_ref());
        assert_eq!(
            x509::parse_pem_certificates(&material.session_ca_pem).unwrap(),
            vec![ca.der().to_vec()]
        );
        let files = material.files();
        assert_eq!(files[0].name, "bundle.pem");
        assert_eq!(files[1].name, "session-ca.pem");
        assert!(!material.bundle_pem.contains("PRIVATE KEY"));
    }
}
