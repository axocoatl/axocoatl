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
//! container Podman lists has a mount on it, and it is still the empty
//! directory Axocoatl made. It runs when [`SessionSandbox`] removes a
//! container: a Session's or Way's at Close, when `axocoatl dev` stops, at a
//! daemon's start after a crash, and a stale one of the same name at the
//! beginning of every container start; and once more when a start that
//! holds a mount point Axocoatl made has its container (or failed). A
//! container of the person's own that kept the directory going away runs
//! nothing, so the directory goes at this process's next such removal or
//! start after that, and stays when the daemon stops first (this process
//! remembers it, and the Axocoatl containers that recorded it are gone). A
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
//! "No container Podman lists has a mount on it" is what one `podman ps -a`,
//! one `podman container inspect` of every container it lists, and one
//! `podman volume inspect` of the volumes those mount say ([`in_use`]). A
//! container keeps the directory when:
//!
//! - `podman ps` lists a mount of it at the directory's path (Axocoatl's own
//!   containers mount their dependency volume there); or
//! - a host directory one of its mounts takes is the directory or a
//!   directory above it, and, when above, the container has a mount at the
//!   place below that mount that is the directory: a destination in
//!   inspect's `Mounts`, a tmpfs in its `HostConfig.Tmpfs`, or one `podman
//!   ps` lists (a development container that bind-mounts the repository and
//!   mounts a volume or a tmpfs on its `node_modules`). Those host
//!   directories are:
//!   - the source of each bind mount;
//!   - each directory an overlay volume (`-v <dir>:<dst>:O`) uses, which
//!     inspect gives among its options as `lowerdir=`, `upperdir=` and
//!     `workdir=` (the last two are in Podman's storage unless `-v
//!     <dir>:<dst>:O,upperdir=<dir>,workdir=<dir>` gives them), and its
//!     source, its merged directory in Podman's storage;
//!   - the device of each volume of the `local` driver made with `o=bind`
//!     or `o=rbind` (`podman volume create --opt type=none --opt o=bind
//!     --opt device=<dir>`, a Compose bind volume) or with `type=bind`
//!     (`--opt type=bind --opt device=<dir>`), which inspect reports only
//!     as the volume's own directory;
//!   - the source of each bind mount the container takes from another with
//!     `--volumes-from`, which its own inspect leaves out (4.9.3 and
//!     6.1.4): read from that other container's inspect, at the same place.
//!     The volumes it takes are in its own inspect; an overlay volume or a
//!     tmpfs Podman does not copy. Podman keeps that other container as it
//!     was named, so one made since under that name is read in its place,
//!     and an entry that is only `ro`, `rw` or `z` (4.9.3 splits `a:ro,z`
//!     in two) names a container only while one of that name is listed.
//!
//! Podman reports a bind's source made absolute and clean but as it was
//! given, links and all (4.9.3 and 6.1.4). So each host directory is first
//! compared, by its spelling, with the directory's path and the path that
//! resolves to now; only when none matches are they resolved on the host
//! (`realpath`), each on a thread of its own (not tokio's blocking pool,
//! whose shutdown waits for every task), [`RESOLVE_THREADS`] at a time and
//! for at most [`RESOLVE_TIMEOUT`] in all. One that is missing, is not a
//! directory or loops resolves to nothing. A source is read as it resolves
//! when Axocoatl looks, not as it did when the container started. A call
//! that needs a host directory another call is resolving waits for that
//! resolution rather than starting another, and none waits for one that has
//! run for [`RESOLVE_TIMEOUT`] already: a directory that never resolves
//! holds one thread, and delays only the calls made within
//! [`RESOLVE_TIMEOUT`] of when its resolution began.
//!
//! Removing the directory under a container that has a mount on it breaks
//! that mount: a bind mount of the directory itself, or a bind volume of it,
//! is left on a deleted directory nothing can be created in (on Linux and
//! through a Podman machine on macOS), and on Linux the kernel detaches every
//! mount on a removed directory, in every mount namespace. So when Podman or
//! the host cannot say, every candidate it could concern stays for a later
//! call: Podman fails, times out, prints more than 1 MiB, lists a container
//! without an id or mounts a volume without a usable name, leaves out a
//! container or volume it was asked about (one it says no longer exists,
//! `no such container` or `no such volume`, went since it was listed, and
//! the rest are inspected once more without it); a container takes mounts
//! with `--volumes-from` from one Podman no longer lists (Podman lets that
//! one be removed while the other exists, and then says nothing of the bind
//! mounts it took); or resolving a host directory fails otherwise or does
//! not return in time. Resolving fails with a permission for an overlay
//! volume's merged directory when its container runs with
//! `--userns=keep-id` (Podman 4.9.3 on Linux: the container's own directory
//! in Podman's storage then belongs to a subordinate uid, mode 0700), so
//! such a container keeps every mount point Axocoatl made until it is
//! removed. A stale NFS, SMB or sshfs mount fails with a stale file handle
//! or does not return. So does a container that has never started (inspect
//! reports an overlay volume only once its container has, and removing the
//! directory would make its start fail), and one that mounts a volume of
//! another driver than `local` (a volume plugin, whose host directory Podman
//! does not report). Each such keep is logged at `info` once for each mount
//! point and reason, naming the container and its mount when a container is
//! the reason, or Podman's error when Podman did not answer ([`log_kept`]).
//!
//! A container's processes can mount on the directory themselves (a tmpfs
//! over `node_modules` in a bind of the repository), which no inspect
//! reports: one run `--privileged` or with `CAP_SYS_ADMIN` added, and any
//! other whose mounts neither SELinux (`container_t`, enforcing) nor AppArmor
//! (Podman's `containers-default` profile, which rootless Podman does not
//! apply: none on Ubuntu 24.04, Podman 4.9.3) stops, in a user namespace of
//! its own (`unshare -Urm`; measured in Podman 6.1.4 with `--security-opt
//! label=disable`, refused with its default `container_t` label, and in
//! rootless Podman 4.9.3 on Ubuntu 24.04, whose
//! `kernel.apparmor_restrict_unprivileged_userns` is 1). Such a container
//! keeps the directory when a host directory it takes is above it, whatever
//! is mounted below ([`Kept::MayMount`]), and the daemon says which container
//! once. A container that has a mount on the directory is named before one
//! whose processes only may mount there, so that what is said is what holds
//! it. A container's `--rootfs` is a host directory it takes, at `/`.
//!
//! Nothing is removed while a container this Podman does not list could
//! have a mount on it ([`held_elsewhere`]): when Docker is installed, Podman
//! lists another connection (every Podman machine on macOS has a rootful
//! one), rootful Podman is installed on Linux (`/run/podman` exists, which
//! the podman packages of Fedora and Ubuntu make at every boot, whether or
//! not rootful Podman's API is enabled), or `podman ps -a --external` lists
//! a container `podman ps -a` does not (Buildah's, a `podman build`'s,
//! which cannot be inspected). Every mount point then stays, empty and the
//! person's, and the daemon says why once. On a Mac, and on Fedora and
//! Ubuntu with their podman packages, that is always.
//!
//! What this does not see, so a mount on the directory that removing it
//! breaks:
//! - containers of rootful Podman (`sudo podman`) on a Linux computer
//!   without `/run/podman` (a Podman not installed from such a package), and
//!   of container engines other than Docker and Podman (nerdctl, LXC);
//! - the tmpfs Podman mounts for an image's `VOLUME` under
//!   `--image-volume=tmpfs` (neither inspect nor `podman ps` reports it);
//! - a bind through a link that has been removed or changed since the
//!   container started;
//! - the directory reached through another mount of it: a bind mount on the
//!   host of a directory above it, or on macOS a spelling through
//!   `/System/Volumes/Data`, which resolving keeps as it is.
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
/// object a line: its id, its mounts, its tmpfs mounts, which `Mounts`
/// leaves out (`GetMounts` in Podman's `libpod/container_inspect.go`
/// discards them, 4.9.3 to 6.1.4), and the containers it takes mounts from
/// with `--volumes-from`, whose bind mounts `Mounts` leaves out too; its
/// root filesystem when given with `--rootfs`; whether it may mount
/// filesystems itself (`--privileged`, or `CAP_SYS_ADMIN` added, or neither
/// SELinux nor AppArmor confining it: its SELinux process label and its
/// AppArmor profile); and when it started (`0001-01-01T00:00:00Z` for never:
/// Podman reports an overlay volume only once its container has started).
/// The id's field is `ID` (Podman cannot evaluate `.Id`, its JSON name,
/// inside `json`). Every field is there in Podman 4.9.3 and 6.1.4.
const INSPECT_FORMAT: &str = r#"{"Id":{{json .ID}},"Mounts":{{json .Mounts}},"Tmpfs":{{json .HostConfig.Tmpfs}},"VolumesFrom":{{json .HostConfig.VolumesFrom}},"Rootfs":{{json .Rootfs}},"Privileged":{{json .HostConfig.Privileged}},"CapAdd":{{json .HostConfig.CapAdd}},"ProcessLabel":{{json .ProcessLabel}},"AppArmorProfile":{{json .AppArmorProfile}},"StartedAt":{{json .State.StartedAt}}}"#;
/// What `podman container inspect` says for a container that never started.
const NEVER_STARTED: &str = "0001-01-01T00:00:00Z";
/// What `podman volume inspect` prints of each volume, one JSON object a
/// line: its name, its driver and the options it was made with.
const VOLUME_FORMAT: &str =
    r#"{"Name":{{json .Name}},"Driver":{{json .Driver}},"Options":{{json .Options}}}"#;
/// How long one [`release_unused`] waits, in all, for the host to resolve the
/// host directories it could not match by their spelling. Resolving a path
/// takes microseconds; one on a stale NFS, SMB or sshfs mount may never
/// return.
const RESOLVE_TIMEOUT: Duration = Duration::from_secs(5);
/// How many host directories one [`release_unused`] resolves at a time.
const RESOLVE_THREADS: usize = 16;

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
pub(crate) enum Released {
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

/// Why [`release_unused`] kept a mount point it made where it is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Kept {
    /// `podman ps` lists a mount at its path (Axocoatl's own containers
    /// mount their dependency volume there).
    Listed,
    /// `podman container inspect` says the container `container` has a mount
    /// on it: the host directory `source` (a bind's source, one it takes
    /// with `--volumes-from`, a directory an overlay volume uses or a bind
    /// volume's device, as Podman spells it) is it or a directory above it,
    /// read by its spelling or as it resolves on the host, and the directory
    /// is at `at` in that container.
    Mounted {
        container: String,
        source: PathBuf,
        at: PathBuf,
    },
    /// No container Podman lists has a mount on it, but the processes of the
    /// container `container` may mount filesystems themselves (`why`: it runs
    /// `--privileged`, has `CAP_SYS_ADMIN`, or nothing confines its mounts)
    /// and it takes the host directory `source`, which is above it, so that
    /// the directory is at `at` in that container: they could mount on it
    /// there, which no inspect reports ([`Inspected::may_mount`]).
    MayMount {
        container: String,
        source: PathBuf,
        at: PathBuf,
        why: &'static str,
    },
    /// Podman or the host could not say whether a container has a mount on
    /// it: why.
    Unknown(String),
    /// A container this Podman does not list could have a mount on it: one
    /// of another engine (Docker), of another Podman connection or machine,
    /// of rootful Podman, or one Podman lists only with `--external`
    /// ([`held_elsewhere`]): which.
    Elsewhere(String),
}

/// What [`release_unused`] decided for one mount point it made and no start
/// is using.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Decision {
    Kept(Kept),
    Released(Released),
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
        in_use: &HashMap<PathBuf, Kept>,
    ) -> Vec<(PathBuf, Released)> {
        let mut released = Vec::new();
        for (path, generation) in candidates {
            let Some(made) = self.made.get(&path) else {
                continue;
            };
            if made.starting > 0 || made.generation != generation || in_use.contains_key(&path) {
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
    /// `storage` for a container Podman lists only with `--external` (a
    /// Buildah container, or a `podman build`'s), which it cannot inspect.
    #[serde(rename = "State", default)]
    state: Option<String>,
}

impl Listed {
    fn external(&self) -> bool {
        self.state.as_deref() == Some("storage")
    }

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
    /// that is not a volume or an image `bind`), without its tmpfs mounts.
    #[serde(rename = "Mounts", default)]
    mounts: Option<Vec<InspectedMount>>,
    /// Its tmpfs mounts: destination and options (`HostConfig.Tmpfs`).
    #[serde(rename = "Tmpfs", default)]
    tmpfs: Option<HashMap<String, String>>,
    /// The containers it takes mounts from (`--volumes-from
    /// <container>[:<options>]`), as they were given: a name or an id or
    /// the start of one ([`volumes_from_container`]).
    #[serde(rename = "VolumesFrom", default)]
    volumes_from: Option<Vec<String>>,
    /// Its root filesystem, a host directory, when given with `--rootfs`.
    #[serde(rename = "Rootfs", default)]
    rootfs: Option<String>,
    #[serde(rename = "Privileged", default)]
    privileged: Option<bool>,
    /// Capabilities added (`CAP_SYS_ADMIN`, or `SYS_ADMIN` as given).
    #[serde(rename = "CapAdd", default)]
    cap_add: Option<Vec<String>>,
    /// Its SELinux process label (`system_u:system_r:container_t:s0:c7,c27`),
    /// empty when SELinux does not label it (`--security-opt
    /// label=disable`, or a host without SELinux).
    #[serde(rename = "ProcessLabel", default)]
    process_label: Option<String>,
    /// Its AppArmor profile (`containers-default-0.57.4`), empty on a host
    /// without AppArmor and under rootless Podman, which applies none
    /// (Podman 4.9.3 on Ubuntu 24.04, AppArmor enabled).
    #[serde(rename = "AppArmorProfile", default)]
    apparmor_profile: Option<String>,
    /// When it last started; [`NEVER_STARTED`] when it never has.
    #[serde(rename = "StartedAt", default)]
    started_at: Option<String>,
}

impl Inspected {
    /// Whether its processes may mount filesystems themselves (a tmpfs
    /// over a directory of a bind mount), which no inspect reports: it runs
    /// `--privileged` or with `CAP_SYS_ADMIN` (or every capability) added,
    /// or nothing confines its mounts. A process in a container without
    /// `CAP_SYS_ADMIN` can still make a user namespace of its own and mount
    /// there, on a directory of a bind of the host's (`unshare -Urm`, then
    /// `mount -t tmpfs`: Podman 6.1.4's default seccomp profile allows it),
    /// unless SELinux (`container_t`, enforcing) or AppArmor (Podman's
    /// `containers-default` profile, which denies every mount) stops it.
    /// With `--security-opt label=disable`, `label=type:spc_t`, on a host
    /// with neither (Arch Linux), under rootless Podman on a host without
    /// SELinux (Ubuntu: rootless Podman applies no AppArmor profile) or with
    /// SELinux permissive, it may.
    /// `selinux_enforcing`: SELinux enforces its policy where the container
    /// runs ([`selinux_enforcing`]). Why it may, as a person reads it, or
    /// `None` when it may not.
    fn may_mount(&self, selinux_enforcing: bool) -> Option<&'static str> {
        let added = self.cap_add.iter().flatten().any(|cap| {
            matches!(
                cap.trim_start_matches("CAP_").to_ascii_uppercase().as_str(),
                "SYS_ADMIN" | "ALL"
            )
        });
        let apparmor = self
            .apparmor_profile
            .as_deref()
            .is_some_and(|profile| profile.starts_with("containers-default"));
        let selinux = selinux_enforcing
            && self
                .process_label
                .as_deref()
                .and_then(|label| label.split(':').nth(2))
                .is_some_and(|kind| matches!(kind, "container_t" | "container_init_t"));
        if self.privileged == Some(true) {
            Some("it runs --privileged")
        } else if added {
            Some("it has CAP_SYS_ADMIN")
        } else if apparmor || selinux {
            None
        } else {
            Some(
                "neither SELinux (container_t, enforcing) nor AppArmor (containers-default) \
                 confines its mounts, so its processes can mount in a user namespace of their own",
            )
        }
    }
}

/// Whether SELinux enforces its policy where Podman runs containers, now
/// (`setenforce` changes it): on Linux, `/sys/fs/selinux/enforce` reads `1`;
/// elsewhere Podman runs them in a machine, whose Fedora CoreOS enforces it
/// (Podman 6.1.4's machine reads `1`), and nothing is removed there anyway
/// ([`held_elsewhere`]: every machine has a rootful connection).
fn selinux_enforcing() -> bool {
    !cfg!(target_os = "linux")
        || std::fs::read_to_string("/sys/fs/selinux/enforce")
            .is_ok_and(|enforce| enforce.trim() == "1")
}

#[derive(Debug, Default, Clone, serde::Deserialize)]
struct InspectedMount {
    /// `volume` (named, anonymous or an image's `VOLUME`), `image`, or
    /// `bind` for every other one.
    #[serde(rename = "Type", default)]
    kind: Option<String>,
    /// A volume's name.
    #[serde(rename = "Name", default)]
    name: Option<String>,
    /// A bind's host path, absolute and clean but with links unresolved; a
    /// volume's own directory in Podman's storage; an overlay volume's merged
    /// directory there; an image's reference.
    #[serde(rename = "Source", default)]
    source: Option<String>,
    #[serde(rename = "Destination", default)]
    destination: Option<String>,
    /// Its mount options: an overlay volume's (`-v <dir>:<dst>:O`) name the
    /// host directories it uses, `lowerdir=<dir>[:<dir>...]`,
    /// `upperdir=<dir>` and `workdir=<dir>` (the last two in Podman's
    /// storage unless `-v <dir>:<dst>:O,upperdir=<dir>,workdir=<dir>` gives
    /// them).
    #[serde(rename = "Options", default)]
    options: Option<Vec<String>>,
}

impl InspectedMount {
    fn is_volume(&self) -> bool {
        self.kind.as_deref() == Some("volume")
    }

    /// The host directories an overlay volume uses, each with its option's
    /// name; none for any other mount.
    fn overlay_directories(&self) -> Vec<(&'static str, &str)> {
        let mut directories = Vec::new();
        for option in self.options.iter().flatten() {
            for key in OVERLAY_DIRECTORIES {
                let Some(value) = option
                    .strip_prefix(key)
                    .and_then(|rest| rest.strip_prefix('='))
                else {
                    continue;
                };
                if key == "lowerdir" {
                    directories.extend(value.split(':').map(|dir| (key, dir)));
                } else {
                    directories.push((key, value));
                }
            }
        }
        directories
    }
}

/// The options that name the host directories an overlay mount uses.
const OVERLAY_DIRECTORIES: [&str; 3] = ["lowerdir", "upperdir", "workdir"];

/// One volume as [`VOLUME_FORMAT`] prints it.
#[derive(Debug, Default, Clone, serde::Deserialize)]
struct InspectedVolume {
    #[serde(rename = "Name", default)]
    name: Option<String>,
    #[serde(rename = "Driver", default)]
    driver: Option<String>,
    #[serde(rename = "Options", default)]
    options: Option<HashMap<String, String>>,
}

impl InspectedVolume {
    /// The host directory this volume binds at its own directory whenever a
    /// container that uses it starts, as it was given: a volume of the
    /// `local` driver with an absolute `device`, made with `o=bind` or
    /// `o=rbind` (`podman volume create --opt type=none --opt o=bind --opt
    /// device=<dir>`) or with `type=bind` and any `o` (`--opt type=bind
    /// --opt device=<dir>`: Podman mounts it with `-o bind`). Podman hands
    /// any other `type`, `rbind` too, to `mount -t`, which binds nothing
    /// (4.9.3 and 6.1.4 then fail the container's start).
    fn bound(&self) -> Option<PathBuf> {
        if !matches!(self.driver.as_deref(), None | Some("local")) {
            return None;
        }
        let options = self.options.as_ref()?;
        let binds = options.get("o").is_some_and(|o| {
            o.split(',')
                .any(|option| matches!(option.trim(), "bind" | "rbind"))
        }) || options.get("type").map(String::as_str) == Some("bind");
        let device = Path::new(options.get("device")?);
        (binds && device.is_absolute()).then(|| device.to_path_buf())
    }
}

/// Whether `name` is a volume name Podman makes or takes
/// (`[a-zA-Z0-9][a-zA-Z0-9_.-]*`), so never taken for an option.
fn plausible_volume_name(name: &str) -> bool {
    name.len() <= 255
        && name
            .bytes()
            .next()
            .is_some_and(|byte| byte.is_ascii_alphanumeric())
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'.' | b'-'))
}

