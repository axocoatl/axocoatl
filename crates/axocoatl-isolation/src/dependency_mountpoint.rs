//! The host mount point of a Node dependency volume.
//!
//! A Node project's Session (or Way) container mounts its dependency volume
//! over `<repository>/node_modules`, inside the repository it bind-mounts from
//! the host. A mount needs a directory to mount over, and that directory is in
//! the person's own tree. When the repository has no `node_modules`, the OCI
//! runtime makes one itself, as the container's root: under rootless Podman on
//! Linux that is a subordinate uid of the person, with crun's mount-point mode
//! 01755, so the person cannot create files in it and their own `npm install`
//! fails with EACCES until they remove it. On macOS the Podman machine makes it
//! as the person, but it stays behind all the same.
//!
//! So Axocoatl makes the mount point itself, as the person, right before the
//! container is created ([`MountpointLease::acquire`]), and removes it again
//! once no container mounts it, if it is still empty ([`release_unused`]),
//! which every removal of a Session container runs. A repository that had no
//! `node_modules` has none once Axocoatl's containers are gone; one that had
//! one keeps it as it was: this module never makes or removes a directory the
//! repository already had. While a container mounts it, the empty directory
//! belongs to the person, who can install into it on the host (the container
//! keeps seeing its volume) and then keeps it.
//!
//! This process remembers the mount points it made. A container that mounts
//! one carries [`DEPENDENCY_MOUNTPOINT_LABEL`] naming it, so after a daemon
//! that stopped without removing its containers (a crash, `kill -9`), the next
//! one removes the mount point once it removed those containers
//! (`SessionSandbox::remove_owned_containers`).
use std::collections::{HashMap, HashSet};
use std::path::{Component, Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use tokio::process::Command;

use crate::session_sandbox::SessionSandbox;

/// The label naming the dependency mount point Axocoatl made for a container.
pub(crate) const DEPENDENCY_MOUNTPOINT_LABEL: &str = "io.axocoatl.dependency-mountpoint";
/// The directory a Node dependency volume is mounted over.
pub(crate) const NODE_MODULES: &str = "node_modules";
/// One `podman ps` that lists every container's mounts. Container removal
/// waits for it, so it is as short as one removal command's deadline.
const LIST_TIMEOUT: Duration = Duration::from_secs(10);

/// A mount point this process made and has not removed yet.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Made {
    /// Container starts that may mount it and have not finished starting.
    starting: usize,
    /// Changes at each start, so a removal that looked before one began
    /// never removes what that start may mount.
    generation: u64,
}

/// What [`Registry::release`] did with one mount point.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Released {
    /// Removed: it was empty and no container mounted it.
    Removed,
    /// Gone already.
    Absent,
    /// It has content now (the person installed into it), so it is theirs.
    KeptNotEmpty,
    /// It could not be removed, with why.
    Failed(String),
}

#[derive(Debug, Default)]
struct Registry {
    made: HashMap<PathBuf, Made>,
    next_generation: u64,
}

impl Registry {
    fn bump(&mut self) -> u64 {
        self.next_generation += 1;
        self.next_generation
    }

    /// Ready `<working_dir>/node_modules` for a container start. Returns the
    /// path when it is a mount point this process made (now, or for an
    /// earlier container), counted as starting; `None` when the repository has
    /// its own (or it could not be made, and Podman makes it as before).
    fn acquire(&mut self, working_dir: &Path) -> Option<PathBuf> {
        let path = working_dir.join(NODE_MODULES);
        let tracked = self.made.contains_key(&path);
        match std::fs::symlink_metadata(&path) {
            Ok(metadata) if tracked && metadata.file_type().is_dir() => {}
            Ok(_) => {
                // The repository's own, or no longer the directory this
                // process made: never this module's to remove.
                self.made.remove(&path);
                return None;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                // `create_dir` never follows a link at the final component
                // and fails when anything is there by now.
                if let Err(error) = std::fs::create_dir(&path) {
                    tracing::warn!(
                        path = %path.display(),
                        %error,
                        "could not make the Node dependency mount point; Podman makes it"
                    );
                    self.made.remove(&path);
                    return None;
                }
            }
            Err(error) => {
                tracing::warn!(
                    path = %path.display(),
                    %error,
                    "could not inspect the Node dependency mount point; Podman makes it"
                );
                self.made.remove(&path);
                return None;
            }
        }
        let generation = self.bump();
        let entry = self.made.entry(path.clone()).or_default();
        entry.starting += 1;
        entry.generation = generation;
        Some(path)
    }

