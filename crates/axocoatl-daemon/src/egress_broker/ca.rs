//! A Session's certificate authority.
//!
//! One ECDSA P-256 key pair per Session, generated in memory and never
//! written anywhere, signs a 30-day authority certificate and a 24-hour
//! server certificate per route host. Containers trust the authority
//! through the trust files ([`super::trust`]); its key never leaves the
//! daemon. The authority has no name constraints, so a configuration reload
//! can add routes without a new authority.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use ring::rand::{SecureRandom, SystemRandom};
use ring::signature::{EcdsaKeyPair, KeyPair, ECDSA_P256_SHA256_ASN1_SIGNING};
use rustls::crypto::CryptoProvider;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use rustls::sign::CertifiedKey;
use zeroize::Zeroizing;

use super::x509::{self, CertificateFields, Profile};
use super::BrokerError;

/// How long a Session's authority is valid.
pub const CA_VALIDITY: Duration = Duration::from_secs(30 * 24 * 3600);
/// How long one server certificate is valid.
pub const LEAF_VALIDITY: Duration = Duration::from_secs(24 * 3600);
/// Certificates start this long before they are made, for clock skew
/// between the computer and the Podman machine.
pub const BACKDATE: Duration = Duration::from_secs(3600);
/// A cached server certificate is replaced when it has less than this left.
const LEAF_RENEW_BEFORE: Duration = Duration::from_secs(3600);
/// Most server certificates kept at once.
const MAX_LEAVES: usize = 256;
const ORGANIZATION: &str = "Axocoatl";

struct Leaf {
    der: CertificateDer<'static>,
    pkcs8: Zeroizing<Vec<u8>>,
    not_after: SystemTime,
    certified: Arc<CertifiedKey>,
}

/// One Session's certificate authority. See the module documentation.
pub struct SessionCa {
    session: String,
    common_name: String,
    rng: SystemRandom,
    key: EcdsaKeyPair,
    key_id: [u8; 20],
    der: CertificateDer<'static>,
    not_after: SystemTime,
    provider: Arc<CryptoProvider>,
    leaves: Mutex<HashMap<String, Arc<Leaf>>>,
}

impl std::fmt::Debug for SessionCa {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SessionCa")
            .field("session", &self.session)
            .field("common_name", &self.common_name)
            .finish_non_exhaustive()
    }
}

fn serial(rng: &SystemRandom) -> Result<[u8; 16], BrokerError> {
    let mut serial = [0u8; 16];
    rng.fill(&mut serial)
        .map_err(|_| BrokerError::Certificate("the system random source failed".into()))?;
    Ok(serial)
}

fn generate_key(rng: &SystemRandom) -> Result<(EcdsaKeyPair, Zeroizing<Vec<u8>>), BrokerError> {
    let pkcs8 = EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_ASN1_SIGNING, rng)
        .map_err(|_| BrokerError::Certificate("generating a P-256 key failed".into()))?;
    let pkcs8 = Zeroizing::new(pkcs8.as_ref().to_vec());
    let key = EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_ASN1_SIGNING, &pkcs8, rng)
        .map_err(|error| BrokerError::Certificate(format!("loading a P-256 key: {error}")))?;
    Ok((key, pkcs8))
}

impl SessionCa {
    /// A new authority for `session`, valid from an hour ago for 30 days.
    pub fn new(session: &str) -> Result<Self, BrokerError> {
        Self::new_at(session, SystemTime::now())
    }

    pub(crate) fn new_at(session: &str, now: SystemTime) -> Result<Self, BrokerError> {
        let rng = SystemRandom::new();
        let (key, _pkcs8) = generate_key(&rng)?;
        let public = key.public_key().as_ref().to_vec();
        let key_id = x509::key_identifier(&public);
        let short: String = session.chars().take(36).collect();
        let common_name = format!("Axocoatl Session {short}");
        let not_after = now + CA_VALIDITY;
        let tbs = x509::tbs_certificate(&CertificateFields {
            serial: serial(&rng)?,
            organization: ORGANIZATION,
            issuer_common_name: &common_name,
            subject_common_name: &common_name,
            not_before: now - BACKDATE,
            not_after,
            subject_public_key: &public,
            issuer_key_id: &key_id,
            profile: Profile::Authority,
        });
        let signature = key
            .sign(&rng, &tbs)
            .map_err(|_| BrokerError::Certificate("signing the authority failed".into()))?;
        let der = CertificateDer::from(x509::certificate(&tbs, signature.as_ref()));
        Ok(Self {
            session: session.to_string(),
            common_name,
            rng,
            key,
            key_id,
            der,
            not_after,
            provider: super::crypto_provider(),
            leaves: Mutex::new(HashMap::new()),
        })
    }

