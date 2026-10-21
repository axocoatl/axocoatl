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
//! once no container mounts it, if it is still the empty directory it made
//! ([`release_unused`]), which every removal of a Session container runs. A
//! repository that had no `node_modules` has none once Axocoatl's containers
//! are gone; one that had one keeps it as it was: this module never makes or
//! removes a directory the repository already had. While a container mounts
//! it, the empty directory belongs to the person, who can install into it on
//! the host (the container keeps seeing its volume) and then keeps it.
//!
//! "The directory it made" is the [`Identity`] (device, inode and, where the
//! filesystem records one, birth time) of the directory `mkdir` made, read
//! without following a link. Whatever else is at that path later (the
//! person's own `node_modules` put in its place, or a link) is left as it is,
//! and nothing is removed through a link: `rmdir` removes only an empty
//! directory and never follows one.
//!
//! "No container mounts it" is what Podman lists: a container that mounts
//! something at that path, or one that bind-mounts it, or a directory above
//! it, somewhere else and mounts something at the same place below that bind
//! (a development container's `node_modules` volume), keeps it. On Linux,
//! removing a directory that another mount namespace has a mount on detaches
//! that mount, so when Podman cannot say, the directory stays for a later
//! removal. Containers of another runtime (Docker) are not in Podman's list.
//!
//! This process remembers the mount points it made. A container that mounts
//! one carries [`DEPENDENCY_MOUNTPOINT_LABEL`] naming it and
//! [`DEPENDENCY_MOUNTPOINT_IDENTITY_LABEL`] recording which directory it was,
//! so after a daemon that stopped without removing its containers (a crash,
//! `kill -9`), the next one learns of the mount point when it removes those
//! containers, by its runtime authority ([`adopt_labelled`]) or by name
//! ([`adopt_labelled_named`]), and removes it with them if it is still that
//! directory (a restart that numbers the device differently keeps it).
use std::collections::{HashMap, HashSet};
use std::ffi::OsStr;
use std::path::{Component, Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use tokio::process::Command;

use crate::session_sandbox::SessionSandbox;

/// The label naming the dependency mount point Axocoatl made for a container.
pub(crate) const DEPENDENCY_MOUNTPOINT_LABEL: &str = "io.axocoatl.dependency-mountpoint";
/// The label recording which directory that mount point was when Axocoatl
/// made it ([`Identity::label`]).
pub(crate) const DEPENDENCY_MOUNTPOINT_IDENTITY_LABEL: &str =
    "io.axocoatl.dependency-mountpoint-identity";
/// The directory a Node dependency volume is mounted over.
pub(crate) const NODE_MODULES: &str = "node_modules";
/// One `podman ps` that lists every container's mounts, or one
/// `podman container inspect`. Container removal waits for it, so it is as
/// short as one removal command's deadline.
const LIST_TIMEOUT: Duration = Duration::from_secs(10);

/// Which directory is at a path: its device and inode, and its birth time
/// when the filesystem records one (an inode number is used again once its
/// directory is gone). Read with `lstat`, so a link is never its target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Identity {
    device: u64,
    inode: u64,
    /// Seconds and nanoseconds since the Unix epoch.
    born: Option<(u64, u32)>,
}

impl Identity {
    /// The identity of the directory at `path` itself: `Ok(None)` when
    /// something else is there (a link, a file).
    fn at(path: &Path) -> std::io::Result<Option<Self>> {
        std::fs::symlink_metadata(path).map(|metadata| Self::of(&metadata))
    }

    #[cfg(unix)]
    fn of(metadata: &std::fs::Metadata) -> Option<Self> {
        use std::os::unix::fs::MetadataExt;
        metadata.file_type().is_dir().then(|| Self {
            device: metadata.dev(),
            inode: metadata.ino(),
            born: metadata
                .created()
                .ok()
                .and_then(|born| born.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|born| (born.as_secs(), born.subsec_nanos())),
        })
    }

    /// Without device and inode numbers no directory is told apart, so
    /// none is made or removed.
    #[cfg(not(unix))]
    fn of(_: &std::fs::Metadata) -> Option<Self> {
        None
    }

    /// `<device>:<inode>:<seconds>.<nanoseconds>`, with `-` for a birth
    /// time the filesystem does not record.
    fn label(&self) -> String {
        let born = match self.born {
            Some((seconds, nanos)) => format!("{seconds}.{nanos:09}"),
            None => "-".to_string(),
        };
        format!("{}:{}:{born}", self.device, self.inode)
    }

    /// The identity [`Identity::label`] wrote, and nothing else.
    fn parse(text: &str) -> Option<Self> {
        fn number<T: std::str::FromStr>(text: &str) -> Option<T> {
            if text.is_empty() || !text.bytes().all(|byte| byte.is_ascii_digit()) {
                return None;
            }
            text.parse().ok()
        }
        let mut fields = text.split(':');
        let device = number(fields.next()?)?;
        let inode = number(fields.next()?)?;
        let born = match fields.next()? {
            "-" => None,
            born => {
                let (seconds, nanos) = born.split_once('.')?;
                if nanos.len() != 9 {
                    return None;
                }
                Some((number(seconds)?, number(nanos)?))
            }
        };
        if fields.next().is_some() {
            return None;
        }
        Some(Self {
            device,
            inode,
            born,
        })
    }
}

