//! Capability-style persistence beneath an operator-owned directory.
//!
//! Axocoatl stores control-plane state beside user-selected Workspaces. A
//! logical path check is not enough there: code running in a Workspace can
//! pre-create symlinked directories or predictable temporary-file links. This
//! module keeps every descendant traversal relative to an already-opened
//! directory on Unix, refuses symlink components with `O_NOFOLLOW`, and uses
//! same-directory `create_new` temporary files for atomic replacement.

use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io::{self, Read, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

#[cfg(unix)]
use std::os::unix::ffi::OsStrExt;

#[cfg(unix)]
use rustix::fd::OwnedFd;
#[cfg(unix)]
use rustix::fs::{self, AtFlags, FileType, FlockOperation, Mode, OFlags};

/// An opened directory used as the authority for descendant persistence.
///
/// Callers should anchor this at `AXOCOATL_DATA_DIR` and pass only relative
/// store paths to [`SecureDir::child`]. The ambient path is retained for logs
/// and compatibility APIs; Unix I/O itself stays relative to the open handle.
#[derive(Clone, Debug)]
pub struct SecureDir {
    path: PathBuf,
    #[cfg(unix)]
    fd: Arc<OwnedFd>,
}

/// File type reported by an anchored directory scan.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SecureEntryType {
    File,
    Directory,
    Other,
}

/// One nofollow leaf read through a retained [`SecureDir`] capability.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SecureLeaf {
    Regular { bytes: Vec<u8>, mode: u32 },
    Symlink { target: PathBuf },
    Directory,
}

/// One direct child of a [`SecureDir`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SecureDirEntry {
    pub name: OsString,
    pub file_type: SecureEntryType,
}

impl SecureDir {
    /// Open an existing directory without following a symlink at the boundary.
    pub fn open(path: impl AsRef<Path>) -> io::Result<Self> {
        let path = path.as_ref().to_path_buf();
        #[cfg(unix)]
        {
            let fd = fs::open(
                &path,
                OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::empty(),
            )
            .map_err(io::Error::from)?;
            Ok(Self {
                path,
                fd: Arc::new(fd),
            })
        }
        #[cfg(not(unix))]
        {
            let metadata = std::fs::symlink_metadata(&path)?;
            if !metadata.file_type().is_dir() || metadata.file_type().is_symlink() {
                return Err(invalid_path(&path, "is not a real directory"));
            }
            Ok(Self { path })
        }
    }

