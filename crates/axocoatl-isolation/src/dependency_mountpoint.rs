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
//! container is created ([`MountpointLease::acquire`]). [`release_unused`]
//! removes it again when no container start of this process is using it, no
//! container Podman lists mounts anything on it, and it is still the empty
//! directory Axocoatl made. It runs when [`SessionSandbox`] removes a
//! container (a Session's or Way's at Close, when `axocoatl dev` stops, at a
//! daemon's start after a crash, a stale one before a start) and at the end
//! of every container start. A container of the person's own that kept the
//! directory going away runs nothing, so the directory goes at this
//! process's next such start or removal after that, and stays when the
//! daemon stops first (this process remembers it, and the Axocoatl
//! containers that recorded it are gone). A
//! repository that had a `node_modules` keeps it as it was: this module never
//! makes or removes a directory the repository already had. While a container
//! mounts it, the empty directory belongs to the person, who can install into
//! it on the host (the container keeps seeing its volume) and then keeps it.
//!
//! "The directory it made": right after `mkdir`, Axocoatl marks the directory
//! with the extended attribute [`MARK_ATTRIBUTE`], a random nonce that this
//! process and the container's labels record. Only a directory that is opened
//! without following a link, still carries that exact nonce and is empty
//! (`rmdir`) is removed. A directory the person made again at that path has no
//! such attribute, even when it got the same inode number (ext4 hands a freed
//! one out again at once, with a birth time that can match to the
//! nanosecond); a link is never followed; a copy that kept its extended
//! attributes (`cp -a`) carries the nonce and counts as the one made. Where
//! the filesystem does not take the attribute (NFS v3, tmpfs before Linux
//! 6.6, some FUSE filesystems), the directory is told apart by its device,
//! inode and, where the filesystem records one, birth time instead, which a
//! directory made again at once in its place can match.
//!
//! "No container mounts anything on it" is what one `podman ps -a` and one
//! `podman container inspect` of every container it lists say. A container
//! keeps it when a mount destination of it is that path; when a mount source
//! of it resolves to the directory (it bind-mounts the directory, wherever);
//! or when a mount source resolves to a directory above it and the container
//! mounts something (a volume, a bind, a tmpfs) at the place below that mount
//! that is the directory (a development container's `node_modules` volume).
//! Removing the directory under such a container breaks its mount: a bind
//! mount of the directory itself is left on a deleted directory nothing can
//! be created in (on Linux and through a Podman machine on macOS), and on
//! Linux the kernel detaches every mount on a removed directory, in every
//! mount namespace. So when Podman cannot say (it fails, times out, prints
//! more than 1 MiB, or lists a container without an id), every candidate
//! stays for a later call. Podman lists only the containers of the
//! connection it uses: containers of another runtime (Docker), of rootful
//! Podman (`sudo podman`) while Axocoatl's is rootless, or of another Podman
//! connection or machine are not seen, and their mounts on the directory
//! break when it is removed.
//!
//! This process remembers the mount points it made. A container that mounts
//! one carries [`DEPENDENCY_MOUNTPOINT_LABEL`] naming it and
//! [`DEPENDENCY_MOUNTPOINT_IDENTITY_LABEL`] recording its nonce (and identity),
//! so after a daemon that stopped without removing its containers (a crash,
//! `kill -9`), the next one learns of the mount point when it removes those
//! containers, by its runtime authority ([`adopt_labelled`]) or by name
//! ([`adopt_labelled_named`]), and removes it with them if it is still that
//! directory.
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
/// made it ([`Mark::label`]).
pub(crate) const DEPENDENCY_MOUNTPOINT_IDENTITY_LABEL: &str =
    "io.axocoatl.dependency-mountpoint-identity";
/// The extended attribute on the directory Axocoatl made: its [`Nonce`] in
/// lowercase hexadecimal. A `user.` name, the namespace Linux lets the
/// directory's owner write; macOS takes the same name.
pub(crate) const MARK_ATTRIBUTE: &str = "user.axocoatl.mountpoint";
/// The directory a Node dependency volume is mounted over.
pub(crate) const NODE_MODULES: &str = "node_modules";
/// One `podman ps` that lists every container's mounts, or one
/// `podman container inspect`. Container removal waits for it, so it is as
/// short as one removal command's deadline.
const LIST_TIMEOUT: Duration = Duration::from_secs(10);
/// What `podman container inspect` prints of each container, one JSON
/// object a line: its id, its mounts, and its tmpfs mounts, which `Mounts`
/// leaves out (`GetMounts` in Podman's `libpod/container_inspect.go`
/// discards them, 4.9.3 to 6.1.4). The id's field is `ID` (Podman cannot
/// evaluate `.Id`, its JSON name, inside `json`).
const INSPECT_FORMAT: &str =
    r#"{"Id":{{json .ID}},"Mounts":{{json .Mounts}},"Tmpfs":{{json .HostConfig.Tmpfs}}}"#;

/// Which directory is at a path: its device and inode, and its birth time
/// when the filesystem records one (an inode number is used again once its
/// directory is gone). Read from the directory opened without following a
/// link, so a link is never its target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Identity {
    device: u64,
    inode: u64,
    /// Seconds and nanoseconds since the Unix epoch.
    born: Option<(u64, u32)>,
}

impl Identity {
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
}

/// A random value, 128 bits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Nonce([u8; 16]);

impl Nonce {
    fn random() -> Result<Self, getrandom::Error> {
        let mut bytes = [0_u8; 16];
        getrandom::fill(&mut bytes)?;
        Ok(Self(bytes))
    }

    fn hex(&self) -> String {
        self.0.iter().map(|byte| format!("{byte:02x}")).collect()
    }

    /// What [`Nonce::hex`] wrote, and nothing else.
    fn parse(text: &[u8]) -> Option<Self> {
        if text.len() != 32 {
            return None;
        }
        let digit = |byte: u8| match byte {
            b'0'..=b'9' => Some(byte - b'0'),
            b'a'..=b'f' => Some(byte - b'a' + 10),
            _ => None,
        };
        let mut bytes = [0_u8; 16];
        for (byte, pair) in bytes.iter_mut().zip(text.chunks_exact(2)) {
            *byte = (digit(pair[0])? << 4) | digit(pair[1])?;
        }
        Some(Self(bytes))
    }
}