/// A mount point this process made (or a daemon before it) and has not
/// removed yet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Made {
    /// The directory `mkdir` made: only that directory is ever removed.
    identity: Identity,
    /// Container starts that may mount it and have not finished starting.
    starting: usize,
    /// Changes at each start, so a removal that looked before one began
    /// never removes what that start may mount.
    generation: u64,
}

/// What [`Registry::release`] did with one mount point.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Released {
    /// Removed: it was the empty directory Axocoatl made, and no container
    /// mounted it.
    Removed,
    /// Gone already.
    Absent,
    /// It has content now (the person installed into it), so it is theirs.
    KeptNotEmpty,
    /// Another directory, a link or a file is at its path now (the person
    /// put their own there): left as it is.
    KeptNotMade,
    /// It could not be removed, with why.
    Failed(String),
}

#[derive(Debug, Default)]
struct Registry {
    made: HashMap<PathBuf, Made>,
    next_generation: u64,
}

/// `mkdir` the mount point and read which directory that made. `None` when
/// it could not be made (Podman makes it then, as before) or told apart: a
/// directory not identified is never tracked, so never removed.
fn make(path: &Path) -> Option<Identity> {
    if !cfg!(unix) {
        return None;
    }
    // `create_dir` never follows a link at the final component and fails
    // when anything is there by now.
    if let Err(error) = std::fs::create_dir(path) {
        tracing::warn!(
            path = %path.display(),
            %error,
            "could not make the Node dependency mount point; Podman makes it"
        );
        return None;
    }
    match Identity::at(path) {
        Ok(Some(identity)) => Some(identity),
        Ok(None) => {
            tracing::warn!(
                path = %path.display(),
                "the Node dependency mount point was replaced as it was made; it stays"
            );
            None
        }
        Err(error) => {
            tracing::warn!(
                path = %path.display(),
                %error,
                "could not read the Node dependency mount point just made; it stays"
            );
            None
        }
    }
}

impl Registry {
    fn bump(&mut self) -> u64 {
        self.next_generation += 1;
        self.next_generation
    }

    /// Ready `<working_dir>/node_modules` for a container start. Returns the
    /// path and its identity when it is a mount point this process made
    /// (now, or for an earlier container), counted as starting; `None` when
    /// the repository has its own (or it could not be made, and Podman
    /// makes it as before).
    fn acquire(&mut self, working_dir: &Path) -> Option<(PathBuf, Identity)> {
        let path = working_dir.join(NODE_MODULES);
        let identity = match Identity::at(&path) {
            Ok(found) => match self.made.get(&path) {
                // Still the directory made for an earlier container.
                Some(made) if found == Some(made.identity) => made.identity,
                // The repository's own, or no longer the directory made
                // here: never this module's to remove.
                _ => {
                    self.made.remove(&path);
                    return None;
                }
            },
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => match make(&path) {
                Some(identity) => identity,
                None => {
                    self.made.remove(&path);
                    return None;
                }
            },
            Err(error) => {
                tracing::warn!(
                    path = %path.display(),
                    %error,
                    "could not inspect the Node dependency mount point; Podman makes it"
                );
                self.made.remove(&path);
                return None;
            }
        };
        let generation = self.bump();
        let entry = self.made.entry(path.clone()).or_insert(Made {
            identity,
            starting: 0,
            generation,
        });
        entry.identity = identity;
        entry.starting += 1;
        entry.generation = generation;
        Some((path, identity))
    }

    /// One start of `path` finished, whether or not its container exists.
    fn finish_start(&mut self, path: &Path) {
        if let Some(entry) = self.made.get_mut(path) {
            entry.starting = entry.starting.saturating_sub(1);
        }
    }

