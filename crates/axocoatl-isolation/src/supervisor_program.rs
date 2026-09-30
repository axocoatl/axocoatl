//! Verified first-party Linux supervisor payloads, installed separately from
//! repository content. Public executable bytes use mode 0555 so the selected
//! container USER can execute the read-only bind; the host parent stays private.
//! A digest identifies approved bytes; ELF validation only
//! checks executable compatibility and is not a substitute for that trust.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::Arc;
#[cfg(unix)]
use std::time::{Duration, Instant};

use axocoatl_core::SecureDir;
use sha2::{Digest, Sha256};

use crate::error::IsolationError;

pub(crate) const SUPERVISOR_CONTAINER_PATH: &str = "/axocoatl-exec-supervisor";
const MAX_PROGRAM_BYTES: usize = 64 * 1024 * 1024;
const MAX_PROGRAM_HEADERS: usize = 256;
#[cfg(unix)]
const INSTALL_LOCK_TIMEOUT: Duration = Duration::from_secs(10);
#[cfg(unix)]
const INSTALL_LOCK_RETRY: Duration = Duration::from_millis(10);

#[derive(Clone, Debug)]
pub struct SupervisorProgram {
    root: SecureDir,
    file: Arc<File>,
    file_identity: String,
    path: PathBuf,
    filename: String,
    architecture: &'static str,
    sha256: String,
}

fn invalid(message: impl Into<String>) -> IsolationError {
    IsolationError::OciSetupFailed(format!("trusted execution supervisor: {}", message.into()))
}

fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn private_root(root: &SecureDir) -> Result<(), IsolationError> {
    root.verify_ambient_identity()?;
    #[cfg(unix)]
    root.require_owner_and_private_writes(rustix::process::geteuid().as_raw())?;
    #[cfg(not(unix))]
    return Err(invalid(
        "executable installation requires a Unix ownership boundary",
    ));
    #[cfg(unix)]
    Ok(())
}

fn file_identity(file: &File) -> Result<String, IsolationError> {
    let metadata = file.metadata()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if !metadata.is_file()
            || metadata.nlink() != 1
            || metadata.uid() != rustix::process::geteuid().as_raw()
            || metadata.mode() & 0o7777 != 0o555
        {
            return Err(invalid(
                "installed payload must be uniquely linked, owned, and mode 0555",
            ));
        }
        Ok(format!("{}:{}", metadata.dev(), metadata.ino()))
    }
    #[cfg(not(unix))]
    {
        let _ = metadata;
        Err(invalid("executable file identity requires a Unix host"))
    }
}

fn read_bounded(file: &mut File) -> Result<Vec<u8>, IsolationError> {
    file.seek(SeekFrom::Start(0))?;
    let mut bytes = Vec::new();
    file.take(MAX_PROGRAM_BYTES as u64 + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() > MAX_PROGRAM_BYTES {
        return Err(invalid("payload exceeds its byte bound"));
    }
    Ok(bytes)
}

#[cfg(unix)]
fn wait_for_install_lock(lock: &SecureDir, timeout: Duration) -> Result<(), IsolationError> {
    let started = Instant::now();
    let expired = || {
        invalid(format!(
            "timed out waiting for the executable installation lock after {} ms",
            timeout.as_millis(),
        ))
    };
    loop {
        private_root(lock)?;
        match lock.try_lock_exclusive() {
            Ok(()) => return private_root(lock),
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
                ) => {}
            Err(error) => return Err(error.into()),
        }
        let Some(remaining) = timeout
            .checked_sub(started.elapsed())
            .filter(|remaining| !remaining.is_zero())
        else {
            return Err(expired());
        };
        std::thread::sleep(remaining.min(INSTALL_LOCK_RETRY));
        if started.elapsed() >= timeout {
            return Err(expired());
        }
    }
}

