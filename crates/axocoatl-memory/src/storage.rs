//! Portable, containment-safe keys for agent-owned files.
//!
//! Public `AgentId` values are logical identities, not path components. Session
//! and attempt actors deliberately add `:` and `#`, while callers outside the
//! config loader can construct any string. Keep the logical id in file contents
//! and derive a stable component for every filesystem boundary.

use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

const MAX_PRESERVED_COMPONENT_BYTES: usize = 64;

/// Return a deterministic, Windows-portable single path component for an id.
///
/// Existing shipped lowercase config ids keep their old paths. Scoped,
/// uppercase, overlong, or otherwise non-portable ids use a collision-resistant
/// digest instead. Limiting preserved values to 64 bytes also makes the
/// `sha256-<hex>` namespace disjoint from raw preserved ids.
pub fn storage_key(id: &str) -> String {
    if is_preserved_component(id) {
        return id.to_string();
    }

    let digest = Sha256::digest(id.as_bytes());
    format!("sha256-{digest:x}")
}

/// The current containment-safe path for an agent-owned resource.
pub fn storage_path(root: impl AsRef<Path>, id: &str) -> PathBuf {
    root.as_ref().join("v1").join(storage_key(id))
}

/// Return a safe pre-1.0 raw component that may be inspected read-only while
/// migrating data. This never admits separators or dot components.
///
/// On Windows, the historical `:`-scoped layout could never have been created
/// natively. Only POSIX builds consult raw legacy names; current writes always
/// use [`storage_key`].
#[cfg(unix)]
pub fn legacy_storage_path(root: impl AsRef<Path>, id: &str) -> Option<PathBuf> {
    legacy_storage_component(id).map(|component| root.as_ref().join(component))
}

#[cfg(not(unix))]
pub fn legacy_storage_path(_root: impl AsRef<Path>, _id: &str) -> Option<PathBuf> {
    None
}

/// A pre-1.0 raw id that is safe to interpolate into a legacy filename.
#[cfg(unix)]
pub fn legacy_storage_component(id: &str) -> Option<&str> {
    is_legacy_posix_component(id).then_some(id)
}

#[cfg(not(unix))]
pub fn legacy_storage_component(_id: &str) -> Option<&str> {
    None
}

fn is_preserved_component(id: &str) -> bool {
    let mut bytes = id.bytes();
    !id.is_empty()
        && id.len() <= MAX_PRESERVED_COMPONENT_BYTES
        && bytes
            .next()
            .is_some_and(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
        && bytes.all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'-' | b'_')
        })
}

#[cfg(unix)]
fn is_legacy_posix_component(id: &str) -> bool {
    !id.is_empty()
        && id != "."
        && id != ".."
        && id.len() <= 255
        && !id
            .bytes()
            .any(|byte| byte == b'/' || byte == b'\\' || byte == 0 || byte.is_ascii_control())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shipped_ids_keep_their_existing_component() {
        for id in ["coder", "review-agent_2", "9-worker"] {
            assert_eq!(storage_key(id), id);
        }
    }

    #[test]
    fn scoped_and_unsafe_ids_are_distinct_portable_components() {
        let scoped = storage_key("ses-123:coder");
        assert!(scoped.starts_with("sha256-"));
        assert_eq!(scoped.len(), 71);
        assert_ne!(scoped, storage_key("ses-123#coder"));
        assert_ne!(storage_key("Coder"), storage_key("coder"));
        assert_ne!(storage_key("a/b"), storage_key("a?b"));

        for id in ["", ".", "..", "../outside", "/tmp/outside", "a\\b"] {
            let key = storage_key(id);
            assert!(key.starts_with("sha256-"));
            assert_eq!(Path::new(&key).components().count(), 1);
        }
    }

    #[cfg(unix)]
    #[test]
    fn legacy_fallback_is_one_bounded_posix_component() {
        let root = Path::new("root");
        assert_eq!(
            legacy_storage_path(root, "ses-123:coder"),
            Some(root.join("ses-123:coder"))
        );
        for id in ["", ".", "..", "../outside", "/tmp/outside", "a\\b"] {
            assert!(legacy_storage_path(root, id).is_none(), "accepted {id:?}");
        }
    }
}
