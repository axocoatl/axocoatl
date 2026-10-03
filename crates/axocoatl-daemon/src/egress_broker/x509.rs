//! A small DER encoder for the two certificate profiles the broker issues:
//! a Session's certificate authority and a server certificate per route
//! host. Both use ECDSA P-256 with SHA-256 and X.509 v3 extensions. No
//! certificate crate is in the lock, so the structures are written here by
//! hand; the tests check every certificate with `rustls-webpki`.
//!
//! The profile also satisfies OpenSSL's strict verification (Python 3.13
//! turns it on by default): the authority's basic constraints are critical,
//! both certificates carry key usage and key identifiers, and serials are
//! positive.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use sha2::{Digest, Sha256};

const TAG_BOOLEAN: u8 = 0x01;
const TAG_INTEGER: u8 = 0x02;
const TAG_BIT_STRING: u8 = 0x03;
const TAG_OCTET_STRING: u8 = 0x04;
const TAG_OID: u8 = 0x06;
const TAG_UTF8_STRING: u8 = 0x0c;
const TAG_UTC_TIME: u8 = 0x17;
const TAG_GENERALIZED_TIME: u8 = 0x18;
const TAG_SEQUENCE: u8 = 0x30;
const TAG_SET: u8 = 0x31;

/// ecdsa-with-SHA256.
const OID_ECDSA_SHA256: &[u64] = &[1, 2, 840, 10045, 4, 3, 2];
/// id-ecPublicKey.
const OID_EC_PUBLIC_KEY: &[u64] = &[1, 2, 840, 10045, 2, 1];
/// prime256v1 (secp256r1).
const OID_P256: &[u64] = &[1, 2, 840, 10045, 3, 1, 7];
const OID_COMMON_NAME: &[u64] = &[2, 5, 4, 3];
const OID_ORGANIZATION: &[u64] = &[2, 5, 4, 10];
const OID_BASIC_CONSTRAINTS: &[u64] = &[2, 5, 29, 19];
const OID_KEY_USAGE: &[u64] = &[2, 5, 29, 15];
const OID_EXT_KEY_USAGE: &[u64] = &[2, 5, 29, 37];
const OID_SUBJECT_ALT_NAME: &[u64] = &[2, 5, 29, 17];
const OID_SUBJECT_KEY_ID: &[u64] = &[2, 5, 29, 14];
const OID_AUTHORITY_KEY_ID: &[u64] = &[2, 5, 29, 35];
/// id-kp-serverAuth.
const OID_SERVER_AUTH: &[u64] = &[1, 3, 6, 1, 5, 5, 7, 3, 1];

/// Longest common name written; longer names are left out of the subject
/// (X.520 bounds it at 64 characters).
const MAX_COMMON_NAME: usize = 64;

fn length(len: usize, out: &mut Vec<u8>) {
    if len < 0x80 {
        out.push(len as u8);
        return;
    }
    let bytes = len.to_be_bytes();
    let skip = bytes.iter().take_while(|byte| **byte == 0).count();
    out.push(0x80 | (bytes.len() - skip) as u8);
    out.extend_from_slice(&bytes[skip..]);
}

/// One tag-length-value.
pub(crate) fn tlv(tag: u8, content: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(content.len() + 4);
    out.push(tag);
    length(content.len(), &mut out);
    out.extend_from_slice(content);
    out
}

fn constructed(tag: u8, parts: &[&[u8]]) -> Vec<u8> {
    tlv(tag, &parts.concat())
}

fn sequence(parts: &[&[u8]]) -> Vec<u8> {
    constructed(TAG_SEQUENCE, parts)
}

/// A non-negative INTEGER from big-endian magnitude bytes.
fn unsigned_integer(bytes: &[u8]) -> Vec<u8> {
    let skip = bytes
        .iter()
        .take_while(|byte| **byte == 0)
        .count()
        .min(bytes.len().saturating_sub(1));
    let bytes = &bytes[skip..];
    let mut content = Vec::with_capacity(bytes.len() + 1);
    if bytes.first().is_none_or(|first| first & 0x80 != 0) {
        content.push(0);
    }
    content.extend_from_slice(bytes);
    tlv(TAG_INTEGER, &content)
}

