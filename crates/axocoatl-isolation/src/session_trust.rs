//! Trust files for a Session with egress routes: the bundle of this
//! computer's roots plus the Session's certificate authority, delivered
//! into containers without a bind mount.
//!
//! The files go into a Podman volume, `axo-ca-{session}`, that Session
//! containers mount read-only at [`TRUST_MOUNT_DIR`]. The volume is filled
//! with `podman volume import` from a tar stream built here (ustar, files
//! owned by root, mode 0644), which also works over a remote Podman
//! connection because the stream goes through the client's stdin. When the
//! volume cannot be filled that way, [`copy_trust_files`] puts the same
//! stream into a running container with `podman cp`.
//!
//! The files hold only certificates. The authority's private key stays in
//! the daemon.

use std::process::Stdio;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tokio::io::AsyncWriteExt;
use tokio::process::Command;

use crate::egress_image::ROLE_LABEL;
use crate::session_sandbox::RUNTIME_AUTHORITY_LABEL;

/// Volume name prefix; removal and orphan reaping match it.
pub const TRUST_VOLUME_PREFIX: &str = "axo-ca-";
/// Where Session containers mount the trust volume.
pub const TRUST_MOUNT_DIR: &str = "/etc/axocoatl/ca";
/// Role label value of the trust volume.
pub const TRUST_ROLE: &str = "trust";
/// Most files and bytes one trust stream may hold.
pub const MAX_TRUST_FILES: usize = 16;
pub const MAX_TRUST_BYTES: usize = 8 * 1024 * 1024;
const COMMAND_TIMEOUT: Duration = Duration::from_secs(60);
const STDERR_MAX: usize = 4096;
const BLOCK: usize = 512;

/// One file of the trust volume.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrustFile {
    /// A plain file name: 1-100 letters, digits, `.`, `_` or `-`.
    pub name: String,
    pub contents: Vec<u8>,
}

/// The Session's trust volume.
pub fn trust_volume_name(session_id: &str) -> String {
    format!("{TRUST_VOLUME_PREFIX}{session_id}")
}

/// The `--mount` value that gives a container the trust volume read-only.
pub fn trust_mount_arg(session_id: &str) -> String {
    format!(
        "type=volume,source={},destination={TRUST_MOUNT_DIR},ro=true",
        trust_volume_name(session_id)
    )
}

/// Who owns the trust volume, for its labels.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TrustVolumeSpec {
    pub session_id: String,
    /// The daemon's runtime authority label value.
    pub runtime_authority: Option<String>,
    /// Extra `key=value` labels (tests use `io.axocoatl.test`).
    pub labels: Vec<String>,
}

fn valid_session_id(session_id: &str) -> bool {
    !session_id.is_empty()
        && session_id.len() <= 128
        && session_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
}

fn valid_file_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 100
        && name != "."
        && name != ".."
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
}

/// `value` as `width - 1` octal digits and a NUL.
fn octal(field: &mut [u8], value: u64) -> Result<(), String> {
    let digits = field.len() - 1;
    let text = format!("{value:0digits$o}");
    if text.len() > digits {
        return Err(format!("{value} does not fit a {digits}-digit tar field"));
    }
    field[..digits].copy_from_slice(text.as_bytes());
    field[digits] = 0;
    Ok(())
}

/// A ustar stream of `files`, each a regular file owned by root (uid and
/// gid 0) with mode 0644, ending with two zero blocks.
pub fn build_tar(files: &[TrustFile]) -> Result<Vec<u8>, String> {
    let mtime = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs());
    build_tar_at(files, mtime)
}

pub(crate) fn build_tar_at(files: &[TrustFile], mtime: u64) -> Result<Vec<u8>, String> {
    if files.is_empty() || files.len() > MAX_TRUST_FILES {
        return Err(format!("a trust stream holds 1-{MAX_TRUST_FILES} files"));
    }
    let total: usize = files.iter().map(|file| file.contents.len()).sum();
    if total > MAX_TRUST_BYTES {
        return Err(format!(
            "trust files are larger than {MAX_TRUST_BYTES} bytes"
        ));
    }
    let mut out = Vec::with_capacity(total + (files.len() * 2 + 2) * BLOCK);
    let mut names = std::collections::HashSet::new();
    for file in files {
        if !valid_file_name(&file.name) {
            return Err(format!("{:?} is not a plain file name", file.name));
        }
        if !names.insert(file.name.as_str()) {
            return Err(format!("{} is listed twice", file.name));
        }
        let mut header = [0u8; BLOCK];
        header[..file.name.len()].copy_from_slice(file.name.as_bytes());
        octal(&mut header[100..108], 0o644)?;
        octal(&mut header[108..116], 0)?;
        octal(&mut header[116..124], 0)?;
        octal(&mut header[124..136], file.contents.len() as u64)?;
        octal(&mut header[136..148], mtime)?;
        header[148..156].fill(b' ');
        header[156] = b'0';
        header[257..263].copy_from_slice(b"ustar\0");
        header[263..265].copy_from_slice(b"00");
        header[265..269].copy_from_slice(b"root");
        header[297..301].copy_from_slice(b"root");
        let checksum: u64 = header.iter().map(|byte| u64::from(*byte)).sum();
        // Six octal digits, a NUL and a space.
        let text = format!("{checksum:06o}");
        header[148..154].copy_from_slice(text.as_bytes());
        header[154] = 0;
        header[155] = b' ';
        out.extend_from_slice(&header);
        out.extend_from_slice(&file.contents);
        let padding = (BLOCK - file.contents.len() % BLOCK) % BLOCK;
        out.extend(std::iter::repeat_n(0u8, padding));
    }
    out.extend(std::iter::repeat_n(0u8, 2 * BLOCK));
    Ok(out)
}

