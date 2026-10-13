//! The data root's receipt signing key (docs/design/1.4-receipts.md, 2.1):
//! one Ed25519 key, made with `ring` the first time the daemon signs (or by
//! `axocoatl receipt key`), stored in the secret store as
//! [`RECEIPT_KEY_SECRET`] (`0600`, replaced atomically, under the secrets
//! lock) and read from it each time a receipt is signed.
//!
//! The stored value is the PKCS#8 v2 document, base64, inside a tagged
//! one-line JSON object that begins with [`KEY_VALUE_PREFIX`]. The egress
//! broker refuses to send any credential value that begins so, and any
//! credential named [`RECEIPT_KEY_SECRET`], however a route names the file
//! or variable: the key never leaves the data root. Only the public key is
//! ever shown. Nothing here formats a private key into an error or a log.

use std::path::Path;

use axocoatl_session::receipt::{public_key_of, PublicKey};
use base64::Engine as _;
use ring::signature::Ed25519KeyPair;
use zeroize::Zeroizing;

use crate::secret_store::{self, SecretsLock};

/// The secret that holds the key.
pub const RECEIPT_KEY_SECRET: &str = "receipt-signing-key";

/// How the stored value begins (`MANAGED_VALUE_PREFIX`-style: the broker
/// refuses to send it).
pub const KEY_VALUE_PREFIX: &str = "{\"format\":\"axocoatl.receipt-signing-key/";

/// The stored value's format.
const KEY_FORMAT: &str = "axocoatl.receipt-signing-key/1";

/// The refusal when anything would send the key upstream.
pub const NEVER_A_ROUTE_CREDENTIAL: &str = "it is Axocoatl's receipt signing key \
     (secret receipt-signing-key), which signs receipts on this computer and is never sent \
     to any host";

/// Whether `name` is the receipt key's secret name.
pub fn is_receipt_key_name(name: &str) -> bool {
    name == RECEIPT_KEY_SECRET
}

/// The data root's signing key. It has no `Debug` that could print it.
pub struct SigningKey {
    pair: Ed25519KeyPair,
}

impl SigningKey {
    pub fn key_pair(&self) -> &Ed25519KeyPair {
        &self.pair
    }

    pub fn public(&self) -> PublicKey {
        public_key_of(&self.pair)
    }
}