fn oid(arcs: &[u64]) -> Vec<u8> {
    let mut content = Vec::new();
    let mut push = |mut value: u64| {
        let mut digits = vec![(value & 0x7f) as u8];
        value >>= 7;
        while value > 0 {
            digits.push(0x80 | (value & 0x7f) as u8);
            value >>= 7;
        }
        digits.reverse();
        content.extend_from_slice(&digits);
    };
    push(arcs[0] * 40 + arcs[1]);
    for arc in &arcs[2..] {
        push(*arc);
    }
    tlv(TAG_OID, &content)
}

fn bit_string(unused_bits: u8, bytes: &[u8]) -> Vec<u8> {
    let mut content = Vec::with_capacity(bytes.len() + 1);
    content.push(unused_bits);
    content.extend_from_slice(bytes);
    tlv(TAG_BIT_STRING, &content)
}

fn octet_string(bytes: &[u8]) -> Vec<u8> {
    tlv(TAG_OCTET_STRING, bytes)
}

fn boolean_true() -> Vec<u8> {
    tlv(TAG_BOOLEAN, &[0xff])
}

/// Days since 1970-01-01 to (year, month, day), proleptic Gregorian.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if month <= 2 { year + 1 } else { year }, month, day)
}

/// UTCTime for 1950-2049, GeneralizedTime otherwise (RFC 5280 4.1.2.5).
pub(crate) fn time(at: SystemTime) -> Vec<u8> {
    let seconds = at
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_secs() as i64;
    let (year, month, day) = civil_from_days(seconds.div_euclid(86_400));
    let rest = seconds.rem_euclid(86_400);
    let (hour, minute, second) = (rest / 3600, rest % 3600 / 60, rest % 60);
    if (1950..2050).contains(&year) {
        let text = format!(
            "{:02}{month:02}{day:02}{hour:02}{minute:02}{second:02}Z",
            year % 100
        );
        tlv(TAG_UTC_TIME, text.as_bytes())
    } else {
        let text = format!("{year:04}{month:02}{day:02}{hour:02}{minute:02}{second:02}Z");
        tlv(TAG_GENERALIZED_TIME, text.as_bytes())
    }
}

/// A Name of an organization and, when it fits, a common name.
fn name(organization: &str, common_name: &str) -> Vec<u8> {
    let attribute = |id: &[u64], value: &str| {
        constructed(
            TAG_SET,
            &[&sequence(&[
                &oid(id),
                &tlv(TAG_UTF8_STRING, value.as_bytes()),
            ])],
        )
    };
    let mut parts = vec![attribute(OID_ORGANIZATION, organization)];
    if !common_name.is_empty() && common_name.chars().count() <= MAX_COMMON_NAME {
        parts.push(attribute(OID_COMMON_NAME, common_name));
    }
    let parts: Vec<&[u8]> = parts.iter().map(Vec::as_slice).collect();
    sequence(&parts)
}

fn extension(id: &[u64], critical: bool, value: &[u8]) -> Vec<u8> {
    if critical {
        sequence(&[&oid(id), &boolean_true(), &octet_string(value)])
    } else {
        sequence(&[&oid(id), &octet_string(value)])
    }
}

fn signature_algorithm() -> Vec<u8> {
    sequence(&[&oid(OID_ECDSA_SHA256)])
}

/// A key identifier: the first 20 bytes of the SHA-256 of the public key
/// (RFC 7093, method 1).
pub(crate) fn key_identifier(public_key: &[u8]) -> [u8; 20] {
    let digest = Sha256::digest(public_key);
    let mut id = [0u8; 20];
    id.copy_from_slice(&digest[..20]);
    id
}

