//! Axocoatl's secret store: owner-only files under
//! `{data root}/secrets/<name>`, written by `axocoatl secret set <name>`
//! from stdin (never argv) and read by the egress route broker per request
//! (`CredentialSource::File`). A value never enters a container, a record or
//! a log. Owner: workstream `agents`.

use std::path::{Path, PathBuf};

/// Directory under the data root.
pub const SECRETS_DIR: &str = "secrets";

#[derive(Debug, thiserror::Error)]
pub enum SecretStoreError {
    #[error("secret store: not implemented: {0}")]
    NotImplemented(&'static str),
    #[error("secret store: {0}")]
    Invalid(String),
    #[error("secret store: {0}")]
    Io(#[from] std::io::Error),
}

/// The file that holds secret `name` under `data_dir`.
pub fn secret_path(data_dir: &Path, name: &str) -> PathBuf {
    data_dir.join(SECRETS_DIR).join(name)
}

/// Store `value` as secret `name` (0600, atomic replace).
pub fn set_secret(_data_dir: &Path, _name: &str, _value: &[u8]) -> Result<(), SecretStoreError> {
    Err(SecretStoreError::NotImplemented("secret_store::set_secret"))
}

/// Names of stored secrets (never values).
pub fn list_secrets(_data_dir: &Path) -> Result<Vec<String>, SecretStoreError> {
    Err(SecretStoreError::NotImplemented(
        "secret_store::list_secrets",
    ))
}

pub fn remove_secret(_data_dir: &Path, _name: &str) -> Result<bool, SecretStoreError> {
    Err(SecretStoreError::NotImplemented(
        "secret_store::remove_secret",
    ))
}