/// Run `podman args`, optionally feeding `input` on stdin; bounded in time
/// and in the stderr it keeps.
async fn podman(args: &[String], input: Option<&[u8]>) -> Result<Vec<u8>, String> {
    let mut command = Command::new("podman");
    command
        .args(args)
        .kill_on_drop(true)
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let what = format!(
        "podman {} {}",
        args.first().map(String::as_str).unwrap_or_default(),
        args.get(1).map(String::as_str).unwrap_or_default()
    );
    let mut child = command
        .spawn()
        .map_err(|error| format!("{what}: {error}"))?;
    let stdin = child.stdin.take();
    let input = input.map(<[u8]>::to_vec);
    let run = async move {
        if let (Some(mut stdin), Some(input)) = (stdin, input) {
            stdin.write_all(&input).await?;
            stdin.shutdown().await?;
            drop(stdin);
        }
        child.wait_with_output().await
    };
    let output = tokio::time::timeout(COMMAND_TIMEOUT, run)
        .await
        .map_err(|_| format!("{what} timed out"))?
        .map_err(|error| format!("{what}: {error}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr[..output.stderr.len().min(STDERR_MAX)])
            .trim()
            .to_string();
        return Err(format!("{what} failed: {stderr}"));
    }
    Ok(output.stdout)
}

/// `podman volume create` arguments for the trust volume (pure).
pub fn trust_volume_create_args(spec: &TrustVolumeSpec) -> Vec<String> {
    let mut args = vec!["volume".into(), "create".into(), "--ignore".into()];
    if let Some(authority) = &spec.runtime_authority {
        args.push("--label".into());
        args.push(format!("{RUNTIME_AUTHORITY_LABEL}={authority}"));
    }
    args.push("--label".into());
    args.push(format!("{ROLE_LABEL}={TRUST_ROLE}"));
    for label in &spec.labels {
        args.push("--label".into());
        args.push(label.clone());
    }
    args.push(trust_volume_name(&spec.session_id));
    args
}

/// Create the Session's trust volume if needed and import `files` into it.
/// Importing replaces files of the same name; nothing else in the volume
/// changes.
pub async fn populate_trust_volume(
    spec: &TrustVolumeSpec,
    files: &[TrustFile],
) -> Result<(), String> {
    if !valid_session_id(&spec.session_id) {
        return Err(format!(
            "Session id {:?} cannot name a trust volume",
            spec.session_id
        ));
    }
    let tar = build_tar(files)?;
    podman(&trust_volume_create_args(spec), None).await?;
    podman(
        &[
            "volume".into(),
            "import".into(),
            trust_volume_name(&spec.session_id),
            "-".into(),
        ],
        Some(&tar),
    )
    .await?;
    Ok(())
}

/// The fallback: copy `files` into [`TRUST_MOUNT_DIR`] of a running
/// container with `podman cp`, from the same tar stream. The directory must
/// exist and be writable from outside the container (not a read-only
/// mount).
pub async fn copy_trust_files(container: &str, files: &[TrustFile]) -> Result<(), String> {
    if container.is_empty()
        || container.starts_with('-')
        || !container
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        return Err(format!("{container:?} is not a container name"));
    }
    let tar = build_tar(files)?;
    podman(
        &[
            "cp".into(),
            "-".into(),
            format!("{container}:{TRUST_MOUNT_DIR}"),
        ],
        Some(&tar),
    )
    .await?;
    Ok(())
}