impl SupervisorProgram {
    /// Install the first-party Linux supervisor carried inside Axocoatl itself.
    /// The architecture must come from the selected Linux runtime or image;
    /// the host architecture is not a substitute for that observation.
    /// This performs blocking I/O and may wait for another installer. Async
    /// startup uses `install_embedded_async` to keep its executor responsive.
    pub fn install_embedded(
        architecture: &str,
        private_dir: &SecureDir,
    ) -> Result<Self, IsolationError> {
        let embedded = crate::supervisor_embedded::payload(architecture)?;
        embedded.verify_protocol()?;
        let program = Self::install_bytes(embedded.bytes, embedded.sha256, private_dir)?;
        if program.architecture() != embedded.architecture {
            return Err(invalid(
                "embedded payload architecture does not match its identity",
            ));
        }
        Ok(program)
    }

    /// Keep filesystem lock waits and durable installation off the async
    /// executor. Cancelling this await cannot start a container: the owned
    /// worker can only finish installing immutable bytes in the private root,
    /// which a later exact retry verifies and reuses.
    pub(crate) async fn install_embedded_async(
        architecture: &str,
        private_dir: &SecureDir,
    ) -> Result<Self, IsolationError> {
        let architecture = architecture.to_owned();
        let private_dir = private_dir.clone();
        tokio::task::spawn_blocking(move || Self::install_embedded(&architecture, &private_dir))
            .await
            .map_err(|error| invalid(format!("owned executable installer failed: {error}")))?
    }

    /// Copy exact approved bytes into a private content-addressed executable.
    /// Existing payloads are never intentionally rewritten. Uncertain writes
    /// return no capability; a later exact installation verifies and fsyncs it.
    /// This synchronous operation can wait for the bounded installation lock.
    pub fn install(
        source: &Path,
        expected_sha256: &str,
        private_dir: &SecureDir,
    ) -> Result<Self, IsolationError> {
        let parent = source
            .parent()
            .filter(|path| path.is_absolute())
            .ok_or_else(|| invalid("source must have an absolute parent directory"))?;
        // The approved digest binds source bytes. Host aliases in ancestors
        // (for example macOS /var) are allowed; the opened parent and final
        // regular file remain descriptor-relative and the leaf is nofollow.
        let source_root = SecureDir::open(parent)?;
        let name = source
            .file_name()
            .ok_or_else(|| invalid("source has no filename"))?;
        let mut source_file = source_root.open_file_limited(name, MAX_PROGRAM_BYTES)?;
        let bytes = read_bounded(&mut source_file)?;
        Self::install_bytes(&bytes, expected_sha256, private_dir)
    }

    /// Both embedded and explicit-source installation use this same boundary.
    /// No executable is written before its size, digest, and ELF are checked.
    fn install_bytes(
        bytes: &[u8],
        expected_sha256: &str,
        private_dir: &SecureDir,
    ) -> Result<Self, IsolationError> {
        if bytes.len() > MAX_PROGRAM_BYTES {
            return Err(invalid("payload exceeds its byte bound"));
        }
        if expected_sha256.len() != 64
            || !expected_sha256
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err(invalid(
                "expected SHA-256 must be 64 lowercase hexadecimal digits",
            ));
        }
        private_root(private_dir)?;
        if digest(bytes) != expected_sha256 {
            return Err(invalid("payload does not match its approved SHA-256"));
        }
        let architecture = validate_elf(bytes)?;
        // Serialize trusted installers without retaining this lock throughout
        // a sandbox lifetime or replacing another reader's installed inode.
        #[cfg(unix)]
        let install_lock = private_dir.child("supervisor-install-lock")?;
        #[cfg(unix)]
        wait_for_install_lock(&install_lock, INSTALL_LOCK_TIMEOUT)?;
        private_root(private_dir)?;
        let filename = format!("supervisor-sha256-{expected_sha256}");
        let path = private_dir.path().join(&filename);
        if path
            .to_str()
            .is_none_or(|path| path.contains([',', ':', '\n', '\r']))
        {
            return Err(invalid(
                "installed path cannot be represented as an exact Podman bind mount",
            ));
        }
        let mut file = match private_dir.open_file_limited(&filename, MAX_PROGRAM_BYTES) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                private_dir.atomic_write_with_mode(&filename, bytes, 0o555)?;
                private_dir.open_file_limited(&filename, MAX_PROGRAM_BYTES)?
            }
            Err(error) => return Err(error.into()),
        };
        let identity = file_identity(&file)?;
        if read_bounded(&mut file)? != bytes {
            return Err(invalid(
                "existing content-addressed payload differs from approved bytes",
            ));
        }
        file.sync_all()?;
        private_dir.sync_all()?;
        let program = Self {
            root: private_dir.clone(),
            file: Arc::new(file),
            file_identity: identity,
            path,
            filename,
            architecture,
            sha256: expected_sha256.to_owned(),
        };
        program.verify()?;
        Ok(program)
    }

    /// Revalidate both retained and ambient identities and actual bytes. A
    /// replacement with identical contents still invalidates this capability.
    pub fn verify(&self) -> Result<(), IsolationError> {
        private_root(&self.root)?;
        if file_identity(&self.file)? != self.file_identity {
            return Err(invalid("retained executable identity changed"));
        }
        let mut current = self
            .root
            .open_file_limited(&self.filename, MAX_PROGRAM_BYTES)?;
        if file_identity(&current)? != self.file_identity {
            return Err(invalid("installed executable was replaced"));
        }
        let bytes = read_bounded(&mut current)?;
        if digest(&bytes) != self.sha256 || validate_elf(&bytes)? != self.architecture {
            return Err(invalid("installed executable bytes changed"));
        }
        self.root.verify_ambient_identity()?;
        Ok(())
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
    pub fn architecture(&self) -> &str {
        self.architecture
    }
    pub fn sha256(&self) -> &str {
        &self.sha256
    }
    pub(crate) fn private_dir(&self) -> &SecureDir {
        &self.root
    }
}