    /// Open a directory, creating this one final component if absent.
    ///
    /// The parent is the caller's trust boundary. For nested managed paths,
    /// prefer [`SecureDir::child`] so every created component is anchored.
    pub fn open_or_create(path: impl AsRef<Path>) -> io::Result<Self> {
        let path = path.as_ref();
        match std::fs::symlink_metadata(path) {
            Ok(metadata) if metadata.file_type().is_dir() && !metadata.file_type().is_symlink() => {
            }
            Ok(_) => return Err(invalid_path(path, "is not a real directory")),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                std::fs::create_dir(path)?;
            }
            Err(error) => return Err(error),
        }
        Self::open(path)
    }

    /// Open or create an entire path while refusing symlinked components.
    ///
    /// Absolute Unix paths are walked from `/`; relative paths are walked from
    /// the already-open current directory. This is primarily a compatibility
    /// seam for callers that historically supplied a not-yet-created store
    /// root. Product code should still prefer one `SecureDir` opened at the
    /// configured control-plane root and descendant [`SecureDir::child`] calls.
    pub fn open_or_create_all(path: impl AsRef<Path>) -> io::Result<Self> {
        let path = path.as_ref();
        #[cfg(unix)]
        {
            let walk_path = normalize_platform_absolute_prefix(path)?;
            let anchor = if walk_path.is_absolute() {
                Self::open(Path::new("/"))?
            } else {
                Self::open(std::env::current_dir()?)?
            };
            let mut relative = PathBuf::new();
            for component in walk_path.components() {
                match component {
                    Component::RootDir | Component::CurDir => {}
                    Component::Normal(value) => relative.push(value),
                    Component::ParentDir | Component::Prefix(_) => {
                        return Err(invalid_path(
                            &walk_path,
                            "contains a non-descendant component",
                        ));
                    }
                }
            }
            anchor.child(relative)
        }
        #[cfg(not(unix))]
        {
            std::fs::create_dir_all(path)?;
            Self::open(path)
        }
    }

    /// Open or create a descendant directory without accepting absolute,
    /// parent, dot, or symlink components.
    pub fn child(&self, relative: impl AsRef<Path>) -> io::Result<Self> {
        self.child_impl(relative.as_ref(), true)
    }

    /// Open an existing descendant directory without following symlinks.
    pub fn existing_child(&self, relative: impl AsRef<Path>) -> io::Result<Self> {
        self.child_impl(relative.as_ref(), false)
    }

    /// Ambient display path for compatibility and diagnostics.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Restrict the opened directory itself to the current user without
    /// reopening its ambient path.
    pub fn restrict_owner_only(&self) -> io::Result<()> {
        #[cfg(unix)]
        {
            fs::fchmod(self.fd.as_ref(), Mode::RUSR | Mode::WUSR | Mode::XUSR)
                .map_err(io::Error::from)
        }
        #[cfg(not(unix))]
        {
            // Windows ACL hardening is owned by the platform installer. A DOS
            // readonly flag is not an equivalent access-control boundary.
            Ok(())
        }
    }

    /// Admit an existing authority root only when it is owned by the expected
    /// Unix uid and has never been left group/world writable. Tightening a
    /// hostile 0777 directory after opening it is insufficient because another
    /// process may already retain an fd or have pre-created managed children.
    #[cfg(unix)]
    pub fn require_owner_and_private_writes(&self, expected_uid: u32) -> io::Result<()> {
        let stat = fs::fstat(self.fd.as_ref()).map_err(io::Error::from)?;
        if stat.st_uid != expected_uid {
            return Err(invalid_path(
                &self.path,
                &format!(
                    "is owned by uid {}, expected effective uid {expected_uid}",
                    stat.st_uid
                ),
            ));
        }
        if stat.st_mode & 0o022 != 0 {
            return Err(invalid_path(
                &self.path,
                "is group/world writable and cannot be trusted as an authority root",
            ));
        }
        Ok(())
    }

    /// Take a nonblocking exclusive advisory lock on this already-opened
    /// directory inode. The lock survives unlink/replacement of child lock
    /// files and remains held while any clone of this capability is alive.
    #[cfg(unix)]
    pub fn try_lock_exclusive(&self) -> io::Result<()> {
        fs::flock(self.fd.as_ref(), FlockOperation::NonBlockingLockExclusive)
            .map_err(io::Error::from)
    }

    /// Verify that the configured ambient path still resolves to this exact
    /// opened directory.
    pub fn verify_ambient_identity(&self) -> io::Result<()> {
        #[cfg(unix)]
        {
            let reopened = fs::open(
                &self.path,
                OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::empty(),
            )
            .map_err(io::Error::from)?;
            let expected = fs::fstat(self.fd.as_ref()).map_err(io::Error::from)?;
            let actual = fs::fstat(&reopened).map_err(io::Error::from)?;
            if expected.st_dev != actual.st_dev || expected.st_ino != actual.st_ino {
                return Err(invalid_path(
                    &self.path,
                    "no longer resolves to the opened directory",
                ));
            }
            Ok(())
        }
        #[cfg(not(unix))]
        {
            let metadata = std::fs::symlink_metadata(&self.path)?;
            if metadata.file_type().is_dir() && !metadata.file_type().is_symlink() {
                Ok(())
            } else {
                Err(invalid_path(&self.path, "is not a real directory"))
            }
        }
    }

    /// List direct children without following directory-entry symlinks.
    pub fn entries(&self) -> io::Result<Vec<SecureDirEntry>> {
        self.entries_limited(usize::MAX)
    }

    /// Bounded direct-child scan. The limit is enforced while iterating so a
    /// repository-controlled directory cannot force an unbounded name vector
    /// before its caller gets a chance to reject it.
    pub fn entries_limited(&self, max_entries: usize) -> io::Result<Vec<SecureDirEntry>> {
        #[cfg(unix)]
        {
            let mut directory = fs::Dir::read_from(self.fd.as_ref()).map_err(io::Error::from)?;
            let mut entries = Vec::new();
            for entry in &mut directory {
                let entry = entry.map_err(io::Error::from)?;
                let bytes = entry.file_name().to_bytes();
                if matches!(bytes, b"." | b"..") {
                    continue;
                }
                if entries.len() >= max_entries {
                    return Err(invalid_path(
                        &self.path,
                        "contains more entries than the permitted scan limit",
                    ));
                }
                let name = OsStr::from_bytes(bytes).to_os_string();
                let stat = fs::statat(self.fd.as_ref(), &name, AtFlags::SYMLINK_NOFOLLOW)
                    .map_err(io::Error::from)?;
                let file_type = match FileType::from_raw_mode(stat.st_mode) {
                    FileType::RegularFile => SecureEntryType::File,
                    FileType::Directory => SecureEntryType::Directory,
                    _ => SecureEntryType::Other,
                };
                entries.push(SecureDirEntry { name, file_type });
            }
            Ok(entries)
        }
        #[cfg(not(unix))]
        {
            let mut entries = Vec::new();
            for entry in std::fs::read_dir(&self.path)? {
                let entry = entry?;
                if entries.len() >= max_entries {
                    return Err(invalid_path(
                        &self.path,
                        "contains more entries than the permitted scan limit",
                    ));
                }
                let metadata = std::fs::symlink_metadata(entry.path())?;
                let file_type = if metadata.file_type().is_symlink() {
                    SecureEntryType::Other
                } else if metadata.file_type().is_file() {
                    SecureEntryType::File
                } else if metadata.file_type().is_dir() {
                    SecureEntryType::Directory
                } else {
                    SecureEntryType::Other
                };
                entries.push(SecureDirEntry {
                    name: entry.file_name(),
                    file_type,
                });
            }
            Ok(entries)
        }
    }

    /// Require an exact (case-sensitive byte/OsString) regular-file entry
    /// match before a compatibility read. This prevents a case-insensitive
    /// filesystem from resolving one legacy logical identity as another.
    pub fn has_exact_file(&self, name: impl AsRef<OsStr>) -> io::Result<bool> {
        self.has_exact_entry(name.as_ref(), SecureEntryType::File)
    }

    /// Exact-name counterpart for a direct child directory.
    pub fn has_exact_directory(&self, name: impl AsRef<OsStr>) -> io::Result<bool> {
        self.has_exact_entry(name.as_ref(), SecureEntryType::Directory)
    }

    fn has_exact_entry(&self, name: &OsStr, expected: SecureEntryType) -> io::Result<bool> {
        validate_relative(Path::new(name))?;
        match self.entries()?.into_iter().find(|entry| entry.name == name) {
            Some(entry) if entry.file_type == expected => Ok(true),
            Some(_) => Err(invalid_path(
                &self.path.join(name),
                "has an unexpected file type",
            )),
            None => Ok(false),
        }
    }

    /// Read one regular descendant file without following its final symlink.
    pub fn read(&self, relative: impl AsRef<Path>) -> io::Result<Vec<u8>> {
        let (parent, name) = self.parent_and_name(relative.as_ref(), false)?;
        #[cfg(unix)]
        {
            let fd = fs::openat(
                parent.fd.as_ref(),
                &name,
                OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::empty(),
            )
            .map_err(io::Error::from)?;
            ensure_regular_fd(&fd, &parent.path.join(&name))?;
            let mut file = File::from(fd);
            let mut bytes = Vec::new();
            file.read_to_end(&mut bytes)?;
            Ok(bytes)
        }
        #[cfg(not(unix))]
        {
            let path = parent.path.join(name);
            ensure_regular_path(&path)?;
            std::fs::read(path)
        }
    }

    /// Read one regular descendant through a single opened descriptor while
    /// enforcing a byte ceiling before allocation and again at EOF. This is
    /// intended for small repository-owned control files whose path may be
    /// concurrently replaced by sandboxed code.
    pub fn read_limited(
        &self,
        relative: impl AsRef<Path>,
        max_bytes: usize,
    ) -> io::Result<Vec<u8>> {
        let relative = relative.as_ref();
        let mut file = self.open_file_limited(relative, max_bytes)?;
        let read_ceiling = max_bytes.saturating_add(1) as u64;
        let mut bytes = Vec::with_capacity(max_bytes.min(8 * 1024));
        std::io::Read::by_ref(&mut file)
            .take(read_ceiling)
            .read_to_end(&mut bytes)?;
        if bytes.len() > max_bytes {
            return Err(invalid_path(
                &self.path.join(relative),
                "grew beyond the permitted byte limit while being read",
            ));
        }
        Ok(bytes)
    }

    /// Open one uniquely-linked regular descendant for a bounded streaming
    /// read. The descriptor is opened without following the final component,
    /// and a sparse or already-oversized file is rejected from `st_size`
    /// before the caller can allocate from it. Callers must still stop after
    /// `max_bytes + 1` bytes so concurrent growth is detected.
    pub fn open_file_limited(
        &self,
        relative: impl AsRef<Path>,
        max_bytes: usize,
    ) -> io::Result<File> {
        let (parent, name) = self.parent_and_name(relative.as_ref(), false)?;
        #[cfg(unix)]
        {
            let fd = fs::openat(
                parent.fd.as_ref(),
                &name,
                OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::empty(),
            )
            .map_err(io::Error::from)?;
            ensure_regular_fd(&fd, &parent.path.join(&name))?;
            let stat = fs::fstat(&fd).map_err(io::Error::from)?;
            if stat.st_size < 0 || stat.st_size as u64 > max_bytes as u64 {
                return Err(invalid_path(
                    &parent.path.join(&name),
                    "exceeds the permitted byte limit",
                ));
            }
            Ok(File::from(fd))
        }
        #[cfg(not(unix))]
        {
            let path = parent.path.join(&name);
            ensure_regular_path(&path)?;
            let file = File::open(&path)?;
            let metadata = file.metadata()?;
            if metadata.len() > max_bytes as u64 {
                return Err(invalid_path(&path, "exceeds the permitted byte limit"));
            }
            Ok(file)
        }
    }

    /// Read one leaf and its type without following the leaf or any parent.
    /// Regular-file bytes and mode come from the same opened descriptor.
    pub fn read_leaf(&self, relative: impl AsRef<Path>) -> io::Result<Option<SecureLeaf>> {
        self.read_leaf_impl(relative.as_ref(), None)
    }

    /// Bounded counterpart to [`SecureDir::read_leaf`]. Regular files are
    /// rejected from descriptor metadata before allocation when their logical
    /// length exceeds `max_bytes`; a sparse file therefore cannot force a
    /// proportional allocation. Symlink target bytes use the same ceiling.
    pub fn read_leaf_limited(
        &self,
        relative: impl AsRef<Path>,
        max_bytes: usize,
    ) -> io::Result<Option<SecureLeaf>> {
        self.read_leaf_impl(relative.as_ref(), Some(max_bytes))
    }

    fn read_leaf_impl(
        &self,
        relative: &Path,
        max_bytes: Option<usize>,
    ) -> io::Result<Option<SecureLeaf>> {
        let (parent, name) = match self.parent_and_name(relative, false) {
            Ok(parts) => parts,
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
                ) =>
            {
                return Ok(None)
            }
            Err(error) => return Err(error),
        };
        #[cfg(unix)]
        {
            let stat = match fs::statat(parent.fd.as_ref(), &name, AtFlags::SYMLINK_NOFOLLOW) {
                Ok(stat) => stat,
                Err(rustix::io::Errno::NOENT | rustix::io::Errno::NOTDIR) => return Ok(None),
                Err(error) => return Err(io::Error::from(error)),
            };
            match FileType::from_raw_mode(stat.st_mode) {
                FileType::RegularFile if stat.st_nlink == 1 => {
                    let fd = fs::openat(
                        parent.fd.as_ref(),
                        &name,
                        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                        Mode::empty(),
                    )
                    .map_err(io::Error::from)?;
                    ensure_regular_fd(&fd, &parent.path.join(&name))?;
                    let opened = fs::fstat(&fd).map_err(io::Error::from)?;
                    if max_bytes.is_some_and(|limit| {
                        opened.st_size < 0 || opened.st_size as u64 > limit as u64
                    }) {
                        return Err(invalid_path(
                            &parent.path.join(&name),
                            "exceeds the permitted byte limit",
                        ));
                    }
                    let mut file = File::from(fd);
                    let mut bytes = Vec::with_capacity(
                        max_bytes
                            .unwrap_or_default()
                            .min(opened.st_size.max(0) as usize)
                            .min(8 * 1024),
                    );
                    match max_bytes {
                        Some(limit) => {
                            std::io::Read::by_ref(&mut file)
                                .take(limit.saturating_add(1) as u64)
                                .read_to_end(&mut bytes)?;
                            if bytes.len() > limit {
                                return Err(invalid_path(
                                    &parent.path.join(&name),
                                    "grew beyond the permitted byte limit while being read",
                                ));
                            }
                        }
                        None => file.read_to_end(&mut bytes).map(|_| ())?,
                    }
                    Ok(Some(SecureLeaf::Regular {
                        bytes,
                        mode: opened.st_mode as u32,
                    }))
                }
                FileType::Symlink => {
                    let target = fs::readlinkat(parent.fd.as_ref(), &name, Vec::new())
                        .map_err(io::Error::from)?;
                    if max_bytes.is_some_and(|limit| target.to_bytes().len() > limit) {
                        return Err(invalid_path(
                            &parent.path.join(&name),
                            "exceeds the permitted byte limit",
                        ));
                    }
                    Ok(Some(SecureLeaf::Symlink {
                        target: PathBuf::from(OsStr::from_bytes(target.to_bytes())),
                    }))
                }
                FileType::Directory => Ok(Some(SecureLeaf::Directory)),
                _ => Err(invalid_path(
                    &parent.path.join(&name),
                    "is not a regular file, symlink, or directory",
                )),
            }
        }
        #[cfg(not(unix))]
        {
            let path = parent.path.join(name);
            let metadata = match std::fs::symlink_metadata(&path) {
                Ok(metadata) => metadata,
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
                    ) =>
                {
                    return Ok(None)
                }
                Err(error) => return Err(error),
            };
            if metadata.file_type().is_symlink() {
                let target = std::fs::read_link(&path)?;
                if max_bytes.is_some_and(|limit| target.as_os_str().len() > limit) {
                    return Err(invalid_path(&path, "exceeds the permitted byte limit"));
                }
                Ok(Some(SecureLeaf::Symlink { target }))
            } else if metadata.is_dir() {
                Ok(Some(SecureLeaf::Directory))
            } else if metadata.is_file() {
                if max_bytes.is_some_and(|limit| metadata.len() > limit as u64) {
                    return Err(invalid_path(&path, "exceeds the permitted byte limit"));
                }
                let bytes = match max_bytes {
                    Some(limit) => self.read_limited(relative, limit)?,
                    None => std::fs::read(&path)?,
                };
                Ok(Some(SecureLeaf::Regular { bytes, mode: 0 }))
            } else {
                Err(invalid_path(
                    &path,
                    "is not a regular file, symlink, or directory",
                ))
            }
        }
    }

    /// Return whether an exact descendant exists as a regular, non-symlink file.
    pub fn is_file(&self, relative: impl AsRef<Path>) -> io::Result<bool> {
        let (parent, name) = match self.parent_and_name(relative.as_ref(), false) {
            Ok(parts) => parts,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(error),
        };
        #[cfg(unix)]
        {
            match fs::statat(parent.fd.as_ref(), &name, AtFlags::SYMLINK_NOFOLLOW) {
                Ok(stat)
                    if FileType::from_raw_mode(stat.st_mode) == FileType::RegularFile
                        && stat.st_nlink == 1 =>
                {
                    Ok(true)
                }
                Ok(_) => Err(invalid_path(
                    &parent.path.join(&name),
                    "exists but is not a regular file",
                )),
                Err(rustix::io::Errno::NOENT) => Ok(false),
                Err(error) => Err(io::Error::from(error)),
            }
        }
        #[cfg(not(unix))]
        {
            match std::fs::symlink_metadata(parent.path.join(name)) {
                Ok(metadata)
                    if metadata.file_type().is_file() && !metadata.file_type().is_symlink() =>
                {
                    Ok(true)
                }
                Ok(_) => Err(invalid_path(
                    &parent.path.join(&name),
                    "exists but is not a regular file",
                )),
                Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
                Err(error) => Err(error),
            }
        }
    }

    /// Return the length of one regular descendant without following links.
    pub fn file_len(&self, relative: impl AsRef<Path>) -> io::Result<u64> {
        let (parent, name) = self.parent_and_name(relative.as_ref(), false)?;
        #[cfg(unix)]
        {
            let fd = fs::openat(
                parent.fd.as_ref(),
                &name,
                OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::empty(),
            )
            .map_err(io::Error::from)?;
            ensure_regular_fd(&fd, &parent.path.join(&name))?;
            fs::fstat(&fd)
                .map(|stat| stat.st_size as u64)
                .map_err(io::Error::from)
        }
        #[cfg(not(unix))]
        {
            let path = parent.path.join(name);
            ensure_regular_path(&path)?;
            std::fs::metadata(path).map(|metadata| metadata.len())
        }
    }

    /// Create one new regular descendant and return its open handle.
    ///
    /// This is for durable leases whose file handle must remain live. Ordinary
    /// state replacement should use [`SecureDir::atomic_write`] instead.
    pub fn create_new(&self, relative: impl AsRef<Path>) -> io::Result<File> {
        let (parent, name) = self.parent_and_name(relative.as_ref(), true)?;
        #[cfg(unix)]
        {
            let fd = fs::openat(
                parent.fd.as_ref(),
                &name,
                OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::RUSR | Mode::WUSR,
            )
            .map_err(io::Error::from)?;
            ensure_regular_fd(&fd, &parent.path.join(&name))?;
            Ok(File::from(fd))
        }
        #[cfg(not(unix))]
        {
            let path = parent.path.join(name);
            std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(path)
        }
    }

    /// Open or create a retained regular file without following its target.
    /// Intended for kernel locks whose inode persists across process restarts.
    pub fn open_lock_file(&self, relative: impl AsRef<Path>) -> io::Result<File> {
        let (parent, name) = self.parent_and_name(relative.as_ref(), true)?;
        parent.reject_non_regular_target(&name)?;
        #[cfg(unix)]
        {
            let fd = fs::openat(
                parent.fd.as_ref(),
                &name,
                OFlags::RDWR | OFlags::CREATE | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::RUSR | Mode::WUSR,
            )
            .map_err(io::Error::from)?;
            ensure_regular_fd(&fd, &parent.path.join(&name))?;
            Ok(File::from(fd))
        }
        #[cfg(not(unix))]
        {
            std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .open(parent.path.join(name))
        }
    }

    /// Atomically replace one regular descendant file.
    ///
    /// The temporary name is unpredictable, opened with `create_new`, written
    /// and fsynced before a same-directory rename. Existing symlink targets are
    /// rejected rather than replaced so tampering is observable and fail-closed.
    pub fn atomic_write(&self, relative: impl AsRef<Path>, bytes: &[u8]) -> io::Result<()> {
        self.atomic_write_with_mode(relative, bytes, 0o600)
    }

    /// Atomically replace one regular descendant and apply exact Unix
    /// permission bits before publication. Non-Unix hosts retain their native
    /// default-file semantics.
    pub fn atomic_write_with_mode(
        &self,
        relative: impl AsRef<Path>,
        bytes: &[u8],
        mode: u32,
    ) -> io::Result<()> {
        let (parent, name) = self.parent_and_name(relative.as_ref(), true)?;
        parent.reject_non_regular_target(&name)?;
        let temporary = OsString::from(format!(".axocoatl-{}.tmp", uuid::Uuid::new_v4()));

        #[cfg(unix)]
        {
            let fd = fs::openat(
                parent.fd.as_ref(),
                &temporary,
                OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::RUSR | Mode::WUSR,
            )
            .map_err(io::Error::from)?;
            let mut file = File::from(fd);
            let result = (|| {
                file.write_all(bytes)?;
                fs::fchmod(&file, Mode::from_raw_mode((mode & 0o777) as _))
                    .map_err(io::Error::from)?;
                file.sync_all()?;
                fs::renameat(parent.fd.as_ref(), &temporary, parent.fd.as_ref(), &name)
                    .map_err(io::Error::from)?;
                fs::fsync(parent.fd.as_ref()).map_err(io::Error::from)
            })();
            if result.is_err() {
                let _ = fs::unlinkat(parent.fd.as_ref(), &temporary, AtFlags::empty());
            }
            result
        }
        #[cfg(not(unix))]
        {
            let temporary_path = parent.path.join(&temporary);
            let target = parent.path.join(&name);
            let mut options = std::fs::OpenOptions::new();
            let mut file = options.write(true).create_new(true).open(&temporary_path)?;
            let result = (|| {
                file.write_all(bytes)?;
                file.sync_all()?;
                std::fs::rename(&temporary_path, &target)
            })();
            if result.is_err() {
                let _ = std::fs::remove_file(&temporary_path);
            }
            result
        }
    }

    /// Create one symbolic-link leaf beneath retained directory capabilities.
    /// Any pre-existing destination fails closed.
    pub fn create_symlink(
        &self,
        relative: impl AsRef<Path>,
        target: impl AsRef<Path>,
    ) -> io::Result<()> {
        let (parent, name) = self.parent_and_name(relative.as_ref(), true)?;
        match parent.read_leaf(Path::new(&name))? {
            None => {}
            Some(_) => return Err(invalid_path(&parent.path.join(&name), "already exists")),
        }
        #[cfg(unix)]
        {
            fs::symlinkat(target.as_ref(), parent.fd.as_ref(), &name).map_err(io::Error::from)?;
            fs::fsync(parent.fd.as_ref()).map_err(io::Error::from)
        }
        #[cfg(not(unix))]
        {
            let _ = target;
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "creating symbolic links is supported only on Unix",
            ))
        }
    }

    /// Append bytes to a regular descendant, optionally syncing before return.
    pub fn append(&self, relative: impl AsRef<Path>, bytes: &[u8], sync: bool) -> io::Result<()> {
        let (parent, name) = self.parent_and_name(relative.as_ref(), true)?;
        parent.reject_non_regular_target(&name)?;
        #[cfg(unix)]
        {
            let fd = fs::openat(
                parent.fd.as_ref(),
                &name,
                OFlags::WRONLY
                    | OFlags::APPEND
                    | OFlags::CREATE
                    | OFlags::NOFOLLOW
                    | OFlags::CLOEXEC,
                Mode::RUSR | Mode::WUSR,
            )
            .map_err(io::Error::from)?;
            ensure_regular_fd(&fd, &parent.path.join(&name))?;
            let mut file = File::from(fd);
            file.write_all(bytes)?;
            if sync {
                file.sync_data()?;
            }
            Ok(())
        }
        #[cfg(not(unix))]
        {
            let path = parent.path.join(name);
            let mut file = std::fs::OpenOptions::new()
                .append(true)
                .create(true)
                .open(path)?;
            file.write_all(bytes)?;
            if sync {
                file.sync_data()?;
            }
            Ok(())
        }
    }

    /// Remove one exact regular descendant. Symlinks and directories fail closed.
    pub fn remove_file(&self, relative: impl AsRef<Path>) -> io::Result<()> {
        let (parent, name) = self.parent_and_name(relative.as_ref(), false)?;
        parent.reject_non_regular_target(&name)?;
        #[cfg(unix)]
        {
            fs::unlinkat(parent.fd.as_ref(), &name, AtFlags::empty()).map_err(io::Error::from)
        }
        #[cfg(not(unix))]
        {
            std::fs::remove_file(parent.path.join(name))
        }
    }

    /// Remove one exact regular file or symbolic-link leaf without following
    /// the link. Directories and special files fail closed.
    pub fn remove_leaf(&self, relative: impl AsRef<Path>) -> io::Result<()> {
        let (parent, name) = self.parent_and_name(relative.as_ref(), false)?;
        #[cfg(unix)]
        {
            let stat = fs::statat(parent.fd.as_ref(), &name, AtFlags::SYMLINK_NOFOLLOW)
                .map_err(io::Error::from)?;
            match FileType::from_raw_mode(stat.st_mode) {
                FileType::RegularFile if stat.st_nlink == 1 => {}
                FileType::Symlink => {}
                _ => {
                    return Err(invalid_path(
                        &parent.path.join(&name),
                        "is not a removable file or symlink",
                    ))
                }
            }
            fs::unlinkat(parent.fd.as_ref(), &name, AtFlags::empty()).map_err(io::Error::from)?;
            fs::fsync(parent.fd.as_ref()).map_err(io::Error::from)
        }
        #[cfg(not(unix))]
        {
            let path = parent.path.join(name);
            let metadata = std::fs::symlink_metadata(&path)?;
            if metadata.file_type().is_file() || metadata.file_type().is_symlink() {
                std::fs::remove_file(path)
            } else {
                Err(invalid_path(&path, "is not a removable file or symlink"))
            }
        }
    }

    /// Remove one exact empty real directory without following a link at any
    /// component. This never recursively removes contents.
    pub fn remove_empty_dir(&self, relative: impl AsRef<Path>) -> io::Result<()> {
        let (parent, name) = self.parent_and_name(relative.as_ref(), false)?;
        // Retaining the child proves the checked entry was a real directory.
        // The final unlinkat below still treats a raced replacement as an
        // exact directory entry and never follows a symlink.
        let _child = parent.existing_child(Path::new(&name))?;
        #[cfg(unix)]
        {
            fs::unlinkat(parent.fd.as_ref(), &name, AtFlags::REMOVEDIR).map_err(io::Error::from)?;
            fs::fsync(parent.fd.as_ref()).map_err(io::Error::from)
        }
        #[cfg(not(unix))]
        {
            std::fs::remove_dir(parent.path.join(name))
        }
    }

    /// Atomically move one regular-file or symbolic-link leaf between retained
    /// directory capabilities. Source and destination parents are resolved
    /// fd-relative; neither an ambient parent swap nor a final symlink can
    /// redirect the mutation.
    pub fn rename_leaf_to(
        &self,
        source: impl AsRef<Path>,
        destination: &Self,
        target: impl AsRef<Path>,
    ) -> io::Result<()> {
        let (source_parent, source_name) = self.parent_and_name(source.as_ref(), false)?;
        let (target_parent, target_name) = destination.parent_and_name(target.as_ref(), true)?;
        #[cfg(unix)]
        {
            let source_stat = fs::statat(
                source_parent.fd.as_ref(),
                &source_name,
                AtFlags::SYMLINK_NOFOLLOW,
            )
            .map_err(io::Error::from)?;
            match FileType::from_raw_mode(source_stat.st_mode) {
                FileType::RegularFile if source_stat.st_nlink == 1 => {}
                FileType::Symlink => {}
                _ => {
                    return Err(invalid_path(
                        &source_parent.path.join(&source_name),
                        "is not a movable file or symlink",
                    ))
                }
            }
            match fs::statat(
                target_parent.fd.as_ref(),
                &target_name,
                AtFlags::SYMLINK_NOFOLLOW,
            ) {
                Ok(stat) => match FileType::from_raw_mode(stat.st_mode) {
                    FileType::RegularFile if stat.st_nlink == 1 => {}
                    FileType::Symlink => {}
                    _ => {
                        return Err(invalid_path(
                            &target_parent.path.join(&target_name),
                            "is not a replaceable file or symlink",
                        ))
                    }
                },
                Err(rustix::io::Errno::NOENT) => {}
                Err(error) => return Err(io::Error::from(error)),
            }
            fs::renameat(
                source_parent.fd.as_ref(),
                &source_name,
                target_parent.fd.as_ref(),
                &target_name,
            )
            .map_err(io::Error::from)?;
            fs::fsync(target_parent.fd.as_ref()).map_err(io::Error::from)?;
            fs::fsync(source_parent.fd.as_ref()).map_err(io::Error::from)
        }
        #[cfg(not(unix))]
        {
            let source_path = source_parent.path.join(source_name);
            let target_path = target_parent.path.join(target_name);
            let source_metadata = std::fs::symlink_metadata(&source_path)?;
            if !(source_metadata.file_type().is_file() || source_metadata.file_type().is_symlink())
            {
                return Err(invalid_path(
                    &source_path,
                    "is not a movable file or symlink",
                ));
            }
            match std::fs::symlink_metadata(&target_path) {
                Ok(metadata)
                    if metadata.file_type().is_file() || metadata.file_type().is_symlink() => {}
                Ok(_) => {
                    return Err(invalid_path(
                        &target_path,
                        "is not a replaceable file or symlink",
                    ))
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
            std::fs::rename(source_path, target_path)
        }
    }

    /// Recursively remove one real descendant directory without following any
    /// link encountered at or below the deletion boundary.
    pub fn remove_dir_all(&self, relative: impl AsRef<Path>) -> io::Result<()> {
        let relative = relative.as_ref();
        let (parent, name) = self.parent_and_name(relative, false)?;
        let child = parent.existing_child(Path::new(&name))?;
        child.remove_open_dir_contents()?;
        #[cfg(unix)]
        {
            fs::unlinkat(parent.fd.as_ref(), &name, AtFlags::REMOVEDIR).map_err(io::Error::from)
        }
        #[cfg(not(unix))]
        {
            std::fs::remove_dir(parent.path.join(name))
        }
    }

    fn remove_open_dir_contents(&self) -> io::Result<()> {
        for entry in self.entries()? {
            #[cfg(unix)]
            {
                if entry.file_type == SecureEntryType::Directory {
                    let child = self.existing_child(Path::new(&entry.name))?;
                    child.remove_open_dir_contents()?;
                    fs::unlinkat(self.fd.as_ref(), &entry.name, AtFlags::REMOVEDIR)
                        .map_err(io::Error::from)?;
                } else {
                    // unlinkat never follows the final component. Removing a
                    // hostile symlink inside a managed tree is therefore safe.
                    fs::unlinkat(self.fd.as_ref(), &entry.name, AtFlags::empty())
                        .map_err(io::Error::from)?;
                }
            }
            #[cfg(not(unix))]
            {
                let path = self.path.join(&entry.name);
                if entry.file_type == SecureEntryType::Directory {
                    self.existing_child(Path::new(&entry.name))?
                        .remove_open_dir_contents()?;
                    std::fs::remove_dir(path)?;
                } else {
                    std::fs::remove_file(path)?;
                }
            }
        }
        Ok(())
    }

    fn parent_and_name(
        &self,
        relative: &Path,
        create_parent: bool,
    ) -> io::Result<(Self, OsString)> {
        validate_relative(relative)?;
        let name = relative
            .file_name()
            .ok_or_else(|| invalid_path(relative, "does not name a file"))?
            .to_os_string();
        let parent_relative = relative.parent().unwrap_or_else(|| Path::new(""));
        let parent = self.child_impl(parent_relative, create_parent)?;
        Ok((parent, name))
    }

    fn reject_non_regular_target(&self, name: &OsStr) -> io::Result<()> {
        #[cfg(unix)]
        {
            match fs::statat(self.fd.as_ref(), name, AtFlags::SYMLINK_NOFOLLOW) {
                Ok(stat)
                    if FileType::from_raw_mode(stat.st_mode) == FileType::RegularFile
                        && stat.st_nlink == 1 =>
                {
                    Ok(())
                }
                Ok(_) => Err(invalid_path(&self.path.join(name), "is not a regular file")),
                Err(rustix::io::Errno::NOENT) => Ok(()),
                Err(error) => Err(io::Error::from(error)),
            }
        }
        #[cfg(not(unix))]
        {
            match std::fs::symlink_metadata(self.path.join(name)) {
                Ok(metadata)
                    if metadata.file_type().is_file() && !metadata.file_type().is_symlink() =>
                {
                    Ok(())
                }
                Ok(_) => Err(invalid_path(&self.path.join(name), "is not a regular file")),
                Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
                Err(error) => Err(error),
            }
        }
    }

    #[cfg(unix)]
    fn child_impl(&self, relative: &Path, create: bool) -> io::Result<Self> {
        validate_relative(relative)?;
        let mut current = self.clone();
        for component in relative.components() {
            let Component::Normal(name) = component else {
                return Err(invalid_path(
                    relative,
                    "contains a non-descendant component",
                ));
            };
            let opened = match fs::openat(
                current.fd.as_ref(),
                name,
                OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::empty(),
            ) {
                Ok(fd) => fd,
                Err(rustix::io::Errno::NOENT) if create => {
                    match fs::mkdirat(
                        current.fd.as_ref(),
                        name,
                        Mode::RUSR | Mode::WUSR | Mode::XUSR,
                    ) {
                        Ok(()) | Err(rustix::io::Errno::EXIST) => {}
                        Err(error) => return Err(io::Error::from(error)),
                    }
                    fs::openat(
                        current.fd.as_ref(),
                        name,
                        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                        Mode::empty(),
                    )
                    .map_err(io::Error::from)?
                }
                Err(error) => return Err(io::Error::from(error)),
            };
            current = Self {
                path: current.path.join(name),
                fd: Arc::new(opened),
            };
        }
        Ok(current)
    }

    #[cfg(not(unix))]
    fn child_impl(&self, relative: &Path, create: bool) -> io::Result<Self> {
        validate_relative(relative)?;
        let mut current = self.path.clone();
        for component in relative.components() {
            let Component::Normal(name) = component else {
                return Err(invalid_path(
                    relative,
                    "contains a non-descendant component",
                ));
            };
            current.push(name);
            match std::fs::symlink_metadata(&current) {
                Ok(metadata)
                    if metadata.file_type().is_dir() && !metadata.file_type().is_symlink() => {}
                Ok(_) => return Err(invalid_path(&current, "is not a real directory")),
                Err(error) if error.kind() == io::ErrorKind::NotFound && create => {
                    std::fs::create_dir(&current)?;
                }
                Err(error) => return Err(error),
            }
        }
        Ok(Self { path: current })
    }
}

#[cfg(unix)]
fn normalize_platform_absolute_prefix(path: &Path) -> io::Result<PathBuf> {
    #[cfg(target_os = "macos")]
    {
        // macOS ships `/var`, `/tmp`, and `/etc` as root-owned compatibility
        // symlinks into `/private`. Resolve only these fixed platform links;
        // every caller-controlled descendant is still walked with NOFOLLOW.
        for prefix in [Path::new("/var"), Path::new("/tmp"), Path::new("/etc")] {
            if let Ok(suffix) = path.strip_prefix(prefix) {
                let canonical = std::fs::canonicalize(prefix)?;
                return Ok(canonical.join(suffix));
            }
        }
    }
    Ok(path.to_path_buf())
}

fn validate_relative(path: &Path) -> io::Result<()> {
    if path.is_absolute()
        || path
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(invalid_path(path, "is not a strict relative descendant"));
    }
    Ok(())
}