/// Remove the Session's trust volume; a missing volume is not an error.
pub async fn remove_trust_volume(session_id: &str) -> Result<(), String> {
    if !valid_session_id(session_id) {
        return Err(format!(
            "Session id {session_id:?} cannot name a trust volume"
        ));
    }
    podman(
        &[
            "volume".into(),
            "rm".into(),
            "--force".into(),
            trust_volume_name(session_id),
        ],
        None,
    )
    .await
    .map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn files() -> Vec<TrustFile> {
        vec![
            TrustFile {
                name: "bundle.pem".into(),
                contents: b"-----BEGIN CERTIFICATE-----\nAAAA\n-----END CERTIFICATE-----\n"
                    .repeat(20),
            },
            TrustFile {
                name: "session-ca.pem".into(),
                contents: b"ca".to_vec(),
            },
        ]
    }

    #[test]
    fn names_and_mount() {
        assert_eq!(trust_volume_name("ses-1"), "axo-ca-ses-1");
        assert_eq!(
            trust_mount_arg("ses-1"),
            "type=volume,source=axo-ca-ses-1,destination=/etc/axocoatl/ca,ro=true"
        );
        let args = trust_volume_create_args(&TrustVolumeSpec {
            session_id: "ses-1".into(),
            runtime_authority: Some("auth".into()),
            labels: vec!["io.axocoatl.test=t".into()],
        })
        .join(" ");
        assert_eq!(
            args,
            "volume create --ignore --label io.axocoatl.runtime-authority=auth --label io.axocoatl.role=trust --label io.axocoatl.test=t axo-ca-ses-1"
        );
    }

    #[test]
    fn the_stream_is_ustar_with_root_owned_0644_files() {
        let tar = build_tar_at(&files(), 1_790_000_000).unwrap();
        assert_eq!(tar.len() % BLOCK, 0);
        // Header, data padded, header, data padded, two zero blocks.
        let first_data = files()[0].contents.len().div_ceil(BLOCK) * BLOCK;
        assert_eq!(tar.len(), BLOCK + first_data + BLOCK + BLOCK + 2 * BLOCK);
        let header = &tar[..BLOCK];
        assert_eq!(&header[..10], b"bundle.pem");
        assert_eq!(&header[100..108], b"0000644\0");
        assert_eq!(&header[108..116], b"0000000\x00");
        assert_eq!(&header[116..124], b"0000000\x00");
        assert_eq!(&header[257..265], b"ustar\x0000");
        assert_eq!(header[156], b'0');
        let stored: u64 =
            u64::from_str_radix(std::str::from_utf8(&header[148..154]).unwrap(), 8).unwrap();
        let mut blank = header.to_vec();
        blank[148..156].fill(b' ');
        assert_eq!(
            stored,
            blank.iter().map(|byte| u64::from(*byte)).sum::<u64>()
        );
        assert!(tar[tar.len() - 2 * BLOCK..].iter().all(|byte| *byte == 0));
    }

    #[test]
    fn bad_names_and_sizes_are_refused() {
        for name in ["", ".", "..", "a/b", "../x", "x y", &"n".repeat(101)] {
            let file = TrustFile {
                name: name.to_string(),
                contents: Vec::new(),
            };
            assert!(build_tar(&[file]).is_err(), "{name:?}");
        }
        assert!(build_tar(&[]).is_err());
        let twice = vec![files()[1].clone(), files()[1].clone()];
        assert!(build_tar(&twice).is_err());
        let huge = TrustFile {
            name: "big".into(),
            contents: vec![0; MAX_TRUST_BYTES + 1],
        };
        assert!(build_tar(&[huge]).is_err());
    }

    /// The system `tar` lists and extracts the stream.
    #[cfg(unix)]
    #[test]
    fn system_tar_reads_the_stream() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("trust.tar");
        std::fs::write(&path, build_tar(&files()).unwrap()).unwrap();
        let listing = std::process::Command::new("tar")
            .arg("-tvf")
            .arg(&path)
            .output()
            .unwrap();
        assert!(listing.status.success(), "{listing:?}");
        let listing = String::from_utf8(listing.stdout).unwrap();
        let lines: Vec<&str> = listing.lines().collect();
        assert_eq!(lines.len(), 2, "{listing}");
        assert!(lines[0].starts_with("-rw-r--r--"), "{listing}");
        assert!(
            lines[0].contains("root") && lines[0].ends_with("bundle.pem"),
            "{listing}"
        );
        assert!(lines[1].ends_with("session-ca.pem"), "{listing}");
        let out = dir.path().join("out");
        std::fs::create_dir(&out).unwrap();
        let extract = std::process::Command::new("tar")
            .arg("-xf")
            .arg(&path)
            .arg("-C")
            .arg(&out)
            .output()
            .unwrap();
        assert!(extract.status.success(), "{extract:?}");
        for file in files() {
            assert_eq!(std::fs::read(out.join(&file.name)).unwrap(), file.contents);
        }
    }
}