    /// One start of `path` finished, whether or not its container exists.
    fn finish_start(&mut self, path: &Path) {
        if let Some(entry) = self.made.get_mut(path) {
            entry.starting = entry.starting.saturating_sub(1);
        }
    }

    /// Remember mount points a container's label says Axocoatl made.
    fn adopt(&mut self, paths: impl IntoIterator<Item = PathBuf>) {
        for path in paths {
            if !self.made.contains_key(&path) {
                let generation = self.bump();
                self.made.insert(
                    path,
                    Made {
                        starting: 0,
                        generation,
                    },
                );
            }
        }
    }

    /// The made mount points no start is using now, with their generation.
    fn candidates(&self) -> Vec<(PathBuf, u64)> {
        self.made
            .iter()
            .filter(|(_, made)| made.starting == 0)
            .map(|(path, made)| (path.clone(), made.generation))
            .collect()
    }

    /// Remove each candidate that no start began using since it was listed
    /// and that no container in `mounted` mounts. Removing only an empty
    /// directory (`rmdir`, which never follows a link) keeps anything the
    /// person put there.
    fn release(
        &mut self,
        candidates: Vec<(PathBuf, u64)>,
        mounted: &HashSet<String>,
    ) -> Vec<(PathBuf, Released)> {
        let mut released = Vec::new();
        for (path, generation) in candidates {
            let Some(made) = self.made.get(&path) else {
                continue;
            };
            if made.starting > 0
                || made.generation != generation
                || mounted.contains(path.to_string_lossy().as_ref())
            {
                continue;
            }
            let outcome = match std::fs::remove_dir(&path) {
                Ok(()) => Released::Removed,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Released::Absent,
                Err(error) if error.kind() == std::io::ErrorKind::DirectoryNotEmpty => {
                    Released::KeptNotEmpty
                }
                Err(error) => Released::Failed(error.to_string()),
            };
            // Whatever happened, it is no longer a mount point this process
            // made and may remove: not empty means it is the person's now.
            self.made.remove(&path);
            released.push((path, outcome));
        }
        released
    }
}

