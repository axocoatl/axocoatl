//! A `PATH` on which no `podman` can be found, on any host.
//!
//! A test that means "Podman is not installed" cannot just leave its fake
//! Podman out and keep `/usr/bin` on `PATH`: Ubuntu installs Podman there
//! (GitHub's ubuntu-24.04 runners have Podman 4.9.3), and the real one is
//! then found and answers. The `PATH` here is one directory holding a link
//! to every program in `/usr/bin` and `/bin` except Podman's own.

use std::path::Path;

/// Fill `directory` (created here) with a link to each program in
/// `/usr/bin` and `/bin` but `podman` and `podman-*`, and return it as a
/// `PATH`.
pub(crate) fn path_without_podman(directory: &Path) -> String {
    std::fs::create_dir(directory).unwrap();
    for system in ["/usr/bin", "/bin"] {
        let Ok(entries) = std::fs::read_dir(system) else {
            continue;
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let text = name.to_string_lossy();
            if text == "podman" || text.starts_with("podman-") {
                continue;
            }
            let link = directory.join(&name);
            // `/bin` is a link to `/usr/bin` on merged-/usr systems: the
            // first one found is kept.
            if link.symlink_metadata().is_err() {
                std::os::unix::fs::symlink(entry.path(), &link).unwrap();
            }
        }
    }
    assert!(
        !directory.join("podman").exists(),
        "{}",
        directory.display()
    );
    directory.display().to_string()
}