fn invalid_path(path: &Path, reason: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        format!("managed path '{}' {reason}", path.display()),
    )
}

#[cfg(unix)]
fn ensure_regular_fd(fd: &OwnedFd, path: &Path) -> io::Result<()> {
    let stat = fs::fstat(fd).map_err(io::Error::from)?;
    if FileType::from_raw_mode(stat.st_mode) == FileType::RegularFile && stat.st_nlink == 1 {
        Ok(())
    } else {
        Err(invalid_path(path, "is not a uniquely linked regular file"))
    }
}

#[cfg(not(unix))]
fn ensure_regular_path(path: &Path) -> io::Result<()> {
    let metadata = std::fs::symlink_metadata(path)?;
    if metadata.file_type().is_file() && !metadata.file_type().is_symlink() {
        Ok(())
    } else {
        Err(invalid_path(path, "is not a regular file"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn atomic_write_roundtrips_and_leaves_no_temporary_file() {
        let root = tempfile::tempdir().unwrap();
        let dir = SecureDir::open(root.path()).unwrap();
        dir.atomic_write("nested/state.json", b"first").unwrap();
        dir.atomic_write("nested/state.json", b"second").unwrap();
        assert_eq!(dir.read("nested/state.json").unwrap(), b"second");
        let names = std::fs::read_dir(root.path().join("nested"))
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect::<Vec<_>>();
        assert_eq!(names, [OsString::from("state.json")]);
    }

    #[test]
    fn limited_read_uses_the_open_file_and_enforces_its_ceiling() {
        let root = tempfile::tempdir().unwrap();
        let dir = SecureDir::open(root.path()).unwrap();
        dir.atomic_write("small.json", b"1234").unwrap();
        assert_eq!(dir.read_limited("small.json", 4).unwrap(), b"1234");
        assert!(dir.read_limited("small.json", 3).is_err());
    }

    #[test]
    fn bounded_directory_scan_rejects_before_collecting_past_the_limit() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("one"), b"1").unwrap();
        std::fs::write(root.path().join("two"), b"2").unwrap();
        let dir = SecureDir::open(root.path()).unwrap();

        assert_eq!(dir.entries_limited(2).unwrap().len(), 2);
        let error = dir.entries_limited(1).unwrap_err().to_string();
        assert!(error.contains("scan limit"), "{error}");
    }

    #[test]
    fn limited_leaf_rejects_dense_and_sparse_files_before_unbounded_reads() {
        let root = tempfile::tempdir().unwrap();
        let dir = SecureDir::open(root.path()).unwrap();
        std::fs::write(root.path().join("dense"), b"12345").unwrap();
        assert!(dir.read_leaf_limited("dense", 4).is_err());

        let sparse = File::create(root.path().join("sparse")).unwrap();
        sparse.set_len(8 * 1024 * 1024 * 1024).unwrap();
        assert!(dir.read_leaf_limited("sparse", 1024).is_err());
        assert!(dir.open_file_limited("sparse", 1024).is_err());
    }

    #[test]
    fn open_or_create_all_walks_an_absolute_nested_path() {
        let root = tempfile::tempdir().unwrap();
        let nested = root.path().join("one/two");
        let dir = SecureDir::open_or_create_all(&nested).unwrap();
        let canonical_nested = std::fs::canonicalize(root.path()).unwrap().join("one/two");
        assert_eq!(dir.path(), canonical_nested);
        dir.atomic_write("state", b"ok").unwrap();
        assert_eq!(std::fs::read(nested.join("state")).unwrap(), b"ok");
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_ancestor_and_target_fail_closed() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("sentinel"), b"safe").unwrap();
        symlink(outside.path(), root.path().join("linked")).unwrap();
        let dir = SecureDir::open(root.path()).unwrap();
        assert!(dir.atomic_write("linked/state.json", b"owned").is_err());
        assert_eq!(
            std::fs::read(outside.path().join("sentinel")).unwrap(),
            b"safe"
        );
        assert!(!outside.path().join("state.json").exists());

        symlink(
            outside.path().join("sentinel"),
            root.path().join("target.json"),
        )
        .unwrap();
        assert!(dir.atomic_write("target.json", b"owned").is_err());
        assert!(dir.read("target.json").is_err());
        assert_eq!(
            std::fs::read(outside.path().join("sentinel")).unwrap(),
            b"safe"
        );
    }

    #[cfg(unix)]
    #[test]
    fn predictable_legacy_temp_symlink_is_never_opened() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(outside.path(), b"safe").unwrap();
        symlink(outside.path(), root.path().join("state.json.tmp")).unwrap();
        let dir = SecureDir::open(root.path()).unwrap();
        dir.atomic_write("state.json", b"owned").unwrap();
        assert_eq!(dir.read("state.json").unwrap(), b"owned");
        assert_eq!(std::fs::read(outside.path()).unwrap(), b"safe");
    }

    #[cfg(unix)]
    #[test]
    fn recursive_removal_never_follows_a_symlinked_boundary_or_child() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let sentinel = outside.path().join("sentinel");
        std::fs::write(&sentinel, b"safe").unwrap();
        symlink(outside.path(), root.path().join("linked-boundary")).unwrap();
        let dir = SecureDir::open(root.path()).unwrap();
        assert!(dir.remove_dir_all("linked-boundary").is_err());
        assert_eq!(std::fs::read(&sentinel).unwrap(), b"safe");

        let managed = root.path().join("managed");
        std::fs::create_dir(&managed).unwrap();
        symlink(&sentinel, managed.join("linked-child")).unwrap();
        std::fs::write(managed.join("owned"), b"delete").unwrap();
        dir.remove_dir_all("managed").unwrap();
        assert!(!managed.exists());
        assert_eq!(std::fs::read(&sentinel).unwrap(), b"safe");
    }

    #[cfg(unix)]
    #[test]
    fn opened_root_survives_path_swap_and_detects_identity_change() {
        let parent = tempfile::tempdir().unwrap();
        let configured = parent.path().join("data");
        let original = parent.path().join("opened-data");
        std::fs::create_dir(&configured).unwrap();
        let root = SecureDir::open(&configured).unwrap();

        std::fs::rename(&configured, &original).unwrap();
        std::fs::create_dir(&configured).unwrap();

        assert!(root.verify_ambient_identity().is_err());
        root.child("sessions")
            .unwrap()
            .atomic_write("state.json", b"owned")
            .unwrap();
        assert_eq!(
            std::fs::read(original.join("sessions/state.json")).unwrap(),
            b"owned"
        );
        assert!(!configured.join("sessions/state.json").exists());
    }

    #[cfg(unix)]
    #[test]
    fn hardlinked_managed_files_fail_closed() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(outside.path(), b"safe").unwrap();
        std::fs::hard_link(outside.path(), root.path().join("managed")).unwrap();
        let dir = SecureDir::open(root.path()).unwrap();

        assert!(dir.read("managed").is_err());
        assert!(dir.append("managed", b"owned", true).is_err());
        assert!(dir.open_lock_file("managed").is_err());
        assert!(dir.atomic_write("managed", b"owned").is_err());
        assert_eq!(std::fs::read(outside.path()).unwrap(), b"safe");
    }

    #[cfg(unix)]
    #[test]
    fn authority_root_requires_expected_owner_and_private_writes_before_chmod() {
        use std::os::unix::fs::PermissionsExt;

        unsafe extern "C" {
            fn geteuid() -> u32;
        }
        let root = tempfile::tempdir().unwrap();
        std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
        let dir = SecureDir::open(root.path()).unwrap();
        // SAFETY: geteuid takes no arguments and has no failure sentinel.
        let effective_uid = unsafe { geteuid() };
        dir.require_owner_and_private_writes(effective_uid).unwrap();
        assert!(dir
            .require_owner_and_private_writes(effective_uid.wrapping_add(1))
            .is_err());

        std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o777)).unwrap();
        assert!(dir.require_owner_and_private_writes(effective_uid).is_err());
    }
}