fn registry() -> std::sync::MutexGuard<'static, Registry> {
    static REGISTRY: OnceLock<Mutex<Registry>> = OnceLock::new();
    REGISTRY
        .get_or_init(|| Mutex::new(Registry::default()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// One container start's hold on its Node dependency mount point. While it
/// is held, [`release_unused`] leaves the mount point alone; dropping it
/// (the container exists, or the start failed) runs [`release_unused`], so a
/// start that made no container leaves no mount point.
#[derive(Debug)]
pub(crate) struct MountpointLease {
    made: Option<PathBuf>,
}

impl MountpointLease {
    /// Make `<working_dir>/node_modules` as the person when the repository
    /// has none, before a container that mounts its dependency volume there
    /// is created.
    pub(crate) fn acquire(working_dir: &Path) -> Self {
        Self {
            made: registry().acquire(working_dir),
        }
    }

    /// The mount point, when Axocoatl made it: the container is labelled
    /// with it ([`DEPENDENCY_MOUNTPOINT_LABEL`]) when it is a plain path.
    pub(crate) fn label_value(&self) -> Option<&str> {
        self.made
            .as_deref()
            .and_then(Path::to_str)
            .filter(|path| !path.chars().any(char::is_control))
    }
}

impl Drop for MountpointLease {
    fn drop(&mut self) {
        let Some(path) = self.made.take() else {
            return;
        };
        registry().finish_start(&path);
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(release_unused());
        }
    }
}

/// Add `--label` [`DEPENDENCY_MOUNTPOINT_LABEL`]`=<path>` ahead of the image
/// in `podman run` arguments, right after the working-directory option.
pub(crate) fn insert_label(args: &mut Vec<String>, path: &str) {
    let at = args
        .iter()
        .position(|arg| arg == "-w")
        .map(|index| index + 2)
        .unwrap_or(args.len().min(6));
    args.splice(
        at..at,
        [
            "--label".to_string(),
            format!("{DEPENDENCY_MOUNTPOINT_LABEL}={path}"),
        ],
    );
}

/// Whether a labelled path can be a mount point Axocoatl made: an absolute,
/// normal path whose last component is `node_modules`.
fn plausible_mountpoint(path: &Path) -> bool {
    path.is_absolute()
        && path.file_name() == Some(std::ffi::OsStr::new(NODE_MODULES))
        && path
            .components()
            .all(|component| matches!(component, Component::RootDir | Component::Normal(_)))
}

/// The container mount destinations and dependency mount point labels in
/// `podman ps -a --format json` output.
fn parse_listing(stdout: &[u8]) -> Result<(HashSet<String>, Vec<PathBuf>), String> {
    #[derive(serde::Deserialize)]
    struct Listed {
        #[serde(rename = "Mounts", default)]
        mounts: Option<Vec<String>>,
        #[serde(rename = "Labels", default)]
        labels: Option<HashMap<String, String>>,
    }
    let text = std::str::from_utf8(stdout).map_err(|error| error.to_string())?;
    let text = text.trim();
    if text.is_empty() || text == "null" {
        return Ok((HashSet::new(), Vec::new()));
    }
    let listed: Vec<Listed> = serde_json::from_str(text).map_err(|error| error.to_string())?;
    let mut mounted = HashSet::new();
    let mut labelled = Vec::new();
    for container in listed {
        mounted.extend(container.mounts.unwrap_or_default());
        if let Some(path) = container
            .labels
            .as_ref()
            .and_then(|labels| labels.get(DEPENDENCY_MOUNTPOINT_LABEL))
            .map(PathBuf::from)
            .filter(|path| plausible_mountpoint(path))
        {
            labelled.push(path);
        }
    }
    Ok((mounted, labelled))
}

async fn list(filters: &[String]) -> Result<(HashSet<String>, Vec<PathBuf>), String> {
    let mut command = Command::new("podman");
    command.args(["ps", "-a"]);
    command.args(filters);
    command.args(["--format", "json"]);
    let output = SessionSandbox::run_bounded_command(command, LIST_TIMEOUT)
        .await
        .map_err(|error| error.to_string())?;
    if output.timed_out {
        return Err(format!("timed out after {} s", LIST_TIMEOUT.as_secs()));
    }
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).trim().to_string());
    }
    if output.stdout_truncated {
        return Err("the container listing was too long to read".into());
    }
    parse_listing(&output.stdout)
}

/// Remember the mount points the containers of `runtime_authority` are
/// labelled with, before a daemon removes those containers at start. Best
/// effort: a mount point it cannot learn of stays, as it did before.
pub(crate) async fn adopt_labelled(runtime_authority: &str) {
    let filters = [
        "--filter".to_string(),
        format!(
            "label={}={runtime_authority}",
            crate::session_sandbox::RUNTIME_AUTHORITY_LABEL
        ),
    ];
    match list(&filters).await {
        Ok((_, labelled)) if !labelled.is_empty() => registry().adopt(labelled),
        Ok(_) => {}
        Err(error) => tracing::debug!(
            %error,
            "could not list the dependency mount points of this daemon's containers"
        ),
    }
}