    /// The Session this authority belongs to.
    pub fn session(&self) -> &str {
        &self.session
    }

    /// The authority certificate.
    pub fn der(&self) -> &CertificateDer<'static> {
        &self.der
    }

    /// The authority certificate in PEM.
    pub fn pem(&self) -> String {
        x509::pem("CERTIFICATE", &self.der)
    }

    /// When the authority stops being valid.
    pub fn not_after(&self) -> SystemTime {
        self.not_after
    }

    /// The server certificate and key for `host`, made on first use and
    /// cached until it has less than an hour left.
    pub fn leaf(
        &self,
        host: &str,
    ) -> Result<(CertificateDer<'static>, PrivateKeyDer<'static>), BrokerError> {
        let leaf = self.leaf_entry(host, SystemTime::now())?;
        let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(leaf.pkcs8.to_vec()));
        Ok((leaf.der.clone(), key))
    }

    /// The certificate and signing key rustls serves for `host`.
    pub(crate) fn certified_key(&self, host: &str) -> Result<Arc<CertifiedKey>, BrokerError> {
        Ok(self.leaf_entry(host, SystemTime::now())?.certified.clone())
    }

    fn leaf_entry(&self, host: &str, now: SystemTime) -> Result<Arc<Leaf>, BrokerError> {
        let mut leaves = self
            .leaves
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(leaf) = leaves.get(host) {
            if leaf.not_after > now + LEAF_RENEW_BEFORE {
                return Ok(leaf.clone());
            }
        }
        if self.not_after <= now + LEAF_RENEW_BEFORE {
            return Err(BrokerError::Certificate(format!(
                "the Session's certificate authority expires within the hour; restart the Session's \
                 runtime for a new one ({})",
                self.common_name
            )));
        }
        let leaf = Arc::new(self.issue(host, now)?);
        if leaves.len() >= MAX_LEAVES && !leaves.contains_key(host) {
            leaves.retain(|_, existing| existing.not_after > now + LEAF_RENEW_BEFORE);
            if leaves.len() >= MAX_LEAVES {
                leaves.clear();
            }
        }
        leaves.insert(host.to_string(), leaf.clone());
        Ok(leaf)
    }

    fn issue(&self, host: &str, now: SystemTime) -> Result<Leaf, BrokerError> {
        let (key, pkcs8) = generate_key(&self.rng)?;
        let public = key.public_key().as_ref().to_vec();
        let not_after = (now + LEAF_VALIDITY).min(self.not_after);
        let tbs = x509::tbs_certificate(&CertificateFields {
            serial: serial(&self.rng)?,
            organization: ORGANIZATION,
            issuer_common_name: &self.common_name,
            subject_common_name: host,
            not_before: now - BACKDATE,
            not_after,
            subject_public_key: &public,
            issuer_key_id: &self.key_id,
            profile: Profile::Server { dns_name: host },
        });
        let signature = self.key.sign(&self.rng, &tbs).map_err(|_| {
            BrokerError::Certificate(format!("signing a certificate for {host} failed"))
        })?;
        let der = CertificateDer::from(x509::certificate(&tbs, signature.as_ref()));
        let signing = self
            .provider
            .key_provider
            .load_private_key(PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(
                pkcs8.to_vec(),
            )))
            .map_err(|error| {
                BrokerError::Certificate(format!("loading the key for {host}: {error}"))
            })?;
        let certified = Arc::new(CertifiedKey::new(vec![der.clone()], signing));
        Ok(Leaf {
            der,
            pkcs8,
            not_after,
            certified,
        })
    }
}

#[cfg(test)]
#[path = "ca_tests.rs"]
mod tests;