/// What a host directory is to the container mount that takes it.
#[derive(Debug, Clone, PartialEq, Eq)]
enum HostKind {
    /// A bind mount's source.
    Bind,
    /// The source of a bind mount the container takes from the container
    /// named here with `--volumes-from`.
    BindFrom(String),
    /// An overlay volume's source: its merged directory, in Podman's
    /// storage.
    OverlayMerged,
    /// A directory an overlay volume uses, with its option's name
    /// (`lowerdir`, `upperdir` or `workdir`).
    Overlay(&'static str),
    /// The device of the bind volume named here ([`InspectedVolume::bound`]).
    VolumeDevice(String),
    /// Its root filesystem (`--rootfs`).
    Rootfs,
}

/// A host directory one of a container's mounts takes.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Host {
    /// As Podman spells it.
    path: PathBuf,
    /// Where that mount is in the container.
    at: PathBuf,
    kind: HostKind,
}

/// A container id as Podman shortens it.
fn short_id(id: &str) -> &str {
    id.get(..12).unwrap_or(id)
}

impl Host {
    fn new(path: impl Into<PathBuf>, at: &Path, kind: HostKind) -> Self {
        Self {
            path: path.into(),
            at: at.to_path_buf(),
            kind,
        }
    }

    /// What it is to container `container`, for a person to read.
    fn describe(&self, container: &str) -> String {
        let (container, at) = (short_id(container), self.at.display());
        match &self.kind {
            HostKind::Bind => format!("the source of container {container}'s bind mount at {at}"),
            HostKind::BindFrom(from) => format!(
                "the source of the bind mount at {at} that container {container} takes from \
                 container {from} with --volumes-from"
            ),
            HostKind::OverlayMerged => format!(
                "the merged directory, in Podman's storage, of container {container}'s overlay \
                 volume at {at}"
            ),
            HostKind::Overlay(option) => {
                format!("the {option} of container {container}'s overlay volume at {at}")
            }
            HostKind::VolumeDevice(name) => {
                format!("the device of volume {name}, which container {container} mounts at {at}")
            }
            HostKind::Rootfs => format!("the root filesystem of container {container}"),
        }
    }

    /// Why a release keeps every mount point it could concern when this
    /// host directory, of container `container`, could not be resolved.
    fn unresolved(&self, container: &str, why: &str, denied: bool) -> String {
        let mut said = format!(
            "could not resolve {}, {}: {why}",
            self.path.display(),
            self.describe(container)
        );
        if denied && self.kind == HostKind::OverlayMerged {
            said.push_str(
                ". Podman keeps it under the container's own directory in its storage, which \
                 belongs to the container's root: a subordinate uid of yours, not you, when the \
                 container runs with --userns=keep-id",
            );
        }
        said.push_str(&format!(
            ". Node dependency mount points Axocoatl made stay while that container exists \
             (until it is removed, or {} resolves)",
            self.path.display()
        ));
        said
    }
}

/// What one container mounts, as far as host directories go.
#[derive(Debug, Default, Clone)]
struct Mounts {
    id: String,
    /// The host directories its mounts take: each bind's source, each
    /// directory an overlay volume uses and its merged directory, each bind
    /// volume's device ([`InspectedVolume::bound`]), and the source of each
    /// bind mount it takes from another container with `--volumes-from`.
    hosts: Vec<Host>,
    /// Every place something is mounted in it: inspect's `Mounts` and
    /// `HostConfig.Tmpfs`, what `podman ps` lists for it, and where the bind
    /// mounts it takes with `--volumes-from` are.
    destinations: HashSet<PathBuf>,
    /// Why its processes may mount filesystems themselves
    /// ([`Inspected::may_mount`]), when they may: any place below a host
    /// directory it takes may have a mount on it.
    may_mount: Option<&'static str>,
    /// Why what it mounts cannot be told, when it cannot.
    unknown: Option<String>,
}

impl Mounts {
    /// What `container` mounts, with `volumes`, every volume it mounts that
    /// still exists and the host directory it binds, if any. `Err` when it
    /// mounts a volume whose name Podman cannot be asked about. `listed` are
    /// the mount destinations `podman ps` lists for it. The bind mounts it
    /// takes with `--volumes-from` are not in its inspect
    /// ([`take_volumes_from`]).
    fn of(
        container: &Inspected,
        listed: &[String],
        volumes: &HashMap<String, Option<PathBuf>>,
    ) -> Result<Self, String> {
        let mut mounts = Self {
            id: container.id.clone().unwrap_or_default(),
            may_mount: container.may_mount(selinux_enforcing()),
            ..Self::default()
        };
        if container.started_at.as_deref() == Some(NEVER_STARTED) {
            mounts.unknown = Some(format!(
                "container {} has never started, and Podman reports an overlay volume only once \
                 its container has. Node dependency mount points Axocoatl made stay until it \
                 starts or is removed",
                short_id(&mounts.id)
            ));
        }
        if let Some(rootfs) = container
            .rootfs
            .as_deref()
            .filter(|rootfs| Path::new(rootfs).is_absolute())
        {
            mounts
                .hosts
                .push(Host::new(rootfs, Path::new("/"), HostKind::Rootfs));
        }
        mounts.destinations.extend(listed.iter().map(PathBuf::from));
        mounts.destinations.extend(
            container
                .tmpfs
                .iter()
                .flatten()
                .map(|(at, _)| PathBuf::from(at)),
        );
        for mount in container.mounts.iter().flatten() {
            let Some(destination) = mount.destination.as_deref().map(PathBuf::from) else {
                continue;
            };
            mounts.destinations.insert(destination.clone());
            if mount.is_volume() {
                let name = mount
                    .name
                    .as_deref()
                    .filter(|name| plausible_volume_name(name))
                    .ok_or_else(|| {
                        format!(
                            "container {} mounts a volume at {} without a usable name",
                            mounts.id,
                            destination.display()
                        )
                    })?;
                // A volume gone since it was listed (`no such volume`) binds
                // nothing any more.
                if let Some(device) = volumes.get(name).cloned().flatten() {
                    mounts.hosts.push(Host::new(
                        device,
                        &destination,
                        HostKind::VolumeDevice(name.to_string()),
                    ));
                }
            } else if mount.kind.as_deref() != Some("image") {
                // A bind's source, or an overlay volume's merged directory
                // and the directories it uses; an image's source is a
                // reference, and `devpts` names a filesystem, not a host
                // directory.
                let overlay = mount.overlay_directories();
                let source_kind = if overlay.is_empty() {
                    HostKind::Bind
                } else {
                    HostKind::OverlayMerged
                };
                let hosts = mount
                    .source
                    .as_deref()
                    .map(|source| (source, source_kind))
                    .into_iter()
                    .chain(
                        overlay
                            .into_iter()
                            .map(|(option, dir)| (dir, HostKind::Overlay(option))),
                    );
                for (host, kind) in hosts {
                    if Path::new(host).is_absolute() {
                        mounts.hosts.push(Host::new(host, &destination, kind));
                    }
                }
            }
        }
        Ok(mounts)
    }

    /// How this container holds the directory spelled `path` on the host
    /// through its host directory `host`, read as `real` (its spelling, or
    /// what it resolves to), when that is the directory or a directory above
    /// it: [`Kept::Mounted`] when it is the directory or something is
    /// mounted at the place below it that is the directory, else
    /// [`Kept::MayMount`] when its processes may mount there themselves.
    fn hold(&self, host: &Host, real: &Path, path: &Path) -> Option<Kept> {
        let below = path.strip_prefix(real).ok()?;
        let at = host.at.join(below);
        let (container, source) = (self.id.clone(), host.path.clone());
        if below.as_os_str().is_empty() || self.destinations.contains(&at) {
            return Some(Kept::Mounted {
                container,
                source,
                at,
            });
        }
        self.may_mount.map(|why| Kept::MayMount {
            container,
            source,
            at,
            why,
        })
    }

    /// How this container holds one of `spellings` through its host
    /// directories, each read with `read`: by the first that has a mount on
    /// it ([`Kept::Mounted`]), else by the first its processes may mount on
    /// ([`Kept::MayMount`]).
    fn over(&self, spellings: &[PathBuf], read: impl Fn(&Path) -> Option<PathBuf>) -> Option<Kept> {
        let mut may = None;
        for host in &self.hosts {
            let Some(real) = read(&host.path) else {
                continue;
            };
            for path in spellings {
                match self.hold(host, &real, path) {
                    Some(mounted @ Kept::Mounted { .. }) => return Some(mounted),
                    Some(kept) => {
                        may.get_or_insert(kept);
                    }
                    None => {}
                }
            }
        }
        may
    }
}

/// The container a `--volumes-from` entry names: what comes before its
/// options (`<container>[:<options>]`), and whether the entry may be options
/// alone, `ro`, `rw` or `z`, the only ones Podman takes: 4.9.3 keeps
/// `--volumes-from a:ro,z` as `a:ro` and `z` (6.1.4 as `a:ro,z`). `None` for
/// an entry that names nothing.
fn volumes_from_container(entry: &str) -> Option<(&str, bool)> {
    let (name, options_alone) = match entry.split_once(':') {
        Some((name, _)) => (name, false),
        None => (entry, matches!(entry, "ro" | "rw" | "z")),
    };
    (!name.is_empty()).then_some((name, options_alone))
}

/// Add to each of `containers` the bind mounts it takes from another with
/// `--volumes-from` (`inspected` say which; `listed` give the names), which
/// its own inspect leaves out: Podman copies the other container's bind
/// mounts, at the same places, when it makes the container (its volumes the
/// container's inspect shows itself, and an overlay volume it does not
/// copy). A container that takes them from one no longer there (removed,
/// which Podman allows while this one exists) mounts what Podman no longer
/// says: [`Mounts::unknown`].
fn take_volumes_from(containers: &mut [Mounts], inspected: &[Inspected], listed: &[Listed]) {
    let own: Vec<Mounts> = containers.to_vec();
    for (container, mounts) in inspected.iter().zip(containers.iter_mut()) {
        for (name, options_alone) in container
            .volumes_from
            .iter()
            .flatten()
            .filter_map(|entry| volumes_from_container(entry))
        {
            // Podman takes a full id, a name or the start of an id. A
            // container listed that is gone since is not among `own`.
            let named: Vec<&str> = listed
                .iter()
                .filter(|entry| entry.named_any_of(&[name.to_string()]))
                .filter_map(|entry| entry.id.as_deref())
                .collect();
            let from: Vec<&Mounts> = own
                .iter()
                .filter(|other| {
                    other.id != mounts.id
                        && (named.contains(&other.id.as_str())
                            || (name.bytes().all(|byte| byte.is_ascii_hexdigit())
                                && other.id.starts_with(name)))
                })
                .collect();
            // `z` is the option unless a container has that name.
            if from.is_empty() && options_alone {
                continue;
            }
            if from.is_empty() {
                mounts.unknown.get_or_insert_with(|| {
                    format!(
                        "container {} takes mounts from container {name} with --volumes-from, \
                         and no container Podman lists is {name} any more, so Podman no longer \
                         says what those mounts are. Node dependency mount points Axocoatl made \
                         stay while container {} exists",
                        short_id(&mounts.id),
                        short_id(&mounts.id)
                    )
                });
                continue;
            }
            for other in from {
                for host in other
                    .hosts
                    .iter()
                    .filter(|host| host.kind == HostKind::Bind)
                {
                    mounts.destinations.insert(host.at.clone());
                    mounts.hosts.push(Host::new(
                        host.path.clone(),
                        &host.at,
                        HostKind::BindFrom(short_id(&other.id).to_string()),
                    ));
                }
            }
        }
    }
}