/// What a certificate is for.
#[derive(Debug, Clone, Copy)]
pub(crate) enum Profile<'a> {
    /// A certificate authority that may sign server certificates only.
    Authority,
    /// A server certificate for one DNS name.
    Server { dns_name: &'a str },
}

/// The fields of one certificate.
#[derive(Debug, Clone, Copy)]
pub(crate) struct CertificateFields<'a> {
    pub serial: [u8; 16],
    pub organization: &'a str,
    pub issuer_common_name: &'a str,
    pub subject_common_name: &'a str,
    pub not_before: SystemTime,
    pub not_after: SystemTime,
    /// Uncompressed P-256 point (65 bytes).
    pub subject_public_key: &'a [u8],
    pub issuer_key_id: &'a [u8; 20],
    pub profile: Profile<'a>,
}

/// The DER of a TBSCertificate, ready to sign.
pub(crate) fn tbs_certificate(fields: &CertificateFields<'_>) -> Vec<u8> {
    let mut serial = fields.serial;
    // Positive, and 16 bytes long in DER.
    serial[0] = (serial[0] & 0x7f) | 0x40;
    let version = tlv(0xa0, &unsigned_integer(&[2]));
    let spki = sequence(&[
        &sequence(&[&oid(OID_EC_PUBLIC_KEY), &oid(OID_P256)]),
        &bit_string(0, fields.subject_public_key),
    ]);
    let subject_key_id = key_identifier(fields.subject_public_key);
    let authority_key_id = sequence(&[&tlv(0x80, fields.issuer_key_id)]);
    let extensions: Vec<Vec<u8>> = match fields.profile {
        Profile::Authority => vec![
            // cA TRUE, pathLenConstraint 0: it signs end-entity certificates only.
            extension(
                OID_BASIC_CONSTRAINTS,
                true,
                &sequence(&[&boolean_true(), &unsigned_integer(&[0])]),
            ),
            // digitalSignature, keyCertSign, cRLSign.
            extension(OID_KEY_USAGE, true, &bit_string(1, &[0x86])),
            extension(OID_SUBJECT_KEY_ID, false, &octet_string(&subject_key_id)),
            extension(OID_AUTHORITY_KEY_ID, false, &authority_key_id),
        ],
        Profile::Server { dns_name } => vec![
            extension(OID_BASIC_CONSTRAINTS, true, &sequence(&[])),
            // digitalSignature.
            extension(OID_KEY_USAGE, true, &bit_string(7, &[0x80])),
            extension(
                OID_EXT_KEY_USAGE,
                false,
                &sequence(&[&oid(OID_SERVER_AUTH)]),
            ),
            extension(
                OID_SUBJECT_ALT_NAME,
                false,
                &sequence(&[&tlv(0x82, dns_name.as_bytes())]),
            ),
            extension(OID_SUBJECT_KEY_ID, false, &octet_string(&subject_key_id)),
            extension(OID_AUTHORITY_KEY_ID, false, &authority_key_id),
        ],
    };
    let extensions: Vec<&[u8]> = extensions.iter().map(Vec::as_slice).collect();
    sequence(&[
        &version,
        &unsigned_integer(&serial),
        &signature_algorithm(),
        &name(fields.organization, fields.issuer_common_name),
        &sequence(&[&time(fields.not_before), &time(fields.not_after)]),
        &name(fields.organization, fields.subject_common_name),
        &spki,
        &tlv(0xa3, &sequence(&extensions)),
    ])
}

/// The DER of a Certificate from its TBSCertificate and the ECDSA
/// signature (an ASN.1 `Ecdsa-Sig-Value`) over it.
pub(crate) fn certificate(tbs: &[u8], signature: &[u8]) -> Vec<u8> {
    sequence(&[tbs, &signature_algorithm(), &bit_string(0, signature)])
}