/// `axocoatl@<hostname>`: the comment of the key's trusted-keys line.
pub fn host_comment() -> String {
    let mut buffer = [0u8; 256];
    // SAFETY: the buffer is valid for its whole length, and gethostname
    // writes at most that many bytes.
    let status =
        unsafe { libc::gethostname(buffer.as_mut_ptr().cast::<libc::c_char>(), buffer.len()) };
    let name = if status == 0 {
        let end = buffer.iter().position(|byte| *byte == 0).unwrap_or(0);
        String::from_utf8_lossy(&buffer[..end])
            .chars()
            .filter(|ch| ch.is_ascii_graphic())
            .collect::<String>()
    } else {
        String::new()
    };
    if name.is_empty() {
        "axocoatl@localhost".into()
    } else {
        format!("axocoatl@{name}")
    }
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredKey {
    format: String,
    algorithm: String,
    pkcs8: String,
}

/// `{"format":"axocoatl.receipt-signing-key/1","algorithm":"ed25519","pkcs8":"<base64>"}`.
fn encode(pkcs8: &[u8]) -> Zeroizing<Vec<u8>> {
    let encoded = Zeroizing::new(base64::engine::general_purpose::STANDARD.encode(pkcs8));
    let mut value = Zeroizing::new(Vec::with_capacity(encoded.len() + 96));
    value.extend_from_slice(
        format!("{{\"format\":\"{KEY_FORMAT}\",\"algorithm\":\"ed25519\",\"pkcs8\":\"").as_bytes(),
    );
    value.extend_from_slice(encoded.as_bytes());
    value.extend_from_slice(b"\"}");
    value
}

fn decode(value: &[u8]) -> Result<SigningKey, String> {
    let unreadable = || {
        format!(
            "the stored {RECEIPT_KEY_SECRET} is not a receipt signing key; replace it with \
             `axocoatl receipt key --rotate`"
        )
    };
    if !value.starts_with(KEY_VALUE_PREFIX.as_bytes()) {
        return Err(unreadable());
    }
    let stored: StoredKey = serde_json::from_slice(value).map_err(|_| unreadable())?;
    let pkcs8_text = Zeroizing::new(stored.pkcs8);
    if stored.format != KEY_FORMAT || stored.algorithm != "ed25519" {
        return Err(unreadable());
    }
    let pkcs8 = Zeroizing::new(
        base64::engine::general_purpose::STANDARD
            .decode(pkcs8_text.as_bytes())
            .map_err(|_| unreadable())?,
    );
    let pair = Ed25519KeyPair::from_pkcs8(&pkcs8).map_err(|_| unreadable())?;
    Ok(SigningKey { pair })
}

fn generate(lock: &SecretsLock) -> Result<SigningKey, String> {
    let random = ring::rand::SystemRandom::new();
    let document = Ed25519KeyPair::generate_pkcs8(&random)
        .map_err(|_| "the system random source could not make a key".to_string())?;
    let pkcs8 = Zeroizing::new(document.as_ref().to_vec());
    let value = encode(&pkcs8);
    lock.set(RECEIPT_KEY_SECRET, &value)
        .map_err(|error| format!("storing the receipt signing key: {error}"))?;
    let pair = Ed25519KeyPair::from_pkcs8(&pkcs8)
        .map_err(|_| "the new receipt signing key could not be read back".to_string())?;
    Ok(SigningKey { pair })
}

fn locked(data_dir: &Path) -> Result<SecretsLock, String> {
    secret_store::lock_secrets(data_dir, true)
        .map_err(|error| format!("the secret store cannot be opened: {error}"))?
        .ok_or_else(|| "the secrets directory could not be created".to_string())
}

/// The stored key, if there is one.
pub fn load(data_dir: &Path) -> Result<Option<SigningKey>, String> {
    let Some(lock) = secret_store::lock_secrets(data_dir, false)
        .map_err(|error| format!("the secret store cannot be opened: {error}"))?
    else {
        return Ok(None);
    };
    match lock
        .read(RECEIPT_KEY_SECRET)
        .map_err(|error| format!("the receipt signing key cannot be read: {error}"))?
    {
        Some(value) => decode(&value).map(Some),
        None => Ok(None),
    }
}

/// The stored key, made and stored first when there is none (under the
/// secrets lock, so two processes never make two keys). The flag says
/// whether it was made now.
pub fn load_or_create(data_dir: &Path) -> Result<(SigningKey, bool), String> {
    let lock = locked(data_dir)?;
    match lock
        .read(RECEIPT_KEY_SECRET)
        .map_err(|error| format!("the receipt signing key cannot be read: {error}"))?
    {
        Some(value) => decode(&value).map(|key| (key, false)),
        None => generate(&lock).map(|key| (key, true)),
    }
}

/// Replace the key with a new one. Returns it and the public key it
/// replaced, when there was a readable one: receipts that key signed still
/// verify with its trusted-keys line.
pub fn rotate(data_dir: &Path) -> Result<(SigningKey, Option<PublicKey>), String> {
    let lock = locked(data_dir)?;
    let previous = lock
        .read(RECEIPT_KEY_SECRET)
        .ok()
        .flatten()
        .and_then(|value| decode(&value).ok())
        .map(|key| key.public());
    generate(&lock).map(|key| (key, previous))
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn data_root() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        dir
    }

    #[test]
    fn the_key_is_made_once_stored_owner_only_and_rotated() {
        let root = data_root();
        assert!(load(root.path()).unwrap().is_none());
        let (key, created) = load_or_create(root.path()).unwrap();
        assert!(created);
        let (again, created) = load_or_create(root.path()).unwrap();
        assert!(!created);
        assert_eq!(key.public(), again.public());
        assert_eq!(load(root.path()).unwrap().unwrap().public(), key.public());
        let path = secret_store::secret_path(root.path(), RECEIPT_KEY_SECRET);
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        let stored = std::fs::read_to_string(&path).unwrap();
        assert!(stored.starts_with(KEY_VALUE_PREFIX), "the stored format");
        assert!(!stored.contains('\n'));
        // Signing with the stored key verifies with its public key.
        let signature = key.key_pair().sign(b"message");
        assert!(key.public().verifies(b"message", signature.as_ref()));

        let (rotated, previous) = rotate(root.path()).unwrap();
        assert_eq!(previous, Some(key.public()));
        assert_ne!(rotated.public(), key.public());
        assert_eq!(
            load(root.path()).unwrap().unwrap().public(),
            rotated.public()
        );
        // A value that is not a key is refused without being shown.
        secret_store::set_secret(root.path(), RECEIPT_KEY_SECRET, b"not-a-key").unwrap();
        let error = load_or_create(root.path()).err().unwrap();
        assert!(
            error.contains("--rotate") && !error.contains("not-a-key"),
            "{error}"
        );
        let (_, previous) = rotate(root.path()).unwrap();
        assert_eq!(previous, None);
        assert!(load(root.path()).unwrap().is_some());
    }

    /// However a route names it (by the secret's name, by another name for
    /// its file, or through an environment variable), the egress broker
    /// never reads the key as a credential, and the loadout policy never
    /// adds it.
    #[test]
    fn the_broker_never_sends_the_receipt_key() {
        use crate::egress_broker::CredentialSource;
        let root = data_root();
        load_or_create(root.path()).unwrap();
        let path = secret_store::secret_path(root.path(), RECEIPT_KEY_SECRET);
        let stored = std::fs::read_to_string(&path).unwrap();
        for name in [RECEIPT_KEY_SECRET, "innocent-name"] {
            let error = CredentialSource::File(path.clone())
                .read(name, &[])
                .unwrap_err();
            assert!(
                error.reason.contains("receipt signing key"),
                "{name}: {error}"
            );
            assert!(!error.to_string().contains(&stored[30..60]));
        }
        let variable = format!("AXOCOATL_TEST_RECEIPT_KEY_{}", std::process::id());
        std::env::set_var(&variable, &stored);
        let error = CredentialSource::Env(variable.clone())
            .read("from-env", &[])
            .unwrap_err();
        std::env::remove_var(&variable);
        assert!(error.reason.contains("receipt signing key"), "{error}");
        let error = secret_store::credential_source(root.path(), RECEIPT_KEY_SECRET).unwrap_err();
        assert!(error.to_string().contains("never sent"), "{error}");
        let error = secret_store::credential_source_in(
            &root.path().join(secret_store::SECRETS_DIR),
            RECEIPT_KEY_SECRET,
        )
        .unwrap_err();
        assert!(error.to_string().contains("never sent"), "{error}");
    }

    #[test]
    fn the_host_comment_names_this_computer() {
        let comment = host_comment();
        assert!(comment.starts_with("axocoatl@") && comment.len() > "axocoatl@".len());
        assert!(!comment.contains(char::is_whitespace));
    }
}