fn u16_at(bytes: &[u8], offset: usize) -> Result<u16, IsolationError> {
    Ok(u16::from_le_bytes(
        bytes
            .get(offset..offset + 2)
            .ok_or_else(|| invalid("truncated ELF field"))?
            .try_into()
            .unwrap(),
    ))
}
fn u32_at(bytes: &[u8], offset: usize) -> Result<u32, IsolationError> {
    Ok(u32::from_le_bytes(
        bytes
            .get(offset..offset + 4)
            .ok_or_else(|| invalid("truncated ELF field"))?
            .try_into()
            .unwrap(),
    ))
}
fn u64_at(bytes: &[u8], offset: usize) -> Result<u64, IsolationError> {
    Ok(u64::from_le_bytes(
        bytes
            .get(offset..offset + 8)
            .ok_or_else(|| invalid("truncated ELF field"))?
            .try_into()
            .unwrap(),
    ))
}

fn bounded_range(
    offset: u64,
    length: u64,
    bound: usize,
) -> Result<std::ops::Range<usize>, IsolationError> {
    let end = offset
        .checked_add(length)
        .filter(|end| *end <= bound as u64)
        .ok_or_else(|| invalid("ELF range exceeds payload"))?;
    Ok(offset as usize..end as usize)
}

fn validate_elf(bytes: &[u8]) -> Result<&'static str, IsolationError> {
    if bytes.len() < 64
        || &bytes[..7] != b"\x7fELF\x02\x01\x01"
        || !matches!(bytes[7], 0 | 3)
        || bytes[8] != 0
        || u16_at(bytes, 52)? != 64
        || u16_at(bytes, 54)? != 56
        || !matches!(u16_at(bytes, 16)?, 2 | 3)
        || u32_at(bytes, 20)? != 1
    {
        return Err(invalid("requires a Linux ELF64 little-endian executable"));
    }
    let architecture = match u16_at(bytes, 18)? {
        62 => "x86_64",
        183 => "aarch64",
        _ => return Err(invalid("unsupported ELF architecture")),
    };
    let count = usize::from(u16_at(bytes, 56)?);
    if count == 0 || count > MAX_PROGRAM_HEADERS {
        return Err(invalid("ELF program-header count exceeds its bound"));
    }
    let headers = bounded_range(u64_at(bytes, 32)?, (count * 56) as u64, bytes.len())?;
    if headers.start < 64 {
        return Err(invalid("ELF program headers overlap its header"));
    }
    let entry = u64_at(bytes, 24)?;
    let mut executable_entry = false;
    for header in (headers.start..headers.end).step_by(56) {
        let kind = u32_at(bytes, header)?;
        let segment = bounded_range(
            u64_at(bytes, header + 8)?,
            u64_at(bytes, header + 32)?,
            bytes.len(),
        )?;
        if kind == 3 {
            return Err(invalid("dynamic interpreter is not permitted"));
        }
        if kind == 1 {
            let virtual_address = u64_at(bytes, header + 16)?;
            let file_size = u64_at(bytes, header + 32)?;
            let memory_size = u64_at(bytes, header + 40)?;
            let end = virtual_address
                .checked_add(file_size)
                .ok_or_else(|| invalid("ELF executable range overflow"))?;
            if file_size > memory_size || virtual_address.checked_add(memory_size).is_none() {
                return Err(invalid("ELF segment exceeds its memory range"));
            }
            executable_entry |=
                u32_at(bytes, header + 4)? & 1 != 0 && entry >= virtual_address && entry < end;
        }
        if kind == 2 {
            if segment.len() % 16 != 0 {
                return Err(invalid("invalid ELF dynamic table"));
            }
            let mut terminated = false;
            for dynamic in (segment.start..segment.end).step_by(16) {
                match u64_at(bytes, dynamic)? {
                    0 => {
                        terminated = true;
                        break;
                    }
                    1 | 0x7fffffff | 0x7ffffffd => {
                        return Err(invalid("shared-library dependency is not permitted"))
                    }
                    _ => {}
                }
            }
            if !terminated {
                return Err(invalid("unterminated ELF dynamic table"));
            }
        }
    }
    if !executable_entry {
        return Err(invalid("ELF entry is outside its executable file segments"));
    }
    Ok(architecture)
}