/// PEM text of one DER block.
pub(crate) fn pem(label: &str, der: &[u8]) -> String {
    use base64::Engine as _;
    let encoded = base64::engine::general_purpose::STANDARD.encode(der);
    let mut out = format!("-----BEGIN {label}-----\n");
    for line in encoded.as_bytes().chunks(64) {
        out.push_str(std::str::from_utf8(line).expect("base64 is ASCII"));
        out.push('\n');
    }
    out.push_str(&format!("-----END {label}-----\n"));
    out
}

/// The DER blocks of every `CERTIFICATE` in PEM text.
pub(crate) fn parse_pem_certificates(text: &str) -> Result<Vec<Vec<u8>>, String> {
    use base64::Engine as _;
    const BEGIN: &str = "-----BEGIN CERTIFICATE-----";
    const END: &str = "-----END CERTIFICATE-----";
    let mut found = Vec::new();
    let mut rest = text;
    while let Some(start) = rest.find(BEGIN) {
        let after = &rest[start + BEGIN.len()..];
        let end = after.find(END).ok_or("a PEM certificate has no END line")?;
        let body: String = after[..end]
            .chars()
            .filter(|c| !c.is_ascii_whitespace())
            .collect();
        let der = base64::engine::general_purpose::STANDARD
            .decode(body)
            .map_err(|error| format!("a PEM certificate is not valid base64: {error}"))?;
        if der.first() != Some(&TAG_SEQUENCE) {
            return Err("a PEM certificate does not hold DER".into());
        }
        found.push(der);
        rest = &after[end + END.len()..];
    }
    if found.is_empty() {
        return Err("no PEM certificate found".into());
    }
    Ok(found)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn der_lengths_integers_and_oids_encode_canonically() {
        assert_eq!(tlv(0x04, &[1, 2]), [0x04, 2, 1, 2]);
        assert_eq!(&tlv(0x04, &[0; 200])[..3], [0x04, 0x81, 200]);
        assert_eq!(&tlv(0x04, &[0; 300])[..4], [0x04, 0x82, 0x01, 0x2c]);
        assert_eq!(unsigned_integer(&[0]), [0x02, 1, 0]);
        assert_eq!(unsigned_integer(&[0, 0, 5]), [0x02, 1, 5]);
        assert_eq!(unsigned_integer(&[0x80]), [0x02, 2, 0, 0x80]);
        assert_eq!(
            oid(OID_ECDSA_SHA256),
            [0x06, 8, 0x2a, 0x86, 0x48, 0xce, 0x3d, 0x04, 0x03, 0x02]
        );
        assert_eq!(oid(OID_COMMON_NAME), [0x06, 3, 0x55, 0x04, 0x03]);
    }

    #[test]
    fn times_switch_to_generalized_time_in_2050() {
        let at = |seconds: u64| UNIX_EPOCH + Duration::from_secs(seconds);
        assert_eq!(&time(at(0))[2..], b"700101000000Z");
        // 2026-10-02 12:34:56 UTC.
        assert_eq!(&time(at(1_790_944_496))[2..], b"261002123456Z");
        // 2000-02-29.
        assert_eq!(&time(at(951_782_400))[2..], b"000229000000Z");
        // 2050-01-01 00:00:00 UTC.
        let generalized = time(at(2_524_608_000));
        assert_eq!(generalized[0], TAG_GENERALIZED_TIME);
        assert_eq!(&generalized[2..], b"20500101000000Z");
    }

    #[test]
    fn pem_round_trips() {
        let der = sequence(&[&unsigned_integer(&[7; 100])]);
        let text = pem("CERTIFICATE", &der);
        assert!(text.lines().all(|line| line.len() <= 64));
        assert_eq!(parse_pem_certificates(&text).unwrap(), vec![der.clone()]);
        let two = format!("junk\n{text}more\n{text}");
        assert_eq!(parse_pem_certificates(&two).unwrap().len(), 2);
        assert!(parse_pem_certificates("nothing").is_err());
        assert!(parse_pem_certificates("-----BEGIN CERTIFICATE-----\n!!\n").is_err());
    }
}
