//! A temporary directory Axocoatl accepts as a data root whatever the
//! caller's umask.
//!
//! `tempfile::tempdir()` makes its directory with the process umask: under
//! 002, Ubuntu's login default, it is 0775, and Axocoatl refuses a group- or
//! world-writable directory as a data or authority root. A test whose
//! temporary directory is such a root makes it here, 0700, as the daemon
//! makes its own.
#![allow(dead_code)]

use std::os::unix::fs::PermissionsExt;

/// A new temporary directory, 0700.
pub fn private_tempdir() -> tempfile::TempDir {
    tempfile::Builder::new()
        .permissions(std::fs::Permissions::from_mode(0o700))
        .tempdir()
        .unwrap()
}
