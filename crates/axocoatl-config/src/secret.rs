//! A redacting wrapper for credentials held in config.

use serde::{Deserialize, Serialize};

/// A string holding a secret (API key, token). Its `Debug` redacts the value,
/// so a stray `debug!(?config)` or `{:?}` can never leak a credential into
/// logs. `Serialize`/`Deserialize` are transparent so config still round-trips
/// to and from YAML unchanged. Read the value explicitly — and only where you
/// must — via [`SecretString::expose_secret`].
#[derive(Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SecretString(String);

impl SecretString {
    /// Borrow the underlying secret. Call sites are deliberately explicit so a
    /// grep for `expose_secret` surfaces every place a credential is read.
    pub fn expose_secret(&self) -> &str {
        &self.0
    }

    /// Whether the secret is empty (e.g. an unset `${ENV}` that interpolated to
    /// "") — used to skip wiring a provider with no key.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl std::fmt::Debug for SecretString {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.0.is_empty() {
            f.write_str(r#"SecretString("")"#)
        } else {
            f.write_str(r#"SecretString("<redacted>")"#)
        }
    }
}

impl From<String> for SecretString {
    fn from(s: String) -> Self {
        Self(s)
    }
}

impl From<&str> for SecretString {
    fn from(s: &str) -> Self {
        Self(s.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_redacts_the_value() {
        let s = SecretString::from("sk-super-secret-123");
        let shown = format!("{s:?}");
        assert!(!shown.contains("super-secret"), "Debug leaked the secret");
        assert!(shown.contains("<redacted>"));
    }

    #[test]
    fn debug_distinguishes_empty() {
        assert_eq!(
            format!("{:?}", SecretString::default()),
            r#"SecretString("")"#
        );
    }

    #[test]
    fn expose_returns_the_real_value() {
        let s = SecretString::from("sk-abc");
        assert_eq!(s.expose_secret(), "sk-abc");
        assert!(!s.is_empty());
        assert!(SecretString::default().is_empty());
    }

    #[test]
    fn serde_is_transparent() {
        // Deserializes from a bare string and serializes back to one — so YAML
        // config round-trips with no structural change.
        let s: SecretString = serde_json::from_str(r#""sk-xyz""#).unwrap();
        assert_eq!(s.expose_secret(), "sk-xyz");
        assert_eq!(serde_json::to_string(&s).unwrap(), r#""sk-xyz""#);
    }
}