#[cfg(test)]
pub(crate) fn test_elf(machine: u16) -> Vec<u8> {
    let mut bytes = vec![0; 256];
    bytes[..7].copy_from_slice(b"\x7fELF\x02\x01\x01");
    bytes[16..18].copy_from_slice(&2_u16.to_le_bytes());
    bytes[18..20].copy_from_slice(&machine.to_le_bytes());
    bytes[20..24].copy_from_slice(&1_u32.to_le_bytes());
    bytes[24..32].copy_from_slice(&0x4000c0_u64.to_le_bytes());
    bytes[32..40].copy_from_slice(&64_u64.to_le_bytes());
    bytes[52..54].copy_from_slice(&64_u16.to_le_bytes());
    bytes[54..56].copy_from_slice(&56_u16.to_le_bytes());
    bytes[56..58].copy_from_slice(&1_u16.to_le_bytes());
    bytes[64..68].copy_from_slice(&1_u32.to_le_bytes());
    bytes[68..72].copy_from_slice(&5_u32.to_le_bytes());
    bytes[80..88].copy_from_slice(&0x400000_u64.to_le_bytes());
    bytes[96..104].copy_from_slice(&256_u64.to_le_bytes());
    bytes[104..112].copy_from_slice(&256_u64.to_le_bytes());
    bytes
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    fn install(bytes: &[u8]) -> (tempfile::TempDir, SupervisorProgram) {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("source");
        std::fs::write(&source, bytes).unwrap();
        let private = SecureDir::open(root.path())
            .unwrap()
            .child("private")
            .unwrap();
        let program = SupervisorProgram::install(&source, &digest(bytes), &private).unwrap();
        (root, program)
    }

    #[test]
    fn exact_installation_is_immutable_and_keeps_both_linux_architectures() {
        for (machine, architecture) in [(62, "x86_64"), (183, "aarch64")] {
            let bytes = test_elf(machine);
            let (root, program) = install(&bytes);
            assert_eq!(program.architecture(), architecture);
            assert_eq!(program.sha256(), digest(&bytes));
            assert_eq!(
                std::fs::metadata(program.path()).unwrap().mode() & 0o7777,
                0o555
            );
            let repeat = SupervisorProgram::install(
                &root.path().join("source"),
                &digest(&bytes),
                program.private_dir(),
            )
            .unwrap();
            assert_eq!(repeat.file_identity, program.file_identity);
            program.clone().verify().unwrap();
            std::fs::write(root.path().join("source"), b"changed source").unwrap();
            program.verify().unwrap();
        }
    }

    #[test]
    fn incorrect_hash_or_executable_format_never_installs_a_payload() {
        let root = tempfile::tempdir().unwrap();
        let private = SecureDir::open(root.path())
            .unwrap()
            .child("private")
            .unwrap();
        let source = root.path().join("source");
        let valid = test_elf(62);
        std::fs::write(&source, &valid).unwrap();
        assert!(SupervisorProgram::install(&source, &"0".repeat(64), &private).is_err());
        let mut cases = vec![test_elf(40), valid[..63].to_vec()];
        let mut wrong_endian = valid.clone();
        wrong_endian[5] = 2;
        cases.push(wrong_endian);
        let mut interpreter = valid.clone();
        interpreter[56..58].copy_from_slice(&2_u16.to_le_bytes());
        interpreter[120..124].copy_from_slice(&3_u32.to_le_bytes());
        cases.push(interpreter);
        for tag in [1_u64, 0x7fffffff, 0x7ffffffd] {
            let mut dependency = valid.clone();
            dependency[56..58].copy_from_slice(&2_u16.to_le_bytes());
            dependency[120..124].copy_from_slice(&2_u32.to_le_bytes());
            dependency[128..136].copy_from_slice(&208_u64.to_le_bytes());
            dependency[152..160].copy_from_slice(&32_u64.to_le_bytes());
            dependency[208..216].copy_from_slice(&tag.to_le_bytes());
            cases.push(dependency);
        }
        let mut overflowing = valid.clone();
        overflowing[32..40].copy_from_slice(&u64::MAX.to_le_bytes());
        cases.push(overflowing);
        let mut bad_entry = valid;
        bad_entry[24..32].copy_from_slice(&1_u64.to_le_bytes());
        cases.push(bad_entry);
        for bytes in cases {
            std::fs::write(&source, &bytes).unwrap();
            assert!(SupervisorProgram::install(&source, &digest(&bytes), &private).is_err());
        }
        assert!(private.entries().unwrap().is_empty());
    }

    #[test]
    fn payload_replacement_or_mutation_invalidates_retained_capability() {
        let bytes = test_elf(62);
        let (_root, program) = install(&bytes);
        program
            .private_dir()
            .atomic_write_with_mode(&program.filename, &bytes, 0o555)
            .unwrap();
        assert!(program.verify().is_err());
        let (_root, program) = install(&bytes);
        std::fs::set_permissions(program.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::write(program.path(), b"corrupt").unwrap();
        std::fs::set_permissions(program.path(), std::fs::Permissions::from_mode(0o555)).unwrap();
        assert!(program.verify().is_err());
    }

    #[test]
    fn root_replacement_symlink_source_and_writable_private_root_are_refused() {
        let bytes = test_elf(62);
        let (root, program) = install(&bytes);
        let retained = root.path().join("retained");
        std::fs::rename(program.private_dir().path(), &retained).unwrap();
        std::fs::create_dir(program.private_dir().path()).unwrap();
        assert!(program.verify().is_err());
        let link = root.path().join("linked-source");
        std::os::unix::fs::symlink(root.path().join("source"), &link).unwrap();
        let private = SecureDir::open(root.path())
            .unwrap()
            .child("other-private")
            .unwrap();
        assert!(SupervisorProgram::install(&link, &digest(&bytes), &private).is_err());
        std::fs::set_permissions(private.path(), std::fs::Permissions::from_mode(0o777)).unwrap();
        assert!(
            SupervisorProgram::install(&root.path().join("source"), &digest(&bytes), &private)
                .is_err()
        );
    }

    #[test]
    fn embedded_payloads_install_exact_bytes_for_the_runtime_architecture() {
        let root = tempfile::tempdir().unwrap();
        let private = SecureDir::open(root.path())
            .unwrap()
            .child("private")
            .unwrap();
        for (architecture, alias) in [("x86_64", "amd64"), ("aarch64", "arm64")] {
            let embedded = crate::supervisor_embedded::payload(architecture).unwrap();
            let program = SupervisorProgram::install_embedded(architecture, &private).unwrap();
            assert_eq!(program.architecture(), architecture);
            assert_eq!(program.sha256(), embedded.sha256);
            assert_eq!(std::fs::read(program.path()).unwrap(), embedded.bytes);
            assert_eq!(
                std::fs::metadata(program.path()).unwrap().mode() & 0o7777,
                0o555
            );
            let repeat = SupervisorProgram::install_embedded(alias, &private).unwrap();
            assert_eq!(program.file_identity, repeat.file_identity);
            program.verify().unwrap();
        }
    }

    #[test]
    fn unknown_runtime_architecture_does_not_install_anything() {
        let root = tempfile::tempdir().unwrap();
        let private = SecureDir::open(root.path())
            .unwrap()
            .child("private")
            .unwrap();
        for architecture in ["", "arm", "i686", "riscv64", "AARCH64", "x86_64\n"] {
            assert!(SupervisorProgram::install_embedded(architecture, &private).is_err());
        }
        assert!(private.entries().unwrap().is_empty());
    }

    #[test]
    fn changed_or_truncated_real_payload_bytes_never_install() {
        let root = tempfile::tempdir().unwrap();
        let private = SecureDir::open(root.path())
            .unwrap()
            .child("private")
            .unwrap();
        for architecture in ["x86_64", "aarch64"] {
            let embedded = crate::supervisor_embedded::payload(architecture).unwrap();
            let mut changed = embedded.bytes.to_vec();
            let middle = changed.len() / 2;
            changed[middle] ^= 1;
            assert!(SupervisorProgram::install_bytes(&changed, embedded.sha256, &private).is_err());
            assert!(SupervisorProgram::install_bytes(
                &embedded.bytes[..embedded.bytes.len() - 1],
                embedded.sha256,
                &private,
            )
            .is_err());
        }
        assert!(private.entries().unwrap().is_empty());
    }

    #[test]
    fn embedded_retry_rejects_corruption_without_replacing_the_payload() {
        let root = tempfile::tempdir().unwrap();
        let private = SecureDir::open(root.path())
            .unwrap()
            .child("private")
            .unwrap();
        let program = SupervisorProgram::install_embedded("aarch64", &private).unwrap();
        let mut corrupt = std::fs::read(program.path()).unwrap();
        let middle = corrupt.len() / 2;
        corrupt[middle] ^= 1;
        std::fs::set_permissions(program.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::write(program.path(), &corrupt).unwrap();
        std::fs::set_permissions(program.path(), std::fs::Permissions::from_mode(0o555)).unwrap();
        assert!(program.verify().is_err());
        assert!(SupervisorProgram::install_embedded("aarch64", &private).is_err());
        assert_eq!(std::fs::read(program.path()).unwrap(), corrupt);
    }

    #[test]
    fn embedded_retry_rejects_aliases_to_the_installed_executable() {
        for hard_link in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let private = SecureDir::open(root.path())
                .unwrap()
                .child("private")
                .unwrap();
            let program = SupervisorProgram::install_embedded("x86_64", &private).unwrap();
            if hard_link {
                std::fs::hard_link(program.path(), root.path().join("untrusted-alias")).unwrap();
            } else {
                let moved = root.path().join("moved-payload");
                std::fs::rename(program.path(), &moved).unwrap();
                std::os::unix::fs::symlink(moved, program.path()).unwrap();
            }
            assert!(program.verify().is_err());
            assert!(SupervisorProgram::install_embedded("x86_64", &private).is_err());
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn concurrent_installers_wait_off_executor_and_reuse_the_same_inodes() {
        let root = tempfile::tempdir().unwrap();
        let private = SecureDir::open(root.path())
            .unwrap()
            .child("private")
            .unwrap();
        let held_lock = private.child("supervisor-install-lock").unwrap();
        held_lock.try_lock_exclusive().unwrap();
        let (started, mut observed) = tokio::sync::mpsc::unbounded_channel();
        let mut installers = Vec::new();
        for architecture in ["x86_64", "aarch64", "x86_64", "aarch64"] {
            let private = private.clone();
            let started = started.clone();
            installers.push(tokio::spawn(async move {
                // Prove actual kernel lock contention before notifying the
                // test. A fresh descriptor is essential: cloned flock owners
                // would share the same open-file description and reenter it.
                let probe = private.child("supervisor-install-lock").unwrap();
                assert_eq!(
                    probe.try_lock_exclusive().unwrap_err().kind(),
                    std::io::ErrorKind::WouldBlock
                );
                drop(probe);
                started.send(()).unwrap();
                drop(started);
                SupervisorProgram::install_embedded_async(architecture, &private).await
            }));
        }
        drop(started);
        for _ in 0..installers.len() {
            observed.recv().await.unwrap();
        }
        // This single-thread executor must keep serving timers while all
        // installers wait on their blocking workers behind the held lock.
        let tick = Instant::now();
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(
            tick.elapsed() < Duration::from_secs(2),
            "installer lock waits stalled the async executor"
        );
        assert!(
            installers.iter().all(|installer| !installer.is_finished()),
            "ordinary concurrent installation must wait instead of failing immediately"
        );
        assert!(private
            .entries()
            .unwrap()
            .iter()
            .all(|entry| entry.name == "supervisor-install-lock"));
        drop(held_lock);
        let mut installed = Vec::new();
        for installer in installers {
            let program = tokio::time::timeout(INSTALL_LOCK_TIMEOUT, installer)
                .await
                .expect("installer must complete after the lock is released")
                .unwrap()
                .unwrap();
            program.verify().unwrap();
            installed.push(program);
        }
        assert_eq!(installed[0].file_identity, installed[2].file_identity);
        assert_eq!(installed[1].file_identity, installed[3].file_identity);
        assert_ne!(installed[0].sha256(), installed[1].sha256());
        assert_ne!(installed[0].file_identity, installed[1].file_identity);
    }

    #[test]
    fn installation_lock_timeout_is_bounded_and_preserves_the_current_owner() {
        let root = tempfile::tempdir().unwrap();
        let private = SecureDir::open(root.path())
            .unwrap()
            .child("private")
            .unwrap();
        let held_lock = private.child("supervisor-install-lock").unwrap();
        held_lock.try_lock_exclusive().unwrap();
        let contender = private.child("supervisor-install-lock").unwrap();
        let bound = Duration::from_millis(30);
        let started = Instant::now();
        let error = wait_for_install_lock(&contender, bound).unwrap_err();
        assert!(started.elapsed() >= bound);
        assert!(error
            .to_string()
            .contains("timed out waiting for the executable installation lock"));
        assert_eq!(
            contender.try_lock_exclusive().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
        drop(held_lock);
        wait_for_install_lock(&contender, bound).unwrap();
        // No timeout path replaces the locking directory or creates payloads.
        assert_eq!(private.entries().unwrap().len(), 1);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn cancelled_installation_await_can_be_retried_without_replacing_verified_bytes() {
        let root = tempfile::tempdir().unwrap();
        let private = SecureDir::open(root.path())
            .unwrap()
            .child("private")
            .unwrap();
        let held_lock = private.child("supervisor-install-lock").unwrap();
        held_lock.try_lock_exclusive().unwrap();
        let cancelled_root = private.clone();
        let caller = tokio::spawn(async move {
            SupervisorProgram::install_embedded_async("aarch64", &cancelled_root).await
        });
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(!caller.is_finished());
        caller.abort();
        assert!(caller.await.unwrap_err().is_cancelled());
        assert!(private
            .entries()
            .unwrap()
            .iter()
            .all(|entry| entry.name == "supervisor-install-lock"));
        drop(held_lock);

        // The detached blocking installer and this new caller serialize on
        // the same real lock. Either may install first; neither can replace
        // the content-addressed inode already verified by the other.
        let first = SupervisorProgram::install_embedded_async("aarch64", &private)
            .await
            .unwrap();
        let repeated = SupervisorProgram::install_embedded_async("aarch64", &private)
            .await
            .unwrap();
        assert_eq!(first.file_identity, repeated.file_identity);
        assert_eq!(first.sha256(), repeated.sha256());
        first.verify().unwrap();
        repeated.verify().unwrap();
    }
}