/// What resolving one host directory said.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Resolved {
    /// The path it resolves to.
    To(PathBuf),
    /// Nothing is there now: it, or a directory on the way, is missing, is
    /// not a directory, or is a link that loops.
    Nothing,
    /// The host does not let this process look (EACCES or EPERM): why.
    Denied(String),
    /// The host could not say otherwise: why.
    Unknown(String),
}

impl Resolved {
    fn of(resolved: std::io::Result<PathBuf>) -> Self {
        use std::io::ErrorKind;
        // `ErrorKind::FilesystemLoop` is not stable yet.
        #[cfg(unix)]
        let loops = |error: &std::io::Error| {
            error.raw_os_error() == Some(rustix::io::Errno::LOOP.raw_os_error())
        };
        #[cfg(not(unix))]
        let loops = |_: &std::io::Error| false;
        match resolved {
            Ok(path) => Self::To(path),
            Err(error)
                if matches!(error.kind(), ErrorKind::NotFound | ErrorKind::NotADirectory)
                    || loops(&error) =>
            {
                Self::Nothing
            }
            Err(error) if error.kind() == ErrorKind::PermissionDenied => {
                Self::Denied(error.to_string())
            }
            Err(error) => Self::Unknown(error.to_string()),
        }
    }
}

/// How host directories are resolved: `std::fs::canonicalize`, but for tests.
type Resolve = std::sync::Arc<dyn Fn(&Path) -> std::io::Result<PathBuf> + Send + Sync>;

/// A host directory being resolved: since when, and what it resolves to
/// once that is known.
struct Resolving {
    since: std::time::Instant,
    outcome: tokio::sync::watch::Receiver<Option<Resolved>>,
}

/// The host directories being resolved, by any call.
fn resolving() -> std::sync::MutexGuard<'static, HashMap<PathBuf, Resolving>> {
    static RESOLVING: OnceLock<Mutex<HashMap<PathBuf, Resolving>>> = OnceLock::new();
    RESOLVING
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Start resolving `host` with `resolve` on a thread of its own, or join the
/// resolution of it under way, so a directory that never resolves holds one
/// thread, not one a call. `Err` when that resolution has run for `timeout`
/// already (it is blocked: no call waits for it any more) or no thread
/// could be started.
fn start_resolving(
    host: &Path,
    resolve: &Resolve,
    timeout: Duration,
) -> Result<tokio::sync::watch::Receiver<Option<Resolved>>, String> {
    let mut under_way = resolving();
    if let Some(resolving) = under_way.get(host) {
        if resolving.since.elapsed() < timeout {
            return Ok(resolving.outcome.clone());
        }
        return Err(format!(
            "an earlier resolution of it has not returned in {} s",
            resolving.since.elapsed().as_secs()
        ));
    }
    let (sender, receiver) = tokio::sync::watch::channel(None);
    let (resolve, thread_host) = (resolve.clone(), host.to_path_buf());
    std::thread::Builder::new()
        .name("axocoatl-resolve".into())
        .spawn(move || {
            /// No longer under way, however the thread ends.
            struct Done(PathBuf);
            impl Drop for Done {
                fn drop(&mut self) {
                    resolving().remove(&self.0);
                }
            }
            let done = Done(thread_host.clone());
            let outcome = Resolved::of(resolve(&thread_host));
            drop(done);
            sender.send_replace(Some(outcome));
        })
        .map_err(|error| format!("could not start resolving it: {error}"))?;
    // Still locked, so the thread's `Done` removes this after it is in.
    under_way.insert(
        host.to_path_buf(),
        Resolving {
            since: std::time::Instant::now(),
            outcome: receiver.clone(),
        },
    );
    Ok(receiver)
}

/// Resolve each of `hosts` with `resolve`, each on a thread of its own,
/// [`RESOLVE_THREADS`] at a time, for at most `timeout` in all. Not on
/// tokio's blocking pool: a runtime's shutdown waits for every blocking
/// task, and one blocked on a stale network filesystem never returns. Every
/// one of `hosts` is in the result; one not resolved in time is
/// [`Resolved::Unknown`], and its thread finishes on its own.
async fn resolve_all(
    hosts: HashSet<PathBuf>,
    resolve: &Resolve,
    timeout: Duration,
) -> HashMap<PathBuf, Resolved> {
    let deadline = tokio::time::Instant::now() + timeout;
    let late = format!("not resolved within {} s", timeout.as_secs_f32());
    let hosts: Vec<PathBuf> = hosts.into_iter().collect();
    let mut resolved = HashMap::new();
    for batch in hosts.chunks(RESOLVE_THREADS) {
        let mut waiting = Vec::new();
        for host in batch {
            if tokio::time::Instant::now() >= deadline {
                resolved.insert(host.clone(), Resolved::Unknown(late.clone()));
                continue;
            }
            match start_resolving(host, resolve, timeout) {
                Ok(outcome) => waiting.push((host.clone(), outcome)),
                Err(why) => {
                    resolved.insert(host.clone(), Resolved::Unknown(why));
                }
            }
        }
        for (host, mut outcome) in waiting {
            let outcome =
                match tokio::time::timeout_at(deadline, outcome.wait_for(Option::is_some)).await {
                    Ok(Ok(outcome)) => outcome.clone(),
                    Ok(Err(_)) => None,
                    Err(_) => Some(Resolved::Unknown(late.clone())),
                };
            resolved.insert(
                host,
                outcome.unwrap_or_else(|| {
                    Resolved::Unknown("its resolution ended without an answer".into())
                }),
            );
        }
    }
    resolved
}

/// What holds the directory spelled `spellings` among `containers`, each
/// host directory read with `read`: the first container, in Podman's order,
/// that has a mount on it ([`Kept::Mounted`]), else the first whose
/// processes may mount on it ([`Kept::MayMount`]), so that what is said
/// names a container that holds it rather than one that only could.
fn held_by(
    containers: &[Mounts],
    spellings: &[PathBuf],
    read: impl Fn(&Path) -> Option<PathBuf>,
) -> Option<Kept> {
    let mut may = None;
    for container in containers {
        match container.over(spellings, &read) {
            Some(mounted @ Kept::Mounted { .. }) => return Some(mounted),
            Some(kept) => {
                may.get_or_insert(kept);
            }
            None => {}
        }
    }
    may
}

/// The mount points among `unmounted` that one of `containers` has a mount
/// on, or may mount on, with why ([`held_by`]), or that the host cannot say
/// about ([`Kept::Unknown`]).
/// Each host directory is compared by its spelling with the mount point's
/// path and the path that resolves to now first; only when that finds no
/// container with a mount on one are they resolved, with `resolve`, for at
/// most `timeout`: a container whose host directory is spelled through a
/// link may have a mount on it where another only may mount.
async fn decide(
    unmounted: Vec<PathBuf>,
    containers: &[Mounts],
    resolve: &Resolve,
    timeout: Duration,
) -> HashMap<PathBuf, Kept> {
    let mut kept = HashMap::new();
    let mut undecided = Vec::new();
    for path in unmounted {
        // This process's own path (canonical when it was made), never a
        // container's.
        let mut spellings = vec![path.clone()];
        if let Ok(canonical) = std::fs::canonicalize(&path) {
            if canonical != path {
                spellings.push(canonical);
            }
        }
        match held_by(containers, &spellings, |host| Some(host.to_path_buf())) {
            Some(mounted @ Kept::Mounted { .. }) => {
                kept.insert(path, mounted);
            }
            may => undecided.push((path, spellings, may)),
        }
    }
    if undecided.is_empty() {
        return kept;
    }
    let hosts: HashSet<PathBuf> = containers
        .iter()
        .flat_map(|container| container.hosts.iter().map(|host| host.path.clone()))
        .collect();
    let resolved = resolve_all(hosts, resolve, timeout).await;
    // The first container, in Podman's order, whose mounts cannot be told.
    let unknown = containers.iter().find_map(|container| {
        container.unknown.clone().or_else(|| {
            container
                .hosts
                .iter()
                .find_map(|host| match resolved.get(&host.path) {
                    Some(Resolved::Denied(why)) => Some(host.unresolved(&container.id, why, true)),
                    Some(Resolved::Unknown(why)) => {
                        Some(host.unresolved(&container.id, why, false))
                    }
                    _ => None,
                })
        })
    });
    for (path, spellings, may) in undecided {
        let held = held_by(containers, &spellings, |host| match resolved.get(host) {
            Some(Resolved::To(real)) => Some(real.clone()),
            _ => None,
        });
        let why = match (held, may) {
            (Some(mounted @ Kept::Mounted { .. }), _) => Some(mounted),
            (held, may) => may.or(held).or_else(|| unknown.clone().map(Kept::Unknown)),
        };
        if let Some(why) = why {
            kept.insert(path, why);
        }
    }
    kept
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

/// The mount points among `candidates` that a listed container has a mount
/// on, or that Podman or the host cannot say about, each with why.
async fn in_use(listed: &[Listed], candidates: &[(PathBuf, u64)]) -> HashMap<PathBuf, Kept> {
    let mounted = mounted(listed);
    let (listed_here, unmounted): (Vec<PathBuf>, Vec<PathBuf>) = candidates
        .iter()
        .map(|(path, _)| path.clone())
        .partition(|path| mounted.contains(path));
    let mut kept: HashMap<PathBuf, Kept> = listed_here
        .into_iter()
        .map(|path| (path, Kept::Listed))
        .collect();
    if unmounted.is_empty() {
        return kept;
    }
    let containers = match containers(listed).await {
        Ok(containers) => containers,
        Err(error) => {
            tracing::debug!(
                %error,
                "kept Node dependency mount points: Podman did not say which containers mount them"
            );
            kept.extend(
                unmounted
                    .into_iter()
                    .map(|path| (path, Kept::Unknown(error.clone()))),
            );
            return kept;
        }
    };
    let resolve: Resolve = std::sync::Arc::new(|path: &Path| std::fs::canonicalize(path));
    kept.extend(decide(unmounted, &containers, &resolve, RESOLVE_TIMEOUT).await);
    kept
}

/// What every listed container mounts, from one `podman container inspect`
/// of them all and one `podman volume inspect` of the volumes they mount.
async fn containers(listed: &[Listed]) -> Result<Vec<Mounts>, String> {
    let ids = every_id(listed).ok_or("a listed container has no id")?;
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let inspected: Vec<Inspected> = inspect_existing(
        &["container", "inspect"],
        INSPECT_FORMAT,
        &ids,
        "container",
        |container: &Inspected| container.id.as_deref(),
        podman_output,
    )
    .await?;
    let mut names: Vec<String> = inspected
        .iter()
        .flat_map(|container| container.mounts.iter().flatten())
        .filter(|mount| mount.is_volume())
        .filter_map(|mount| mount.name.clone())
        .filter(|name| plausible_volume_name(name))
        .collect();
    names.sort();
    names.dedup();
    let inspected_volumes: Vec<InspectedVolume> = if names.is_empty() {
        Vec::new()
    } else {
        inspect_existing(
            &["volume", "inspect"],
            VOLUME_FORMAT,
            &names,
            "volume",
            |volume: &InspectedVolume| volume.name.as_deref(),
            podman_output,
        )
        .await?
    };
    let volumes: HashMap<String, Option<PathBuf>> = inspected_volumes
        .iter()
        .filter_map(|volume| Some((volume.name.clone()?, volume.bound())))
        .collect();
    let mut mounts = inspected
        .iter()
        .map(|container| {
            let listed_mounts = listed
                .iter()
                .find(|entry| entry.id.is_some() && entry.id == container.id)
                .and_then(|entry| entry.mounts.as_deref())
                .unwrap_or_default();
            Mounts::of(container, listed_mounts, &volumes)
        })
        .collect::<Result<Vec<Mounts>, String>>()?;
    take_volumes_from(&mut mounts, &inspected, listed);
    plugin_volumes_unknown(&mut mounts, &inspected, &inspected_volumes);
    Ok(mounts)
}

/// Mark each of `containers` that mounts a volume of a driver other than
/// `local` (a volume plugin) as one whose mounts cannot be told: Podman does
/// not report which host directory such a volume uses.
fn plugin_volumes_unknown(
    containers: &mut [Mounts],
    inspected: &[Inspected],
    volumes: &[InspectedVolume],
) {
    let plugins: HashMap<&str, &str> = volumes
        .iter()
        .filter_map(|volume| {
            let driver = volume
                .driver
                .as_deref()
                .filter(|driver| *driver != "local")?;
            Some((volume.name.as_deref()?, driver))
        })
        .collect();
    if plugins.is_empty() {
        return;
    }
    for (container, mounts) in inspected.iter().zip(containers.iter_mut()) {
        let plugin = container
            .mounts
            .iter()
            .flatten()
            .filter(|mount| mount.is_volume())
            .find_map(|mount| {
                let name = mount.name.as_deref()?;
                Some((name, *plugins.get(name)?))
            });
        if let Some((name, driver)) = plugin {
            mounts.unknown.get_or_insert_with(|| {
                format!(
                    "container {} mounts volume {name} of the volume plugin {driver}, and Podman \
                     does not say which host directory such a volume uses. Node dependency mount \
                     points Axocoatl made stay while container {} exists",
                    short_id(&mounts.id),
                    short_id(&mounts.id)
                )
            });
        }
    }
}

/// One bounded `podman` command, its standard output.
async fn podman_output(args: Vec<String>) -> Result<Vec<u8>, String> {
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
    parse_json_array(&podman_output(args).await?)
}

/// One `podman <command> --format <format> <names>` (checked names, never
/// taken for options) that prints one JSON object a line, each named by
/// `key`; `run` runs Podman. When Podman says some of `names` do not exist
/// ([`gone`]), they went since they were listed: once more without them.
/// An error unless Podman printed every name it was last asked about.
async fn inspect_existing<T, Run, Ran>(
    command: &[&str],
    format: &str,
    names: &[String],
    what: &str,
    key: fn(&T) -> Option<&str>,
    run: Run,
) -> Result<Vec<T>, String>
where
    T: serde::de::DeserializeOwned,
    Run: Fn(Vec<String>) -> Ran,
    Ran: std::future::Future<Output = Result<Vec<u8>, String>>,
{
    let once = |names: &[String]| {
        let mut args: Vec<String> = command.iter().map(|arg| arg.to_string()).collect();
        args.extend(["--format".to_string(), format.to_string()]);
        args.extend_from_slice(names);
        let ran = run(args);
        let names = names.to_vec();
        async move {
            let printed: Vec<T> = parse_json_lines(&ran.await?)?;
            match names
                .iter()
                .find(|name| !printed.iter().any(|one| key(one) == Some(name.as_str())))
            {
                Some(name) => Err(format!("Podman did not inspect {what} {name}")),
                None => Ok(printed),
            }
        }
    };
    let error = match once(names).await {
        Ok(printed) => return Ok(printed),
        Err(error) => error,
    };
    let gone = gone(&error, what, names);
    if gone.is_empty() {
        return Err(error);
    }
    let rest: Vec<String> = names
        .iter()
        .filter(|name| !gone.contains(name.as_str()))
        .cloned()
        .collect();
    if rest.is_empty() {
        return Ok(Vec::new());
    }
    once(&rest)
        .await
        .map_err(|again| format!("{error}; then, without the {what}s gone since: {again}"))
}

/// The `names` Podman's `error` says do not exist: one line `no such <what>
/// <name>` each, the last after `Error: `, the name quoted from Podman 5 on;
/// or, for one removed while `inspect` ran (Podman 6.1.4), `no container
/// with ID <id> found in database: no such container`.
fn gone<'a>(error: &str, what: &str, names: &'a [String]) -> HashSet<&'a str> {
    let prefix = format!("no such {what} ");
    let with_id = format!("no {what} with ID ");
    let in_database = format!(" found in database: no such {what}");
    error
        .lines()
        .filter_map(|line| {
            let line = line.trim();
            let line = line.strip_prefix("Error: ").unwrap_or(line);
            let name = line
                .strip_prefix(&prefix)
                .or_else(|| line.strip_prefix(&with_id)?.strip_suffix(&in_database))?
                .trim()
                .trim_matches('"');
            names
                .iter()
                .find(|wanted| wanted.as_str() == name)
                .map(String::as_str)
        })
        .collect()
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