/// How the directory Axocoatl made is told from any other at its path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Mark {
    /// The directory `mkdir` made, as it was read then.
    identity: Identity,
    /// The value of its [`MARK_ATTRIBUTE`]; `None` when the filesystem did
    /// not take it, and then only `identity` tells it apart.
    nonce: Option<Nonce>,
}

impl Mark {
    /// `<identity>:<nonce>` ([`Identity::label`], [`Nonce::hex`]), with `-`
    /// for no nonce.
    fn label(&self) -> String {
        let nonce = self
            .nonce
            .map_or_else(|| "-".to_string(), |nonce| nonce.hex());
        format!("{}:{nonce}", self.identity.label())
    }

    /// The mark [`Mark::label`] wrote, and nothing else.
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
        let nonce = match fields.next()? {
            "-" => None,
            nonce => Some(Nonce::parse(nonce.as_bytes())?),
        };
        if fields.next().is_some() {
            return None;
        }
        Some(Self {
            identity: Identity {
                device,
                inode,
                born,
            },
            nonce,
        })
    }
}

/// What is at a mount point's path, opened without following a link.
#[derive(Debug)]
enum Opened {
    /// A directory, open for reading.
    Directory(std::fs::File),
    /// Nothing.
    Absent,
    /// A link, a file or anything else that is not a directory.
    Other,
}

#[cfg(unix)]
fn open_directory(path: &Path) -> std::io::Result<Opened> {
    use rustix::fs::{Mode, OFlags};
    use rustix::io::Errno;
    match rustix::fs::open(
        path,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    ) {
        Ok(directory) => Ok(Opened::Directory(std::fs::File::from(directory))),
        Err(error) if error == Errno::NOENT => Ok(Opened::Absent),
        // `O_NOFOLLOW` refuses a link (ELOOP), `O_DIRECTORY` anything else
        // that is not a directory (ENOTDIR).
        Err(error) if error == Errno::LOOP || error == Errno::NOTDIR => Ok(Opened::Other),
        Err(error) => Err(error.into()),
    }
}

#[cfg(not(unix))]
fn open_directory(_: &Path) -> std::io::Result<Opened> {
    Err(std::io::ErrorKind::Unsupported.into())
}

#[cfg(any(target_os = "linux", target_os = "android", target_vendor = "apple"))]
fn write_nonce(directory: &std::fs::File, nonce: Nonce) -> std::io::Result<()> {
    rustix::fs::fsetxattr(
        directory,
        MARK_ATTRIBUTE,
        nonce.hex().as_bytes(),
        rustix::fs::XattrFlags::CREATE,
    )
    .map_err(Into::into)
}

#[cfg(not(any(target_os = "linux", target_os = "android", target_vendor = "apple")))]
fn write_nonce(_: &std::fs::File, _: Nonce) -> std::io::Result<()> {
    Err(std::io::ErrorKind::Unsupported.into())
}

/// The nonce `directory` carries; `None` when it carries none (or the
/// filesystem keeps no extended attributes).
#[cfg(any(target_os = "linux", target_os = "android", target_vendor = "apple"))]
fn read_nonce(directory: &std::fs::File) -> Option<Nonce> {
    let mut value = [0_u8; 64];
    let length = rustix::fs::fgetxattr(directory, MARK_ATTRIBUTE, &mut value[..]).ok()?;
    Nonce::parse(value.get(..length)?)
}

#[cfg(not(any(target_os = "linux", target_os = "android", target_vendor = "apple")))]
fn read_nonce(_: &std::fs::File) -> Option<Nonce> {
    None
}

/// What is at a tracked mount point's path now.
#[derive(Debug, PartialEq, Eq)]
enum Found {
    /// The directory Axocoatl made ([`Mark`]).
    Made,
    /// Nothing.
    Absent,
    /// A directory Axocoatl did not make, a link or a file.
    Other,
}

/// Whether the directory `mark` describes is at `path`: one that carries
/// its nonce, or, when the filesystem took none, has its identity.
fn look(path: &Path, mark: &Mark) -> std::io::Result<Found> {
    let directory = match open_directory(path)? {
        Opened::Directory(directory) => directory,
        Opened::Absent => return Ok(Found::Absent),
        Opened::Other => return Ok(Found::Other),
    };
    let made = match mark.nonce {
        // The nonce alone tells it apart, also after a restart that
        // numbers the device differently.
        Some(nonce) => read_nonce(&directory) == Some(nonce),
        None => Identity::of(&directory.metadata()?) == Some(mark.identity),
    };
    Ok(if made { Found::Made } else { Found::Other })
}

/// A mount point this process made (or a daemon before it) and has not
/// removed yet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Made {
    /// The directory `mkdir` made: only that directory is ever removed.
    mark: Mark,
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

/// `mkdir` the mount point and mark it. `None` when it could not be made
/// (Podman makes it then, as before) or read back: a directory not told
/// apart is never tracked, so never removed.
fn make(path: &Path) -> Option<Mark> {
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
    let directory = match open_directory(path) {
        Ok(Opened::Directory(directory)) => directory,
        Ok(Opened::Absent | Opened::Other) => {
            tracing::warn!(
                path = %path.display(),
                "the Node dependency mount point was replaced as it was made; it stays"
            );
            return None;
        }
        Err(error) => {
            tracing::warn!(
                path = %path.display(),
                %error,
                "could not open the Node dependency mount point just made; it stays"
            );
            return None;
        }
    };
    let identity = match directory.metadata().map(|metadata| Identity::of(&metadata)) {
        Ok(Some(identity)) => identity,
        Ok(None) => return None,
        Err(error) => {
            tracing::warn!(
                path = %path.display(),
                %error,
                "could not read the Node dependency mount point just made; it stays"
            );
            return None;
        }
    };
    let nonce = Nonce::random()
        .map_err(|error| std::io::Error::other(error.to_string()))
        .and_then(|nonce| write_nonce(&directory, nonce).map(|()| nonce));
    let nonce = match nonce {
        Ok(nonce) => Some(nonce),
        Err(error) => {
            tracing::info!(
                path = %path.display(),
                %error,
                "could not mark the Node dependency mount point with {MARK_ATTRIBUTE}; \
                 telling it apart by device, inode and birth time"
            );
            None
        }
    };
    Some(Mark { identity, nonce })
}