/// Remove every Node dependency mount point this process made that no start
/// is using and no container (of any daemon) mounts, if it is still empty.
/// Without one to remove it asks Podman nothing. Best effort: when Podman
/// cannot list its containers, everything stays for a later call.
pub(crate) async fn release_unused() {
    let candidates = registry().candidates();
    if candidates.is_empty() {
        return;
    }
    let mounted = match list(&[]).await {
        Ok((mounted, _)) => mounted,
        Err(error) => {
            tracing::debug!(%error, "kept Node dependency mount points: Podman did not list its containers");
            return;
        }
    };
    for (path, outcome) in registry().release(candidates, &mounted) {
        match outcome {
            Released::Removed => tracing::info!(
                path = %path.display(),
                "removed the empty Node dependency mount point Axocoatl made"
            ),
            Released::Absent => {}
            Released::KeptNotEmpty => tracing::info!(
                path = %path.display(),
                "kept the Node dependency mount point Axocoatl made: it has content now"
            ),
            Released::Failed(error) => tracing::warn!(
                path = %path.display(),
                %error,
                "could not remove the empty Node dependency mount point Axocoatl made"
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mounted(paths: &[&Path]) -> HashSet<String> {
        paths
            .iter()
            .map(|path| path.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn a_missing_mount_point_is_made_as_the_person_and_removed_when_unused() {
        let repo = tempfile::tempdir().unwrap();
        let path = repo.path().join(NODE_MODULES);
        let mut registry = Registry::default();

        assert_eq!(registry.acquire(repo.path()), Some(path.clone()));
        let metadata = std::fs::symlink_metadata(&path).unwrap();
        assert!(metadata.is_dir());
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            // This process's uid, never a container's.
            assert_eq!(metadata.uid(), rustix::process::getuid().as_raw());
            // No sticky bit: not the OCI runtime's 01755 marker.
            assert_eq!(metadata.mode() & 0o1000, 0);
        }
        // Starting: never removed, even with no container listed.
        assert!(registry.candidates().is_empty());
        registry.finish_start(&path);

        // A container mounts it: kept.
        let candidates = registry.candidates();
        assert_eq!(candidates.len(), 1);
        assert!(registry
            .release(candidates, &mounted(&[repo.path(), &path]))
            .is_empty());
        assert!(path.is_dir());

        // No container mounts it any more: removed.
        let candidates = registry.candidates();
        assert_eq!(
            registry.release(candidates, &mounted(&[repo.path()])),
            vec![(path.clone(), Released::Removed)]
        );
        assert!(!path.exists());
        assert!(registry.made.is_empty());
    }

    #[test]
    fn the_repositorys_own_node_modules_is_never_made_or_removed() {
        let repo = tempfile::tempdir().unwrap();
        let path = repo.path().join(NODE_MODULES);
        std::fs::create_dir(&path).unwrap();
        let mut registry = Registry::default();

        assert_eq!(registry.acquire(repo.path()), None);
        assert!(registry.candidates().is_empty());
        assert!(registry.release(Vec::new(), &HashSet::new()).is_empty());
        assert!(path.is_dir());

        // A file or a link there is not this module's either.
        let file_repo = tempfile::tempdir().unwrap();
        std::fs::write(file_repo.path().join(NODE_MODULES), "x").unwrap();
        assert_eq!(registry.acquire(file_repo.path()), None);
        assert!(file_repo.path().join(NODE_MODULES).is_file());
        #[cfg(unix)]
        {
            let link_repo = tempfile::tempdir().unwrap();
            let target = tempfile::tempdir().unwrap();
            std::os::unix::fs::symlink(target.path(), link_repo.path().join(NODE_MODULES)).unwrap();
            assert_eq!(registry.acquire(link_repo.path()), None);
            assert!(target.path().is_dir());
        }
        assert!(registry.made.is_empty());
    }

    #[test]
    fn what_the_person_installed_into_it_stays() {
        let repo = tempfile::tempdir().unwrap();
        let path = repo.path().join(NODE_MODULES);
        let mut registry = Registry::default();
        assert!(registry.acquire(repo.path()).is_some());
        registry.finish_start(&path);
        std::fs::create_dir(path.join("left-pad")).unwrap();

        let candidates = registry.candidates();
        assert_eq!(
            registry.release(candidates, &HashSet::new()),
            vec![(path.clone(), Released::KeptNotEmpty)]
        );
        assert!(path.join("left-pad").is_dir());
        // It is the person's now: a later start does not take it back.
        assert_eq!(registry.acquire(repo.path()), None);
        assert!(registry.made.is_empty());
    }

    #[test]
    fn a_start_that_began_after_the_listing_keeps_the_mount_point() {
        let repo = tempfile::tempdir().unwrap();
        let path = repo.path().join(NODE_MODULES);
        let mut registry = Registry::default();
        assert!(registry.acquire(repo.path()).is_some());
        registry.finish_start(&path);

        // A removal lists the candidates and asks Podman; meanwhile a second
        // Session on the same repository starts and creates its container,
        // which that listing could not see.
        let stale = registry.candidates();
        assert_eq!(registry.acquire(repo.path()), Some(path.clone()));
        registry.finish_start(&path);
        assert!(registry.release(stale, &HashSet::new()).is_empty());
        assert!(path.is_dir());

        // Sequential Sessions share the mount point the first one made: the
        // last container's removal removes it.
        let candidates = registry.candidates();
        assert_eq!(
            registry.release(candidates, &HashSet::new()),
            vec![(path.clone(), Released::Removed)]
        );
        assert!(!path.exists());
    }

    #[test]
    fn a_removed_mount_point_is_made_again_for_the_next_start() {
        let repo = tempfile::tempdir().unwrap();
        let path = repo.path().join(NODE_MODULES);
        let mut registry = Registry::default();
        assert!(registry.acquire(repo.path()).is_some());
        registry.finish_start(&path);
        // The person removed it while it was tracked.
        std::fs::remove_dir(&path).unwrap();
        assert_eq!(registry.acquire(repo.path()), Some(path.clone()));
        assert!(path.is_dir());
        registry.finish_start(&path);
        let candidates = registry.candidates();
        assert_eq!(
            registry.release(candidates, &HashSet::new()),
            vec![(path, Released::Removed)]
        );
    }

    #[test]
    fn a_crashed_daemons_labelled_mount_points_are_adopted_and_removed() {
        let repo = tempfile::tempdir().unwrap();
        let path = repo.path().join(NODE_MODULES);
        std::fs::create_dir(&path).unwrap();
        let listing = serde_json::json!([
            {
                "Names": ["axo-ses-ses-1"],
                "Mounts": [repo.path(), &path],
                "Labels": {
                    "io.axocoatl.runtime-authority": "a".repeat(64),
                    DEPENDENCY_MOUNTPOINT_LABEL: path,
                }
            },
            { "Names": ["other"], "Mounts": null, "Labels": null },
            { "Names": ["bad"], "Labels": { DEPENDENCY_MOUNTPOINT_LABEL: "relative/node_modules" } },
            { "Names": ["bad2"], "Labels": { DEPENDENCY_MOUNTPOINT_LABEL: "/x/../etc" } },
            { "Names": ["bad3"], "Labels": { DEPENDENCY_MOUNTPOINT_LABEL: "/home/person/src" } }
        ]);
        let (in_use, labelled) = parse_listing(listing.to_string().as_bytes()).unwrap();
        assert!(in_use.contains(path.to_string_lossy().as_ref()));
        assert_eq!(labelled, vec![path.clone()]);

        let mut registry = Registry::default();
        registry.adopt(labelled);
        // Kept while that container exists, removed once it is gone.
        let candidates = registry.candidates();
        assert!(registry.release(candidates, &in_use).is_empty());
        let candidates = registry.candidates();
        assert_eq!(
            registry.release(candidates, &HashSet::new()),
            vec![(path.clone(), Released::Removed)]
        );
        assert!(!path.exists());
    }

    #[test]
    fn an_empty_listing_parses() {
        for text in ["", "null", "[]", " []\n"] {
            let (mounted, labelled) = parse_listing(text.as_bytes()).unwrap();
            assert!(mounted.is_empty() && labelled.is_empty(), "{text:?}");
        }
        assert!(parse_listing(b"not json").is_err());
    }

    #[test]
    fn the_label_goes_ahead_of_the_image() {
        let mut args: Vec<String> = [
            "run", "-d", "--name", "c", "-v", "/r:/r:rw", "-w", "/r", "img",
        ]
        .iter()
        .map(|arg| arg.to_string())
        .collect();
        insert_label(&mut args, "/r/node_modules");
        assert_eq!(
            args[8..],
            [
                "--label".to_string(),
                format!("{DEPENDENCY_MOUNTPOINT_LABEL}=/r/node_modules"),
                "img".to_string()
            ]
        );
        let lease = MountpointLease {
            made: Some(PathBuf::from("/r/a\nb/node_modules")),
        };
        assert_eq!(lease.label_value(), None);
    }
}
