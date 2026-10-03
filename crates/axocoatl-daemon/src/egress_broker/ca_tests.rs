//! The Session authority's certificates, checked with `rustls-webpki`, and
//! a TLS handshake against a server certificate it issued.

use super::*;
use rustls::pki_types::{ServerName, UnixTime};

fn verify(
    ca: &CertificateDer<'_>,
    leaf: &CertificateDer<'_>,
    host: &str,
    at: SystemTime,
) -> Result<(), webpki::Error> {
    let anchor = webpki::anchor_from_trusted_cert(ca)?;
    let end_entity = webpki::EndEntityCert::try_from(leaf)?;
    let time = UnixTime::since_unix_epoch(at.duration_since(std::time::UNIX_EPOCH).unwrap());
    end_entity.verify_for_usage(
        webpki::ALL_VERIFICATION_ALGS,
        &[anchor],
        &[],
        time,
        webpki::KeyUsage::server_auth(),
        None,
        None,
    )?;
    end_entity.verify_is_valid_for_subject_name(&ServerName::try_from(host.to_string()).unwrap())
}

#[test]
fn webpki_accepts_each_leaf_for_its_host_only() {
    let ca = SessionCa::new("ses-ca-test").unwrap();
    let (github, _) = ca.leaf("github.com").unwrap();
    let (registry, _) = ca.leaf("registry.npmjs.org").unwrap();
    let now = SystemTime::now();
    verify(ca.der(), &github, "github.com", now).unwrap();
    verify(ca.der(), &registry, "registry.npmjs.org", now).unwrap();
    assert!(matches!(
        verify(ca.der(), &github, "api.github.com", now),
        Err(webpki::Error::CertNotValidForName(_))
    ));
    assert!(verify(ca.der(), &github, "registry.npmjs.org", now).is_err());
    // Not yet valid before the backdate, expired after a day.
    assert!(matches!(
        verify(
            ca.der(),
            &github,
            "github.com",
            now - BACKDATE - Duration::from_secs(120)
        ),
        Err(webpki::Error::CertNotValidYet { .. })
    ));
    assert!(matches!(
        verify(
            ca.der(),
            &github,
            "github.com",
            now + LEAF_VALIDITY + Duration::from_secs(120)
        ),
        Err(webpki::Error::CertExpired { .. })
    ));
    // Another Session's authority does not vouch for this leaf.
    let other = SessionCa::new("ses-other").unwrap();
    assert!(verify(other.der(), &github, "github.com", now).is_err());
    // The authority is not a server certificate.
    assert!(verify(ca.der(), ca.der(), "github.com", now).is_err());
}

#[test]
fn leaves_are_cached_per_host_and_bounded_by_the_authority() {
    let now = SystemTime::now();
    let ca = SessionCa::new_at("ses-cache", now).unwrap();
    let (first, _) = ca.leaf("a.example").unwrap();
    let (again, _) = ca.leaf("a.example").unwrap();
    let (other, _) = ca.leaf("b.example").unwrap();
    assert_eq!(first, again);
    assert_ne!(first, other);
    assert_eq!(ca.not_after(), now + CA_VALIDITY);
    // An authority about to expire issues nothing.
    let old = SessionCa::new_at("ses-old", now - CA_VALIDITY + Duration::from_secs(60)).unwrap();
    let error = old.leaf("a.example").unwrap_err().to_string();
    assert!(error.contains("expires within the hour"), "{error}");
    assert!(ca.pem().starts_with("-----BEGIN CERTIFICATE-----\n"));
    // The debug form names the Session, never key material.
    let shown = format!("{ca:?}");
    assert!(
        shown.contains("ses-cache") && !shown.contains("key"),
        "{shown}"
    );
}

#[test]
fn the_authority_is_a_ca_and_the_leaf_a_server_certificate() {
    let ca = SessionCa::new("ses-ext").unwrap();
    let der = ca.der().as_ref();
    let contains = |haystack: &[u8], needle: &[u8]| {
        haystack
            .windows(needle.len())
            .any(|window| window == needle)
    };
    // basicConstraints, critical, cA TRUE, pathLen 0.
    assert!(contains(
        der,
        &[
            0x06, 0x03, 0x55, 0x1d, 0x13, 0x01, 0x01, 0xff, 0x04, 0x08, 0x30, 0x06, 0x01, 0x01,
            0xff, 0x02, 0x01, 0x00
        ]
    ));
    // keyUsage, critical: digitalSignature, keyCertSign, cRLSign.
    assert!(contains(
        der,
        &[0x06, 0x03, 0x55, 0x1d, 0x0f, 0x01, 0x01, 0xff, 0x04, 0x04, 0x03, 0x02, 0x01, 0x86]
    ));
    let (leaf, _) = ca.leaf("a.example").unwrap();
    // serverAuth extended key usage, and the host as a DNS name.
    assert!(contains(
        leaf.as_ref(),
        &[0x2b, 0x06, 0x01, 0x05, 0x05, 0x07, 0x03, 0x01]
    ));
    assert!(contains(leaf.as_ref(), b"\x82\x09a.example"));
}

#[tokio::test]
async fn a_client_trusting_only_the_authority_completes_a_handshake() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let ca = SessionCa::new("ses-handshake").unwrap();
    let (cert, key) = ca.leaf("api.example").unwrap();
    let provider = crate::egress_broker::crypto_provider();
    let server = rustls::ServerConfig::builder_with_provider(provider.clone())
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(vec![cert], key)
        .unwrap();
    let mut roots = rustls::RootCertStore::empty();
    roots.add(ca.der().clone()).unwrap();
    let client = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_root_certificates(roots)
        .with_no_client_auth();
    let (client_io, server_io) = tokio::io::duplex(64 * 1024);
    let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(server));
    let server_task = tokio::spawn(async move {
        let mut tls = acceptor.accept(server_io).await.unwrap();
        let mut buf = [0u8; 5];
        tls.read_exact(&mut buf).await.unwrap();
        tls.write_all(b"world").await.unwrap();
        tls.shutdown().await.unwrap();
        buf
    });
    let connector = tokio_rustls::TlsConnector::from(Arc::new(client));
    let mut tls = connector
        .connect(ServerName::try_from("api.example").unwrap(), client_io)
        .await
        .unwrap();
    tls.write_all(b"hello").await.unwrap();
    let mut answer = Vec::new();
    tls.read_to_end(&mut answer).await.unwrap();
    assert_eq!(answer, b"world");
    assert_eq!(&server_task.await.unwrap(), b"hello");
}