impl Registry {
    fn bump(&mut self) -> u64 {
        self.next_generation += 1;
        self.next_generation
    }

    /// Ready `<working_dir>/node_modules` for a container start. Returns the
    /// path and its mark when it is a mount point this process made (now,
    /// or for an earlier container), counted as starting; `None` when the
    /// repository has its own (or it could not be made, and Podman makes it
    /// as before).
    fn acquire(&mut self, working_dir: &Path) -> Option<(PathBuf, Mark)> {
        let path = working_dir.join(NODE_MODULES);
        let mark = match std::fs::symlink_metadata(&path) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => match make(&path) {
                Some(mark) => mark,
                None => {
                    self.made.remove(&path);
                    return None;
                }
            },
            Ok(_) => match self.made.get(&path) {
                // Still the directory made for an earlier container.
                Some(made) if matches!(look(&path, &made.mark), Ok(Found::Made)) => made.mark,
                // The repository's own, or no longer the directory made
                // here: never this module's to remove.
                _ => {
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
            mark,
            starting: 0,
            generation,
        });
        entry.mark = mark;
        entry.starting += 1;
        entry.generation = generation;
        Some((path, mark))
    }

    /// One start of `path` finished, whether or not its container exists.
    fn finish_start(&mut self, path: &Path) {
        if let Some(entry) = self.made.get_mut(path) {
            entry.starting = entry.starting.saturating_sub(1);
        }
    }