/// Say why a release kept the mount point at `path`. When Axocoatl cannot
/// tell, a container it cannot see could have a mount on it, or a
/// container's processes could mount on it themselves, at `info`, which the
/// daemon shows by default: the directory stays, and nothing else tells the
/// person why. Each such reason is said once for each mount point in a
/// process (for a container that could mount on it, once for each container
/// and place), not at every container start and removal.
fn log_kept(path: &Path, why: &Kept) {
    match why {
        Kept::Unknown(why) if first_said(path, why) => tracing::info!(
            path = %path.display(),
            %why,
            "kept the empty Node dependency mount point Axocoatl made: it cannot tell whether a \
             container has a mount on it, and removes it at a later container start or removal \
             once it can"
        ),
        Kept::Elsewhere(why) if first_said(path, why) => tracing::info!(
            path = %path.display(),
            %why,
            "kept the empty Node dependency mount point Axocoatl made: a container it cannot see \
             could have a mount on it, so it stays, empty and yours"
        ),
        Kept::MayMount {
            container,
            source,
            at,
            why,
        } if first_said(path, &format!("{container} may mount at {}", at.display())) => {
            tracing::info!(
                path = %path.display(),
                container = %short_id(container),
                source = %source.display(),
                at = %at.display(),
                %why,
                "kept the empty Node dependency mount point Axocoatl made: a container that takes \
                 a directory above it could mount on it itself, so it stays while that container \
                 exists"
            )
        }
        Kept::Unknown(_) | Kept::Elsewhere(_) | Kept::MayMount { .. } => {}
        Kept::Listed | Kept::Mounted { .. } => tracing::debug!(
            path = %path.display(),
            ?why,
            "kept the Node dependency mount point Axocoatl made"
        ),
    }
}

/// Whether this process has not said `why` for `path` yet (and now has).
fn first_said(path: &Path, why: &str) -> bool {
    static SAID: OnceLock<Mutex<HashSet<(PathBuf, String)>>> = OnceLock::new();
    let mut said = SAID
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    // Bounded: a long-lived daemon with many repositories says some again.
    if said.len() >= 4096 {
        said.clear();
    }
    said.insert((path.to_path_buf(), why.to_string()))
}

/// Live tests of the removal itself run as if [`held_elsewhere`] found
/// nothing on the computer they run on.
#[cfg(test)]
pub(crate) static ASSUME_NOTHING_ELSEWHERE: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

#[cfg(test)]
thread_local! {
    /// What [`held_elsewhere`] finds, on this thread, for a test that sets
    /// it (a `#[tokio::test]` runs what it spawns on its own thread).
    static HELD_ELSEWHERE_ON_THIS_THREAD: std::cell::RefCell<Option<String>> =
        const { std::cell::RefCell::new(None) };
}

/// Why a container the `podman` this process runs does not list could have
/// a mount on a directory Axocoatl made, when one could; `None` when none
/// could. Removing the directory breaks such a mount, and Axocoatl cannot ask
/// about it, so then every mount point stays, empty and the person's:
/// - Docker: a `docker` program on `PATH` that is not Podman's own `docker`
///   (a link to `podman`, or a script that runs it), `DOCKER_HOST` (unless
///   it names this user's rootless Podman socket, as a Podman machine sets
///   it), or a Docker socket (`/var/run/docker.sock`,
///   `~/.docker/run/docker.sock`, `$XDG_RUNTIME_DIR/docker.sock`);
/// - another Podman connection than the one this process uses (`podman
///   system connection list`): another machine, or a machine's rootful
///   connection, as every Podman machine on macOS has;
/// - rootful Podman on Linux (`/run/podman`, which the podman packages of
///   Fedora and Ubuntu make at every boot), when this process is not root:
///   `sudo podman` containers are not listed to the person.
pub(crate) async fn held_elsewhere() -> Option<String> {
    #[cfg(test)]
    if let Some(found) = HELD_ELSEWHERE_ON_THIS_THREAD.with(|found| found.borrow().clone()) {
        return Some(found);
    }
    #[cfg(test)]
    if ASSUME_NOTHING_ELSEWHERE.load(std::sync::atomic::Ordering::Relaxed) {
        return None;
    }
    held_elsewhere_here().await
}

/// [`held_elsewhere`] on this computer, as it is.
pub(crate) async fn held_elsewhere_here() -> Option<String> {
    let env = |name: &str| std::env::var_os(name).filter(|value| !value.is_empty());
    let mut sockets = vec![PathBuf::from("/var/run/docker.sock")];
    if let Some(home) = env("HOME") {
        sockets.push(Path::new(&home).join(".docker/run/docker.sock"));
    }
    let runtime = env("XDG_RUNTIME_DIR").map(PathBuf::from);
    if let Some(runtime) = &runtime {
        sockets.push(runtime.join("docker.sock"));
    }
    // This user's rootless Podman socket, which a Podman machine (Fedora
    // CoreOS) names in DOCKER_HOST.
    let own_podman = runtime
        .or_else(|| cfg!(unix).then(|| PathBuf::from(format!("/run/user/{}", current_uid()))))
        .map(|runtime| runtime.join("podman/podman.sock"));
    let mut found: Vec<String> = Vec::new();
    found.extend(docker_engine(
        env("PATH").as_deref(),
        &sockets,
        env("DOCKER_HOST").as_deref(),
        own_podman.as_deref(),
    ));
    let connection = env("CONTAINER_CONNECTION");
    let connection = connection.as_deref().and_then(OsStr::to_str);
    match podman_output(
        ["system", "connection", "list", "--format", "json"]
            .map(String::from)
            .to_vec(),
    )
    .await
    .and_then(|stdout| other_connections(&stdout, connection, env("CONTAINER_HOST").is_some()))
    {
        Ok(others) if others.is_empty() => {}
        Ok(others) => found.push(format!(
            "Podman has the connection{} {}, whose containers this one does not list",
            if others.len() == 1 { "" } else { "s" },
            others.join(", ")
        )),
        Err(error) => found.push(format!("Podman did not list its connections: {error}")),
    }
    found.extend(rootful_podman(
        Path::new("/"),
        cfg!(target_os = "linux"),
        running_as_root(),
    ));
    (!found.is_empty()).then(|| found.join("; "))
}

#[cfg(unix)]
fn running_as_root() -> bool {
    rustix::process::geteuid().is_root()
}

#[cfg(not(unix))]
fn running_as_root() -> bool {
    false
}

#[cfg(unix)]
fn current_uid() -> u32 {
    rustix::process::getuid().as_raw()
}

#[cfg(not(unix))]
fn current_uid() -> u32 {
    0
}

/// Docker, when `path` (`PATH`) has a `docker` that is not Podman's, one of
/// `sockets` exists, or `DOCKER_HOST` (`docker_host`) names another engine
/// than `own_podman`, this user's rootless Podman socket.
fn docker_engine(
    path: Option<&OsStr>,
    sockets: &[PathBuf],
    docker_host: Option<&OsStr>,
    own_podman: Option<&Path>,
) -> Option<String> {
    if let Some(docker_host) = docker_host {
        let named = docker_host
            .to_str()
            .and_then(|host| host.strip_prefix("unix://"));
        if named.is_none() || named.map(Path::new) != own_podman {
            return Some("DOCKER_HOST names a Docker engine".to_string());
        }
    }
    if let Some(socket) = sockets.iter().find(|socket| socket.exists()) {
        return Some(format!("a Docker API socket {} exists", socket.display()));
    }
    let docker = std::env::split_paths(path?)
        .map(|directory| directory.join("docker"))
        .find(|candidate| candidate.is_file())?;
    (!podman_as_docker(&docker)).then(|| format!("Docker is installed ({})", docker.display()))
}

/// Whether `docker` is Podman's own `docker` (podman-docker, Podman
/// Desktop): a link to `podman`, or a script that runs it.
fn podman_as_docker(docker: &Path) -> bool {
    let real = std::fs::canonicalize(docker).unwrap_or_else(|_| docker.to_path_buf());
    if real
        .file_name()
        .and_then(OsStr::to_str)
        .is_some_and(|name| name.starts_with("podman"))
    {
        return true;
    }
    use std::io::Read;
    let mut start = Vec::with_capacity(4096);
    let read = std::fs::File::open(&real)
        .and_then(|file| file.take(4096).read_to_end(&mut start))
        .is_ok();
    read && start.starts_with(b"#!") && start.windows(6).any(|window| window == b"podman")
}

/// The connections `podman system connection list --format json` printed
/// (`stdout`) other than the one this process uses: `connection`
/// (`CONTAINER_CONNECTION`) or the default one, or none when `CONTAINER_HOST`
/// (`container_host`) points elsewhere.
fn other_connections(
    stdout: &[u8],
    connection: Option<&str>,
    container_host: bool,
) -> Result<Vec<String>, String> {
    #[derive(serde::Deserialize)]
    struct Connection {
        #[serde(rename = "Name", default)]
        name: String,
        #[serde(rename = "Default", default)]
        default: bool,
    }
    let connections: Vec<Connection> = parse_json_array(stdout)?;
    Ok(connections
        .iter()
        .filter(|listed| match (connection, container_host) {
            (Some(used), _) => listed.name != used,
            (None, true) => true,
            (None, false) => !listed.default,
        })
        .map(|listed| listed.name.clone())
        .collect())
}

/// Rootful Podman, on Linux (`linux`) for a process that is not root:
/// `/run/podman` (under `root`) exists. It is the directory of rootful
/// Podman's API socket, which the podman packages of Fedora and Ubuntu make
/// at every boot (`D! /run/podman 0700 root root` in their tmpfiles.d, read
/// in Fedora CoreOS 44 and Ubuntu 24.04) whether or not that socket is
/// enabled: rootful Podman is installed, and `sudo podman` can run
/// containers without the API.
fn rootful_podman(root: &Path, linux: bool, as_root: bool) -> Option<String> {
    (linux && !as_root && root.join("run/podman").is_dir()).then(|| {
        "rootful Podman is installed (/run/podman exists), and the containers `sudo podman` \
         runs are not listed to you"
            .to_string()
    })
}