    /// Remember mount points a container's labels say Axocoatl made, with
    /// which directories they were.
    fn adopt(&mut self, made: impl IntoIterator<Item = (PathBuf, Identity)>) {
        for (path, identity) in made {
            if !self.made.contains_key(&path) {
                let generation = self.bump();
                self.made.insert(
                    path,
                    Made {
                        identity,
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

    /// Remove each candidate that no start began using since it was listed,
    /// that is not `in_use`, and that is still the directory made for it.
    /// Removing only that directory, and only when it is empty (`rmdir`,
    /// which never follows a link), keeps anything the person put there.
    fn release(
        &mut self,
        candidates: Vec<(PathBuf, u64)>,
        in_use: &HashSet<PathBuf>,
    ) -> Vec<(PathBuf, Released)> {
        let mut released = Vec::new();
        for (path, generation) in candidates {
            let Some(made) = self.made.get(&path) else {
                continue;
            };
            if made.starting > 0 || made.generation != generation || in_use.contains(&path) {
                continue;
            }
            let outcome = match Identity::at(&path) {
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Released::Absent,
                Err(error) => Released::Failed(error.to_string()),
                Ok(found) if found != Some(made.identity) => Released::KeptNotMade,
                Ok(_) => match std::fs::remove_dir(&path) {
                    Ok(()) => Released::Removed,
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => Released::Absent,
                    Err(error) if error.kind() == std::io::ErrorKind::DirectoryNotEmpty => {
                        Released::KeptNotEmpty
                    }
                    Err(error) => Released::Failed(error.to_string()),
                },
            };
            // Whatever happened, it is no longer a mount point this process
            // made and may remove: not empty means it is the person's now,
            // and another directory at its path never was this module's.
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
    made: Option<(PathBuf, Identity)>,
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

    /// When Axocoatl made the mount point and its path is plain, the
    /// container's labels for it: [`DEPENDENCY_MOUNTPOINT_LABEL`] naming it
    /// and [`DEPENDENCY_MOUNTPOINT_IDENTITY_LABEL`] recording which
    /// directory it is, as `key=value`.
    pub(crate) fn labels(&self) -> Option<[String; 2]> {
        let (path, identity) = self.made.as_ref()?;
        let path = path
            .to_str()
            .filter(|path| !path.chars().any(char::is_control))?;
        Some([
            format!("{DEPENDENCY_MOUNTPOINT_LABEL}={path}"),
            format!(
                "{DEPENDENCY_MOUNTPOINT_IDENTITY_LABEL}={}",
                identity.label()
            ),
        ])
    }
}

impl Drop for MountpointLease {
    fn drop(&mut self) {
        let Some((path, _)) = self.made.take() else {
            return;
        };
        registry().finish_start(&path);
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            #[cfg(test)]
            let ticket = lease_releases::spawned();
            runtime.spawn(async move {
                release_unused().await;
                #[cfg(test)]
                lease_releases::finished(ticket);
            });
        }
    }
}

/// Which [`release_unused`] runs that a dropped [`MountpointLease`] spawned
/// have finished, so a live test can look at the mount point after the one
/// that runs once its container exists.
#[cfg(test)]
pub(crate) mod lease_releases {
    use std::collections::BTreeSet;
    use std::sync::{Mutex, MutexGuard};
    use std::time::Duration;

    /// The last ticket handed out, and those whose release has not finished.
    static RELEASES: Mutex<(u64, BTreeSet<u64>)> = Mutex::new((0, BTreeSet::new()));

    fn releases() -> MutexGuard<'static, (u64, BTreeSet<u64>)> {
        RELEASES
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    pub(super) fn spawned() -> u64 {
        let mut releases = releases();
        releases.0 += 1;
        let ticket = releases.0;
        releases.1.insert(ticket);
        ticket
    }

    pub(super) fn finished(ticket: u64) {
        releases().1.remove(&ticket);
    }

    /// Wait until every release a lease spawned before this call finished.
    pub(crate) async fn settled() {
        let last = releases().0;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
        while releases().1.range(..=last).next().is_some() {
            assert!(
                tokio::time::Instant::now() < deadline,
                "a dropped lease's release did not finish within 60 s"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
}

/// Add `--label <key=value>` for each of `labels` ahead of the image in
/// `podman run` arguments, right after the working-directory option.
pub(crate) fn insert_labels(args: &mut Vec<String>, labels: &[String]) {
    let at = args
        .iter()
        .position(|arg| arg == "-w")
        .map(|index| index + 2)
        .unwrap_or(args.len().min(6));
    args.splice(
        at..at,
        labels
            .iter()
            .flat_map(|label| ["--label".to_string(), label.clone()]),
    );
}

/// Whether a labelled path can be a mount point Axocoatl made: an absolute,
/// normal path whose last component is `node_modules`.
fn plausible_mountpoint(path: &Path) -> bool {
    path.is_absolute()
        && path.file_name() == Some(OsStr::new(NODE_MODULES))
        && path
            .components()
            .all(|component| matches!(component, Component::RootDir | Component::Normal(_)))
}

/// Whether `id` is a container id Podman prints: hexadecimal, so never
/// taken for an option.
fn plausible_container_id(id: &str) -> bool {
    (12..=64).contains(&id.len()) && id.bytes().all(|byte| byte.is_ascii_hexdigit())
}

/// One container in `podman ps -a --format json` output.
#[derive(Debug, Default, serde::Deserialize)]
struct Listed {
    #[serde(rename = "Id", default)]
    id: Option<String>,
    #[serde(rename = "Names", default)]
    names: Option<Vec<String>>,
    /// Where its volumes and bind mounts are mounted in it.
    #[serde(rename = "Mounts", default)]
    mounts: Option<Vec<String>>,
    #[serde(rename = "Labels", default)]
    labels: Option<HashMap<String, String>>,
}

impl Listed {
    /// The mount point its labels say Axocoatl made for it, and which
    /// directory that was, when both are plausible.
    fn labelled_mountpoint(&self) -> Option<(PathBuf, Identity)> {
        let labels = self.labels.as_ref()?;
        let path = PathBuf::from(labels.get(DEPENDENCY_MOUNTPOINT_LABEL)?);
        let identity = Identity::parse(labels.get(DEPENDENCY_MOUNTPOINT_IDENTITY_LABEL)?)?;
        plausible_mountpoint(&path).then_some((path, identity))
    }

    fn named_any_of(&self, names: &[String]) -> bool {
        self.names.iter().flatten().any(|name| {
            names
                .iter()
                .any(|wanted| name.trim_start_matches('/') == wanted)
        })
    }
}

/// One container in `podman container inspect` output.
#[derive(Debug, Default, serde::Deserialize)]
struct Inspected {
    #[serde(rename = "Mounts", default)]
    mounts: Option<Vec<InspectedMount>>,
}

#[derive(Debug, Default, serde::Deserialize)]
struct InspectedMount {
    #[serde(rename = "Type", default)]
    kind: Option<String>,
    #[serde(rename = "Source", default)]
    source: Option<String>,
    #[serde(rename = "Destination", default)]
    destination: Option<String>,
}

impl Inspected {
    /// Whether this container sees the host directory `path` through a
    /// bind mount (of it, or of a directory above it) and has something
    /// mounted there: a development container that bind-mounts the
    /// repository at `/workspaces/x` with a volume at
    /// `/workspaces/x/node_modules`, or one that bind-mounts `path` itself.
    fn mounts_over(&self, path: &Path) -> bool {
        let mounts: Vec<&InspectedMount> = self.mounts.iter().flatten().collect();
        mounts
            .iter()
            .filter(|mount| mount.kind.as_deref() == Some("bind"))
            .any(|bind| {
                let (Some(source), Some(destination)) =
                    (bind.source.as_deref(), bind.destination.as_deref())
                else {
                    return false;
                };
                // `path` is canonical; the source is spelled as Podman was
                // given it.
                let source =
                    std::fs::canonicalize(source).unwrap_or_else(|_| PathBuf::from(source));
                let Ok(below) = path.strip_prefix(&source) else {
                    return false;
                };
                if below.as_os_str().is_empty() {
                    return true;
                }
                let seen = Path::new(destination).join(below);
                mounts.iter().any(|mount| {
                    mount.destination.as_deref().map(Path::new) == Some(seen.as_path())
                })
            })
    }
}

fn parse_json_array<T: serde::de::DeserializeOwned>(stdout: &[u8]) -> Result<Vec<T>, String> {
    let text = std::str::from_utf8(stdout).map_err(|error| error.to_string())?;
    let text = text.trim();
    if text.is_empty() || text == "null" {
        return Ok(Vec::new());
    }
    serde_json::from_str(text).map_err(|error| error.to_string())
}

/// Every mount destination of the listed containers.
fn mounted(listed: &[Listed]) -> HashSet<PathBuf> {
    listed
        .iter()
        .flat_map(|container| container.mounts.iter().flatten())
        .map(PathBuf::from)
        .collect()
}

/// The containers to inspect before removing any of `made`: those that
/// mount something named `node_modules` other than one of them, which may
/// be one of them seen through a bind mount elsewhere. `None` when one of
/// them has no id Podman could be asked about.
fn to_inspect(listed: &[Listed], made: &HashSet<&Path>) -> Option<Vec<String>> {
    let mut ids = Vec::new();
    for container in listed {
        let another = container
            .mounts
            .iter()
            .flatten()
            .map(Path::new)
            .any(|destination| {
                destination.file_name() == Some(OsStr::new(NODE_MODULES))
                    && !made.contains(destination)
            });
        if another {
            let id = container
                .id
                .as_deref()
                .filter(|id| plausible_container_id(id))?;
            ids.push(id.to_string());
        }
    }
    Some(ids)
}

/// Add to `in_use` each of `unmounted` that an `inspected` container mounts
/// something over; when Podman could not say, every one of them.
fn conclude(
    in_use: &mut HashSet<PathBuf>,
    unmounted: Vec<PathBuf>,
    inspected: Result<Vec<Inspected>, String>,
) {
    match inspected {
        Ok(containers) => in_use.extend(unmounted.into_iter().filter(|path| {
            containers
                .iter()
                .any(|container| container.mounts_over(path))
        })),
        Err(error) => {
            tracing::debug!(
                %error,
                "kept Node dependency mount points: Podman did not say which containers mount them"
            );
            in_use.extend(unmounted);
        }
    }
}

/// The mount points among `candidates` that a listed container mounts
/// something over, as far as Podman says; one it cannot say about counts.
async fn in_use(listed: &[Listed], candidates: &[(PathBuf, u64)]) -> HashSet<PathBuf> {
    let mut in_use = mounted(listed);
    let unmounted: Vec<PathBuf> = candidates
        .iter()
        .map(|(path, _)| path)
        .filter(|path| !in_use.contains(*path))
        .cloned()
        .collect();
    if unmounted.is_empty() {
        return in_use;
    }
    let made: HashSet<&Path> = candidates.iter().map(|(path, _)| path.as_path()).collect();
    let inspected = match to_inspect(listed, &made) {
        Some(ids) if ids.is_empty() => return in_use,
        Some(ids) => inspect(&ids).await,
        None => Err("a container that mounts a node_modules has no id".to_string()),
    };
    conclude(&mut in_use, unmounted, inspected);
    in_use
}

/// One bounded `podman` command whose output is a JSON array.
async fn podman_json<T: serde::de::DeserializeOwned>(args: &[String]) -> Result<Vec<T>, String> {
    let mut command = Command::new("podman");
    command.args(args);
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
        return Err("Podman's output was too long to read".into());
    }
    parse_json_array(&output.stdout)
}

async fn list(filters: &[String]) -> Result<Vec<Listed>, String> {
    let mut args = vec!["ps".to_string(), "-a".to_string()];
    args.extend_from_slice(filters);
    args.extend(["--format".to_string(), "json".to_string()]);
    podman_json(&args).await
}

/// `podman container inspect` of the containers `ids` (checked
/// [`plausible_container_id`]s).
async fn inspect(ids: &[String]) -> Result<Vec<Inspected>, String> {
    let mut args = vec![
        "container".to_string(),
        "inspect".to_string(),
        "--format".to_string(),
        "json".to_string(),
    ];
    args.extend_from_slice(ids);
    podman_json(&args).await
}

fn adopt_from(listed: &[Listed], keep: impl Fn(&Listed) -> bool) {
    let made: Vec<(PathBuf, Identity)> = listed
        .iter()
        .filter(|container| keep(container))
        .filter_map(Listed::labelled_mountpoint)
        .collect();
    if !made.is_empty() {
        registry().adopt(made);
    }
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
        Ok(listed) => adopt_from(&listed, |_| true),
        Err(error) => tracing::debug!(
            %error,
            "could not list the dependency mount points of this daemon's containers"
        ),
    }
}

/// Remember the mount points the containers named exactly `containers` are
/// labelled with, before they are removed by name: after a restart, this
/// process did not make them, but a daemon before it did. Best effort.
pub(crate) async fn adopt_labelled_named(containers: &[String]) {
    if containers.is_empty() {
        return;
    }
    let filters: Vec<String> = containers
        .iter()
        .flat_map(|name| ["--filter".to_string(), format!("name={name}")])
        .collect();
    match list(&filters).await {
        Ok(listed) => adopt_from(&listed, |container| container.named_any_of(containers)),
        Err(error) => tracing::debug!(
            %error,
            "could not list the dependency mount points of the containers being removed"
        ),
    }
}

/// Remove every Node dependency mount point this process made that no start
/// is using and no container (of any daemon, or the person's own) mounts
/// anything over, if it is still the empty directory made for it. Without
/// one to remove it asks Podman nothing. Best effort: when Podman cannot
/// list its containers, or say what one mounts, everything it could concern
/// stays for a later call.
pub(crate) async fn release_unused() {
    let candidates = registry().candidates();
    if candidates.is_empty() {
        return;
    }
    let in_use = match list(&[]).await {
        Ok(listed) => in_use(&listed, &candidates).await,
        Err(error) => {
            tracing::debug!(%error, "kept Node dependency mount points: Podman did not list its containers");
            return;
        }
    };
    for (path, outcome) in registry().release(candidates, &in_use) {
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
            Released::KeptNotMade => tracing::info!(
                path = %path.display(),
                "kept node_modules: it is not the directory Axocoatl made any more"
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

    fn parse_listing(stdout: &[u8]) -> Result<Vec<Listed>, String> {
        parse_json_array(stdout)
    }

    fn mounting(paths: &[&Path]) -> HashSet<PathBuf> {
        paths.iter().map(|path| path.to_path_buf()).collect()
    }

    /// Acquire and finish one start, as a container start does.
    fn made_for_a_container(registry: &mut Registry, repo: &Path) -> PathBuf {
        let (path, _) = registry.acquire(repo).expect("made");
        registry.finish_start(&path);
        path
    }

    #[test]
    fn a_missing_mount_point_is_made_as_the_person_and_removed_when_unused() {
        let repo = tempfile::tempdir().unwrap();
        let path = repo.path().join(NODE_MODULES);
        let mut registry = Registry::default();

        let (acquired, identity) = registry.acquire(repo.path()).unwrap();
        assert_eq!(acquired, path);
        let metadata = std::fs::symlink_metadata(&path).unwrap();
        assert!(metadata.is_dir());
        assert_eq!(Identity::of(&metadata), Some(identity));
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
            .release(candidates, &mounting(&[repo.path(), &path]))
            .is_empty());
        assert!(path.is_dir());

        // No container mounts it any more: removed.
        let candidates = registry.candidates();
        assert_eq!(
            registry.release(candidates, &mounting(&[repo.path()])),
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
        let mut registry = Registry::default();
        let path = made_for_a_container(&mut registry, repo.path());
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

    /// The person moves the directory Axocoatl made aside and puts their
    /// own empty `node_modules` in its place: theirs is not the directory
    /// made, so it stays, and so does the one moved aside.
    #[test]
    fn another_empty_directory_in_its_place_is_never_removed() {
        let repo = tempfile::tempdir().unwrap();
        let mut registry = Registry::default();
        let path = made_for_a_container(&mut registry, repo.path());
        let aside = repo.path().join("node_modules.made");
        std::fs::rename(&path, &aside).unwrap();
        std::fs::create_dir(&path).unwrap();

        let candidates = registry.candidates();
        assert_eq!(
            registry.release(candidates, &HashSet::new()),
            vec![(path.clone(), Released::KeptNotMade)]
        );
        assert!(path.is_dir());
        assert!(aside.is_dir());
        assert!(registry.made.is_empty());
        // Nor does a later start take it as its own.
        assert_eq!(registry.acquire(repo.path()), None);
        assert!(registry.made.is_empty());
    }

    /// A link in its place, even to the very directory Axocoatl made, is
    /// neither followed nor removed.
    #[cfg(unix)]
    #[test]
    fn a_link_in_its_place_is_never_followed_or_removed() {
        let repo = tempfile::tempdir().unwrap();
        let mut registry = Registry::default();
        let path = made_for_a_container(&mut registry, repo.path());
        let aside = repo.path().join("node_modules.made");
        std::fs::rename(&path, &aside).unwrap();
        std::os::unix::fs::symlink(&aside, &path).unwrap();

        let candidates = registry.candidates();
        assert_eq!(
            registry.release(candidates, &HashSet::new()),
            vec![(path.clone(), Released::KeptNotMade)]
        );
        assert!(std::fs::symlink_metadata(&path)
            .unwrap()
            .file_type()
            .is_symlink());
        assert!(aside.is_dir());
        assert!(registry.made.is_empty());
    }

    /// A directory the person filled in its place is kept for both reasons.
    #[test]
    fn a_non_empty_directory_in_its_place_is_never_removed() {
        let repo = tempfile::tempdir().unwrap();
        let mut registry = Registry::default();
        let path = made_for_a_container(&mut registry, repo.path());
        std::fs::remove_dir(&path).unwrap();
        std::fs::create_dir(&path).unwrap();
        std::fs::write(path.join("package.json"), "{}").unwrap();

        let candidates = registry.candidates();
        let released = registry.release(candidates, &HashSet::new());
        assert_eq!(released.len(), 1);
        assert!(
            matches!(
                released[0].1,
                Released::KeptNotMade | Released::KeptNotEmpty
            ),
            "{released:?}"
        );
        assert!(path.join("package.json").is_file());
    }

    #[test]
    fn a_start_that_began_after_the_listing_keeps_the_mount_point() {
        let repo = tempfile::tempdir().unwrap();
        let mut registry = Registry::default();
        let path = made_for_a_container(&mut registry, repo.path());

        // A removal lists the candidates and asks Podman; meanwhile a second
        // Session on the same repository starts and creates its container,
        // which that listing could not see.
        let stale = registry.candidates();
        assert_eq!(made_for_a_container(&mut registry, repo.path()), path);
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
        let mut registry = Registry::default();
        let path = made_for_a_container(&mut registry, repo.path());
        // The person removed it while it was tracked.
        std::fs::remove_dir(&path).unwrap();
        assert_eq!(made_for_a_container(&mut registry, repo.path()), path);
        assert!(path.is_dir());
        let candidates = registry.candidates();
        assert_eq!(
            registry.release(candidates, &HashSet::new()),
            vec![(path, Released::Removed)]
        );
    }

    #[test]
    fn the_identity_label_reads_back_and_nothing_else_does() {
        for identity in [
            Identity {
                device: 16_777_234,
                inode: 98_765,
                born: Some((1_760_000_000, 7)),
            },
            Identity {
                device: 2049,
                inode: 1,
                born: None,
            },
        ] {
            assert_eq!(Identity::parse(&identity.label()), Some(identity));
        }
        assert_eq!(
            Identity {
                device: 1,
                inode: 2,
                born: Some((3, 4))
            }
            .label(),
            "1:2:3.000000004"
        );
        for bad in [
            "",
            "1:2",
            "1:2:",
            "1:2:3",
            "1:2:3.4",
            "1:2:3.0000000004",
            "1:2:-:4",
            "+1:2:-",
            "1:-2:-",
            "a:2:-",
            "1:2:3.00000000x",
            "99999999999999999999:2:-",
        ] {
            assert_eq!(Identity::parse(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn a_crashed_daemons_labelled_mount_points_are_adopted_and_removed() {
        let repo = tempfile::tempdir().unwrap();
        let path = repo.path().join(NODE_MODULES);
        std::fs::create_dir(&path).unwrap();
        let identity = Identity::at(&path).unwrap().unwrap();
        // Another repository whose node_modules was replaced since.
        let replaced_repo = tempfile::tempdir().unwrap();
        let replaced = replaced_repo.path().join(NODE_MODULES);
        std::fs::create_dir(&replaced).unwrap();
        let replaced_identity = Identity::at(&replaced).unwrap().unwrap();
        std::fs::rename(&replaced, replaced_repo.path().join("aside")).unwrap();
        std::fs::create_dir(&replaced).unwrap();
        let listing = serde_json::json!([
            {
                "Id": "a".repeat(64),
                "Names": ["axo-ses-ses-1"],
                "Mounts": [repo.path(), &path],
                "Labels": {
                    "io.axocoatl.runtime-authority": "a".repeat(64),
                    DEPENDENCY_MOUNTPOINT_LABEL: path,
                    DEPENDENCY_MOUNTPOINT_IDENTITY_LABEL: identity.label(),
                }
            },
            {
                "Names": ["axo-ses-ses-2"],
                "Labels": {
                    DEPENDENCY_MOUNTPOINT_LABEL: replaced,
                    DEPENDENCY_MOUNTPOINT_IDENTITY_LABEL: replaced_identity.label(),
                }
            },
            { "Names": ["other"], "Mounts": null, "Labels": null },
            { "Names": ["no-identity"], "Labels": { DEPENDENCY_MOUNTPOINT_LABEL: "/r/node_modules" } },
            { "Names": ["bad-identity"], "Labels": {
                DEPENDENCY_MOUNTPOINT_LABEL: "/r/node_modules",
                DEPENDENCY_MOUNTPOINT_IDENTITY_LABEL: "1:2"
            } },
            { "Names": ["bad"], "Labels": {
                DEPENDENCY_MOUNTPOINT_LABEL: "relative/node_modules",
                DEPENDENCY_MOUNTPOINT_IDENTITY_LABEL: "1:2:-"
            } },
            { "Names": ["bad2"], "Labels": {
                DEPENDENCY_MOUNTPOINT_LABEL: "/x/../etc",
                DEPENDENCY_MOUNTPOINT_IDENTITY_LABEL: "1:2:-"
            } },
            { "Names": ["bad3"], "Labels": {
                DEPENDENCY_MOUNTPOINT_LABEL: "/home/person/src",
                DEPENDENCY_MOUNTPOINT_IDENTITY_LABEL: "1:2:-"
            } }
        ]);
        let listed = parse_listing(listing.to_string().as_bytes()).unwrap();
        let in_use = mounted(&listed);
        assert!(in_use.contains(&path));
        let labelled: Vec<(PathBuf, Identity)> = listed
            .iter()
            .filter_map(Listed::labelled_mountpoint)
            .collect();
        assert_eq!(
            labelled,
            vec![
                (path.clone(), identity),
                (replaced.clone(), replaced_identity)
            ]
        );
        // Only the containers being removed, by exact name.
        assert!(listed[0].named_any_of(&["axo-ses-ses-1".to_string()]));
        assert!(!listed[0].named_any_of(&["axo-ses-ses".to_string()]));
        assert!(Listed {
            names: Some(vec!["/axo-ses-ses-1".into()]),
            ..Listed::default()
        }
        .named_any_of(&["axo-ses-ses-1".to_string()]));

        let mut registry = Registry::default();
        registry.adopt(labelled);
        // Kept while that container exists, removed once it is gone; the
        // replaced one is the person's.
        let candidates = registry.candidates();
        assert_eq!(
            registry.release(candidates, &in_use),
            vec![(replaced.clone(), Released::KeptNotMade)]
        );
        let candidates = registry.candidates();
        assert_eq!(
            registry.release(candidates, &HashSet::new()),
            vec![(path.clone(), Released::Removed)]
        );
        assert!(!path.exists());
        assert!(replaced.is_dir());
    }

    /// A development container that bind-mounts the repository elsewhere
    /// and mounts its own `node_modules` volume there has a mount on the
    /// directory Axocoatl made, though no mount destination is its path:
    /// it stays while that container exists.
    #[test]
    fn a_container_that_mounts_over_it_through_a_bind_mount_keeps_it() {
        let repo = tempfile::tempdir().unwrap();
        let repo_path = std::fs::canonicalize(repo.path()).unwrap();
        let path = repo_path.join(NODE_MODULES);
        let other = tempfile::tempdir().unwrap();
        let other_path = std::fs::canonicalize(other.path())
            .unwrap()
            .join(NODE_MODULES);
        let made: HashSet<&Path> = [path.as_path(), other_path.as_path()].into();
        let devcontainer = "d".repeat(64);
        let listing = serde_json::json!([
            // Axocoatl's own: its mount is the path itself.
            { "Id": "a".repeat(64), "Mounts": [&repo_path, &path] },
            // The development container.
            { "Id": devcontainer, "Mounts": ["/workspaces/x", "/workspaces/x/node_modules"] },
            // Mounts nothing named node_modules: never inspected.
            { "Id": "c".repeat(64), "Mounts": ["/workspaces/x"] },
            { "Id": "e".repeat(64), "Mounts": null }
        ]);
        let listed = parse_listing(listing.to_string().as_bytes()).unwrap();
        assert_eq!(to_inspect(&listed, &made), Some(vec![devcontainer.clone()]));

        let inspected: Vec<Inspected> = parse_json_array(
            serde_json::json!([{
                "Id": devcontainer,
                "Mounts": [
                    { "Type": "bind", "Source": repo.path(), "Destination": "/workspaces/x" },
                    { "Type": "volume", "Name": "x-node-modules",
                      "Source": "/home/person/.local/share/containers/storage/volumes/x-node-modules/_data",
                      "Destination": "/workspaces/x/node_modules" }
                ]
            }])
            .to_string()
            .as_bytes(),
        )
        .unwrap();
        assert!(inspected[0].mounts_over(&path));
        assert!(!inspected[0].mounts_over(&other_path));

        // One that binds the repository's parent sees it one level down.
        let parent = Inspected {
            mounts: Some(vec![
                InspectedMount {
                    kind: Some("bind".into()),
                    source: Some(repo_path.parent().unwrap().display().to_string()),
                    destination: Some("/src".into()),
                },
                InspectedMount {
                    kind: Some("volume".into()),
                    source: None,
                    destination: Some(
                        Path::new("/src")
                            .join(repo_path.file_name().unwrap())
                            .join(NODE_MODULES)
                            .display()
                            .to_string(),
                    ),
                },
            ]),
        };
        assert!(parent.mounts_over(&path));
        // One that bind-mounts the directory itself, wherever.
        let itself = Inspected {
            mounts: Some(vec![InspectedMount {
                kind: Some("bind".into()),
                source: Some(path.display().to_string()),
                destination: Some("/deps".into()),
            }]),
        };
        assert!(itself.mounts_over(&path));
        // A bind of the repository with nothing mounted below it does not.
        let plain = Inspected {
            mounts: Some(vec![InspectedMount {
                kind: Some("bind".into()),
                source: Some(repo_path.display().to_string()),
                destination: Some("/w".into()),
            }]),
        };
        assert!(!plain.mounts_over(&path));

        let mut in_use = HashSet::new();
        conclude(
            &mut in_use,
            vec![path.clone(), other_path.clone()],
            Ok(inspected),
        );
        assert_eq!(in_use, [path.clone()].into());
        // When Podman cannot say, both stay.
        let mut in_use = HashSet::new();
        conclude(
            &mut in_use,
            vec![path.clone(), other_path.clone()],
            Err("no such container".into()),
        );
        assert_eq!(in_use, [path.clone(), other_path.clone()].into());
        // A container to inspect without a usable id cannot be asked.
        let listed = parse_listing(
            serde_json::json!([{ "Id": "-x", "Mounts": ["/w/node_modules"] }])
                .to_string()
                .as_bytes(),
        )
        .unwrap();
        assert_eq!(to_inspect(&listed, &made), None);
    }

    #[test]
    fn an_empty_listing_parses() {
        for text in ["", "null", "[]", " []\n"] {
            assert!(
                parse_listing(text.as_bytes()).unwrap().is_empty(),
                "{text:?}"
            );
        }
        assert!(parse_listing(b"not json").is_err());
    }

    #[test]
    fn the_labels_go_ahead_of_the_image() {
        let mut args: Vec<String> = [
            "run", "-d", "--name", "c", "-v", "/r:/r:rw", "-w", "/r", "img",
        ]
        .iter()
        .map(|arg| arg.to_string())
        .collect();
        let identity = Identity {
            device: 1,
            inode: 2,
            born: None,
        };
        let lease = MountpointLease {
            made: Some((PathBuf::from("/r/node_modules"), identity)),
        };
        let labels = lease.labels().unwrap();
        insert_labels(&mut args, &labels);
        assert_eq!(
            args[8..],
            [
                "--label".to_string(),
                format!("{DEPENDENCY_MOUNTPOINT_LABEL}=/r/node_modules"),
                "--label".to_string(),
                format!("{DEPENDENCY_MOUNTPOINT_IDENTITY_LABEL}=1:2:-"),
                "img".to_string()
            ]
        );
        let lease = MountpointLease {
            made: Some((PathBuf::from("/r/a\nb/node_modules"), identity)),
        };
        assert_eq!(lease.labels(), None);
    }
}