    /// Remember mount points a container's labels say Axocoatl made, with
    /// which directories they were.
    fn adopt(&mut self, made: impl IntoIterator<Item = (PathBuf, Mark)>) {
        for (path, mark) in made {
            if !self.made.contains_key(&path) {
                let generation = self.bump();
                self.made.insert(
                    path,
                    Made {
                        mark,
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
            let outcome = match look(&path, &made.mark) {
                Ok(Found::Absent) => Released::Absent,
                Ok(Found::Other) => Released::KeptNotMade,
                Err(error) => Released::Failed(error.to_string()),
                Ok(Found::Made) => match std::fs::remove_dir(&path) {
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
    made: Option<(PathBuf, Mark)>,
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
        let (path, mark) = self.made.as_ref()?;
        let path = path
            .to_str()
            .filter(|path| !path.chars().any(char::is_control))?;
        Some([
            format!("{DEPENDENCY_MOUNTPOINT_LABEL}={path}"),
            format!("{DEPENDENCY_MOUNTPOINT_IDENTITY_LABEL}={}", mark.label()),
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

/// Whether the directory at `path` carries a [`Nonce`] in [`MARK_ATTRIBUTE`].
#[cfg(test)]
pub(crate) fn carries_a_nonce(path: &Path) -> bool {
    matches!(open_directory(path), Ok(Opened::Directory(directory)) if read_nonce(&directory).is_some())
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
    /// Where its volumes, bind mounts and tmpfs mounts are mounted in it
    /// (Podman's `UserVolumes`).
    #[serde(rename = "Mounts", default)]
    mounts: Option<Vec<String>>,
    #[serde(rename = "Labels", default)]
    labels: Option<HashMap<String, String>>,
}

impl Listed {
    /// The mount point its labels say Axocoatl made for it, and which
    /// directory that was, when both are plausible.
    fn labelled_mountpoint(&self) -> Option<(PathBuf, Mark)> {
        let labels = self.labels.as_ref()?;
        let path = PathBuf::from(labels.get(DEPENDENCY_MOUNTPOINT_LABEL)?);
        let mark = Mark::parse(labels.get(DEPENDENCY_MOUNTPOINT_IDENTITY_LABEL)?)?;
        plausible_mountpoint(&path).then_some((path, mark))
    }

    fn named_any_of(&self, names: &[String]) -> bool {
        self.names.iter().flatten().any(|name| {
            names
                .iter()
                .any(|wanted| name.trim_start_matches('/') == wanted)
        })
    }
}

/// One container as [`INSPECT_FORMAT`] prints it.
#[derive(Debug, Default, Clone, serde::Deserialize)]
struct Inspected {
    #[serde(rename = "Id", default)]
    id: Option<String>,
    /// Its volumes, image volumes and other mounts (Podman calls every one
    /// that is not a volume `bind`), without its tmpfs mounts.
    #[serde(rename = "Mounts", default)]
    mounts: Option<Vec<InspectedMount>>,
    /// Its tmpfs mounts: destination and options (`HostConfig.Tmpfs`).
    #[serde(rename = "Tmpfs", default)]
    tmpfs: Option<HashMap<String, String>>,
}

#[derive(Debug, Default, Clone, serde::Deserialize)]
struct InspectedMount {
    #[serde(rename = "Source", default)]
    source: Option<String>,
    #[serde(rename = "Destination", default)]
    destination: Option<String>,
}

impl Inspected {
    /// Whether this container has a mount on the host directory `path`
    /// (canonical): a mount whose source resolves to `path` (it
    /// bind-mounts the directory, wherever), or one whose source resolves
    /// to a directory above it while something is mounted at the place
    /// below that mount that is `path`: a development container that
    /// bind-mounts the repository at `/workspaces/x` with a volume, a bind
    /// or a tmpfs at `/workspaces/x/node_modules`. `listed` are the mount
    /// destinations `podman ps` lists for it.
    fn mounts_over(&self, path: &Path, listed: &[String]) -> bool {
        let mounts: Vec<&InspectedMount> = self.mounts.iter().flatten().collect();
        let destinations: HashSet<&Path> = mounts
            .iter()
            .filter_map(|mount| mount.destination.as_deref())
            .chain(
                self.tmpfs
                    .iter()
                    .flatten()
                    .map(|(destination, _)| destination.as_str()),
            )
            .chain(listed.iter().map(String::as_str))
            .map(Path::new)
            .collect();
        mounts.iter().any(|mount| {
            let (Some(source), Some(destination)) =
                (mount.source.as_deref(), mount.destination.as_deref())
            else {
                return false;
            };
            // `path` is canonical; the source is spelled as Podman was
            // given it.
            let source = std::fs::canonicalize(source).unwrap_or_else(|_| PathBuf::from(source));
            let Ok(below) = path.strip_prefix(&source) else {
                return false;
            };
            below.as_os_str().is_empty()
                || destinations.contains(Path::new(destination).join(below).as_path())
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

/// One JSON value a line, as a `--format` template prints them.
fn parse_json_lines<T: serde::de::DeserializeOwned>(stdout: &[u8]) -> Result<Vec<T>, String> {
    let text = std::str::from_utf8(stdout).map_err(|error| error.to_string())?;
    text.lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str(line).map_err(|error| error.to_string()))
        .collect()
}

/// Every mount destination of the listed containers.
fn mounted(listed: &[Listed]) -> HashSet<PathBuf> {
    listed
        .iter()
        .flat_map(|container| container.mounts.iter().flatten())
        .map(PathBuf::from)
        .collect()
}

/// The ids of every listed container, to inspect them all; `None` when
/// one has no id Podman could be asked about.
fn every_id(listed: &[Listed]) -> Option<Vec<String>> {
    listed
        .iter()
        .map(|container| {
            container
                .id
                .as_deref()
                .filter(|id| plausible_container_id(id))
                .map(str::to_string)
        })
        .collect()
}

/// Add to `in_use` each of `unmounted` that an `inspected` container (of
/// those `listed`) has a mount on; when Podman could not say, every one of
/// them.
fn conclude(
    in_use: &mut HashSet<PathBuf>,
    unmounted: Vec<PathBuf>,
    listed: &[Listed],
    inspected: Result<Vec<Inspected>, String>,
) {
    fn listed_mounts<'a>(listed: &'a [Listed], id: Option<&str>) -> &'a [String] {
        listed
            .iter()
            .find(|container| container.id.is_some() && container.id.as_deref() == id)
            .and_then(|container| container.mounts.as_deref())
            .unwrap_or_default()
    }
    match inspected {
        Ok(containers) => in_use.extend(unmounted.into_iter().filter(|path| {
            containers.iter().any(|container| {
                container.mounts_over(path, listed_mounts(listed, container.id.as_deref()))
            })
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

/// The mount points among `candidates` that a listed container has a mount
/// on, as far as Podman says; one it cannot say about counts.
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
    let inspected = match every_id(listed) {
        Some(ids) if ids.is_empty() => return in_use,
        Some(ids) => inspect(&ids).await,
        None => Err("a listed container has no id".to_string()),
    };
    conclude(&mut in_use, unmounted, listed, inspected);
    in_use
}

/// One bounded `podman` command, its standard output.
async fn podman_output(args: &[String]) -> Result<Vec<u8>, String> {
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
    Ok(output.stdout)
}

async fn list(filters: &[String]) -> Result<Vec<Listed>, String> {
    let mut args = vec!["ps".to_string(), "-a".to_string()];
    args.extend_from_slice(filters);
    args.extend(["--format".to_string(), "json".to_string()]);
    parse_json_array(&podman_output(&args).await?)
}

/// One `podman container inspect` of the containers `ids` (checked
/// [`plausible_container_id`]s), each as [`INSPECT_FORMAT`] prints it. An
/// error unless Podman printed every one of them.
async fn inspect(ids: &[String]) -> Result<Vec<Inspected>, String> {
    let mut args = vec![
        "container".to_string(),
        "inspect".to_string(),
        "--format".to_string(),
        INSPECT_FORMAT.to_string(),
    ];
    args.extend_from_slice(ids);
    let inspected: Vec<Inspected> = parse_json_lines(&podman_output(&args).await?)?;
    every_one_inspected(ids, &inspected)?;
    Ok(inspected)
}

fn every_one_inspected(ids: &[String], inspected: &[Inspected]) -> Result<(), String> {
    match ids.iter().find(|id| {
        !inspected
            .iter()
            .any(|container| container.id.as_deref() == Some(id.as_str()))
    }) {
        Some(id) => Err(format!("Podman did not inspect container {id}")),
        None => Ok(()),
    }
}

fn adopt_from(listed: &[Listed], keep: impl Fn(&Listed) -> bool) {
    let made: Vec<(PathBuf, Mark)> = listed
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
/// is using and no container Podman lists (of any daemon, or the person's
/// own) has a mount on, if it is still the empty directory made for it.
/// Without one to remove it asks Podman nothing. Best effort: when Podman
/// cannot list its containers, or say what one mounts, everything it could
/// concern stays for a later call.
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

    /// The mark of a mount point made here, which carries a nonce: the
    /// temporary directories these tests use are on filesystems that take
    /// user extended attributes (APFS, ext4, overlayfs, tmpfs since Linux
    /// 6.6).
    fn marked(registry: &Registry, path: &Path) -> Mark {
        let mark = registry.made[path].mark;
        assert!(
            mark.nonce.is_some(),
            "the filesystem under {} took no {MARK_ATTRIBUTE}; run these tests with TMPDIR on \
             one that takes user extended attributes",
            path.display()
        );
        mark
    }

    /// The nonce the directory at `path` carries now.
    fn nonce_at(path: &Path) -> Option<Nonce> {
        match open_directory(path).unwrap() {
            Opened::Directory(directory) => read_nonce(&directory),
            other => panic!("{} is not a directory: {other:?}", path.display()),
        }
    }

    /// Release every candidate with no container listed.
    fn release_all(registry: &mut Registry) -> Vec<(PathBuf, Released)> {
        let candidates = registry.candidates();
        registry.release(candidates, &HashSet::new())
    }

    #[test]
    fn a_missing_mount_point_is_made_as_the_person_marked_and_removed_when_unused() {
        let repo = tempfile::tempdir().unwrap();
        let path = repo.path().join(NODE_MODULES);
        let mut registry = Registry::default();

        let (acquired, mark) = registry.acquire(repo.path()).unwrap();
        assert_eq!(acquired, path);
        let metadata = std::fs::symlink_metadata(&path).unwrap();
        assert!(metadata.is_dir());
        assert_eq!(Identity::of(&metadata), Some(mark.identity));
        assert_eq!(marked(&registry, &path), mark);
        assert_eq!(nonce_at(&path), mark.nonce);
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
        assert_eq!(nonce_at(&path), None);

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

    /// The directory Axocoatl made, with something in it: the person's.
    #[test]
    fn what_the_person_installed_into_it_stays() {
        let repo = tempfile::tempdir().unwrap();
        let mut registry = Registry::default();
        let path = made_for_a_container(&mut registry, repo.path());
        marked(&registry, &path);
        std::fs::create_dir(path.join("left-pad")).unwrap();

        assert_eq!(
            release_all(&mut registry),
            vec![(path.clone(), Released::KeptNotEmpty)]
        );
        assert!(path.join("left-pad").is_dir());
        // It is the person's now: a later start does not take it back.
        assert_eq!(registry.acquire(repo.path()), None);
        assert!(registry.made.is_empty());
    }

    /// The person removes the directory Axocoatl made and makes an empty
    /// one at once in its place. On ext4 the new one usually gets the
    /// freed inode number and a birth time equal to the nanosecond, so
    /// device, inode and birth time cannot tell them apart; it carries no
    /// nonce, so it stays.
    #[test]
    fn a_directory_made_again_in_its_place_is_never_removed() {
        for _ in 0..20 {
            let repo = tempfile::tempdir().unwrap();
            let mut registry = Registry::default();
            let path = made_for_a_container(&mut registry, repo.path());
            marked(&registry, &path);
            std::fs::remove_dir(&path).unwrap();
            std::fs::create_dir(&path).unwrap();
            assert_eq!(nonce_at(&path), None);

            assert_eq!(
                release_all(&mut registry),
                vec![(path.clone(), Released::KeptNotMade)]
            );
            assert!(path.is_dir());
            // Nor does a later start take it as its own.
            assert_eq!(registry.acquire(repo.path()), None);
            assert!(registry.made.is_empty());
        }
    }

    /// The person moves the directory Axocoatl made aside and puts their
    /// own empty `node_modules` in its place: theirs is not the directory
    /// made, so it stays, and so does the one moved aside.
    #[test]
    fn another_empty_directory_in_its_place_is_never_removed() {
        let repo = tempfile::tempdir().unwrap();
        let mut registry = Registry::default();
        let path = made_for_a_container(&mut registry, repo.path());
        marked(&registry, &path);
        let aside = repo.path().join("node_modules.made");
        std::fs::rename(&path, &aside).unwrap();
        std::fs::create_dir(&path).unwrap();

        assert_eq!(
            release_all(&mut registry),
            vec![(path.clone(), Released::KeptNotMade)]
        );
        assert!(path.is_dir());
        assert!(aside.is_dir());
        assert!(registry.made.is_empty());
        assert_eq!(registry.acquire(repo.path()), None);
        assert!(registry.made.is_empty());
    }

    /// A directory in its place that carries another nonce (another
    /// daemon's, or one set by hand) is not the one made here.
    #[cfg(any(target_os = "linux", target_vendor = "apple"))]
    #[test]
    fn a_directory_with_another_nonce_is_never_removed() {
        let repo = tempfile::tempdir().unwrap();
        let mut registry = Registry::default();
        let path = made_for_a_container(&mut registry, repo.path());
        let mark = marked(&registry, &path);
        let other = Nonce::random().unwrap();
        assert_ne!(Some(other), mark.nonce);
        let Opened::Directory(directory) = open_directory(&path).unwrap() else {
            panic!("made a directory");
        };
        rustix::fs::fsetxattr(
            &directory,
            MARK_ATTRIBUTE,
            other.hex().as_bytes(),
            rustix::fs::XattrFlags::REPLACE,
        )
        .unwrap();
        assert_eq!(nonce_at(&path), Some(other));
        // Same directory, same inode: only the nonce differs.
        assert_eq!(
            std::fs::symlink_metadata(&path)
                .map(|metadata| Identity::of(&metadata))
                .unwrap(),
            Some(mark.identity)
        );

        assert_eq!(
            release_all(&mut registry),
            vec![(path.clone(), Released::KeptNotMade)]
        );
        assert!(path.is_dir());

        // Nor is one whose attribute is not a nonce at all.
        let repo = tempfile::tempdir().unwrap();
        let path = made_for_a_container(&mut registry, repo.path());
        let Opened::Directory(directory) = open_directory(&path).unwrap() else {
            panic!("made a directory");
        };
        rustix::fs::fsetxattr(
            &directory,
            MARK_ATTRIBUTE,
            b"not a nonce",
            rustix::fs::XattrFlags::REPLACE,
        )
        .unwrap();
        assert_eq!(
            release_all(&mut registry),
            vec![(path.clone(), Released::KeptNotMade)]
        );
        assert!(path.is_dir());
    }

    /// A link in its place, even to the very directory Axocoatl made
    /// (which still carries its nonce), is neither followed nor removed.
    #[cfg(unix)]
    #[test]
    fn a_link_in_its_place_is_never_followed_or_removed() {
        let repo = tempfile::tempdir().unwrap();
        let mut registry = Registry::default();
        let path = made_for_a_container(&mut registry, repo.path());
        let mark = marked(&registry, &path);
        let aside = repo.path().join("node_modules.made");
        std::fs::rename(&path, &aside).unwrap();
        std::os::unix::fs::symlink(&aside, &path).unwrap();
        assert_eq!(nonce_at(&aside), mark.nonce);
        assert!(matches!(open_directory(&path).unwrap(), Opened::Other));

        assert_eq!(
            release_all(&mut registry),
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
        marked(&registry, &path);
        std::fs::remove_dir(&path).unwrap();
        std::fs::create_dir(&path).unwrap();
        std::fs::write(path.join("package.json"), "{}").unwrap();

        let released = release_all(&mut registry);
        assert_eq!(released, vec![(path.clone(), Released::KeptNotMade)]);
        assert!(path.join("package.json").is_file());
    }

    /// Where the filesystem took no nonce, device, inode and birth time
    /// tell the directory apart, as far as they can.
    #[test]
    fn without_a_nonce_the_identity_tells_it_apart() {
        let forget_nonce = |registry: &mut Registry, path: &Path| {
            registry.made.get_mut(path).unwrap().mark.nonce = None;
        };
        let repo = tempfile::tempdir().unwrap();
        let mut registry = Registry::default();
        let path = made_for_a_container(&mut registry, repo.path());
        forget_nonce(&mut registry, &path);
        // Another directory moved into its place has another inode.
        let aside = repo.path().join("node_modules.made");
        std::fs::rename(&path, &aside).unwrap();
        std::fs::create_dir(&path).unwrap();
        assert_eq!(
            release_all(&mut registry),
            vec![(path.clone(), Released::KeptNotMade)]
        );
        assert!(path.is_dir() && aside.is_dir());

        // The directory made is removed, and a later start reuses it while
        // it is tracked.
        let repo = tempfile::tempdir().unwrap();
        let path = made_for_a_container(&mut registry, repo.path());
        forget_nonce(&mut registry, &path);
        assert_eq!(made_for_a_container(&mut registry, repo.path()), path);
        assert_eq!(
            release_all(&mut registry),
            vec![(path.clone(), Released::Removed)]
        );
        assert!(!path.exists());
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
        assert_eq!(
            release_all(&mut registry),
            vec![(path.clone(), Released::Removed)]
        );
        assert!(!path.exists());
    }

    #[test]
    fn a_removed_mount_point_is_made_again_for_the_next_start() {
        let repo = tempfile::tempdir().unwrap();
        let mut registry = Registry::default();
        let path = made_for_a_container(&mut registry, repo.path());
        let first = marked(&registry, &path);
        // The person removed it while it was tracked.
        std::fs::remove_dir(&path).unwrap();
        assert_eq!(made_for_a_container(&mut registry, repo.path()), path);
        assert!(path.is_dir());
        let second = marked(&registry, &path);
        assert_ne!(
            first.nonce, second.nonce,
            "each directory made, its own nonce"
        );
        assert_eq!(nonce_at(&path), second.nonce);
        assert_eq!(release_all(&mut registry), vec![(path, Released::Removed)]);
    }

    #[test]
    fn the_mark_label_reads_back_and_nothing_else_does() {
        let nonce = Nonce(*b"\x00\x01\x02\x03\x04\x05\x06\x07\x08\x09\x0a\x0b\x0c\x0d\x0e\xff");
        assert_eq!(nonce.hex(), "000102030405060708090a0b0c0d0eff");
        for mark in [
            Mark {
                identity: Identity {
                    device: 16_777_234,
                    inode: 98_765,
                    born: Some((1_760_000_000, 7)),
                },
                nonce: Some(nonce),
            },
            Mark {
                identity: Identity {
                    device: 2049,
                    inode: 1,
                    born: None,
                },
                nonce: None,
            },
        ] {
            assert_eq!(Mark::parse(&mark.label()), Some(mark));
        }
        assert_eq!(
            Mark {
                identity: Identity {
                    device: 1,
                    inode: 2,
                    born: Some((3, 4))
                },
                nonce: Some(nonce),
            }
            .label(),
            "1:2:3.000000004:000102030405060708090a0b0c0d0eff"
        );
        for bad in [
            "",
            "1:2",
            "1:2:",
            "1:2:3",
            "1:2:-",
            "1:2:3.4:-",
            "1:2:3.0000000004:-",
            "1:2:-:",
            "1:2:-:-:-",
            "+1:2:-:-",
            "1:-2:-:-",
            "a:2:-:-",
            "1:2:3.00000000x:-",
            "99999999999999999999:2:-:-",
            "1:2:-:000102030405060708090a0b0c0d0e",
            "1:2:-:000102030405060708090a0b0c0d0eff00",
            "1:2:-:000102030405060708090A0B0C0D0EFF",
            "1:2:-:000102030405060708090a0b0c0d0eg0",
        ] {
            assert_eq!(Mark::parse(bad), None, "{bad:?}");
        }
        for bad in [&b""[..], b"0", b"000102030405060708090a0b0c0d0ef", b"zz"] {
            assert_eq!(Nonce::parse(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn a_crashed_daemons_labelled_mount_points_are_adopted_and_removed() {
        let repo = tempfile::tempdir().unwrap();
        let path = repo.path().join(NODE_MODULES);
        let mark = make(&path).unwrap();
        assert!(mark.nonce.is_some());
        // A restart numbered the device differently: the nonce still
        // tells the directory apart.
        let renumbered_repo = tempfile::tempdir().unwrap();
        let renumbered = renumbered_repo.path().join(NODE_MODULES);
        let mut renumbered_mark = make(&renumbered).unwrap();
        renumbered_mark.identity.device ^= 1;
        // Another repository whose node_modules was made again since.
        let replaced_repo = tempfile::tempdir().unwrap();
        let replaced = replaced_repo.path().join(NODE_MODULES);
        let replaced_mark = make(&replaced).unwrap();
        std::fs::remove_dir(&replaced).unwrap();
        std::fs::create_dir(&replaced).unwrap();
        let listing = serde_json::json!([
            {
                "Id": "a".repeat(64),
                "Names": ["axo-ses-ses-1"],
                "Mounts": [repo.path(), &path],
                "Labels": {
                    "io.axocoatl.runtime-authority": "a".repeat(64),
                    DEPENDENCY_MOUNTPOINT_LABEL: path,
                    DEPENDENCY_MOUNTPOINT_IDENTITY_LABEL: mark.label(),
                }
            },
            {
                "Names": ["axo-ses-ses-2"],
                "Labels": {
                    DEPENDENCY_MOUNTPOINT_LABEL: replaced,
                    DEPENDENCY_MOUNTPOINT_IDENTITY_LABEL: replaced_mark.label(),
                }
            },
            {
                "Names": ["axo-ses-ses-3"],
                "Labels": {
                    DEPENDENCY_MOUNTPOINT_LABEL: renumbered,
                    DEPENDENCY_MOUNTPOINT_IDENTITY_LABEL: renumbered_mark.label(),
                }
            },
            { "Names": ["other"], "Mounts": null, "Labels": null },
            { "Names": ["no-identity"], "Labels": { DEPENDENCY_MOUNTPOINT_LABEL: "/r/node_modules" } },
            { "Names": ["bad-identity"], "Labels": {
                DEPENDENCY_MOUNTPOINT_LABEL: "/r/node_modules",
                DEPENDENCY_MOUNTPOINT_IDENTITY_LABEL: "1:2:-"
            } },
            { "Names": ["bad"], "Labels": {
                DEPENDENCY_MOUNTPOINT_LABEL: "relative/node_modules",
                DEPENDENCY_MOUNTPOINT_IDENTITY_LABEL: "1:2:-:-"
            } },
            { "Names": ["bad2"], "Labels": {
                DEPENDENCY_MOUNTPOINT_LABEL: "/x/../etc",
                DEPENDENCY_MOUNTPOINT_IDENTITY_LABEL: "1:2:-:-"
            } },
            { "Names": ["bad3"], "Labels": {
                DEPENDENCY_MOUNTPOINT_LABEL: "/home/person/src",
                DEPENDENCY_MOUNTPOINT_IDENTITY_LABEL: "1:2:-:-"
            } }
        ]);
        let listed = parse_listing(listing.to_string().as_bytes()).unwrap();
        let in_use = mounted(&listed);
        assert!(in_use.contains(&path));
        let labelled: Vec<(PathBuf, Mark)> = listed
            .iter()
            .filter_map(Listed::labelled_mountpoint)
            .collect();
        assert_eq!(
            labelled,
            vec![
                (path.clone(), mark),
                (replaced.clone(), replaced_mark),
                (renumbered.clone(), renumbered_mark)
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
        let released: HashMap<PathBuf, Released> =
            registry.release(candidates, &in_use).into_iter().collect();
        assert_eq!(
            released,
            [
                (renumbered.clone(), Released::Removed),
                (replaced.clone(), Released::KeptNotMade),
            ]
            .into()
        );
        assert_eq!(
            release_all(&mut registry),
            vec![(path.clone(), Released::Removed)]
        );
        assert!(!path.exists());
        assert!(!renumbered.exists());
        assert!(replaced.is_dir());
    }

    /// What Podman 6.1.4 printed for `podman container inspect --format
    /// INSPECT_FORMAT` of a container run with `-v <repo>:/workspaces/x
    /// --tmpfs /workspaces/x/node_modules -v <repo>/node_modules:/deps`, and
    /// of one with no mounts.
    #[test]
    fn inspect_output_parses_one_container_a_line() {
        let output = br#"{"Id":"15944f798e01584918a5d129eaa2ef79f149ec49ac42ae7783159f92c0c40760","Mounts":[{"Type":"bind","Source":"/r","Destination":"/workspaces/x","Driver":"","Mode":"","Options":["rbind"],"RW":true,"Propagation":"rprivate"},{"Type":"bind","Source":"/r/node_modules","Destination":"/deps","Driver":"","Mode":"","Options":["rbind"],"RW":true,"Propagation":"rprivate"}],"Tmpfs":{"/workspaces/x/node_modules":"rprivate,nosuid,nodev,tmpcopyup"}}
{"Id":"a1def83ac41616a3032bf95e99a81f599fcab63ad5b009dc930415f483fcb84f","Mounts":[],"Tmpfs":{}}
"#;
        let inspected: Vec<Inspected> = parse_json_lines(output).unwrap();
        assert_eq!(inspected.len(), 2);
        assert_eq!(inspected[0].mounts.as_ref().unwrap().len(), 2);
        assert!(inspected[0]
            .tmpfs
            .as_ref()
            .unwrap()
            .contains_key("/workspaces/x/node_modules"));
        let ids: Vec<String> = inspected
            .iter()
            .map(|container| container.id.clone().unwrap())
            .collect();
        assert_eq!(every_one_inspected(&ids, &inspected), Ok(()));
        let more = [ids.clone(), vec!["b".repeat(64)]].concat();
        assert!(every_one_inspected(&more, &inspected).is_err());
        // `null` mounts and tmpfs, as an older Podman may print them.
        let empty: Vec<Inspected> =
            parse_json_lines(br#"{"Id":"ab","Mounts":null,"Tmpfs":null}"#).unwrap();
        assert!(!empty[0].mounts_over(Path::new("/r/node_modules"), &[]));
        assert!(parse_json_lines::<Inspected>(b"{\"Id\":").is_err());
    }

    /// Every container Podman lists is inspected: a container of the
    /// person's own has a mount on the directory Axocoatl made when it
    /// bind-mounts it, wherever, or bind-mounts a directory above it and
    /// mounts a volume, a bind or a tmpfs at the place that is the
    /// directory, though no mount destination of it is that path.
    #[test]
    fn a_container_with_a_mount_on_it_keeps_it() {
        let repo = tempfile::tempdir().unwrap();
        let repo_path = std::fs::canonicalize(repo.path()).unwrap();
        let path = repo_path.join(NODE_MODULES);
        std::fs::create_dir(&path).unwrap();
        let other = tempfile::tempdir().unwrap();
        let other_path = std::fs::canonicalize(other.path())
            .unwrap()
            .join(NODE_MODULES);
        let devcontainer = "d".repeat(64);
        let listing = serde_json::json!([
            // Axocoatl's own: its mount is the path itself.
            { "Id": "a".repeat(64), "Mounts": [&repo_path, &path] },
            // The development container.
            { "Id": devcontainer, "Mounts": ["/workspaces/x", "/workspaces/x/node_modules"] },
            // Mounts nothing named node_modules: inspected all the same.
            { "Id": "c".repeat(64), "Mounts": ["/deps"] },
            { "Id": "e".repeat(64), "Mounts": null }
        ]);
        let listed = parse_listing(listing.to_string().as_bytes()).unwrap();
        assert_eq!(
            every_id(&listed),
            Some(vec![
                "a".repeat(64),
                devcontainer.clone(),
                "c".repeat(64),
                "e".repeat(64)
            ])
        );

        let mount = |source: &Path, destination: &str| InspectedMount {
            source: Some(source.display().to_string()),
            destination: Some(destination.into()),
        };
        let volume = |destination: &str| InspectedMount {
            source: Some("/home/person/.local/share/containers/storage/volumes/v/_data".into()),
            destination: Some(destination.into()),
        };
        // The repository as the person spelled it (not canonical on macOS).
        let development = Inspected {
            id: Some(devcontainer.clone()),
            mounts: Some(vec![
                mount(repo.path(), "/workspaces/x"),
                volume("/workspaces/x/node_modules"),
            ]),
            tmpfs: None,
        };
        assert!(development.mounts_over(&path, &[]));
        assert!(!development.mounts_over(&other_path, &[]));

        // One that binds the repository's parent sees it one level down.
        let below = Path::new("/src")
            .join(repo_path.file_name().unwrap())
            .join(NODE_MODULES);
        let parent = Inspected {
            mounts: Some(vec![
                mount(repo_path.parent().unwrap(), "/src"),
                volume(below.to_str().unwrap()),
            ]),
            ..Inspected::default()
        };
        assert!(parent.mounts_over(&path, &[]));
        // One that bind-mounts the directory itself, wherever, spelled as
        // the person did.
        let itself = Inspected {
            mounts: Some(vec![mount(&repo.path().join(NODE_MODULES), "/deps")]),
            ..Inspected::default()
        };
        assert!(itself.mounts_over(&path, &[]));
        assert!(!itself.mounts_over(&other_path, &[]));
        // A tmpfs there, which inspect's `Mounts` leaves out.
        let tmpfs = Inspected {
            mounts: Some(vec![mount(repo.path(), "/workspaces/x")]),
            tmpfs: Some(
                [(
                    "/workspaces/x/node_modules".to_string(),
                    "rw,rprivate,nosuid,nodev,tmpcopyup".to_string(),
                )]
                .into(),
            ),
            ..Inspected::default()
        };
        assert!(tmpfs.mounts_over(&path, &[]));
        // A mount there that only `podman ps` lists.
        let plain = Inspected {
            mounts: Some(vec![mount(&repo_path, "/w")]),
            ..Inspected::default()
        };
        assert!(plain.mounts_over(&path, &["/w/node_modules".to_string()]));
        // A bind of the repository with nothing mounted below it does not.
        assert!(!plain.mounts_over(&path, &[]));
        assert!(!plain.mounts_over(&path, &["/w/node_modules/x".to_string()]));
        // Nor does a bind of something inside it, or of a sibling.
        let inside = Inspected {
            mounts: Some(vec![
                mount(&repo_path.join("src"), "/src"),
                volume("/src/node_modules"),
            ]),
            ..Inspected::default()
        };
        assert!(!inside.mounts_over(&path, &[]));

        let mut in_use = HashSet::new();
        conclude(
            &mut in_use,
            vec![path.clone(), other_path.clone()],
            &listed,
            Ok(vec![development.clone()]),
        );
        assert_eq!(in_use, [path.clone()].into());
        // `podman ps`'s mounts of the same container count too.
        let listed_plain = Inspected {
            id: Some(devcontainer.clone()),
            mounts: Some(vec![mount(&repo_path, "/workspaces/x")]),
            tmpfs: None,
        };
        let mut in_use = HashSet::new();
        conclude(
            &mut in_use,
            vec![path.clone(), other_path.clone()],
            &listed,
            Ok(vec![listed_plain]),
        );
        assert_eq!(in_use, [path.clone()].into());
        // When Podman cannot say, both stay.
        let mut in_use = HashSet::new();
        conclude(
            &mut in_use,
            vec![path.clone(), other_path.clone()],
            &listed,
            Err("no such container".into()),
        );
        assert_eq!(in_use, [path.clone(), other_path.clone()].into());
        // A container without a usable id cannot be asked.
        let listed = parse_listing(
            serde_json::json!([{ "Id": "a".repeat(64) }, { "Id": "-x", "Mounts": ["/deps"] }])
                .to_string()
                .as_bytes(),
        )
        .unwrap();
        assert_eq!(every_id(&listed), None);
        let listed = parse_listing(br#"[{ "Names": ["n"] }]"#).unwrap();
        assert_eq!(every_id(&listed), None);
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
        assert!(parse_json_lines::<Inspected>(b"").unwrap().is_empty());
        assert!(parse_json_lines::<Inspected>(b"\n\n").unwrap().is_empty());
    }

    #[test]
    fn the_labels_go_ahead_of_the_image() {
        let mut args: Vec<String> = [
            "run", "-d", "--name", "c", "-v", "/r:/r:rw", "-w", "/r", "img",
        ]
        .iter()
        .map(|arg| arg.to_string())
        .collect();
        let mark = Mark {
            identity: Identity {
                device: 1,
                inode: 2,
                born: None,
            },
            nonce: Some(Nonce([0xab; 16])),
        };
        let lease = MountpointLease {
            made: Some((PathBuf::from("/r/node_modules"), mark)),
        };
        let labels = lease.labels().unwrap();
        insert_labels(&mut args, &labels);
        assert_eq!(
            args[8..],
            [
                "--label".to_string(),
                format!("{DEPENDENCY_MOUNTPOINT_LABEL}=/r/node_modules"),
                "--label".to_string(),
                format!(
                    "{DEPENDENCY_MOUNTPOINT_IDENTITY_LABEL}=1:2:-:{}",
                    "ab".repeat(16)
                ),
                "img".to_string()
            ]
        );
        let lease = MountpointLease {
            made: Some((PathBuf::from("/r/a\nb/node_modules"), mark)),
        };
        assert_eq!(lease.labels(), None);
    }
}