/// Remove every Node dependency mount point this process made that no start
/// is using and no container Podman lists (of any daemon, or the person's
/// own) has a mount on, if it is still the empty directory made for it, and
/// say what it decided for each one no start is using. Without one to
/// remove it asks Podman nothing. Best effort: when Podman cannot list its
/// containers, or Podman or the host cannot say what one mounts, or a
/// container it cannot see could have a mount on it ([`held_elsewhere`]),
/// everything it could concern stays for a later call.
pub(crate) async fn release_unused() -> Vec<(PathBuf, Decision)> {
    let candidates = registry().candidates();
    if candidates.is_empty() {
        return Vec::new();
    }
    let in_use = match held_elsewhere().await {
        Some(why) => candidates
            .iter()
            .map(|(path, _)| (path.clone(), Kept::Elsewhere(why.clone())))
            .collect(),
        None => match list(&["--external".to_string()]).await {
            Ok(listed) => {
                let (external, listed): (Vec<Listed>, Vec<Listed>) =
                    listed.into_iter().partition(Listed::external);
                match external.first() {
                    Some(external) => {
                        let why = format!(
                            "Podman lists container {} only with --external (a Buildah \
                             container, or a podman build's), and cannot say what it mounts",
                            external
                                .names
                                .iter()
                                .flatten()
                                .next()
                                .map(String::as_str)
                                .or(external.id.as_deref().map(short_id))
                                .unwrap_or("(unnamed)")
                        );
                        candidates
                            .iter()
                            .map(|(path, _)| (path.clone(), Kept::Elsewhere(why.clone())))
                            .collect()
                    }
                    None => in_use(&listed, &candidates).await,
                }
            }
            Err(error) => {
                tracing::debug!(%error, "kept Node dependency mount points: Podman did not list its containers");
                let why = format!("Podman did not list its containers: {error}");
                candidates
                    .iter()
                    .map(|(path, _)| (path.clone(), Kept::Unknown(why.clone())))
                    .collect()
            }
        },
    };
    let mut decided: Vec<(PathBuf, Decision)> = candidates
        .iter()
        .filter_map(|(path, _)| {
            let why = in_use.get(path)?;
            log_kept(path, why);
            Some((path.clone(), Decision::Kept(why.clone())))
        })
        .collect();
    for (path, outcome) in registry().release(candidates, &in_use) {
        match &outcome {
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
        decided.push((path, Decision::Released(outcome)));
    }
    decided
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_listing(stdout: &[u8]) -> Result<Vec<Listed>, String> {
        parse_json_array(stdout)
    }

    fn mounting(paths: &[&Path]) -> HashMap<PathBuf, Kept> {
        paths
            .iter()
            .map(|path| (path.to_path_buf(), Kept::Listed))
            .collect()
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
        registry.release(candidates, &HashMap::new())
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
        assert!(registry.release(Vec::new(), &HashMap::new()).is_empty());
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
        assert!(registry.release(stale, &HashMap::new()).is_empty());
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
        let in_use: HashMap<PathBuf, Kept> = mounted(&listed)
            .into_iter()
            .map(|path| (path, Kept::Listed))
            .collect();
        assert!(in_use.contains_key(&path));
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

    /// What Podman printed for `podman container inspect --format
    /// INSPECT_FORMAT` (the first five before it named `VolumesFrom`): 6.1.4,
    /// of a container run with `-v <repo>:/workspaces/x --tmpfs
    /// /workspaces/x/node_modules -v <repo>/node_modules:/deps`, of one with
    /// no mounts, of one with an image mount and a devpts mount, and of one
    /// with a bind volume; 4.9.3's `podman volume inspect --format
    /// VOLUME_FORMAT` of that bind volume and of one made with `type=bind`
    /// alone, and its inspect of a stopped container with an overlay volume,
    /// `-v <dir>:/o:O`, of a running one whose overlay volume names its
    /// upperdir and workdir, `-v <dir>:/o:O,upperdir=<dir>,workdir=<dir>`,
    /// and of a container run with `-v <repo>:/w` and one run with
    /// `--volumes-from <that one>:ro,z --tmpfs /w/node_modules`.
    #[test]
    fn inspect_output_parses_one_container_a_line() {
        let output = br#"{"Id":"15944f798e01584918a5d129eaa2ef79f149ec49ac42ae7783159f92c0c40760","Mounts":[{"Type":"bind","Source":"/r","Destination":"/workspaces/x","Driver":"","Mode":"","Options":["rbind"],"RW":true,"Propagation":"rprivate"},{"Type":"bind","Source":"/r/node_modules","Destination":"/deps","Driver":"","Mode":"","Options":["rbind"],"RW":true,"Propagation":"rprivate"}],"Tmpfs":{"/workspaces/x/node_modules":"rprivate,nosuid,nodev,tmpcopyup"}}
{"Id":"a1def83ac41616a3032bf95e99a81f599fcab63ad5b009dc930415f483fcb84f","Mounts":[],"Tmpfs":{}}
{"Id":"e82cad8350984f25aa5795c8e141c205814f58d53a7ec46f9ea0790b105d2178","Mounts":[{"Type":"image","Source":"docker.io/library/alpine:3.20","Destination":"/img"},{"Type":"bind","Source":"devpts","Destination":"/dp"}],"Tmpfs":{"/t":"rprivate,nosuid,nodev,tmpcopyup"}}
{"Id":"319e4c26fba24c9044488063e3f962ffa983167b588ae0084b8095aeec831532","Mounts":[{"Type":"volume","Name":"r4iso-bindvol","Source":"/var/home/core/.local/share/containers/storage/volumes/r4iso-bindvol/_data","Destination":"/deps","Driver":"local","Mode":"","Options":["nosuid","nodev","rbind"],"RW":true,"Propagation":"rprivate"}],"Tmpfs":{}}
{"Id":"cc0d62a44de0289ab5a198b85e70fd6d690b751d7ed5143f04ef906c6e56220d","Mounts":[{"Type":"bind","Source":"/home/r4iso/.local/share/containers/storage/overlay-containers/cc0d62a44de0289ab5a198b85e70fd6d690b751d7ed5143f04ef906c6e56220d/userdata/overlay/2342896064/merge","Destination":"/o","Driver":"","Mode":"","Options":["lowerdir=/tmp/r4iso-ov.Dud1/repo/node_modules","upperdir=/home/r4iso/.local/share/containers/storage/overlay-containers/cc0d62a44de0289ab5a198b85e70fd6d690b751d7ed5143f04ef906c6e56220d/userdata/overlay/2342896064/upper","workdir=/home/r4iso/.local/share/containers/storage/overlay-containers/cc0d62a44de0289ab5a198b85e70fd6d690b751d7ed5143f04ef906c6e56220d/userdata/overlay/2342896064/work","userxattr"],"RW":true,"Propagation":"private"}],"Tmpfs":{}}
{"Id":"b2e267e47c59759e49aa368a9b14c92db2302635552918b84d8f7b79024448e3","Mounts":[{"Type":"bind","Source":"/home/r5iso/.local/share/containers/storage/overlay-containers/b2e267e47c59759e49aa368a9b14c92db2302635552918b84d8f7b79024448e3/userdata/overlay/590688343/merge","Destination":"/o","Driver":"","Mode":"","Options":["lowerdir=/tmp/r5f.rA4P/other","upperdir=/tmp/r5f.rA4P/repo/node_modules","workdir=/tmp/r5f.rA4P/work","userxattr"],"RW":true,"Propagation":"private"}],"Tmpfs":{},"VolumesFrom":null}
{"Id":"165161b7d95aabeddcacc303d6f96e271475ed5be59ee9dfa2f31e531455d886","Mounts":[{"Type":"bind","Source":"/tmp/r5f.rA4P/repo","Destination":"/w","Driver":"","Mode":"","Options":["rbind"],"RW":true,"Propagation":"rprivate"}],"Tmpfs":{},"VolumesFrom":null}
{"Id":"69ff6ecdca310b756eba3529d365ff1f0dd2f03459c09584d714dd1004f4dc0b","Mounts":[],"Tmpfs":{"/w/node_modules":"rw,rprivate,nosuid,nodev,tmpcopyup"},"VolumesFrom":["r5f-a:ro","z"]}
"#;
        let inspected: Vec<Inspected> = parse_json_lines(output).unwrap();
        assert_eq!(inspected.len(), 8);
        let volumes: Vec<InspectedVolume> = parse_json_lines(
            br#"{"Name":"r4iso-bindvol","Driver":"local","Options":{"device":"/tmp/r4iso-src.hTid/repo/node_modules","o":"bind","type":"none"}}
{"Name":"r5a-vol-type-bind","Driver":"local","Options":{"device":"/tmp/r5a.PaHi/repo/node_modules","type":"bind"}}
"#,
        )
        .unwrap();
        let volumes: HashMap<String, Option<PathBuf>> = volumes
            .iter()
            .map(|volume| (volume.name.clone().unwrap(), volume.bound()))
            .collect();
        assert_eq!(
            volumes["r5a-vol-type-bind"].as_deref(),
            Some(Path::new("/tmp/r5a.PaHi/repo/node_modules"))
        );
        let mounts: Vec<Mounts> = inspected
            .iter()
            .map(|container| Mounts::of(container, &[], &volumes).unwrap())
            .collect();
        let hosts = |mounts: &Mounts| -> Vec<(String, String, HostKind)> {
            mounts
                .hosts
                .iter()
                .map(|host| {
                    (
                        host.path.display().to_string(),
                        host.at.display().to_string(),
                        host.kind.clone(),
                    )
                })
                .collect()
        };
        let host = |path: &str, at: &str, kind: HostKind| (path.to_string(), at.to_string(), kind);
        assert_eq!(
            hosts(&mounts[0]),
            [
                host("/r", "/workspaces/x", HostKind::Bind),
                host("/r/node_modules", "/deps", HostKind::Bind)
            ]
        );
        assert!(mounts[0]
            .destinations
            .contains(Path::new("/workspaces/x/node_modules")));
        assert!(hosts(&mounts[1]).is_empty() && mounts[1].destinations.is_empty());
        // An image's reference and `devpts` are not host directories; their
        // places are still mounts.
        assert!(hosts(&mounts[2]).is_empty());
        assert_eq!(
            mounts[2].destinations,
            ["/img", "/dp", "/t"]
                .into_iter()
                .map(PathBuf::from)
                .collect()
        );
        // The bind volume: its device, not its own directory.
        assert_eq!(
            hosts(&mounts[3]),
            [host(
                "/tmp/r4iso-src.hTid/repo/node_modules",
                "/deps",
                HostKind::VolumeDevice("r4iso-bindvol".into())
            )]
        );
        // An overlay volume: its merged directory, and the directories it
        // uses, in Podman's storage or given.
        let storage = "/home/r4iso/.local/share/containers/storage/overlay-containers/\
                       cc0d62a44de0289ab5a198b85e70fd6d690b751d7ed5143f04ef906c6e56220d/userdata/\
                       overlay/2342896064";
        assert_eq!(
            hosts(&mounts[4]),
            [
                host(&format!("{storage}/merge"), "/o", HostKind::OverlayMerged),
                host(
                    "/tmp/r4iso-ov.Dud1/repo/node_modules",
                    "/o",
                    HostKind::Overlay("lowerdir")
                ),
                host(
                    &format!("{storage}/upper"),
                    "/o",
                    HostKind::Overlay("upperdir")
                ),
                host(
                    &format!("{storage}/work"),
                    "/o",
                    HostKind::Overlay("workdir")
                ),
            ]
        );
        assert_eq!(
            hosts(&mounts[5])[1..],
            [
                host("/tmp/r5f.rA4P/other", "/o", HostKind::Overlay("lowerdir")),
                host(
                    "/tmp/r5f.rA4P/repo/node_modules",
                    "/o",
                    HostKind::Overlay("upperdir")
                ),
                host("/tmp/r5f.rA4P/work", "/o", HostKind::Overlay("workdir")),
            ]
        );
        // `--volumes-from`: its own inspect has its tmpfs and none of the
        // bind mounts it takes; 4.9.3 splits `r5f-a:ro,z` in two.
        assert!(hosts(&mounts[7]).is_empty());
        assert_eq!(
            inspected[7].volumes_from.as_deref(),
            Some(&["r5f-a:ro".to_string(), "z".to_string()][..])
        );
        assert_eq!(inspected[6].volumes_from, None);
        // A volume gone since (not printed) binds nothing.
        assert!(hosts(&Mounts::of(&inspected[3], &[], &HashMap::new()).unwrap()).is_empty());
        // `null` mounts and tmpfs, as an older Podman may print them.
        let empty: Vec<Inspected> =
            parse_json_lines(br#"{"Id":"ab","Mounts":null,"Tmpfs":null}"#).unwrap();
        let empty = Mounts::of(&empty[0], &[], &HashMap::new()).unwrap();
        assert!(empty.hosts.is_empty() && empty.destinations.is_empty());
        assert!(parse_json_lines::<Inspected>(b"{\"Id\":").is_err());
    }

    #[test]
    fn only_a_local_volume_made_to_bind_binds_a_host_directory() {
        let volume = |driver: Option<&str>, options: &[(&str, &str)]| InspectedVolume {
            name: Some("v".into()),
            driver: driver.map(str::to_string),
            options: Some(
                options
                    .iter()
                    .map(|(key, value)| (key.to_string(), value.to_string()))
                    .collect(),
            ),
        };
        let device = ("device", "/r/node_modules");
        for o in ["bind", "rbind", "ro,bind", "bind,ro", " rbind "] {
            assert_eq!(
                volume(Some("local"), &[("type", "none"), ("o", o), device]).bound(),
                Some(PathBuf::from("/r/node_modules")),
                "{o:?}"
            );
        }
        assert!(volume(None, &[("o", "bind"), device]).bound().is_some());
        // `type=bind`, with or without `o`: Podman mounts it with `-o bind`.
        for options in [
            vec![("type", "bind"), device],
            vec![("type", "bind"), ("o", "ro"), device],
            vec![("type", "bind"), ("o", "uid=1000"), device],
        ] {
            assert_eq!(
                volume(Some("local"), &options).bound(),
                Some(PathBuf::from("/r/node_modules")),
                "{options:?}"
            );
        }
        for (driver, options) in [
            (Some("local"), vec![("o", "uid=1000"), device]),
            (Some("local"), vec![("type", "tmpfs"), ("device", "tmpfs")]),
            // `mount -t rbind`, which fails the container's start.
            (Some("local"), vec![("type", "rbind"), device]),
            (
                Some("local"),
                vec![("o", "bind"), ("device", "r/node_modules")],
            ),
            (
                Some("local"),
                vec![("type", "bind"), ("device", "r/node_modules")],
            ),
            (Some("local"), vec![("o", "bind")]),
            (Some("local"), vec![("type", "bind")]),
            (Some("local"), vec![device]),
            (Some("some-plugin"), vec![("o", "bind"), device]),
            (Some("some-plugin"), vec![("type", "bind"), device]),
        ] {
            assert_eq!(
                volume(driver, &options).bound(),
                None,
                "{driver:?} {options:?}"
            );
        }
        assert_eq!(InspectedVolume::default().bound(), None);

        for name in ["v", "r4iso-bindvol", "a.b_c-d", &"f".repeat(64)] {
            assert!(plausible_volume_name(name), "{name:?}");
        }
        for name in ["", "-v", ".v", "_v", "a b", "a/b", "a\nb", &"f".repeat(256)] {
            assert!(!plausible_volume_name(name), "{name:?}");
        }
        // A volume mount without a name Podman can be asked about cannot be
        // told apart.
        let container = Inspected {
            id: Some("c".repeat(64)),
            mounts: Some(vec![InspectedMount {
                kind: Some("volume".into()),
                name: Some("-x".into()),
                source: Some("/s".into()),
                destination: Some("/d".into()),
                options: None,
            }]),
            ..Inspected::default()
        };
        assert!(Mounts::of(&container, &[], &HashMap::new()).is_err());
    }

    fn canonicalizing() -> Resolve {
        std::sync::Arc::new(|path: &Path| std::fs::canonicalize(path))
    }

    /// A container with these host directories (as spelled, bind sources)
    /// at these places, and these other mounts.
    fn container(id: &str, hosts: &[(&Path, &str)], others: &[&str]) -> Mounts {
        Mounts {
            id: id.to_string(),
            hosts: hosts
                .iter()
                .map(|(host, at)| Host::new(*host, Path::new(at), HostKind::Bind))
                .collect(),
            destinations: hosts
                .iter()
                .map(|(_, at)| PathBuf::from(at))
                .chain(others.iter().map(PathBuf::from))
                .collect(),
            may_mount: None,
            unknown: None,
        }
    }

    async fn decided(path: &Path, containers: &[Mounts]) -> Option<Kept> {
        decide(
            vec![path.to_path_buf()],
            containers,
            &canonicalizing(),
            Duration::from_secs(5),
        )
        .await
        .remove(path)
    }

    fn mounted_by(id: &str, source: &Path, at: &str) -> Option<Kept> {
        Some(Kept::Mounted {
            container: id.to_string(),
            source: source.to_path_buf(),
            at: PathBuf::from(at),
        })
    }

    /// What [`Inspected::may_mount`] says of a container nothing confines.
    const UNCONFINED: &str = "neither SELinux (container_t, enforcing) nor AppArmor \
                              (containers-default) confines its mounts, so its processes can \
                              mount in a user namespace of their own";

    fn may_mount_by(id: &str, source: &Path, at: &str, why: &'static str) -> Option<Kept> {
        Some(Kept::MayMount {
            container: id.to_string(),
            source: source.to_path_buf(),
            at: PathBuf::from(at),
            why,
        })
    }

    /// Every container Podman lists counts: one of the person's own has a
    /// mount on the directory Axocoatl made when it bind-mounts it, wherever,
    /// or bind-mounts a directory above it and mounts a volume, a bind or a
    /// tmpfs at the place that is the directory, by its spelling or through
    /// a link; and when it mounts a bind volume of it.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_container_with_a_mount_on_it_keeps_it() {
        let repo = tempfile::tempdir().unwrap();
        let repo_path = std::fs::canonicalize(repo.path()).unwrap();
        let path = repo_path.join(NODE_MODULES);
        std::fs::create_dir(&path).unwrap();
        let other = tempfile::tempdir().unwrap();
        let other_path = std::fs::canonicalize(other.path())
            .unwrap()
            .join(NODE_MODULES);
        std::fs::create_dir(&other_path).unwrap();
        let links = tempfile::tempdir().unwrap();
        let link = links.path().join("repo-link");
        std::os::unix::fs::symlink(&repo_path, &link).unwrap();

        // A development container: the repository (as the person spelled
        // it, not canonical on macOS) and a volume on its node_modules.
        let development = container(
            "d",
            &[(repo.path(), "/workspaces/x")],
            &["/workspaces/x/node_modules"],
        );
        assert_eq!(
            decided(&path, std::slice::from_ref(&development)).await,
            mounted_by("d", repo.path(), "/workspaces/x/node_modules")
        );
        assert_eq!(
            decided(&other_path, std::slice::from_ref(&development)).await,
            None
        );
        // One that binds the repository's parent sees it one level down.
        let below = Path::new("/src")
            .join(repo_path.file_name().unwrap())
            .join(NODE_MODULES);
        let parent = container(
            "p",
            &[(repo_path.parent().unwrap(), "/src")],
            &[below.to_str().unwrap()],
        );
        assert_eq!(
            decided(&path, &[parent]).await,
            mounted_by("p", repo_path.parent().unwrap(), below.to_str().unwrap())
        );
        // One that bind-mounts the directory itself, wherever, spelled as the
        // person did, or through a link.
        let spelled = repo.path().join(NODE_MODULES);
        assert_eq!(
            decided(&path, &[container("i", &[(&spelled, "/deps")], &[])]).await,
            mounted_by("i", &spelled, "/deps")
        );
        let linked = link.join(NODE_MODULES);
        assert_eq!(
            decided(&path, &[container("l", &[(&linked, "/deps")], &[])]).await,
            mounted_by("l", &linked, "/deps")
        );
        assert_eq!(
            decided(&other_path, &[container("l", &[(&linked, "/deps")], &[])]).await,
            None
        );
        // The repository through a link, with a tmpfs (or a mount `podman
        // ps` lists) on its node_modules.
        assert_eq!(
            decided(
                &path,
                &[container("t", &[(&link, "/w")], &["/w/node_modules"])]
            )
            .await,
            mounted_by("t", &link, "/w/node_modules")
        );
        // A bind of the repository with nothing mounted below it does not.
        assert_eq!(
            decided(&path, &[container("n", &[(&repo_path, "/w")], &[])]).await,
            None
        );
        assert_eq!(
            decided(
                &path,
                &[container(
                    "n",
                    &[(&repo_path, "/w")],
                    &["/w/node_modules/x"]
                )]
            )
            .await,
            None
        );
        // Nor does a bind of something inside it, or of a sibling.
        let inside = repo_path.join("src");
        std::fs::create_dir(&inside).unwrap();
        assert_eq!(
            decided(
                &path,
                &[container("s", &[(&inside, "/src")], &["/src/node_modules"])]
            )
            .await,
            None
        );
        // A bind volume's device is a host directory like a bind's source
        // (`Mounts::of`), here through the link.
        let volume = Inspected {
            id: Some("v".repeat(64)),
            mounts: Some(vec![InspectedMount {
                kind: Some("volume".into()),
                name: Some("bindvol".into()),
                source: Some("/storage/volumes/bindvol/_data".into()),
                destination: Some("/deps".into()),
                options: None,
            }]),
            ..Inspected::default()
        };
        let volumes = [("bindvol".to_string(), Some(linked.clone()))].into();
        let bound = Mounts::of(&volume, &[], &volumes).unwrap();
        assert_eq!(
            decided(&path, &[bound]).await,
            mounted_by(&"v".repeat(64), &linked, "/deps")
        );
        // The same volume without `o=bind`: its own directory is all it is.
        let plain = Mounts::of(&volume, &[], &[("bindvol".to_string(), None)].into()).unwrap();
        assert_eq!(decided(&path, &[plain]).await, None);
    }

    /// A subscriber that keeps each event's level and fields.
    #[derive(Clone, Default)]
    struct Seen(std::sync::Arc<Mutex<Vec<(tracing::Level, String)>>>);

    impl tracing::Subscriber for Seen {
        fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
            true
        }
        fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
            tracing::span::Id::from_u64(1)
        }
        fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
        fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
        fn event(&self, event: &tracing::Event<'_>) {
            struct Fields(String);
            impl tracing::field::Visit for Fields {
                fn record_debug(
                    &mut self,
                    field: &tracing::field::Field,
                    value: &dyn std::fmt::Debug,
                ) {
                    self.0.push_str(&format!("{}={value:?};", field.name()));
                }
            }
            let mut fields = Fields(String::new());
            event.record(&mut fields);
            self.0
                .lock()
                .unwrap()
                .push((*event.metadata().level(), fields.0));
        }
        fn enter(&self, _: &tracing::span::Id) {}
        fn exit(&self, _: &tracing::span::Id) {}
    }

    /// A mount point kept because Axocoatl cannot tell is said at `info`,
    /// the daemon's default, with why; the other keeps only at `debug`.
    #[test]
    fn a_keep_it_cannot_explain_otherwise_is_said_at_info_with_why() {
        let seen = Seen::default();
        let path = Path::new("/r/node_modules");
        tracing::subscriber::with_default(seen.clone(), || {
            log_kept(
                path,
                &Kept::Unknown("could not resolve /s, the merged directory".into()),
            );
            log_kept(path, &Kept::Listed);
        });
        let seen = seen.0.lock().unwrap();
        assert_eq!(seen.len(), 2, "{seen:?}");
        assert_eq!(seen[0].0, tracing::Level::INFO);
        for said in [
            "path=/r/node_modules;",
            "why=could not resolve /s, the merged directory;",
            "it cannot tell whether a container has a mount on it",
        ] {
            assert!(seen[0].1.contains(said), "{said:?} in {:?}", seen[0].1);
        }
        assert_eq!(seen[1].0, tracing::Level::DEBUG);
    }

    /// A keep Axocoatl cannot explain otherwise is said once for each mount
    /// point and reason, not at every container start and removal.
    #[test]
    fn a_keep_is_said_once_for_each_mount_point_and_reason() {
        let seen = Seen::default();
        let path = Path::new("/said-once/node_modules");
        let docker = Kept::Elsewhere("Docker is installed (/usr/local/bin/docker)".into());
        tracing::subscriber::with_default(seen.clone(), || {
            log_kept(path, &docker);
            log_kept(path, &docker);
            log_kept(
                path,
                &Kept::Unknown("Podman did not list its containers".into()),
            );
            log_kept(
                path,
                &Kept::Unknown("Podman did not list its containers".into()),
            );
            log_kept(Path::new("/said-once-too/node_modules"), &docker);
        });
        let seen = seen.0.lock().unwrap();
        assert_eq!(seen.len(), 3, "{seen:?}");
        assert!(seen.iter().all(|(level, _)| *level == tracing::Level::INFO));
        assert!(seen[0].1.contains("a container it cannot see"), "{seen:?}");
        assert!(seen[0].1.contains("why=Docker is installed"), "{seen:?}");
    }

    /// A release asks first whether a container Axocoatl cannot list could
    /// hold a mount point ([`held_elsewhere`]): while one could, the mount
    /// point a dropped lease leaves (after its container start) stays, the
    /// release says so once at `info` with why, and Podman is not asked
    /// (none runs here).
    #[cfg(unix)]
    #[tokio::test]
    async fn a_mount_point_stays_while_a_container_elsewhere_could_hold_it() {
        let repo = tempfile::tempdir().unwrap();
        let repo = std::fs::canonicalize(repo.path()).unwrap();
        let path = repo.join(NODE_MODULES);
        let why = "Docker is installed (/usr/local/bin/docker)";
        HELD_ELSEWHERE_ON_THIS_THREAD.with(|found| *found.borrow_mut() = Some(why.into()));
        let seen = Seen::default();
        let said = tracing::subscriber::set_default(seen.clone());
        let lease = MountpointLease::acquire(&repo);
        assert!(path.is_dir());
        drop(lease);
        lease_releases::settled().await;
        assert!(path.is_dir(), "kept by the release the lease ran");
        let decided: Vec<(PathBuf, Decision)> = release_unused()
            .await
            .into_iter()
            .filter(|(decided, _)| *decided == path)
            .collect();
        assert_eq!(
            decided,
            [(path.clone(), Decision::Kept(Kept::Elsewhere(why.into())))]
        );
        assert!(path.is_dir());
        drop(said);
        let seen: Vec<(tracing::Level, String)> = seen
            .0
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, fields)| fields.contains(&format!("path={};", path.display())))
            .cloned()
            .collect();
        assert_eq!(seen.len(), 1, "said once for two releases: {seen:?}");
        assert_eq!(seen[0].0, tracing::Level::INFO);
        assert!(seen[0].1.contains(&format!("why={why};")), "{seen:?}");
        assert!(
            seen[0]
                .1
                .contains("a container it cannot see could have a mount on it"),
            "{seen:?}"
        );
        registry().made.remove(&path);
        HELD_ELSEWHERE_ON_THIS_THREAD.with(|found| found.borrow_mut().take());
    }

    /// Docker counts when a `docker` on PATH is not Podman's own, when one
    /// of its sockets exists, or when DOCKER_HOST is set; Podman's `docker`
    /// (a link to podman, or podman-docker's script) does not.
    #[cfg(unix)]
    #[test]
    fn docker_counts_unless_its_docker_is_podman() {
        use std::os::unix::fs::{symlink, PermissionsExt};
        let bin = tempfile::tempdir().unwrap();
        let path = std::env::join_paths([bin.path()]).unwrap();
        let none: Vec<PathBuf> = vec![bin.path().join("no-such.sock")];
        assert_eq!(docker_engine(Some(&path), &none, None, None), None);
        assert_eq!(docker_engine(None, &none, None, None), None);
        // Podman's own docker: a link to podman ...
        std::fs::write(bin.path().join("podman"), b"\x7fELF").unwrap();
        symlink(bin.path().join("podman"), bin.path().join("docker")).unwrap();
        assert_eq!(docker_engine(Some(&path), &none, None, None), None);
        // ... or podman-docker's script.
        std::fs::remove_file(bin.path().join("docker")).unwrap();
        std::fs::write(
            bin.path().join("docker"),
            "#!/bin/sh\n[ -e /etc/containers/nodocker ] || echo 'Emulate Docker CLI using podman.'\nexec /usr/bin/podman \"$@\"\n",
        )
        .unwrap();
        std::fs::set_permissions(
            bin.path().join("docker"),
            std::fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        assert_eq!(docker_engine(Some(&path), &none, None, None), None);
        // Docker's own CLI.
        std::fs::write(bin.path().join("docker"), b"\x7fELF docker cli").unwrap();
        let docker = docker_engine(Some(&path), &none, None, None).unwrap();
        assert!(docker.starts_with("Docker is installed"), "{docker}");
        // A socket, or DOCKER_HOST, without the program.
        let empty = std::env::join_paths([bin.path().join("empty")]).unwrap();
        let socket = bin.path().join("docker.sock");
        std::fs::write(&socket, b"").unwrap();
        let said = docker_engine(Some(&empty), std::slice::from_ref(&socket), None, None).unwrap();
        assert!(said.contains(&socket.display().to_string()), "{said}");
        let own = Path::new("/run/user/501/podman/podman.sock");
        let docker_host =
            |value: &str| docker_engine(Some(&empty), &none, Some(OsStr::new(value)), Some(own));
        assert!(docker_host("unix:///var/run/docker.sock")
            .unwrap()
            .contains("DOCKER_HOST"));
        assert!(docker_host("tcp://127.0.0.1:2375").is_some());
        // A Podman machine (Fedora CoreOS) names the user's own Podman.
        assert_eq!(docker_host("unix:///run/user/501/podman/podman.sock"), None);
        assert!(docker_host("unix:///run/podman/podman.sock").is_some());
    }

    /// The connections Podman lists besides the one this process uses: on
    /// a Mac, a machine's rootful twin.
    #[test]
    fn other_podman_connections_are_those_this_process_does_not_use() {
        // `podman system connection list --format json`, Podman 6.1.4.
        let mac = br#"[{"Name":"podman-machine-default","URI":"ssh://core@127.0.0.1:59451/run/user/501/podman/podman.sock","Identity":"/Users/a/.local/share/containers/podman/machine/machine","IsMachine":true,"Default":true,"ReadWrite":true},{"Name":"podman-machine-default-root","URI":"ssh://root@127.0.0.1:59451/run/podman/podman.sock","Identity":"/Users/a/.local/share/containers/podman/machine/machine","IsMachine":true,"Default":false,"ReadWrite":true}]"#;
        assert_eq!(
            other_connections(mac, None, false).unwrap(),
            ["podman-machine-default-root"]
        );
        assert_eq!(
            other_connections(mac, Some("podman-machine-default-root"), false).unwrap(),
            ["podman-machine-default"]
        );
        assert_eq!(other_connections(mac, None, true).unwrap().len(), 2);
        // Linux without a machine lists none.
        assert!(other_connections(b"[]", None, false).unwrap().is_empty());
        assert!(other_connections(b"", None, false).unwrap().is_empty());
        assert!(other_connections(b"not json", None, false).is_err());
    }

    /// Rootful Podman on Linux, for a process that is not root.
    #[test]
    fn rootful_podman_counts_on_linux_for_a_person() {
        let root = tempfile::tempdir().unwrap();
        assert_eq!(rootful_podman(root.path(), true, false), None);
        std::fs::create_dir_all(root.path().join("run/podman")).unwrap();
        assert_eq!(
            rootful_podman(root.path(), true, false).as_deref(),
            Some(
                "rootful Podman is installed (/run/podman exists), and the containers `sudo \
                 podman` runs are not listed to you"
            )
        );
        assert_eq!(rootful_podman(root.path(), true, true), None);
        assert_eq!(rootful_podman(root.path(), false, false), None);
    }

    /// A container that may mount filesystems itself keeps a mount point
    /// below any host directory it takes; one that never started, or that
    /// mounts a volume plugin's volume, keeps every one; a `--rootfs` above
    /// a mount point is a host directory it takes.
    #[tokio::test]
    async fn what_inspect_cannot_show_keeps_the_mount_point() {
        let repo = tempfile::tempdir().unwrap();
        let repo = std::fs::canonicalize(repo.path()).unwrap();
        let node_modules = repo.join("node_modules");
        std::fs::create_dir(&node_modules).unwrap();
        let inspect = |fields: &str| -> Inspected {
            serde_json::from_str(&format!(
                r#"{{"Id":"{}","Mounts":[{{"Type":"bind","Source":"{}","Destination":"/w"}}],"Tmpfs":{{}},"VolumesFrom":null,{fields}}}"#,
                "c".repeat(64),
                repo.display()
            ))
            .unwrap()
        };
        let started = r#""StartedAt":"2026-10-23T10:24:38.146066598Z""#;
        // Privileged: a tmpfs it mounts on /w/node_modules itself is in no
        // inspect.
        let privileged = inspect(&format!(
            r#""Rootfs":"","Privileged":true,"CapAdd":[],{started}"#
        ));
        assert_eq!(privileged.may_mount(true), Some("it runs --privileged"));
        let mounts = Mounts::of(&privileged, &[], &HashMap::new()).unwrap();
        assert_eq!(
            decided(&node_modules, &[mounts]).await,
            may_mount_by(
                &"c".repeat(64),
                &repo,
                "/w/node_modules",
                "it runs --privileged"
            )
        );
        // CAP_SYS_ADMIN, as Podman 6.1.4 lists it, or as given.
        for caps in [r#"["CAP_SYS_ADMIN"]"#, r#"["SYS_ADMIN"]"#, r#"["ALL"]"#] {
            assert_eq!(
                inspect(&format!(
                    r#""Rootfs":"","Privileged":false,"CapAdd":{caps},{started}"#
                ))
                .may_mount(true),
                Some("it has CAP_SYS_ADMIN")
            );
        }
        // Without it, a container confined by AppArmor (Podman's
        // containers-default profile, where Podman applies it) or by SELinux
        // enforcing (Fedora's Podman) cannot mount, so a bind of the
        // repository alone keeps nothing.
        let confined = |label: &str, profile: &str| {
            inspect(&format!(
                r#""Rootfs":"","Privileged":false,"CapAdd":["CAP_NET_RAW"],"ProcessLabel":"{label}","AppArmorProfile":"{profile}",{started}"#
            ))
        };
        let apparmor = confined("", "containers-default-0.57.4");
        assert_eq!(apparmor.may_mount(false), None);
        let mounts = Mounts::of(&apparmor, &[], &HashMap::new()).unwrap();
        assert_eq!(decided(&node_modules, &[mounts]).await, None);
        let container_t = confined("system_u:system_r:container_t:s0:c7,c27", "");
        assert_eq!(container_t.may_mount(true), None);
        // SELinux permissive does not stop it, nor does spc_t, and with
        // labelling off (`label=disable`, or a host with neither, as Arch
        // Linux, or rootless Podman on Ubuntu, which applies no AppArmor
        // profile) nothing does: it may mount in a user namespace of its
        // own, so a bind of the repository keeps the mount point.
        assert_eq!(container_t.may_mount(false), Some(UNCONFINED));
        assert_eq!(
            confined("system_u:system_r:spc_t:s0:c117,c607", "").may_mount(true),
            Some(UNCONFINED)
        );
        let unconfined = confined("", "");
        assert_eq!(unconfined.may_mount(true), Some(UNCONFINED));
        let mounts = Mounts::of(&unconfined, &[], &HashMap::new()).unwrap();
        assert_eq!(
            decided(&node_modules, &[mounts]).await,
            may_mount_by(&"c".repeat(64), &repo, "/w/node_modules", UNCONFINED)
        );
        // Never started: its overlay volumes are not reported yet.
        let created = inspect(
            r#""Rootfs":"","Privileged":false,"CapAdd":[],"AppArmorProfile":"containers-default-0.57.4","StartedAt":"0001-01-01T00:00:00Z""#,
        );
        let mounts = Mounts::of(&created, &[], &HashMap::new()).unwrap();
        assert!(matches!(
            decided(&node_modules, &[mounts]).await,
            Some(Kept::Unknown(why)) if why.contains("has never started")
        ));
        // A --rootfs above the mount point, with a volume where it is.
        let rootfs: Inspected = serde_json::from_str(&format!(
            r#"{{"Id":"{}","Mounts":[{{"Type":"volume","Name":"deps","Source":"/v/deps/_data","Destination":"/node_modules"}}],"Tmpfs":{{}},"Rootfs":"{}","Privileged":false,"CapAdd":[],{started}}}"#,
            "d".repeat(64),
            repo.display()
        ))
        .unwrap();
        let mounts = Mounts::of(&rootfs, &[], &HashMap::new()).unwrap();
        assert!(matches!(
            decided(&node_modules, &[mounts]).await,
            Some(Kept::Mounted { at, .. }) if at == Path::new("/node_modules")
        ));
        // A volume plugin's volume: Podman does not say what it uses.
        let plugin: Inspected = serde_json::from_str(&format!(
            r#"{{"Id":"{}","Mounts":[{{"Type":"volume","Name":"shared","Destination":"/s"}}],"Tmpfs":{{}},{started}}}"#,
            "e".repeat(64)
        ))
        .unwrap();
        let volumes: Vec<InspectedVolume> = parse_json_lines(
            br#"{"Name":"shared","Driver":"local-persist","Options":{"mountpoint":"/data"}}
"#,
        )
        .unwrap();
        let mut mounts = vec![Mounts::of(&plugin, &[], &HashMap::new()).unwrap()];
        plugin_volumes_unknown(&mut mounts, std::slice::from_ref(&plugin), &volumes);
        assert!(matches!(
            decided(&node_modules, &mounts).await,
            Some(Kept::Unknown(why)) if why.contains("volume plugin local-persist")
        ));
    }

    /// A container that has a mount on the directory is named before one
    /// whose processes only may mount there, whatever Podman's order, also
    /// when only resolving its host directory shows it (the `--volumes-from`
    /// case on a host that does not confine containers: the container it
    /// takes the bind from may mount, the one that takes it has a tmpfs
    /// there). With none that has a mount on it, the first that may mount
    /// is named, before what the host cannot say.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_container_with_a_mount_on_it_is_named_before_one_that_only_may_mount() {
        let repo = tempfile::tempdir().unwrap();
        let repo = std::fs::canonicalize(repo.path()).unwrap();
        let path = repo.join(NODE_MODULES);
        std::fs::create_dir(&path).unwrap();
        let links = tempfile::tempdir().unwrap();
        let link = links.path().join("repo-link");
        std::os::unix::fs::symlink(&repo, &link).unwrap();
        let may = Mounts {
            may_mount: Some(UNCONFINED),
            ..container("from", &[(&repo, "/workspaces/x")], &[])
        };
        let taker = container(
            "taker",
            &[(&repo, "/workspaces/x")],
            &["/workspaces/x/node_modules"],
        );
        let mounted = mounted_by("taker", &repo, "/workspaces/x/node_modules");
        assert_eq!(decided(&path, &[may.clone(), taker.clone()]).await, mounted);
        assert_eq!(decided(&path, &[taker, may.clone()]).await, mounted);
        // The taker's bind is spelled through a link: only resolving it
        // shows its tmpfs is on the directory.
        let linked = container("linked", &[(&link, "/w")], &["/w/node_modules"]);
        assert_eq!(
            decided(&path, &[may.clone(), linked]).await,
            mounted_by("linked", &link, "/w/node_modules")
        );
        // Only one that may mount there: it is named, with why.
        let nothing_below = container("plain", &[(&repo, "/w")], &[]);
        assert_eq!(
            decided(&path, &[nothing_below.clone(), may.clone()]).await,
            may_mount_by("from", &repo, "/workspaces/x/node_modules", UNCONFINED)
        );
        // Before a container whose mounts cannot be told.
        let unknown = Mounts {
            unknown: Some("container unknown has never started".into()),
            ..container("unknown", &[], &[])
        };
        assert_eq!(
            decided(&path, &[unknown.clone(), may]).await,
            may_mount_by("from", &repo, "/workspaces/x/node_modules", UNCONFINED)
        );
        assert!(matches!(
            decided(&path, &[unknown, nothing_below]).await,
            Some(Kept::Unknown(why)) if why.contains("has never started")
        ));
    }

    /// A container that could mount on the directory is said at `info`,
    /// naming it, where its mount would be and why it could, once for each
    /// mount point, container and place.
    #[test]
    fn a_container_that_could_mount_on_it_is_said_once_at_info() {
        let seen = Seen::default();
        let path = Path::new("/may-mount/node_modules");
        let may = Kept::MayMount {
            container: "f".repeat(64),
            source: PathBuf::from("/may-mount"),
            at: PathBuf::from("/w/node_modules"),
            why: "it runs --privileged",
        };
        tracing::subscriber::with_default(seen.clone(), || {
            log_kept(path, &may);
            log_kept(path, &may);
        });
        let seen = seen.0.lock().unwrap();
        assert_eq!(seen.len(), 1, "{seen:?}");
        assert_eq!(seen[0].0, tracing::Level::INFO);
        for said in [
            "path=/may-mount/node_modules;",
            "container=ffffffffffff;",
            "source=/may-mount;",
            "at=/w/node_modules;",
            "why=it runs --privileged;",
            "could mount on it itself",
        ] {
            assert!(seen[0].1.contains(said), "{said:?} in {:?}", seen[0].1);
        }
    }

    /// End-to-end: needs Podman. What `held_elsewhere` finds on this
    /// computer: every connection Podman lists besides the one in use (on a
    /// Mac, the machine's rootful connection), and on Linux rootful Podman
    /// when `/run/podman` exists. Run with:
    ///
    /// ```text
    /// cargo test -p axocoatl-isolation --lib \
    ///   dependency_mountpoint::tests::this_computers_other_engines_are_found -- --ignored --exact
    /// ```
    #[tokio::test]
    #[ignore = "requires podman; run this test explicitly"]
    async fn this_computers_other_engines_are_found() {
        let found = held_elsewhere_here().await;
        println!("held elsewhere: {found:?}");
        let listed = podman_output(
            ["system", "connection", "list", "--format", "json"]
                .map(String::from)
                .to_vec(),
        )
        .await
        .unwrap();
        let connection = std::env::var("CONTAINER_CONNECTION").ok();
        let others = other_connections(
            &listed,
            connection.as_deref(),
            std::env::var_os("CONTAINER_HOST").is_some(),
        )
        .unwrap();
        for other in &others {
            assert!(
                found
                    .as_deref()
                    .unwrap_or_default()
                    .contains(other.as_str()),
                "{other} in {found:?}"
            );
        }
        if cfg!(target_os = "linux") && !running_as_root() && Path::new("/run/podman").is_dir() {
            assert!(found.as_deref().unwrap_or_default().contains("rootful"));
        }
    }

    /// A container Podman lists only with `--external` is told from one it
    /// lists, by its state (Podman 4.9.3, a `buildah from` container).
    #[test]
    fn an_external_container_is_told_by_its_state() {
        let listed: Vec<Listed> = parse_json_array(
            br#"[{"Id":"8679c8c1c3fa82c944492190016ebaa13714e9f24281431492bf0c066f29abc3","Names":["probe-working-container"],"State":"storage","Status":"Storage"},{"Id":"165161b7d95aabeddcacc303d6f96e271475ed5be59ee9dfa2f31e531455d886","Names":["own"],"State":"running","Mounts":["/w"]}]"#,
        )
        .unwrap();
        assert!(listed[0].external());
        assert!(!listed[1].external());
    }

    /// A mount point that a container of another engine or connection could
    /// hold is kept like one a listed container mounts.
    #[test]
    fn a_mount_point_held_elsewhere_stays() {
        let repo = tempfile::tempdir().unwrap();
        let mut registry = Registry::default();
        let path = made_for_a_container(&mut registry, repo.path());
        let candidates = registry.candidates();
        let elsewhere: HashMap<PathBuf, Kept> = [(
            path.clone(),
            Kept::Elsewhere("Podman has the connection podman-machine-default-root".into()),
        )]
        .into();
        assert!(registry.release(candidates, &elsewhere).is_empty());
        assert!(path.is_dir());
        // Once nothing could hold it, it goes.
        assert_eq!(
            release_all(&mut registry),
            [(path.clone(), Released::Removed)]
        );
    }

    #[test]
    fn a_volumes_from_entry_names_its_container_before_its_options() {
        for (entry, container) in [
            ("r5f-a", Some(("r5f-a", false))),
            ("r5f-a:ro", Some(("r5f-a", false))),
            ("r5f-a:ro,z", Some(("r5f-a", false))),
            ("74806aa6c795:rw", Some(("74806aa6c795", false))),
            ("z", Some(("z", true))),
            ("ro", Some(("ro", true))),
            ("rw", Some(("rw", true))),
            ("z:ro", Some(("z", false))),
            ("", None),
            (":ro", None),
        ] {
            assert_eq!(volumes_from_container(entry), container, "{entry:?}");
        }
    }

    /// `--volumes-from`: a container's own inspect leaves out the bind
    /// mounts it takes from another (4.9.3 and 6.1.4), so they are the ones
    /// in that other container's inspect, at the same places: named by its
    /// name, by its id or the start of one, with options as 4.9.3 and 6.1.4
    /// keep them. An overlay volume or a tmpfs is not taken. When the other
    /// container is gone, Podman no longer says what was taken: kept, saying
    /// why.
    #[cfg(unix)]
    #[tokio::test]
    async fn mounts_taken_with_volumes_from_count_and_a_gone_source_keeps_it() {
        const APPARMOR: &str = "containers-default-0.57.4";
        let repo = tempfile::tempdir().unwrap();
        let repo_path = std::fs::canonicalize(repo.path()).unwrap();
        let path = repo_path.join(NODE_MODULES);
        std::fs::create_dir(&path).unwrap();
        let (a, b) = ("a".repeat(64), "b".repeat(64));
        let bind = serde_json::json!({"Type": "bind", "Source": repo_path, "Destination": "/w",
            "Options": ["rbind"]});
        let overlay = serde_json::json!({"Type": "bind", "Source": "/storage/merge",
            "Destination": "/w", "Options": [format!("lowerdir={}", repo_path.display()),
            "upperdir=/storage/upper", "workdir=/storage/work", "userxattr"]});
        let listed = parse_listing(
            serde_json::json!([
                {"Id": a, "Names": ["r5f-a"], "Mounts": ["/w"]},
                {"Id": b, "Names": ["r5f-b"], "Mounts": ["/w/node_modules"]},
            ])
            .to_string()
            .as_bytes(),
        )
        .unwrap();
        // `a` mounts `a_mount` at /w; `b` takes from `from` and has a tmpfs
        // where node_modules is under /w. Only `inspect` of them were
        // inspected. AppArmor confines both (Podman's containers-default
        // profile), so neither may mount in a user namespace of its own.
        let mounts_of = |a_mount: &serde_json::Value, from: &[&str], inspect: &[&str]| {
            let lines = [
                (
                    a.as_str(),
                    serde_json::json!({"Id": a, "Mounts": [a_mount], "Tmpfs": {},
                        "VolumesFrom": null, "AppArmorProfile": APPARMOR}),
                ),
                (
                    b.as_str(),
                    serde_json::json!({"Id": b, "Mounts": [],
                        "Tmpfs": {"/w/node_modules": "rw,rprivate,nosuid,nodev,tmpcopyup"},
                        "VolumesFrom": from, "AppArmorProfile": APPARMOR}),
                ),
            ];
            let output: String = lines
                .iter()
                .filter(|(id, _)| inspect.contains(id))
                .map(|(_, line)| format!("{line}\n"))
                .collect();
            let inspected: Vec<Inspected> = parse_json_lines(output.as_bytes()).unwrap();
            let mut mounts: Vec<Mounts> = inspected
                .iter()
                .map(|container| Mounts::of(container, &[], &HashMap::new()).unwrap())
                .collect();
            take_volumes_from(&mut mounts, &inspected, &listed);
            mounts
        };
        let both = [a.as_str(), b.as_str()];
        let short = a[..12].to_string();
        let short_rw = format!("{short}:rw");
        for from in [
            &["r5f-a"][..],
            &["r5f-a:ro", "z"],
            &["r5f-a:ro,z"],
            &[short.as_str()],
            &[short_rw.as_str()],
            &[a.as_str()],
        ] {
            let mounts = mounts_of(&bind, from, &both);
            assert_eq!(mounts[1].unknown, None, "{from:?}");
            assert_eq!(
                mounts[1].hosts,
                [Host::new(
                    &repo_path,
                    Path::new("/w"),
                    HostKind::BindFrom(short.clone())
                )],
                "{from:?}"
            );
            assert_eq!(
                decided(&path, &mounts).await,
                mounted_by(&b, &repo_path, "/w/node_modules"),
                "{from:?}"
            );
        }
        // Without `--volumes-from`, or with only options, neither container
        // alone has a mount on it.
        for from in [&[][..], &["z"]] {
            let mounts = mounts_of(&bind, from, &both);
            assert_eq!(mounts[1].unknown, None);
            assert_eq!(decided(&path, &mounts).await, None, "{from:?}");
        }
        // A container really named `z`: the entry `z` is its name.
        let named_z = parse_listing(
            serde_json::json!([{"Id": a, "Names": ["z"]}, {"Id": b, "Names": ["r5f-b"]}])
                .to_string()
                .as_bytes(),
        )
        .unwrap();
        let inspected: Vec<Inspected> = parse_json_lines(
            format!(
                "{}\n{}\n",
                serde_json::json!({"Id": a, "Mounts": [bind], "Tmpfs": {}, "VolumesFrom": null,
                    "AppArmorProfile": APPARMOR}),
                serde_json::json!({"Id": b, "Mounts": [], "Tmpfs": {"/w/node_modules": "rw"},
                    "VolumesFrom": ["z"], "AppArmorProfile": APPARMOR})
            )
            .as_bytes(),
        )
        .unwrap();
        let mut mounts: Vec<Mounts> = inspected
            .iter()
            .map(|container| Mounts::of(container, &[], &HashMap::new()).unwrap())
            .collect();
        take_volumes_from(&mut mounts, &inspected, &named_z);
        assert_eq!(
            decided(&path, &mounts).await,
            mounted_by(&b, &repo_path, "/w/node_modules")
        );
        // An overlay volume is not taken.
        let mounts = mounts_of(&overlay, &["r5f-a"], &both);
        assert!(mounts[1].hosts.is_empty() && mounts[1].unknown.is_none());
        assert_eq!(decided(&path, &mounts).await, None);
        // Taken from a container Podman no longer lists, or one listed that
        // was gone by the inspect: what it took cannot be told.
        for (from, inspect) in [(&["r5f-gone:ro"][..], &both[..]), (&["r5f-a"], &both[1..])] {
            let mounts = mounts_of(&bind, from, inspect);
            let Some(Kept::Unknown(why)) = decided(&path, &mounts).await else {
                panic!("{from:?}: {mounts:?}");
            };
            let (name, _) = volumes_from_container(from[0]).unwrap();
            assert!(
                why.contains(&format!(
                    "container {} takes mounts from container {name} with --volumes-from",
                    &b[..12]
                )) && why.contains("no longer says what those mounts are"),
                "{why}"
            );
        }
    }

    /// A host directory the host does not let this process look at keeps
    /// every mount point it could concern, and the reason names the
    /// container, the mount and what the directory is to it. An overlay
    /// volume's merged directory is in Podman's storage, under the
    /// container's own directory, which under `--userns=keep-id` belongs to
    /// a subordinate uid (4.9.3, 0700): the reason says so.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_mount_directory_it_may_not_read_keeps_it_and_says_why() {
        let repo = tempfile::tempdir().unwrap();
        let repo_path = std::fs::canonicalize(repo.path()).unwrap();
        let path = repo_path.join(NODE_MODULES);
        std::fs::create_dir(&path).unwrap();
        let other = tempfile::tempdir().unwrap();
        let id = "6eaf080d3f764584b34cd9d0bd5677eb1983e637e8f3b88c8f68d445ba6fe9d0";
        let storage = format!(
            "/home/r5iso/.local/share/containers/storage/overlay-containers/{id}/userdata/overlay/\
             247019271"
        );
        let line = serde_json::json!({"Id": id, "Mounts": [{"Type": "bind",
            "Source": format!("{storage}/merge"), "Destination": "/o", "Driver": "", "Mode": "",
            "Options": [format!("lowerdir={}", other.path().display()),
                format!("upperdir={storage}/upper"), format!("workdir={storage}/work"),
                "userxattr"],
            "RW": true, "Propagation": "private"}], "Tmpfs": {}, "VolumesFrom": null});
        let inspected: Vec<Inspected> = parse_json_lines(line.to_string().as_bytes()).unwrap();
        let overlay = Mounts::of(&inspected[0], &[], &HashMap::new()).unwrap();
        // As the host answered live: Permission denied for each directory in
        // the container's own one.
        let denying: Resolve = std::sync::Arc::new(|host: &Path| {
            if host.starts_with("/home/r5iso/.local/share/containers") {
                Err(std::io::Error::from_raw_os_error(
                    rustix::io::Errno::ACCESS.raw_os_error(),
                ))
            } else {
                std::fs::canonicalize(host)
            }
        });
        let kept = decide(
            vec![path.clone()],
            std::slice::from_ref(&overlay),
            &denying,
            Duration::from_secs(5),
        )
        .await;
        let Some(Kept::Unknown(why)) = kept.get(&path) else {
            panic!("{kept:?}");
        };
        assert!(
            why.starts_with(&format!(
                "could not resolve {storage}/merge, the merged directory, in Podman's storage, of \
                 container 6eaf080d3f76's overlay volume at /o: Permission denied"
            )),
            "{why}"
        );
        assert!(
            why.contains(
                "a subordinate uid of yours, not you, when the container runs with \
                 --userns=keep-id"
            ),
            "{why}"
        );
        assert!(
            why.ends_with(&format!(
                "stay while that container exists (until it is removed, or {storage}/merge \
                 resolves)"
            )),
            "{why}"
        );
        // A container that does mount it still decides, next to that one.
        assert_eq!(
            decide(
                vec![path.clone()],
                &[overlay, container("i", &[(&path, "/deps")], &[])],
                &denying,
                Duration::from_secs(5),
            )
            .await
            .remove(&path),
            mounted_by("i", &path, "/deps")
        );
        // A bind source it may not read is not blamed on Podman's storage.
        let kept = decide(
            vec![path.clone()],
            &[container(
                "c".repeat(64).as_str(),
                &[(Path::new("/home/r5iso/.local/share/containers/x"), "/x")],
                &[],
            )],
            &denying,
            Duration::from_secs(5),
        )
        .await;
        let Some(Kept::Unknown(why)) = kept.get(&path) else {
            panic!("{kept:?}");
        };
        assert!(
            why.starts_with(
                "could not resolve /home/r5iso/.local/share/containers/x, the source of container \
                 cccccccccccc's bind mount at /x: Permission denied"
            ) && !why.contains("keep-id"),
            "{why}"
        );
    }

    /// A host directory that matches by its spelling decides without
    /// resolving anything on the host; only when none does are they
    /// resolved, each once.
    #[tokio::test]
    async fn a_mount_source_is_resolved_only_when_its_spelling_does_not_match() {
        let repo = tempfile::tempdir().unwrap();
        let repo_path = std::fs::canonicalize(repo.path()).unwrap();
        let path = repo_path.join(NODE_MODULES);
        std::fs::create_dir(&path).unwrap();
        let asked = std::sync::Arc::new(Mutex::new(Vec::<PathBuf>::new()));
        let recording: Resolve = {
            let asked = asked.clone();
            std::sync::Arc::new(move |host: &Path| {
                asked.lock().unwrap().push(host.to_path_buf());
                std::fs::canonicalize(host)
            })
        };
        let unrelated = tempfile::tempdir().unwrap();
        let containers = [
            container("u", &[(unrelated.path(), "/u")], &[]),
            container("u2", &[(unrelated.path(), "/u2")], &[]),
            container("i", &[(&path, "/deps")], &[]),
        ];
        let kept = decide(
            vec![path.clone()],
            &containers,
            &recording,
            Duration::from_secs(5),
        )
        .await;
        assert_eq!(kept.get(&path).cloned(), mounted_by("i", &path, "/deps"));
        assert!(asked.lock().unwrap().is_empty(), "{asked:?}");

        let kept = decide(
            vec![path.clone()],
            &containers[..2],
            &recording,
            Duration::from_secs(5),
        )
        .await;
        assert!(kept.is_empty());
        assert_eq!(*asked.lock().unwrap(), [unrelated.path().to_path_buf()]);
    }

    /// A source that is missing, is not a directory or loops resolves to
    /// nothing, so it keeps nothing; one the host cannot resolve otherwise
    /// keeps every mount point it could concern.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_source_that_errors_keeps_it_only_when_it_could_be_it() {
        let repo = tempfile::tempdir().unwrap();
        let repo_path = std::fs::canonicalize(repo.path()).unwrap();
        let path = repo_path.join(NODE_MODULES);
        std::fs::create_dir(&path).unwrap();
        let scratch = tempfile::tempdir().unwrap();
        let missing = scratch.path().join("gone/node_modules");
        let file = scratch.path().join("file");
        std::fs::write(&file, "x").unwrap();
        let through_file = file.join("node_modules");
        let looping = scratch.path().join("a");
        std::os::unix::fs::symlink(scratch.path().join("b"), &looping).unwrap();
        std::os::unix::fs::symlink(&looping, scratch.path().join("b")).unwrap();
        for source in [&missing, &through_file, &looping.join(NODE_MODULES)] {
            assert!(matches!(
                Resolved::of(std::fs::canonicalize(source)),
                Resolved::Nothing
            ));
            assert_eq!(
                decided(&path, &[container("x", &[(source, "/deps")], &[])]).await,
                None,
                "{}",
                source.display()
            );
        }
        // Any other error (here a permission, then a stale NFS file handle,
        // ESTALE) keeps it.
        let failing: Resolve = std::sync::Arc::new(|host: &Path| {
            Err(if host.ends_with("denied") {
                std::io::Error::from(std::io::ErrorKind::PermissionDenied)
            } else {
                std::io::Error::from_raw_os_error(rustix::io::Errno::STALE.raw_os_error())
            })
        });
        for source in ["/mnt/nfs/denied", "/mnt/nfs/stale"] {
            let kept = decide(
                vec![path.clone()],
                &[container("x", &[(Path::new(source), "/deps")], &[])],
                &failing,
                Duration::from_secs(5),
            )
            .await;
            let Some(Kept::Unknown(why)) = kept.get(&path) else {
                panic!("{source}: {kept:?}");
            };
            assert!(why.contains(source), "{why}");
        }
    }

    /// A source whose resolution never returns (a stale hard-mounted NFS,
    /// SMB or sshfs path): the release waits for it no longer than its
    /// timeout and keeps the mount point; a later release does not start
    /// another resolution of it or wait for it once it has run that long; and
    /// a source that does match still decides next to it.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_source_that_never_resolves_keeps_it_and_does_not_block_the_release() {
        let repo = tempfile::tempdir().unwrap();
        let repo_path = std::fs::canonicalize(repo.path()).unwrap();
        let path = repo_path.join(NODE_MODULES);
        std::fs::create_dir(&path).unwrap();
        let hanging = PathBuf::from(format!("/mnt/stale-{}/hangs", uuid::Uuid::new_v4()));
        let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let resolve: Resolve = {
            let calls = calls.clone();
            std::sync::Arc::new(move |host: &Path| {
                if host.ends_with("hangs") {
                    calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    loop {
                        std::thread::park();
                    }
                }
                std::fs::canonicalize(host)
            })
        };
        let stale = container("s", &[(&hanging, "/data")], &[]);
        let timeout = Duration::from_millis(300);
        let release = |containers: Vec<Mounts>| {
            let (path, resolve) = (path.clone(), resolve.clone());
            async move {
                let began = std::time::Instant::now();
                let kept = decide(vec![path.clone()], &containers, &resolve, timeout).await;
                (kept.get(&path).cloned(), began.elapsed())
            }
        };

        let (kept, waited) = release(vec![stale.clone()]).await;
        assert!(
            waited >= timeout && waited < Duration::from_secs(3),
            "waited {waited:?}"
        );
        let Some(Kept::Unknown(why)) = kept else {
            panic!("{kept:?}");
        };
        assert!(why.contains("not resolved within 0.3 s"), "{why}");
        assert!(path.is_dir());

        // Still blocked: neither resolved again nor waited for.
        let (kept, waited) = release(vec![stale.clone()]).await;
        assert!(waited < timeout, "waited {waited:?}");
        let Some(Kept::Unknown(why)) = kept else {
            panic!("{kept:?}");
        };
        assert!(why.contains("has not returned"), "{why}");
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);

        // A container that does mount it still decides next to it.
        let links = tempfile::tempdir().unwrap();
        let link = links.path().join("nm");
        std::os::unix::fs::symlink(&path, &link).unwrap();
        let (kept, waited) = release(vec![stale, container("l", &[(&link, "/deps")], &[])]).await;
        assert!(waited < timeout, "waited {waited:?}");
        assert_eq!(kept, mounted_by("l", &link, "/deps"));
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    /// Two releases at once that need the same host directory share one
    /// resolution of it.
    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn releases_at_once_share_a_resolution() {
        let repo = tempfile::tempdir().unwrap();
        let repo_path = std::fs::canonicalize(repo.path()).unwrap();
        let path = repo_path.join(NODE_MODULES);
        std::fs::create_dir(&path).unwrap();
        let links = tempfile::tempdir().unwrap();
        let link = links.path().join("nm");
        std::os::unix::fs::symlink(&path, &link).unwrap();
        let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let resolve: Resolve = {
            let calls = calls.clone();
            std::sync::Arc::new(move |host: &Path| {
                calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                std::thread::sleep(Duration::from_millis(300));
                std::fs::canonicalize(host)
            })
        };
        let containers = [container("l", &[(&link, "/deps")], &[])];
        let release = || {
            decide(
                vec![path.clone()],
                &containers,
                &resolve,
                Duration::from_secs(5),
            )
        };
        let (first, second) = tokio::join!(release(), release());
        assert_eq!(first.get(&path).cloned(), mounted_by("l", &link, "/deps"));
        assert_eq!(first, second);
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    /// Podman's "no such" lines, as 4.9.3 and 6.1.4 print them for two of
    /// three ids, the last after `Error: `.
    #[test]
    fn podmans_no_such_lines_name_what_is_gone() {
        let (a, b, c) = ("a".repeat(64), "b".repeat(64), "c".repeat(64));
        let names = [a.clone(), b.clone(), c.clone()];
        for error in [
            format!("no such container {c}\nError: no such container {b}"),
            format!("no such container \"{c}\"\nError: no such container \"{b}\""),
        ] {
            assert_eq!(
                gone(&error, "container", &names),
                [b.as_str(), c.as_str()].into()
            );
        }
        assert_eq!(
            gone(
                "no such volume r4iso-nosuch2\nError: no such volume r4iso-nosuchvol",
                "volume",
                &["r4iso-nosuchvol".to_string(), "r4iso-bindvol".to_string()]
            ),
            ["r4iso-nosuchvol"].into()
        );
        // A container removed while `inspect` ran (Podman 6.1.4).
        assert_eq!(
            gone(
                &format!("Error: no container with ID {a} found in database: no such container"),
                "container",
                &names
            ),
            [a.as_str()].into()
        );
        for error in [
            format!("Error: no such volume {b}"),
            format!("Error: no such container {}", "d".repeat(64)),
            format!("Error: inspecting {b}: no such container"),
            format!("Error: no container with ID {b} found in database: no such volume"),
            format!("WARN[0000] no such container {b}x"),
            "Error: timed out".to_string(),
        ] {
            assert!(gone(&error, "container", &names).is_empty(), "{error}");
        }
    }

    /// The arguments each call of a fake `podman` got.
    type Calls = std::sync::Arc<Mutex<Vec<Vec<String>>>>;
    /// What a fake `podman` answers to one call.
    type Answer = std::future::Ready<Result<Vec<u8>, String>>;

    /// A fake `podman`: for each call, the arguments it got and what it
    /// answers.
    fn fake_podman(
        answers: Vec<Result<String, String>>,
    ) -> (Calls, impl Fn(Vec<String>) -> Answer) {
        let calls = std::sync::Arc::new(Mutex::new(Vec::new()));
        let answers = Mutex::new(answers.into_iter());
        let recorded = calls.clone();
        (calls, move |args: Vec<String>| {
            recorded.lock().unwrap().push(args);
            std::future::ready(
                answers
                    .lock()
                    .unwrap()
                    .next()
                    .expect("no more answers")
                    .map(String::into_bytes),
            )
        })
    }

    async fn inspect_containers<Ran>(
        ids: &[String],
        run: impl Fn(Vec<String>) -> Ran,
    ) -> Result<Vec<Inspected>, String>
    where
        Ran: std::future::Future<Output = Result<Vec<u8>, String>>,
    {
        inspect_existing(
            &["container", "inspect"],
            INSPECT_FORMAT,
            ids,
            "container",
            |container: &Inspected| container.id.as_deref(),
            run,
        )
        .await
    }

    /// A container removed between `podman ps -a` and the inspect: Podman
    /// fails the whole inspect, naming it. The rest are inspected once more
    /// without it, and decide; anything else Podman says keeps everything.
    #[tokio::test]
    async fn a_container_gone_since_the_listing_is_left_out_once() {
        let (a, b) = ("a".repeat(64), "b".repeat(64));
        let ids = [a.clone(), b.clone()];
        let line = |id: &str| format!("{{\"Id\":\"{id}\",\"Mounts\":[],\"Tmpfs\":{{}}}}\n");

        // Gone: once more without it.
        for said in [
            format!("Error: no such container {b}"),
            format!("Error: no such container \"{b}\""),
        ] {
            let (calls, run) = fake_podman(vec![Err(said), Ok(line(&a))]);
            let inspected = inspect_containers(&ids, run).await.unwrap();
            assert_eq!(inspected.len(), 1);
            assert_eq!(inspected[0].id.as_deref(), Some(a.as_str()));
            let calls = calls.lock().unwrap();
            assert_eq!(calls.len(), 2);
            assert_eq!(
                calls[0][..4],
                ["container", "inspect", "--format", INSPECT_FORMAT]
            );
            assert_eq!(calls[0][4..], ids);
            assert_eq!(calls[1][4..], *std::slice::from_ref(&a));
        }
        // All gone: nothing to inspect again.
        let (calls, run) = fake_podman(vec![Err(format!(
            "no such container {a}\nError: no such container {b}"
        ))]);
        assert!(inspect_containers(&ids, run).await.unwrap().is_empty());
        assert_eq!(calls.lock().unwrap().len(), 1);
        // Another goes before the second inspect: Podman cannot say, once.
        let (calls, run) = fake_podman(vec![
            Err(format!("Error: no such container {b}")),
            Err(format!("Error: no such container {a}")),
        ]);
        let error = inspect_containers(&ids, run).await.unwrap_err();
        assert!(error.contains(&a) && error.contains(&b), "{error}");
        assert_eq!(calls.lock().unwrap().len(), 2);
        // Any other failure: not asked again.
        let (calls, run) = fake_podman(vec![Err("Error: timed out".into())]);
        assert!(inspect_containers(&ids, run).await.is_err());
        assert_eq!(calls.lock().unwrap().len(), 1);
        // Success that leaves one out is no answer either.
        let (_, run) = fake_podman(vec![Ok(line(&a))]);
        let error = inspect_containers(&ids, run).await.unwrap_err();
        assert!(error.contains("did not inspect container"), "{error}");
        let (_, run) = fake_podman(vec![Ok(line(&a) + &line(&b))]);
        assert_eq!(inspect_containers(&ids, run).await.unwrap().len(), 2);
    }

    /// End-to-end against real Podman: an inspect of a container that exists
    /// and of one that does not (as one removed since the listing) answers
    /// for the one that exists, and so does a volume inspect. Run with:
    ///
    /// ```text
    /// cargo test -p axocoatl-isolation --lib \
    ///   dependency_mountpoint::tests::podman_names_what_is_gone_and_the_rest_is_inspected \
    ///   -- --ignored --exact --test-threads=1
    /// ```
    #[tokio::test]
    #[ignore = "requires podman; run this test explicitly"]
    async fn podman_names_what_is_gone_and_the_rest_is_inspected() {
        struct Removed(&'static str, String);
        impl Drop for Removed {
            fn drop(&mut self) {
                let _ = std::process::Command::new("podman")
                    .args([self.0, "rm", "-f", &self.1])
                    .output();
            }
        }
        let name = format!("axocoatl-test-gone-{}", uuid::Uuid::new_v4());
        let created = podman_output(
            [
                "create",
                "--name",
                &name,
                "docker.io/library/alpine:3.20",
                "true",
            ]
            .map(str::to_string)
            .to_vec(),
        )
        .await
        .expect("podman create");
        let _container = Removed("container", name.clone());
        let id = String::from_utf8(created).unwrap().trim().to_string();
        assert!(plausible_container_id(&id), "{id}");
        let removed = format!(
            "{:064x}",
            u128::from_be_bytes(*uuid::Uuid::new_v4().as_bytes())
        );
        let ids = [removed.clone(), id.clone()];
        let inspected = inspect_containers(&ids, podman_output).await.unwrap();
        assert_eq!(inspected.len(), 1);
        assert_eq!(inspected[0].id.as_deref(), Some(id.as_str()));

        podman_output(["volume", "create", &name].map(str::to_string).to_vec())
            .await
            .expect("podman volume create");
        let _volume = Removed("volume", name.clone());
        let names = [format!("{name}-gone"), name.clone()];
        let volumes = inspect_existing(
            &["volume", "inspect"],
            VOLUME_FORMAT,
            &names,
            "volume",
            |volume: &InspectedVolume| volume.name.as_deref(),
            podman_output,
        )
        .await
        .unwrap();
        assert_eq!(volumes.len(), 1);
        assert_eq!(volumes[0].name.as_deref(), Some(name.as_str()));
        assert_eq!(volumes[0].bound(), None);
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
