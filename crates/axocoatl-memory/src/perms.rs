//! Restrictive permissions for at-rest data.
//!
//! Checkpoints, daily logs, and the memory stores persist conversation content
//! and tool I/O verbatim — anything a tool returned or a user pasted (including
//! secrets) is durably on disk. These helpers keep that data readable only by
//! the owner: directories `0700`, files `0600`. The umbrella is the `0700` data
//! root (see the daemon bootstrap), which stops other local users from even
//! traversing into the tree; the per-file mode is defense-in-depth.
//!
//! On non-Unix platforms these are no-ops — the user-profile directory there is
//! ACL-restricted by default and the `mode` model does not apply.

use std::path::Path;

/// Restrict a directory to owner-only access (`0700`). Best-effort; a failure
/// is logged, not fatal.
pub fn restrict_dir(path: &Path) {
    set_mode(path, 0o700);
}

/// Restrict a file to owner-only read/write (`0600`). Best-effort.
pub fn restrict_file(path: &Path) {
    set_mode(path, 0o600);
}

#[cfg(unix)]
fn set_mode(path: &Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    if let Err(e) = std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)) {
        tracing::debug!(path = %path.display(), %e, "could not restrict permissions");
    }
}

#[cfg(not(unix))]
fn set_mode(_path: &Path, _mode: u32) {}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn restrict_file_sets_0600() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("secret.bin");
        std::fs::write(&f, b"sk-xyz").unwrap();
        restrict_file(&f);
        let mode = std::fs::metadata(&f).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[test]
    fn restrict_dir_sets_0700() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path().join("data");
        std::fs::create_dir_all(&d).unwrap();
        restrict_dir(&d);
        let mode = std::fs::metadata(&d).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700);
    }
}
