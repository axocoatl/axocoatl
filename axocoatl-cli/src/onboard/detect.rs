//! Step 0: what this computer has. Nothing here changes anything: every
//! command it runs only reads (`--version`, `podman info`, `podman machine
//! list`, `GET /api/version`).

use std::path::{Path, PathBuf};

use axocoatl_isolation::podman_report::{self, PodmanReport};

use super::host::{free_bytes, port_free, Cmd, Host};
use crate::ollama_setup;

/// `/etc/os-release`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct OsRelease {
    pub(crate) id: String,
    pub(crate) id_like: Vec<String>,
    pub(crate) version_id: String,
    pub(crate) pretty_name: String,
}

impl OsRelease {
    pub(crate) fn parse(text: &str) -> Self {
        let mut release = Self::default();
        for line in text.lines() {
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            let value = value
                .trim()
                .trim_matches('"')
                .trim_matches('\'')
                .to_string();
            match key.trim() {
                "ID" => release.id = value.to_ascii_lowercase(),
                "ID_LIKE" => {
                    release.id_like = value
                        .split_whitespace()
                        .map(str::to_ascii_lowercase)
                        .collect()
                }
                "VERSION_ID" => release.version_id = value,
                "PRETTY_NAME" => release.pretty_name = value,
                _ => {}
            }
        }
        release
    }

    fn is(&self, family: &str) -> bool {
        self.id == family || self.id_like.iter().any(|like| like == family)
    }

    /// The package manager that installs Podman here.
    pub(crate) fn package_manager(&self) -> Option<PackageManager> {
        if self.is("debian") || self.is("ubuntu") {
            Some(PackageManager::Apt)
        } else if self.is("fedora") || self.is("rhel") || self.is("centos") {
            Some(PackageManager::Dnf)
        } else if self.is("arch") {
            Some(PackageManager::Pacman)
        } else if self.is("suse") || self.id.starts_with("opensuse") || self.is("sles") {
            Some(PackageManager::Zypper)
        } else {
            None
        }
    }

    /// Ubuntu 22.04 (and older), whose Podman (3.4.4) Axocoatl cannot use.
    pub(crate) fn podman_too_old(&self) -> bool {
        self.id == "ubuntu"
            && self
                .version_id
                .split('.')
                .next()
                .and_then(|major| major.parse::<u32>().ok())
                .is_some_and(|major| major <= 22)
    }

    pub(crate) fn name(&self) -> String {
        if !self.pretty_name.is_empty() {
            self.pretty_name.clone()
        } else if !self.id.is_empty() {
            format!("{} {}", self.id, self.version_id)
                .trim()
                .to_string()
        } else {
            "Linux".to_string()
        }
    }
}

/// How Podman (and other system packages) are installed on this Linux.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PackageManager {
    Apt,
    Dnf,
    Pacman,
    Zypper,
}

impl PackageManager {
    pub(crate) fn install(self, packages: &[&str]) -> Cmd {
        let mut args: Vec<&str> = match self {
            PackageManager::Apt => vec!["apt-get", "install", "-y"],
            PackageManager::Dnf => vec!["dnf", "install", "-y"],
            PackageManager::Pacman => vec!["pacman", "-S", "--noconfirm"],
            PackageManager::Zypper => vec!["zypper", "install", "-y"],
        };
        args.extend_from_slice(packages);
        Cmd::sudo(args[0], &args[1..])
    }

    /// The package with `newuidmap` and `newgidmap`, which rootless Podman
    /// maps subordinate ids with: `uidmap` on Debian and Ubuntu (Podman
    /// only recommends it), `shadow-utils` on Fedora, `shadow` on Arch (as
    /// `rpm -qf` and `pacman -Qo` say in their images). openSUSE moved them
    /// from `shadow` to `shadow-pw-mgmt` in Tumbleweed, so no one name
    /// fits every release: `None`.
    pub(crate) fn id_map_package(self) -> Option<&'static str> {
        match self {
            PackageManager::Apt => Some("uidmap"),
            PackageManager::Dnf => Some("shadow-utils"),
            PackageManager::Pacman => Some("shadow"),
            PackageManager::Zypper => None,
        }
    }

    /// The command that installs [`PackageManager::id_map_package`], or
    /// reinstalls it when it is `installed`: `apt-get install` and `dnf
    /// install` do nothing for an installed package ("already the newest
    /// version", "Nothing to do"), even when a helper's file was deleted or
    /// lost its setuid bit or file capabilities. `apt-get install
    /// --reinstall` only when dpkg says uidmap is installed; `dnf
    /// reinstall` unless rpm says shadow-utils is not installed (it always
    /// is, and `dnf reinstall` refuses one that is not); `pacman -S`
    /// reinstalls an installed package anyway.
    pub(crate) fn id_map_install(self, installed: Option<bool>) -> Option<Cmd> {
        let package = self.id_map_package()?;
        Some(match (self, installed) {
            (PackageManager::Apt, Some(true)) => {
                Cmd::sudo("apt-get", &["install", "--reinstall", "-y", package])
            }
            (PackageManager::Dnf, Some(false)) => self.install(&[package]),
            (PackageManager::Dnf, _) => Cmd::sudo("dnf", &["reinstall", "-y", package]),
            _ => self.install(&[package]),
        })
    }

    /// The read-only command that says whether `package` is installed:
    /// `dpkg-query -W -f '${db:Status-Status}' uidmap` prints `installed`
    /// (it exits 1 for a package dpkg does not know), `rpm -q` and `pacman
    /// -Q` exit 0 for an installed package and 1 for one that is not.
    pub(crate) fn package_query(self, package: &str) -> (&'static str, Vec<String>) {
        let (program, args): (&'static str, Vec<&str>) = match self {
            PackageManager::Apt => (
                "dpkg-query",
                vec!["-W", "-f", "${db:Status-Status}", package],
            ),
            PackageManager::Dnf | PackageManager::Zypper => ("rpm", vec!["-q", package]),
            PackageManager::Pacman => ("pacman", vec!["-Q", package]),
        };
        (program, args.into_iter().map(str::to_string).collect())
    }

    /// The read-only command that names the package owning the file at
    /// `path`: `dpkg-query -S` (`uidmap: /usr/bin/newuidmap`), `rpm -qf
    /// --qf '%{NAME}\n'`, `pacman -Qqo`. Each exits 1 for a file no
    /// package owns.
    pub(crate) fn owner_query(self, path: &str) -> (&'static str, Vec<String>) {
        let (program, args): (&'static str, Vec<&str>) = match self {
            PackageManager::Apt => ("dpkg-query", vec!["-S", path]),
            PackageManager::Dnf | PackageManager::Zypper => {
                ("rpm", vec!["-qf", "--qf", "%{NAME}\n", path])
            }
            PackageManager::Pacman => ("pacman", vec!["-Qqo", path]),
        };
        (program, args.into_iter().map(str::to_string).collect())
    }

    /// Whether `package` owns the file [`PackageManager::owner_query`]
    /// asked about, from what the query printed and its exit code; `None`
    /// when that does not say.
    pub(crate) fn owns(self, package: &str, code: Option<i32>, stdout: &str) -> Option<bool> {
        match code {
            Some(0) => Some(stdout.lines().any(|line| match self {
                // `pkg1, pkg2: /path`; a diversion line has no package.
                PackageManager::Apt => line.split_once(": ").is_some_and(|(owners, _)| {
                    owners.split(", ").any(|owner| owner.trim() == package)
                }),
                _ => line.trim() == package,
            })),
            Some(1) => Some(false),
            _ => None,
        }
    }

    /// Whether the package [`PackageManager::package_query`] asked about is
    /// installed, from what the query printed and its exit code; `None`
    /// when that does not say.
    pub(crate) fn package_installed(self, code: Option<i32>, stdout: &str) -> Option<bool> {
        match (self, code) {
            (PackageManager::Apt, Some(0)) => Some(stdout.trim() == "installed"),
            (_, Some(0)) => Some(true),
            (_, Some(1)) => Some(false),
            _ => None,
        }
    }

    /// The read-only command that lists the files of the installed
    /// `package`: `dpkg-query -L`, `rpm -q --qf` with the mode and the file
    /// capabilities rpm gives each one (a reinstall gives them again), and
    /// `pacman -Ql` (`shadow /usr/bin/newuidmap`; pacman's records hold no
    /// file capabilities: Arch's shadow 4.20 records newuidmap as mode 755
    /// and gives it `cap_setuid=ep` from the package's extended attributes).
    /// `None` for zypper, whose records are not read.
    pub(crate) fn files_query(self, package: &str) -> Option<(&'static str, Vec<String>)> {
        let (program, args): (&'static str, Vec<&str>) = match self {
            PackageManager::Apt => ("dpkg-query", vec!["-L", package]),
            PackageManager::Dnf => (
                "rpm",
                vec![
                    "-q",
                    "--qf",
                    "[%{FILENAMES}\\t%{FILEMODES:octal}\\t%{FILECAPS}\\n]",
                    package,
                ],
            ),
            PackageManager::Pacman => ("pacman", vec!["-Ql", package]),
            PackageManager::Zypper => return None,
        };
        Some((program, args.into_iter().map(str::to_string).collect()))
    }

    /// The files [`PackageManager::files_query`] printed: a path a line
    /// for dpkg, `path<TAB>mode<TAB>capabilities` for rpm (`100755`,
    /// `cap_setuid=ep`, or nothing), `package path` for pacman.
    pub(crate) fn package_files(self, stdout: &str) -> Vec<PackageFile> {
        stdout
            .lines()
            .map(|line| match self {
                PackageManager::Pacman => line.split_once(' ').map_or("", |(_, path)| path),
                _ => line,
            })
            .filter(|line| line.starts_with('/'))
            .map(|line| {
                let mut fields = line.split('\t');
                let path = fields.next().unwrap_or_default().to_string();
                match self {
                    PackageManager::Dnf => PackageFile {
                        path,
                        mode: fields
                            .next()
                            .and_then(|mode| u32::from_str_radix(mode.trim(), 8).ok()),
                        caps: fields.next().map(|caps| caps.trim().to_string()),
                    },
                    _ => PackageFile {
                        path,
                        mode: None,
                        caps: None,
                    },
                }
            })
            .collect()
    }
}

/// A file of an installed package, as its package manager records it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PackageFile {
    pub(crate) path: String,
    /// The mode rpm installs it with (`0o100755`); dpkg does not record
    /// one.
    pub(crate) mode: Option<u32>,
    /// The file capabilities rpm gives it (`cap_setuid=ep`, empty for
    /// none); dpkg gives none.
    pub(crate) caps: Option<String>,
}

/// The file of `files` that is the program `name`: in a `bin` or `sbin`
/// directory (`/usr/bin/newuidmap`, not its manual page).
pub(crate) fn program_file<'a>(files: &'a [PackageFile], name: &str) -> Option<&'a PackageFile> {
    files.iter().find(|file| {
        let path = Path::new(&file.path);
        path.file_name().is_some_and(|file_name| file_name == name)
            && path
                .parent()
                .and_then(Path::file_name)
                .is_some_and(|directory| directory == "bin" || directory == "sbin")
    })
}

/// The diversion of `path` that `dpkg-query -S PATH` printed (`local
/// diversion from: P` and `local diversion to: D`, or `diversion by PKG
/// from: …`): where dpkg writes the package's file instead, and the package
/// that added it (`None`: a local one, added by hand).
pub(crate) fn dpkg_diversion(stdout: &str, path: &str) -> Option<(String, Option<String>)> {
    let mut from = None;
    let mut to = None;
    let mut by = None;
    for line in stdout.lines() {
        let (who, rest) = if let Some(rest) = line.strip_prefix("local diversion ") {
            (None, rest)
        } else if let Some(rest) = line.strip_prefix("diversion by ") {
            match rest.split_once(' ') {
                Some((package, rest)) => (Some(package.to_string()), rest),
                None => continue,
            }
        } else {
            continue;
        };
        if let Some(diverted) = rest.strip_prefix("from: ") {
            from = Some(diverted.trim().to_string());
            by = who;
        } else if let Some(diverted) = rest.strip_prefix("to: ") {
            to = Some(diverted.trim().to_string());
        }
    }
    match (from, to) {
        (Some(from), Some(to)) if from == path && to != path => Some((to, by)),
        _ => None,
    }
}

/// The mode a dpkg stat override gives `path`, from what
/// `dpkg-statoverride --list PATH` printed (`root root 755 /usr/bin/x`;
/// nothing, exit 1, for no override): dpkg applies it whenever it writes
/// the file, a reinstall too.
pub(crate) fn dpkg_override_mode(stdout: &str, path: &str) -> Option<u32> {
    stdout.lines().find_map(|line| {
        let fields: Vec<&str> = line.split_whitespace().collect();
        match fields.as_slice() {
            [_, _, mode, overridden] if *overridden == path => u32::from_str_radix(mode, 8).ok(),
            _ => None,
        }
    })
}

/// The `path-exclude` that keeps dpkg from writing `path` (checked with
/// dpkg 1.22 in ubuntu:24.04), as its line and file, under `root`: dpkg
/// reads the files of `etc/dpkg/dpkg.cfg.d` whose names are only letters,
/// digits, `-` and `_`, in order, then `etc/dpkg/dpkg.cfg`; each
/// `path-exclude` or `path-include` line (`=` or a space, quotes dropped)
/// is a glob, and the last one that matches decides.
pub(crate) fn dpkg_excluded(root: &Path, path: &str) -> Option<(String, String)> {
    let directory = root.join("etc/dpkg/dpkg.cfg.d");
    let mut files: Vec<PathBuf> = std::fs::read_dir(&directory)
        .map(|entries| {
            entries
                .flatten()
                .filter(|entry| {
                    entry.file_name().to_str().is_some_and(|name| {
                        !name.is_empty()
                            && name
                                .chars()
                                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
                    })
                })
                .map(|entry| entry.path())
                .collect()
        })
        .unwrap_or_default();
    files.sort();
    files.push(root.join("etc/dpkg/dpkg.cfg"));
    let mut decided: Option<(String, String)> = None;
    for file in files {
        let Ok(text) = std::fs::read_to_string(&file) else {
            continue;
        };
        for line in text.lines() {
            let line = line.trim();
            let Some(end) = line.find(|c: char| c == '=' || c.is_whitespace()) else {
                continue;
            };
            let (option, value) = line.split_at(end);
            let exclude = match option {
                "path-exclude" => true,
                "path-include" => false,
                _ => continue,
            };
            let pattern = value
                .trim_start_matches(|c: char| c == '=' || c.is_whitespace())
                .trim()
                .trim_matches('"')
                .trim_matches('\'');
            if glob_matches(pattern, path) {
                decided = exclude.then(|| {
                    let shown = file
                        .strip_prefix(root)
                        .map(|relative| Path::new("/").join(relative))
                        .unwrap_or(file.clone());
                    (line.to_string(), shown.display().to_string())
                });
            }
        }
    }
    decided
}

/// The pattern of pacman's `option` (`NoExtract` or `NoUpgrade`, in the
/// `[options]` of `/etc/pacman.conf` under `root` and the files it
/// `Include`s there) that keeps pacman from writing `path` (absolute), with
/// the file that sets it, as pacman decides: each pattern is matched with
/// fnmatch(3) against the path without its leading `/`, the last match wins,
/// and one starting with `!` matches to write it after all.
pub(crate) fn pacman_kept(root: &Path, option: &str, path: &str) -> Option<(String, String)> {
    let relative = path.trim_start_matches('/');
    let mut patterns: Vec<(String, String)> = Vec::new();
    let mut files = vec![root.join("etc/pacman.conf")];
    let mut read = 0;
    while let Some(file) = files.get(read).cloned() {
        read += 1;
        // Bounded: an `Include` cannot make this loop for ever.
        if read > 64 {
            break;
        }
        let Ok(text) = std::fs::read_to_string(&file) else {
            continue;
        };
        let shown = file
            .strip_prefix(root)
            .map(|relative| Path::new("/").join(relative))
            .unwrap_or(file.clone())
            .display()
            .to_string();
        // An included file starts in the section that includes it.
        let mut options = read > 1;
        for line in text.lines() {
            let line = line.split('#').next().unwrap_or_default().trim();
            if line.starts_with('[') && line.ends_with(']') {
                options = &line[1..line.len() - 1] == "options";
                continue;
            }
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            if !options {
                continue;
            }
            match key.trim() {
                key if key == option => patterns.extend(
                    value
                        .split_whitespace()
                        .map(|pattern| (pattern.to_string(), shown.clone())),
                ),
                "Include" => {
                    let included = value.trim();
                    let (directory, name) = match included.rsplit_once('/') {
                        Some((directory, name)) => (directory, name),
                        None => continue,
                    };
                    let directory = root.join(directory.trim_start_matches('/'));
                    let mut found: Vec<PathBuf> = std::fs::read_dir(&directory)
                        .map(|entries| {
                            entries
                                .flatten()
                                .filter(|entry| {
                                    entry
                                        .file_name()
                                        .to_str()
                                        .is_some_and(|file| glob_matches(name, file))
                                })
                                .map(|entry| entry.path())
                                .collect()
                        })
                        .unwrap_or_default();
                    found.sort();
                    files.extend(found);
                }
                _ => {}
            }
        }
    }
    patterns.iter().rev().find_map(|(pattern, file)| {
        let (negated, glob) = match pattern.strip_prefix('!') {
            Some(glob) => (true, glob),
            None => (false, pattern.strip_prefix('\\').unwrap_or(pattern)),
        };
        glob_matches(glob, relative).then(|| (!negated).then(|| (pattern.clone(), file.clone())))
    })?
}

/// `text` matches the glob `pattern` as fnmatch(3) without flags does (as
/// dpkg's filters use it): `*` matches any run of characters, `/` too, `?`
/// any one, `[…]` one of a set (`!` or `^` first negates it, `a-z` is a
/// range), and `\` takes the next character as it is.
pub(crate) fn glob_matches(pattern: &str, text: &str) -> bool {
    fn matches(pattern: &[char], text: &[char]) -> bool {
        match pattern.split_first() {
            None => text.is_empty(),
            Some(('*', rest)) => (0..=text.len()).any(|skip| matches(rest, &text[skip..])),
            Some(('?', rest)) => !text.is_empty() && matches(rest, &text[1..]),
            Some(('[', rest)) => {
                let Some((&first, text_rest)) = text.split_first() else {
                    return false;
                };
                let negated = matches!(rest.first(), Some('!' | '^'));
                let mut index = usize::from(negated);
                let mut found = false;
                let mut closed = false;
                while index < rest.len() {
                    let c = rest[index];
                    // A `]` right after `[` (or `[!`) is one of the set.
                    if c == ']' && index > usize::from(negated) {
                        closed = true;
                        break;
                    }
                    if rest.get(index + 1) == Some(&'-')
                        && rest.get(index + 2).is_some_and(|end| *end != ']')
                    {
                        if rest[index] <= first && first <= rest[index + 2] {
                            found = true;
                        }
                        index += 3;
                    } else {
                        if c == first {
                            found = true;
                        }
                        index += 1;
                    }
                }
                if !closed {
                    // No closing `]`: a plain `[`.
                    return first == '[' && matches(rest, text_rest);
                }
                found != negated && matches(&rest[index + 1..], text_rest)
            }
            Some(('\\', rest)) if !rest.is_empty() => {
                text.first() == Some(&rest[0]) && matches(&rest[1..], &text[1..])
            }
            Some((c, rest)) => text.first() == Some(c) && matches(rest, &text[1..]),
        }
    }
    let pattern: Vec<char> = pattern.chars().collect();
    let text: Vec<char> = text.chars().collect();
    matches(&pattern, &text)
}

/// Where dnf is set to install files without their file capabilities
/// (`nocaps` in `tsflags`), under `root`: the `[main]` section of
/// `etc/dnf/dnf.conf`, or of a dnf5 drop-in (`usr/share/dnf5/libdnf.conf.d`,
/// `etc/dnf/libdnf5.conf.d`). dnf5 adds up `tsflags` across these files
/// (checked with dnf5 5.2.18 in fedora:42: `nocaps` in a drop-in and
/// `nodocs` in dnf.conf give `nocaps, nodocs`), and in one file the last
/// line counts. Read from the files, since `dnf --dump-main-config` writes
/// a log in the home directory of a user who runs it.
pub(crate) fn dnf_nocaps(root: &Path) -> Option<String> {
    let mut files = vec![root.join("etc/dnf/dnf.conf")];
    for directory in ["usr/share/dnf5/libdnf.conf.d", "etc/dnf/libdnf5.conf.d"] {
        let mut found: Vec<PathBuf> = std::fs::read_dir(root.join(directory))
            .map(|entries| {
                entries
                    .flatten()
                    .map(|entry| entry.path())
                    .filter(|path| {
                        path.extension()
                            .is_some_and(|extension| extension == "conf")
                    })
                    .collect()
            })
            .unwrap_or_default();
        found.sort();
        files.extend(found);
    }
    files.into_iter().find_map(|file| {
        let text = std::fs::read_to_string(&file).ok()?;
        let mut section = String::new();
        let mut tsflags = None;
        for line in text.lines() {
            let line = line.trim();
            if line.starts_with('[') && line.ends_with(']') {
                section = line[1..line.len() - 1].trim().to_string();
            } else if let Some((key, value)) = line.split_once('=') {
                if section == "main" && key.trim() == "tsflags" {
                    tsflags = Some(value.trim().to_string());
                }
            }
        }
        tsflags
            .filter(|value| {
                value
                    .split(|c: char| c == ',' || c.is_whitespace())
                    .any(|flag| flag == "nocaps")
            })
            .map(|_| {
                file.strip_prefix(root)
                    .map(|relative| Path::new("/").join(relative))
                    .unwrap_or(file.clone())
                    .display()
                    .to_string()
            })
    })
}

/// Windows Subsystem for Linux.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Wsl {
    One,
    Two,
}

/// WSL from `/proc/sys/kernel/osrelease` (`…-microsoft-standard-WSL2`;
/// WSL 1 reports `…-Microsoft`) or `WSL_DISTRO_NAME`.
pub(crate) fn wsl_from(kernel_release: &str, distro: Option<&str>) -> Option<Wsl> {
    let lower = kernel_release.to_ascii_lowercase();
    if lower.contains("microsoft-standard") || lower.contains("wsl2") {
        Some(Wsl::Two)
    } else if kernel_release.contains("Microsoft") {
        Some(Wsl::One)
    } else if lower.contains("microsoft") || distro.is_some_and(|d| !d.is_empty()) {
        Some(Wsl::Two)
    } else {
        None
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Os {
    MacOs {
        version: Option<String>,
        apple_silicon: bool,
        chip: Option<String>,
    },
    Linux {
        release: OsRelease,
        wsl: Option<Wsl>,
        /// systemd runs (`/run/systemd/system`): the always-on service and
        /// Ollama's own service are available.
        systemd: bool,
    },
}

impl Os {
    pub(crate) fn is_macos(&self) -> bool {
        matches!(self, Os::MacOs { .. })
    }

    pub(crate) fn release(&self) -> Option<&OsRelease> {
        match self {
            Os::Linux { release, .. } => Some(release),
            Os::MacOs { .. } => None,
        }
    }

    pub(crate) fn wsl(&self) -> Option<Wsl> {
        match self {
            Os::Linux { wsl, .. } => *wsl,
            Os::MacOs { .. } => None,
        }
    }
}

/// How the Ollama server at the chosen address is run, which decides how
/// onboarding restarts it and where its settings go.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OllamaService {
    /// `brew services` (a LaunchAgent running as this user, reading
    /// `~/.ollama/server.json` and Homebrew's `services/ollama.env`).
    Brew,
    /// The Ollama macOS app.
    App,
    /// The systemd unit Ollama's Linux installer creates (`User=ollama`).
    Systemd,
    /// Something else, such as `ollama serve` in a terminal.
    Manual,
}

/// The port in an `OLLAMA_HOST` value (`127.0.0.1:11440`,
/// `http://localhost:11440`, `0.0.0.0`): 11434 when it names none.
pub(crate) fn ollama_host_port(value: &str) -> u16 {
    let value = value.trim().trim_matches('"');
    let without_scheme = value.split_once("://").map_or(value, |(_, rest)| rest);
    let authority = without_scheme.split('/').next().unwrap_or_default();
    authority
        .rsplit_once(':')
        .and_then(|(_, port)| port.parse().ok())
        .unwrap_or(11434)
}

/// The port of a base URL (`http://localhost:11434`).
pub(crate) fn url_port(base_url: &str) -> u16 {
    let without_scheme = base_url
        .split_once("://")
        .map_or(base_url, |(_, rest)| rest);
    let authority = without_scheme.split('/').next().unwrap_or_default();
    authority
        .rsplit_once(':')
        .filter(|(host, _)| !host.ends_with(']') || authority.starts_with('['))
        .and_then(|(_, port)| port.parse().ok())
        .unwrap_or(if base_url.starts_with("https://") {
            443
        } else {
            80
        })
}

/// Homebrew's per-user service environment directory:
/// `$XDG_CONFIG_HOME/homebrew/services`, else `~/.homebrew/services`
/// (Homebrew 7 reads `<formula>.env` there on `brew services start` and
/// `restart`).
pub(crate) fn homebrew_services_dir(home: &Path, xdg_config_home: Option<&Path>) -> PathBuf {
    match xdg_config_home.filter(|path| path.is_absolute()) {
        Some(config) => config.join("homebrew").join("services"),
        None => home.join(".homebrew").join("services"),
    }
}

/// The value `NAME=value` sets in an env file's text, or in a plist's
/// `EnvironmentVariables` (`<key>NAME</key><string>value</string>`).
pub(crate) fn variable_in(text: &str, name: &str) -> Option<String> {
    for line in text.lines() {
        let line = line.trim();
        let line = line.strip_prefix("export ").unwrap_or(line);
        if let Some(value) = line.strip_prefix(&format!("{name}=")) {
            return Some(value.trim().trim_matches('"').to_string());
        }
        // systemd: Environment="NAME=value" or Environment=NAME=value
        if let Some(rest) = line.strip_prefix("Environment=") {
            let rest = rest.trim().trim_matches('"');
            if let Some(value) = rest.strip_prefix(&format!("{name}=")) {
                return Some(value.trim_matches('"').to_string());
            }
        }
    }
    let key = format!("<key>{name}</key>");
    let after = text.split_once(&key)?.1;
    let start = after.find("<string>")? + "<string>".len();
    let end = after[start..].find("</string>")?;
    Some(after[start..start + end].trim().to_string())
}

/// The LaunchAgent names Homebrew gives the Ollama service: `sh.brew.*` since
/// Homebrew 5, `homebrew.mxcl.*` before.
pub(crate) const BREW_OLLAMA_PLISTS: [&str; 2] =
    ["sh.brew.ollama.plist", "homebrew.mxcl.ollama.plist"];

/// The brew service's LaunchAgent: the one that exists, else the current name.
pub(crate) fn brew_plist(host: &Host) -> PathBuf {
    let agents = host.home.join("Library").join("LaunchAgents");
    BREW_OLLAMA_PLISTS
        .iter()
        .map(|name| agents.join(name))
        .find(|path| path.is_file())
        .unwrap_or_else(|| agents.join(BREW_OLLAMA_PLISTS[0]))
}

/// The Ollama app, if installed.
pub(crate) fn ollama_app(host: &Host) -> Option<PathBuf> {
    [
        host.at("/Applications/Ollama.app"),
        host.home.join("Applications").join("Ollama.app"),
    ]
    .into_iter()
    .find(|path| path.is_dir())
}

/// The systemd unit and drop-in directory Ollama's installer uses.
pub(crate) const OLLAMA_UNIT: &str = "/etc/systemd/system/ollama.service";
pub(crate) const OLLAMA_DROP_IN_DIR: &str = "/etc/systemd/system/ollama.service.d";

/// Which service serves Ollama on `port`.
pub(crate) fn ollama_service(
    host: &Host,
    xdg_config_home: Option<&Path>,
    port: u16,
) -> OllamaService {
    if host.macos {
        let plist = brew_plist(host);
        if plist.is_file() && host.find("brew").is_some() {
            let env_file = homebrew_services_dir(&host.home, xdg_config_home).join("ollama.env");
            let configured = std::fs::read_to_string(&env_file)
                .ok()
                .and_then(|text| variable_in(&text, "OLLAMA_HOST"))
                .or_else(|| {
                    std::fs::read_to_string(&plist)
                        .ok()
                        .and_then(|text| variable_in(&text, "OLLAMA_HOST"))
                });
            if configured.as_deref().map_or(11434, ollama_host_port) == port {
                return OllamaService::Brew;
            }
        }
        if ollama_app(host).is_some() && port == 11434 {
            return OllamaService::App;
        }
        OllamaService::Manual
    } else {
        let unit = host.at(OLLAMA_UNIT);
        if unit.is_file() {
            let mut configured = std::fs::read_to_string(&unit)
                .ok()
                .and_then(|text| variable_in(&text, "OLLAMA_HOST"));
            if let Ok(entries) = std::fs::read_dir(host.at(OLLAMA_DROP_IN_DIR)) {
                let mut drop_ins: Vec<PathBuf> = entries
                    .flatten()
                    .map(|entry| entry.path())
                    .filter(|path| path.extension().is_some_and(|ext| ext == "conf"))
                    .collect();
                drop_ins.sort();
                for drop_in in drop_ins {
                    if let Some(value) = std::fs::read_to_string(&drop_in)
                        .ok()
                        .and_then(|text| variable_in(&text, "OLLAMA_HOST"))
                    {
                        configured = Some(value);
                    }
                }
            }
            if configured.as_deref().map_or(11434, ollama_host_port) == port {
                return OllamaService::Systemd;
            }
        }
        OllamaService::Manual
    }
}

/// The `ollama` CLI and the versions `ollama --version` reports.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct OllamaCli {
    pub(crate) path: PathBuf,
    /// The CLI's own version.
    pub(crate) client: Option<String>,
    /// The server it reached (`OLLAMA_HOST`), when it reached one.
    pub(crate) server: Option<String>,
}

/// `ollama --version`: `ollama version is 0.20.6` (the server), then
/// `Warning: client version is 0.40.6` when the CLI differs; without a
/// server only the client line.
pub(crate) fn parse_ollama_version(text: &str) -> (Option<String>, Option<String>) {
    let mut server = None;
    let mut client = None;
    for line in text.lines() {
        let line = line.trim();
        if let Some(version) = line.strip_prefix("Warning: client version is ") {
            client = Some(version.trim().to_string());
        } else if let Some(version) = line.strip_prefix("ollama version is ") {
            server = Some(version.trim().to_string());
        }
    }
    let client = client.or_else(|| server.clone());
    (client, server)
}

/// The state of the Ollama server at the chosen address.
#[derive(Debug, Clone)]
pub(crate) enum OllamaServer {
    Unreachable(String),
    Reachable {
        check: Result<axocoatl_llm_ollama::NativeOllamaServerCheck, String>,
        models: Vec<ollama_setup::InstalledModel>,
    },
}

impl OllamaServer {
    pub(crate) async fn probe(base_url: &str) -> Self {
        match ollama_setup::probe_server(base_url).await {
            ollama_setup::ServerProbe::Unreachable(error) => Self::Unreachable(error),
            ollama_setup::ServerProbe::Reachable { models, check } => {
                Self::Reachable { check, models }
            }
        }
    }

    pub(crate) fn version(&self) -> Option<&str> {
        match self {
            Self::Reachable {
                check: Ok(check), ..
            } => check.version.as_deref(),
            _ => None,
        }
    }

    pub(crate) fn models(&self) -> &[ollama_setup::InstalledModel] {
        match self {
            Self::Reachable { models, .. } => models,
            Self::Unreachable(_) => &[],
        }
    }
}

/// What Step 0 found.
#[derive(Debug, Clone)]
pub(crate) struct Facts {
    pub(crate) os: Os,
    pub(crate) memory_bytes: Option<u64>,
    pub(crate) cpus: u64,
    /// Free space where `~/.ollama` lives (the models).
    pub(crate) disk_free_bytes: Option<u64>,
    /// Free space where Podman keeps its machine and images, when that is
    /// another file system than the models'.
    pub(crate) containers_free_bytes: Option<u64>,
    pub(crate) root_user: bool,
    pub(crate) user: String,
    pub(crate) port_8080_free: bool,
    /// `git --version`, or `None` when git is missing (on macOS: the
    /// Command Line Tools shim without the tools).
    pub(crate) git: Option<String>,
    pub(crate) brew: Option<String>,
    pub(crate) podman_path: Option<PathBuf>,
    pub(crate) podman: PodmanReport,
    pub(crate) ollama_cli: Option<OllamaCli>,
    pub(crate) ollama_url: String,
    pub(crate) ollama_server: OllamaServer,
    pub(crate) ollama_service: OllamaService,
    pub(crate) claude: Option<(PathBuf, Option<String>)>,
    pub(crate) codex: Option<PathBuf>,
    pub(crate) gh: Option<PathBuf>,
    /// An `ollama.exe` on PATH inside WSL: the Windows app, which WSL in NAT
    /// mode cannot reach.
    pub(crate) windows_ollama: bool,
    /// A daemon answers on this user's IPC socket.
    pub(crate) daemon_running: bool,
    /// The running daemon's version (`GET /health`), when it answers.
    pub(crate) daemon_version: Option<String>,
    /// The program the installed Always-On Service runs.
    pub(crate) service_program: Option<PathBuf>,
    pub(crate) xdg_config_home: Option<PathBuf>,
    /// The kernel's Landlock ABI (Linux; `None` elsewhere or without it).
    pub(crate) landlock_abi: Option<i64>,
    /// This user's systemd manager, on a Linux with systemd ([`user_systemd`]).
    pub(crate) user_systemd: Option<UserSystemd>,
    /// SELinux is enforcing (`/sys/fs/selinux/enforce` says 1).
    pub(crate) selinux_enforcing: bool,
    /// The proxy variables set in this environment, by name.
    pub(crate) proxy_variables: Vec<String>,
}

impl Facts {
    pub(crate) fn memory_gb(&self) -> Option<u64> {
        self.memory_bytes.map(axocoatl_isolation::host::memory_gb)
    }
}

fn first_word_version(text: &str) -> Option<String> {
    text.split_whitespace()
        .find(|word| word.chars().next().is_some_and(|c| c.is_ascii_digit()))
        .map(|word| word.trim_end_matches(',').to_string())
}

/// Where Podman keeps its machines and images for this user.
pub(crate) fn containers_dir(home: &Path) -> PathBuf {
    home.join(".local").join("share").join("containers")
}

/// The nearest existing path at or above `path`.
fn existing_ancestor(path: &Path) -> Option<&Path> {
    let mut existing = path;
    while !existing.exists() {
        existing = existing.parent()?;
    }
    Some(existing)
}

/// `a` and `b` are on one file system.
fn same_file_system(a: &Path, b: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let device = |path: &Path| {
            existing_ancestor(path)
                .and_then(|path| std::fs::metadata(path).ok())
                .map(|metadata| metadata.dev())
        };
        device(a).is_some() && device(a) == device(b)
    }
    #[cfg(not(unix))]
    {
        let _ = (a, b);
        true
    }
}

/// Whether this user's systemd manager answers, on a Linux with systemd:
/// the Always-On Service is a `systemctl --user` unit, and rootless Podman
/// keeps its runtime directory in the user's session. A user reached with
/// `su -`, `sudo -iu` or an SSH login on a system that starts no user
/// session has no user bus (`XDG_RUNTIME_DIR` unset), so neither works.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct UserSystemd {
    /// `systemctl --user is-system-running` reached the user's manager.
    pub(crate) reachable: bool,
    /// What it said: its state, or why it did not answer.
    pub(crate) state: String,
    /// `loginctl show-user USER --property=Linger --value`, or for a user
    /// loginctl does not know (no session, no linger) whether
    /// `/var/lib/systemd/linger/USER` exists: the manager keeps running
    /// after the user logs out (`None`: not known).
    pub(crate) linger: Option<bool>,
}

/// The states `systemctl --user is-system-running` prints for a manager it
/// reached.
const USER_MANAGER_STATES: [&str; 6] = [
    "running",
    "degraded",
    "starting",
    "initializing",
    "maintenance",
    "stopping",
];

/// [`UserSystemd`] for `user`, read with `systemctl --user` and `loginctl`.
pub(crate) async fn user_systemd(host: &Host, user: &str) -> UserSystemd {
    let probe = host
        .probe("systemctl", &["--user", "is-system-running"])
        .await;
    let (reachable, state) = match &probe {
        None => (false, "systemctl did not answer".to_string()),
        Some(probe) => {
            let said = probe.stdout.lines().next().unwrap_or_default().trim();
            if USER_MANAGER_STATES.contains(&said) {
                (true, said.to_string())
            } else {
                // Without a user bus it prints `offline` and says why on
                // stderr ("Failed to connect to bus: No medium found").
                let why = probe
                    .stderr
                    .lines()
                    .rev()
                    .map(str::trim)
                    .find(|line| !line.is_empty())
                    .or(Some(said).filter(|said| !said.is_empty()))
                    .unwrap_or("no answer");
                (false, why.to_string())
            }
        }
    };
    let linger = host
        .probe(
            "loginctl",
            &["show-user", user, "--property=Linger", "--value"],
        )
        .await
        .filter(|probe| probe.success)
        .and_then(|probe| match probe.stdout.trim() {
            "yes" => Some(true),
            "no" => Some(false),
            _ => None,
        })
        // The flag `loginctl enable-linger` sets, which systemd reads at
        // boot.
        .or_else(|| {
            let flags = host.at("/var/lib/systemd/linger");
            flags.is_dir().then(|| flags.join(user).exists())
        });
    UserSystemd {
        reachable,
        state,
        linger,
    }
}

/// SELinux is enforcing on this computer (under [`Host::root`]).
pub(crate) fn selinux_enforcing(host: &Host) -> bool {
    std::fs::read_to_string(host.at("/sys/fs/selinux/enforce")).is_ok_and(|text| text.trim() == "1")
}

/// `git --version`'s version, or `None` when git is missing. On macOS,
/// `/usr/bin/git` is a stand-in for the Command Line Tools' git: without
/// the tools it opens Apple's installer instead of answering, so it is run
/// only when `xcode-select -p` finds them.
pub(crate) async fn git_version(host: &Host) -> Option<String> {
    let found = host.find("git")?;
    if host.macos && found == host.at("/usr/bin/git") && !command_line_tools(host).await {
        return None;
    }
    match host.probe(&found.to_string_lossy(), &["--version"]).await {
        Some(probe) if probe.success => first_word_version(&probe.stdout),
        _ => None,
    }
}

/// `xcode-select -p` finds the Command Line Tools (or Xcode).
pub(crate) async fn command_line_tools(host: &Host) -> bool {
    host.probe("xcode-select", &["-p"])
        .await
        .is_some_and(|probe| probe.success && !probe.stdout.trim().is_empty())
}

/// Read this computer. `ollama_url` is the server native Sessions will use;
/// `check_port_8080`: whether to see if port 8080 is free (not when
/// `--port` names the port).
pub(crate) async fn detect(host: &Host, ollama_url: &str, check_port_8080: bool) -> Facts {
    let xdg_config_home = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute());
    let os = if host.macos {
        let version = host
            .probe("/usr/bin/sw_vers", &["-productVersion"])
            .await
            .filter(|probe| probe.success)
            .map(|probe| probe.stdout.trim().to_string());
        let chip = host
            .probe("/usr/sbin/sysctl", &["-n", "machdep.cpu.brand_string"])
            .await
            .filter(|probe| probe.success)
            .map(|probe| probe.stdout.trim().to_string())
            .filter(|chip| !chip.is_empty());
        Os::MacOs {
            version,
            apple_silicon: std::env::consts::ARCH == "aarch64",
            chip,
        }
    } else {
        let release = OsRelease::parse(
            &std::fs::read_to_string(host.at("/etc/os-release")).unwrap_or_default(),
        );
        let kernel =
            std::fs::read_to_string(host.at("/proc/sys/kernel/osrelease")).unwrap_or_default();
        let wsl = wsl_from(&kernel, std::env::var("WSL_DISTRO_NAME").ok().as_deref());
        Os::Linux {
            release,
            wsl,
            systemd: host.at("/run/systemd/system").is_dir(),
        }
    };
    let git = git_version(host).await;
    let user_systemd = match &os {
        Os::Linux { systemd: true, .. } => Some(user_systemd(host, &current_user()).await),
        _ => None,
    };
    let brew = homebrew_version(host).await;
    let podman_path = host.find("podman");
    let user = current_user();
    let mut podman = match &podman_path {
        Some(path) => {
            podman_report::inspect_through(&path.to_string_lossy(), host.podman_connection.clone())
                .await
        }
        None => PodmanReport::not_installed(),
    };
    crate::doctor::read_id_ranges(&mut podman, host, &host.root, &os, &user).await;
    let ollama_cli = match host.find("ollama") {
        Some(path) => {
            let host_value = ollama_url.to_string();
            let probe = host
                .probe_env(
                    &path.to_string_lossy(),
                    &["--version"],
                    &[("OLLAMA_HOST", host_value.as_str())],
                )
                .await;
            let (client, server) = probe
                .map(|probe| parse_ollama_version(&format!("{}\n{}", probe.stdout, probe.stderr)))
                .unwrap_or((None, None));
            Some(OllamaCli {
                path,
                client,
                server,
            })
        }
        None => None,
    };
    let ollama_server = OllamaServer::probe(ollama_url).await;
    let claude =
        crate::connect_cmd::find_claude(None, Some(&host.run_path()), Some(&host.home)).ok();
    let claude = match claude {
        Some(path) => {
            let version = host
                .probe(&path.to_string_lossy(), &["--version"])
                .await
                .filter(|probe| probe.success)
                .and_then(|probe| first_word_version(&probe.stdout));
            Some((path, version))
        }
        None => None,
    };
    let daemon_running =
        axocoatl_daemon::ipc::IpcClient::connect(&axocoatl_daemon::ipc::default_socket_path())
            .await
            .is_ok();
    let models_dir = host.home.join(".ollama");
    let containers = containers_dir(&host.home);
    Facts {
        memory_bytes: axocoatl_isolation::host::memory_bytes(),
        cpus: axocoatl_isolation::host::cpu_count(),
        disk_free_bytes: free_bytes(&models_dir),
        containers_free_bytes: (!same_file_system(&models_dir, &containers))
            .then(|| free_bytes(&containers))
            .flatten(),
        root_user: is_root(),
        user,
        port_8080_free: !check_port_8080 || port_free(8080),
        git,
        brew,
        podman_path,
        podman,
        ollama_service: ollama_service(host, xdg_config_home.as_deref(), url_port(ollama_url)),
        ollama_cli,
        ollama_url: ollama_url.to_string(),
        ollama_server,
        claude,
        codex: host.find("codex"),
        gh: host.find("gh"),
        windows_ollama: os.wsl().is_some() && host.find("ollama.exe").is_some(),
        daemon_running,
        daemon_version: None,
        service_program: crate::doctor::installed_service_program(&host.home, host.macos),
        xdg_config_home,
        landlock_abi: landlock_abi(),
        user_systemd,
        selinux_enforcing: !host.macos && selinux_enforcing(host),
        proxy_variables: crate::doctor::proxy_variables(|name| std::env::var(name).ok()),
        os,
    }
}

/// `brew --version`, on macOS only, where onboarding installs Podman and
/// Ollama with Homebrew. On Linux nothing runs `brew`, or any package
/// manager, except an install the user agreed to.
pub(crate) async fn homebrew_version(host: &Host) -> Option<String> {
    if !host.macos {
        return None;
    }
    match host.probe("brew", &["--version"]).await {
        Some(probe) if probe.success => probe.stdout.lines().next().and_then(first_word_version),
        _ => None,
    }
}

pub(crate) fn is_root() -> bool {
    #[cfg(unix)]
    {
        // SAFETY: geteuid takes no arguments and cannot fail.
        unsafe { libc::geteuid() == 0 }
    }
    #[cfg(not(unix))]
    {
        false
    }
}

pub(crate) fn current_user() -> String {
    std::env::var("USER")
        .ok()
        .filter(|user| !user.is_empty())
        .or_else(|| {
            std::process::Command::new("id")
                .arg("-un")
                .output()
                .ok()
                .filter(|output| output.status.success())
                .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_string())
        })
        .unwrap_or_else(|| "$USER".to_string())
}

/// The kernel's Landlock ABI version on Linux, read with
/// `landlock_create_ruleset(NULL, 0, LANDLOCK_CREATE_RULESET_VERSION)`;
/// `None` elsewhere or when the kernel has no Landlock.
pub(crate) fn landlock_abi() -> Option<i64> {
    #[cfg(target_os = "linux")]
    {
        const LANDLOCK_CREATE_RULESET_VERSION: u32 = 1;
        // SAFETY: with a null attribute pointer, a size of zero and only the
        // version flag, the call reads and writes no memory: it returns the
        // ABI version or -1.
        let abi = unsafe {
            libc::syscall(
                libc::SYS_landlock_create_ruleset,
                std::ptr::null::<libc::c_void>(),
                0usize,
                LANDLOCK_CREATE_RULESET_VERSION,
            )
        };
        // `c_long` is `i64` on the 64-bit targets Axocoatl ships for.
        #[allow(clippy::unnecessary_cast)]
        let abi = abi as i64;
        (abi > 0).then_some(abi)
    }
    #[cfg(not(target_os = "linux"))]
    {
        None
    }
}

fn gb(bytes: u64) -> String {
    format!("{} GB", bytes / 1_000_000_000)
}

/// Step 0's lines.
pub(crate) fn summary(facts: &Facts) -> Vec<String> {
    let mut computer = Vec::new();
    match &facts.os {
        Os::MacOs {
            version,
            apple_silicon,
            chip,
        } => {
            computer.push(format!(
                "macOS {}",
                version.as_deref().unwrap_or("(version unknown)")
            ));
            computer.push(chip.clone().unwrap_or_else(|| {
                if *apple_silicon {
                    "Apple silicon".to_string()
                } else {
                    "Intel".to_string()
                }
            }));
        }
        Os::Linux { release, wsl, .. } => {
            computer.push(match wsl {
                Some(Wsl::Two) => format!("{} (WSL2)", release.name()),
                Some(Wsl::One) => format!("{} (WSL 1)", release.name()),
                None => release.name(),
            });
            computer.push(std::env::consts::ARCH.to_string());
        }
    }
    if let Some(gb) = facts.memory_gb() {
        computer.push(if facts.os.wsl().is_some() {
            format!("{gb} GB memory (by default WSL gives Linux half of Windows' memory)")
        } else {
            format!("{gb} GB memory")
        });
    }
    computer.push(format!("{} cores", facts.cpus));
    match (facts.disk_free_bytes, facts.containers_free_bytes) {
        (Some(free), Some(containers)) => computer.push(format!(
            "{} free for models, {} free for Podman",
            gb(free),
            gb(containers)
        )),
        (Some(free), None) => computer.push(format!("{} free", gb(free))),
        (None, _) => {}
    }
    let mut found = Vec::new();
    let mut missing = Vec::new();
    match &facts.brew {
        Some(version) => found.push(format!("Homebrew {version}")),
        None if facts.os.is_macos() => missing.push("Homebrew".to_string()),
        None => {}
    }
    match &facts.podman.client_version {
        Some(version) => {
            let state = match &facts.podman.readiness {
                axocoatl_isolation::podman::PodmanReadiness::Ready => "ready".to_string(),
                axocoatl_isolation::podman::PodmanReadiness::MachineMissing => {
                    "no machine".to_string()
                }
                axocoatl_isolation::podman::PodmanReadiness::MachineStopped => {
                    "machine stopped".to_string()
                }
                axocoatl_isolation::podman::PodmanReadiness::Unavailable(_) => {
                    "not answering".to_string()
                }
                axocoatl_isolation::podman::PodmanReadiness::NotInstalled => {
                    "not installed".to_string()
                }
            };
            found.push(format!("Podman {version} ({state})"));
        }
        None if facts.podman_path.is_some() => found.push("Podman (version unknown)".into()),
        None => missing.push("Podman".to_string()),
    }
    match (&facts.ollama_cli, &facts.ollama_server) {
        (_, OllamaServer::Reachable { check, .. }) => {
            let (version, cloud) = match check {
                Ok(check) => (
                    check.version.clone().unwrap_or_else(|| "?".into()),
                    if check.cloud_disabled() {
                        "cloud models off"
                    } else {
                        "cloud models on"
                    },
                ),
                Err(_) => ("?".to_string(), "cloud mode unknown"),
            };
            found.push(format!(
                "Ollama {version} at {} ({cloud})",
                facts.ollama_url
            ));
        }
        (Some(cli), OllamaServer::Unreachable(_)) => found.push(format!(
            "Ollama {} (not running at {})",
            cli.client.as_deref().unwrap_or("?"),
            facts.ollama_url
        )),
        (None, OllamaServer::Unreachable(_)) => missing.push("Ollama".to_string()),
    }
    match &facts.claude {
        Some((_, Some(version))) => found.push(format!("claude {version}")),
        Some((_, None)) => found.push("claude".to_string()),
        None => missing.push("claude".to_string()),
    }
    match &facts.codex {
        Some(_) => found.push("codex".to_string()),
        None => missing.push("codex".to_string()),
    }
    match &facts.gh {
        Some(_) => found.push("gh".to_string()),
        None => missing.push("gh".to_string()),
    }
    match &facts.git {
        Some(version) => found.push(format!("git {version}")),
        None => missing.push("git".to_string()),
    }
    match (facts.daemon_running, &facts.daemon_version) {
        (true, Some(version)) => found.push(format!("a running Axocoatl {version} daemon")),
        (true, None) => found.push("a running Axocoatl daemon".to_string()),
        (false, _) => {}
    }
    if let Some(program) = &facts.service_program {
        found.push(format!(
            "the Always-On Service ({}, {})",
            if facts.daemon_running {
                "installed"
            } else {
                "installed, not running"
            },
            program.display()
        ));
    }
    let mut lines = vec![format!("This computer: {}.", computer.join(", "))];
    if !found.is_empty() {
        lines.push(format!("Found: {}.", found.join(", ")));
    }
    if !missing.is_empty() {
        lines.push(format!("Not found: {}.", missing.join(", ")));
    }
    if !facts.port_8080_free {
        lines.push("Port 8080 is in use; Axocoatl will use the next free port.".to_string());
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::super::host::testing::Shims;
    use super::*;

    #[test]
    fn os_release_chooses_the_package_manager_and_spots_ubuntu_22_04() {
        let ubuntu = OsRelease::parse(
            "PRETTY_NAME=\"Ubuntu 24.04.5 LTS\"\nID=ubuntu\nID_LIKE=debian\nVERSION_ID=\"24.04\"\n",
        );
        assert_eq!(ubuntu.package_manager(), Some(PackageManager::Apt));
        assert!(!ubuntu.podman_too_old());
        assert_eq!(ubuntu.name(), "Ubuntu 24.04.5 LTS");
        let jammy = OsRelease::parse("ID=ubuntu\nVERSION_ID=\"22.04\"\n");
        assert!(jammy.podman_too_old());
        let fedora = OsRelease::parse("ID=fedora\nVERSION_ID=42\n");
        assert_eq!(fedora.package_manager(), Some(PackageManager::Dnf));
        let rocky = OsRelease::parse("ID=\"rocky\"\nID_LIKE=\"rhel centos fedora\"\n");
        assert_eq!(rocky.package_manager(), Some(PackageManager::Dnf));
        let arch = OsRelease::parse("ID=arch\n");
        assert_eq!(arch.package_manager(), Some(PackageManager::Pacman));
        let suse = OsRelease::parse("ID=\"opensuse-tumbleweed\"\nID_LIKE=\"opensuse suse\"\n");
        assert_eq!(suse.package_manager(), Some(PackageManager::Zypper));
        assert_eq!(OsRelease::parse("ID=nixos\n").package_manager(), None);
        assert_eq!(
            PackageManager::Apt.install(&["podman"]).display(),
            "sudo apt-get install -y podman"
        );
        assert_eq!(
            PackageManager::Dnf.install(&["podman"]).display(),
            "sudo dnf install -y podman"
        );
        assert_eq!(
            PackageManager::Pacman.install(&["podman"]).display(),
            "sudo pacman -S --noconfirm podman"
        );
        assert_eq!(
            PackageManager::Zypper.install(&["podman"]).display(),
            "sudo zypper install -y podman"
        );
        assert_eq!(PackageManager::Apt.id_map_package(), Some("uidmap"));
        assert_eq!(PackageManager::Dnf.id_map_package(), Some("shadow-utils"));
        assert_eq!(PackageManager::Pacman.id_map_package(), Some("shadow"));
        assert_eq!(PackageManager::Zypper.id_map_package(), None);
    }

    /// What a reinstall would write, as dpkg, dpkg-statoverride and rpm
    /// print their records (their output copied from ubuntu:24.04 with dpkg
    /// 1.22 and fedora:42 with rpm 4.20).
    #[test]
    fn package_records_are_read_as_dpkg_and_rpm_print_them() {
        use PackageManager::{Apt, Dnf, Pacman};
        let list = "/.\n/usr\n/usr/bin\n/usr/bin/getsubids\n/usr/bin/newgidmap\n\
                    /usr/bin/newuidmap\n/usr/share/man/man1/newuidmap.1.gz\n";
        let files = Apt.package_files(list);
        assert_eq!(
            program_file(&files, "newuidmap").map(|file| file.path.as_str()),
            Some("/usr/bin/newuidmap")
        );
        assert_eq!(program_file(&files, "newuidmap").unwrap().mode, None);
        let rpm = "/usr/bin/newgidmap\t100755\tcap_setgid=ep\n/usr/bin/newgrp\t104755\t\n\
                   /usr/bin/newuidmap\t100755\tcap_setuid=ep\n\
                   /usr/share/man/man1/newuidmap.1.gz\t100644\t\n";
        let files = Dnf.package_files(rpm);
        let newuidmap = program_file(&files, "newuidmap").unwrap();
        assert_eq!(newuidmap.path, "/usr/bin/newuidmap");
        assert_eq!(newuidmap.mode, Some(0o100755));
        assert_eq!(newuidmap.caps.as_deref(), Some("cap_setuid=ep"));
        assert_eq!(program_file(&files, "newgrp").unwrap().mode, Some(0o104755));
        assert_eq!(
            program_file(&files, "newgrp").unwrap().caps.as_deref(),
            Some("")
        );
        assert_eq!(program_file(&files, "newusers"), None);
        // `rpm -q` without --qf, or a query that printed no file.
        assert!(Dnf
            .package_files("shadow-utils-4.17.4-1.fc42.aarch64\n")
            .is_empty());
        assert!(Apt.package_files("installed").is_empty());
        // `pacman -Ql shadow`, Arch Linux ARM's shadow 4.20.0.
        assert_eq!(
            Pacman.files_query("shadow"),
            Some(("pacman", vec!["-Ql".to_string(), "shadow".to_string()]))
        );
        let pacman = Pacman.package_files(
            "shadow /usr/\nshadow /usr/bin/\nshadow /usr/bin/newgidmap\nshadow \
             /usr/bin/newuidmap\nshadow /usr/share/man/man1/newuidmap.1.gz\n",
        );
        assert_eq!(
            program_file(&pacman, "newuidmap").map(|file| file.path.as_str()),
            Some("/usr/bin/newuidmap")
        );
        assert!(pacman.iter().all(|file| file.mode.is_none()));
        assert!(Pacman.package_files("shadow 4.20.0.arch1-1\n").is_empty());
        assert_eq!(
            Dnf.files_query("shadow-utils"),
            Some((
                "rpm",
                vec![
                    "-q".to_string(),
                    "--qf".to_string(),
                    "[%{FILENAMES}\\t%{FILEMODES:octal}\\t%{FILECAPS}\\n]".to_string(),
                    "shadow-utils".to_string()
                ]
            ))
        );

        // `dpkg-query -S /usr/bin/newuidmap` with a local diversion, and
        // with one a package added.
        let local = "local diversion from: /usr/bin/newuidmap\nlocal diversion to: \
                     /usr/bin/newuidmap.distrib\nuidmap: /usr/bin/newuidmap\n";
        assert_eq!(
            dpkg_diversion(local, "/usr/bin/newuidmap"),
            Some(("/usr/bin/newuidmap.distrib".to_string(), None))
        );
        let by = "diversion by fakepkg from: /usr/bin/newuidmap\ndiversion by fakepkg to: \
                  /usr/bin/newuidmap.real\nuidmap: /usr/bin/newuidmap\n";
        assert_eq!(
            dpkg_diversion(by, "/usr/bin/newuidmap"),
            Some((
                "/usr/bin/newuidmap.real".to_string(),
                Some("fakepkg".to_string())
            ))
        );
        assert_eq!(dpkg_diversion(by, "/usr/bin/newgidmap"), None);
        assert_eq!(
            dpkg_diversion("uidmap: /usr/bin/newuidmap\n", "/usr/bin/newuidmap"),
            None
        );
        // `dpkg-statoverride --list PATH`: the mode is octal.
        assert_eq!(
            dpkg_override_mode("root root 755 /usr/bin/newuidmap\n", "/usr/bin/newuidmap"),
            Some(0o755)
        );
        assert_eq!(
            dpkg_override_mode("root root 4755 /usr/bin/newuidmap\n", "/usr/bin/newuidmap"),
            Some(0o4755)
        );
        assert_eq!(dpkg_override_mode("", "/usr/bin/newuidmap"), None);
    }

    /// dpkg's path filters, as dpkg 1.22 in ubuntu:24.04 applied them to a
    /// reinstall of uidmap: `=` or a space, quotes dropped, globs with
    /// classes; files of dpkg.cfg.d named with a dot, `~` or a leading dot
    /// are not read; dpkg.cfg.d comes before dpkg.cfg; the last match
    /// decides.
    #[test]
    fn dpkg_path_filters_are_read_as_dpkg_reads_them() {
        let root = tempfile::tempdir().unwrap();
        let d = root.path().join("etc/dpkg/dpkg.cfg.d");
        std::fs::create_dir_all(&d).unwrap();
        let path = "/usr/bin/newgidmap";
        let excluded = |files: &[(&str, &str)]| {
            for entry in std::fs::read_dir(&d).unwrap() {
                std::fs::remove_file(entry.unwrap().path()).unwrap();
            }
            let _ = std::fs::remove_file(root.path().join("etc/dpkg/dpkg.cfg"));
            for (name, text) in files {
                let file = if *name == "dpkg.cfg" {
                    root.path().join("etc/dpkg/dpkg.cfg")
                } else {
                    d.join(name)
                };
                std::fs::write(file, text).unwrap();
            }
            dpkg_excluded(root.path(), path)
        };
        // Ubuntu's container images leave out manual pages and docs only.
        assert_eq!(
            excluded(&[(
                "excludes",
                "# Drop all man pages\npath-exclude=/usr/share/man/*\npath-exclude=/usr/share/doc/*\n"
            )]),
            None
        );
        assert_eq!(
            excluded(&[("90-test", "path-exclude=/usr/bin/newgidmap\n")]),
            Some((
                "path-exclude=/usr/bin/newgidmap".to_string(),
                "/etc/dpkg/dpkg.cfg.d/90-test".to_string()
            ))
        );
        for (files, left_out) in [
            (
                vec![("90-space", "path-exclude /usr/bin/newgidmap\n")],
                true,
            ),
            (
                vec![("90-quoted", "path-exclude=\"/usr/bin/newgid*\"\n")],
                true,
            ),
            (
                vec![("90-dot.conf", "path-exclude=/usr/bin/newgidmap\n")],
                false,
            ),
            (
                vec![(".hidden", "path-exclude=/usr/bin/newgidmap\n")],
                false,
            ),
            (
                vec![("90-tilde~", "path-exclude=/usr/bin/newgidmap\n")],
                false,
            ),
            (
                vec![(
                    "90-incl",
                    "path-exclude=/usr/*\npath-include=/usr/bin/newgid*\n",
                )],
                false,
            ),
            (
                vec![
                    ("80-incl", "path-include=/usr/bin/newgid*\n"),
                    ("90-excl", "path-exclude=/usr/bin/*\n"),
                ],
                true,
            ),
            (
                vec![("90-class", "path-exclude=/usr/bin/newg[a-i]dmap\n")],
                true,
            ),
            (
                vec![("90-class", "path-exclude=/usr/bin/newg[!a-i]dmap\n")],
                false,
            ),
            (
                vec![("dpkg.cfg", "path-exclude=/usr/bin/newgidmap\n")],
                true,
            ),
            (
                vec![
                    ("zz-incl", "path-include=/usr/bin/newgidmap\n"),
                    ("dpkg.cfg", "path-exclude=/usr/bin/newgidmap\n"),
                ],
                true,
            ),
            (
                vec![
                    ("zz-excl", "path-exclude=/usr/bin/newgidmap\n"),
                    ("dpkg.cfg", "path-include=/usr/bin/newgidmap\n"),
                ],
                false,
            ),
        ] {
            assert_eq!(excluded(&files).is_some(), left_out, "{files:?}");
        }
        assert!(glob_matches("/usr/*", "/usr/bin/newgidmap"));
        assert!(glob_matches("/usr/bin/new?idmap", "/usr/bin/newgidmap"));
        assert!(!glob_matches("/usr/bin/new?idmap", "/usr/bin/newidmap"));
        assert!(glob_matches("/usr/bin/\\*", "/usr/bin/*"));
        assert!(!glob_matches("/usr/bin/\\*", "/usr/bin/x"));
        assert!(glob_matches("/usr/bin/[]x]", "/usr/bin/]"));
        assert!(glob_matches("/usr/bin/[x", "/usr/bin/[x"));
    }

    /// pacman's `NoExtract` and `NoUpgrade`, as pacman.conf(5) has pacman
    /// read them: only in `[options]` (of pacman.conf and the files it
    /// includes there), each pattern matched against the path without its
    /// leading `/`, the last match winning.
    #[test]
    fn pacmans_noextract_and_noupgrade_are_read_as_pacman_reads_them() {
        let root = tempfile::tempdir().unwrap();
        let etc = root.path().join("etc");
        std::fs::create_dir_all(etc.join("pacman.d")).unwrap();
        assert_eq!(
            pacman_kept(root.path(), "NoExtract", "/usr/bin/newuidmap"),
            None
        );
        // Arch Linux's own pacman.conf has NoExtract only in comments, and
        // the docker image's adds documentation; neither names a helper.
        std::fs::write(
            etc.join("pacman.conf"),
            "[options]\n#NoExtract   =\nNoExtract  = usr/share/help/* !usr/share/help/en*\n\
             NoExtract  = usr/share/gtk-doc/html/* usr/share/doc/*\nInclude = /etc/pacman.d/*.conf\n\
             \n[core]\nInclude = /etc/pacman.d/mirrorlist\nNoExtract = usr/bin/newuidmap\n",
        )
        .unwrap();
        assert_eq!(
            pacman_kept(root.path(), "NoExtract", "/usr/bin/newuidmap"),
            None,
            "a repository section's line is not pacman's option"
        );
        std::fs::write(
            etc.join("pacman.d/local.conf"),
            "NoExtract = usr/bin/new?idmap # helpers\nNoUpgrade = usr/bin/newgidmap\n",
        )
        .unwrap();
        // A file the [options] section includes counts.
        std::fs::write(
            etc.join("pacman.d/mirrorlist"),
            "Server = https://x/$repo\n",
        )
        .unwrap();
        assert_eq!(
            pacman_kept(root.path(), "NoExtract", "/usr/bin/newuidmap"),
            Some((
                "usr/bin/new?idmap".to_string(),
                "/etc/pacman.d/local.conf".to_string()
            ))
        );
        assert_eq!(
            pacman_kept(root.path(), "NoUpgrade", "/usr/bin/newgidmap"),
            Some((
                "usr/bin/newgidmap".to_string(),
                "/etc/pacman.d/local.conf".to_string()
            ))
        );
        assert_eq!(
            pacman_kept(root.path(), "NoUpgrade", "/usr/bin/newuidmap"),
            None
        );
        // The last match wins: a later `!` pattern extracts it after all.
        std::fs::write(
            etc.join("pacman.d/zz.conf"),
            "NoExtract = !usr/bin/newuidmap\n",
        )
        .unwrap();
        assert_eq!(
            pacman_kept(root.path(), "NoExtract", "/usr/bin/newuidmap"),
            None
        );
        assert!(pacman_kept(root.path(), "NoExtract", "/usr/bin/newgidmap").is_some());
    }

    /// Whether dnf installs files without their file capabilities, as
    /// dnf5 5.2.18 in fedora:42 reads its configuration (`dnf
    /// --dump-main-config`): the last `tsflags` of a file's [main], added up
    /// across dnf.conf and dnf5's drop-ins.
    #[test]
    fn dnf_nocaps_is_read_from_its_configuration() {
        let root = tempfile::tempdir().unwrap();
        let etc = root.path().join("etc/dnf");
        std::fs::create_dir_all(etc.join("libdnf5.conf.d")).unwrap();
        assert_eq!(dnf_nocaps(root.path()), None);
        std::fs::write(etc.join("dnf.conf"), "[main]\ntsflags=nodocs\n").unwrap();
        assert_eq!(dnf_nocaps(root.path()), None);
        std::fs::write(
            etc.join("dnf.conf"),
            "[main]\ntsflags=nodocs\ntsflags=nocaps\n",
        )
        .unwrap();
        assert_eq!(
            dnf_nocaps(root.path()).as_deref(),
            Some("/etc/dnf/dnf.conf")
        );
        std::fs::write(
            etc.join("dnf.conf"),
            "[main]\ntsflags=nocaps\ntsflags=nodocs\n",
        )
        .unwrap();
        assert_eq!(dnf_nocaps(root.path()), None);
        std::fs::write(etc.join("dnf.conf"), "[main]\ntsflags = nodocs, nocaps\n").unwrap();
        assert!(dnf_nocaps(root.path()).is_some());
        // In another section it is not dnf's.
        std::fs::write(etc.join("dnf.conf"), "[fedora]\ntsflags=nocaps\n").unwrap();
        assert_eq!(dnf_nocaps(root.path()), None);
        std::fs::write(
            etc.join("libdnf5.conf.d/90-nocaps.conf"),
            "[main]\ntsflags=nocaps\n",
        )
        .unwrap();
        assert_eq!(
            dnf_nocaps(root.path()).as_deref(),
            Some("/etc/dnf/libdnf5.conf.d/90-nocaps.conf")
        );
    }

    /// What dpkg-query, rpm and pacman print (checked in ubuntu:24.04 and
    /// fedora:42): installed or not, and which package owns a file. The
    /// reinstall form is chosen only when the package is installed.
    #[test]
    fn package_queries_are_read_as_the_package_managers_print_them() {
        use PackageManager::{Apt, Dnf, Pacman};
        assert_eq!(Apt.package_installed(Some(0), "installed"), Some(true));
        assert_eq!(Apt.package_installed(Some(0), "not-installed"), Some(false));
        assert_eq!(Apt.package_installed(Some(0), "config-files"), Some(false));
        // "dpkg-query: no packages found matching uidmap"
        assert_eq!(Apt.package_installed(Some(1), ""), Some(false));
        assert_eq!(
            Dnf.package_installed(Some(0), "shadow-utils-4.17.4-1.fc42.aarch64\n"),
            Some(true)
        );
        assert_eq!(
            Dnf.package_installed(Some(1), "package x is not installed"),
            Some(false)
        );
        assert_eq!(Pacman.package_installed(Some(2), ""), None);
        assert_eq!(Apt.package_installed(None, ""), None);

        assert_eq!(
            Apt.owns("uidmap", Some(0), "uidmap: /usr/bin/newuidmap\n"),
            Some(true)
        );
        assert_eq!(
            Apt.owns("uidmap", Some(0), "other, uidmap: /usr/bin/newuidmap\n"),
            Some(true)
        );
        assert_eq!(
            Apt.owns(
                "uidmap",
                Some(0),
                "diversion by x from: /usr/bin/newuidmap\nx: /usr/bin/newuidmap\n"
            ),
            Some(false)
        );
        assert_eq!(Apt.owns("uidmap", Some(1), ""), Some(false));
        assert_eq!(
            Dnf.owns("shadow-utils", Some(0), "shadow-utils\n"),
            Some(true)
        );
        assert_eq!(Dnf.owns("shadow-utils", Some(1), ""), Some(false));
        assert_eq!(Pacman.owns("shadow", Some(0), "shadow\n"), Some(true));
        assert_eq!(Pacman.owns("shadow", None, ""), None);

        assert_eq!(
            Apt.id_map_install(Some(true)).unwrap().display(),
            "sudo apt-get install --reinstall -y uidmap"
        );
        for state in [Some(false), None] {
            assert_eq!(
                Apt.id_map_install(state).unwrap().display(),
                "sudo apt-get install -y uidmap"
            );
        }
        for state in [Some(true), None] {
            assert_eq!(
                Dnf.id_map_install(state).unwrap().display(),
                "sudo dnf reinstall -y shadow-utils"
            );
        }
        assert_eq!(
            Dnf.id_map_install(Some(false)).unwrap().display(),
            "sudo dnf install -y shadow-utils"
        );
        assert_eq!(
            Pacman.id_map_install(Some(true)).unwrap().display(),
            "sudo pacman -S --noconfirm shadow"
        );
        assert!(PackageManager::Zypper.id_map_install(Some(true)).is_none());
        assert_eq!(
            Apt.package_query("uidmap"),
            (
                "dpkg-query",
                vec![
                    "-W".to_string(),
                    "-f".to_string(),
                    "${db:Status-Status}".to_string(),
                    "uidmap".to_string()
                ]
            )
        );
        assert_eq!(
            Dnf.owner_query("/usr/bin/newuidmap").1,
            ["-qf", "--qf", "%{NAME}\n", "/usr/bin/newuidmap"]
        );
    }

    #[test]
    fn wsl_versions_are_told_apart() {
        assert_eq!(
            wsl_from("5.15.167.4-microsoft-standard-WSL2", None),
            Some(Wsl::Two)
        );
        assert_eq!(wsl_from("4.4.0-19041-Microsoft", None), Some(Wsl::One));
        assert_eq!(wsl_from("6.8.0-142-generic", None), None);
        assert_eq!(wsl_from("6.8.0", Some("Ubuntu")), Some(Wsl::Two));
    }

    #[test]
    fn ollama_versions_and_hosts_are_read() {
        assert_eq!(
            parse_ollama_version("ollama version is 0.40.6\n"),
            (Some("0.40.6".into()), Some("0.40.6".into()))
        );
        assert_eq!(
            parse_ollama_version("ollama version is 0.20.6\nWarning: client version is 0.40.6\n"),
            (Some("0.40.6".into()), Some("0.20.6".into()))
        );
        assert_eq!(
            parse_ollama_version(
                "Warning: could not connect to a running Ollama instance\nWarning: client version is 0.40.6\n"
            ),
            (Some("0.40.6".into()), None)
        );
        assert_eq!(ollama_host_port("127.0.0.1:11440"), 11440);
        assert_eq!(ollama_host_port("http://localhost:11441/"), 11441);
        assert_eq!(ollama_host_port("0.0.0.0"), 11434);
        assert_eq!(url_port("http://localhost:11434"), 11434);
        assert_eq!(url_port("http://127.0.0.1:11440/"), 11440);
        assert_eq!(url_port("http://[::1]:11442"), 11442);
        assert_eq!(url_port("http://localhost"), 80);
        assert_eq!(
            variable_in("# c\nOLLAMA_HOST=127.0.0.1:11440\n", "OLLAMA_HOST").as_deref(),
            Some("127.0.0.1:11440")
        );
        assert_eq!(
            variable_in(
                "[Service]\nEnvironment=\"OLLAMA_HOST=0.0.0.0:11500\"\n",
                "OLLAMA_HOST"
            )
            .as_deref(),
            Some("0.0.0.0:11500")
        );
        assert_eq!(
            variable_in(
                "<key>EnvironmentVariables</key><dict><key>OLLAMA_HOST</key>\n<string>127.0.0.1:11450</string>",
                "OLLAMA_HOST"
            )
            .as_deref(),
            Some("127.0.0.1:11450")
        );
    }

    /// The service onboarding restarts is the one that serves the chosen
    /// address: never the brew service or app for another port.
    #[test]
    fn the_service_serving_the_chosen_port_is_found() {
        let shims = Shims::new();
        let host = shims.host(true);
        assert_eq!(ollama_service(&host, None, 11434), OllamaService::Manual);
        std::fs::create_dir_all(host.at("/Applications/Ollama.app")).unwrap();
        assert_eq!(ollama_service(&host, None, 11434), OllamaService::App);
        assert_eq!(ollama_service(&host, None, 11440), OllamaService::Manual);
        shims.add("brew", "exit 0");
        let plist = brew_plist(&host);
        std::fs::create_dir_all(plist.parent().unwrap()).unwrap();
        std::fs::write(&plist, "<plist/>").unwrap();
        assert_eq!(ollama_service(&host, None, 11434), OllamaService::Brew);
        let env = homebrew_services_dir(&host.home, None).join("ollama.env");
        std::fs::create_dir_all(env.parent().unwrap()).unwrap();
        std::fs::write(&env, "OLLAMA_HOST=127.0.0.1:11440\n").unwrap();
        assert_eq!(ollama_service(&host, None, 11440), OllamaService::Brew);
        assert_eq!(ollama_service(&host, None, 11434), OllamaService::App);

        let linux = shims.host(false);
        assert_eq!(ollama_service(&linux, None, 11434), OllamaService::Manual);
        std::fs::create_dir_all(linux.at(OLLAMA_DROP_IN_DIR)).unwrap();
        std::fs::write(linux.at(OLLAMA_UNIT), "[Service]\nUser=ollama\n").unwrap();
        assert_eq!(ollama_service(&linux, None, 11434), OllamaService::Systemd);
        std::fs::write(
            linux.at(OLLAMA_DROP_IN_DIR).join("host.conf"),
            "[Service]\nEnvironment=\"OLLAMA_HOST=127.0.0.1:11445\"\n",
        )
        .unwrap();
        assert_eq!(ollama_service(&linux, None, 11434), OllamaService::Manual);
        assert_eq!(ollama_service(&linux, None, 11445), OllamaService::Systemd);
    }

    /// Linux onboarding never runs `brew`, even when one is on PATH (the
    /// release gate's guard shims put one there); macOS asks it its
    /// version.
    #[cfg(unix)]
    #[tokio::test]
    async fn only_macos_runs_brew() {
        let shims = Shims::new();
        shims.add("brew", "echo 'Homebrew 7.0.10'");
        assert_eq!(homebrew_version(&shims.host(false)).await, None);
        assert!(shims.calls().is_empty(), "{:?}", shims.calls());
        assert_eq!(
            homebrew_version(&shims.host(true)).await.as_deref(),
            Some("7.0.10")
        );
        assert_eq!(shims.calls(), ["brew --version"]);
    }

    #[test]
    fn homebrew_service_environment_follows_xdg() {
        assert_eq!(
            homebrew_services_dir(Path::new("/Users/a"), None),
            Path::new("/Users/a/.homebrew/services")
        );
        assert_eq!(
            homebrew_services_dir(Path::new("/Users/a"), Some(Path::new("/cfg"))),
            Path::new("/cfg/homebrew/services")
        );
    }

    #[test]
    fn landlock_is_read_only_on_linux() {
        if cfg!(target_os = "linux") {
            // Any kernel CI runs has Landlock or reports it absent; either
            // way the call returns without failing.
            let _ = landlock_abi();
        } else {
            assert_eq!(landlock_abi(), None);
        }
    }

    /// `systemctl --user` reaches the user's manager in a login session,
    /// and not after `su -` (no user bus); linger is read with loginctl.
    #[tokio::test]
    async fn the_user_systemd_session_and_linger_are_read() {
        let shims = Shims::new();
        shims.add(
            "systemctl",
            r#"[ -f "$(dirname "$0")/no-bus" ] && { echo offline; echo "Failed to connect to bus: No medium found" >&2; exit 1; }; echo degraded; exit 1"#,
        );
        shims.add("loginctl", "echo no");
        let host = shims.host(false);
        assert_eq!(
            user_systemd(&host, "alex").await,
            UserSystemd {
                reachable: true,
                state: "degraded".into(),
                linger: Some(false),
            }
        );
        assert!(shims
            .calls()
            .contains(&"loginctl show-user alex --property=Linger --value".to_string()));
        std::fs::write(shims.dir.path().join("no-bus"), "").unwrap();
        let none = user_systemd(&host, "alex").await;
        assert!(!none.reachable);
        assert_eq!(none.state, "Failed to connect to bus: No medium found");
        // A user loginctl does not know: linger is read from its flag file.
        shims.add(
            "loginctl",
            "echo 'Failed to get user: User ID 1001 is not logged in or lingering' >&2; exit 1",
        );
        assert_eq!(user_systemd(&host, "alex").await.linger, None);
        let flags = shims.root.path().join("var/lib/systemd/linger");
        std::fs::create_dir_all(&flags).unwrap();
        assert_eq!(user_systemd(&host, "alex").await.linger, Some(false));
        std::fs::write(flags.join("alex"), "").unwrap();
        assert_eq!(user_systemd(&host, "alex").await.linger, Some(true));
        // No systemctl at all.
        let empty = Shims::new();
        assert!(!user_systemd(&empty.host(false), "alex").await.reachable);
    }

    /// On macOS the git in /usr/bin runs only when `xcode-select -p` finds
    /// the Command Line Tools: without them it would open Apple's
    /// installer. Another git (Homebrew's) runs as it is.
    #[tokio::test]
    async fn macos_git_is_run_only_with_the_command_line_tools() {
        let shims = Shims::new();
        let usr_bin = shims.root.path().join("usr/bin");
        std::fs::create_dir_all(&usr_bin).unwrap();
        let calls = shims.dir.path().join("calls.log");
        std::fs::write(
            usr_bin.join("git"),
            format!(
                "#!/bin/sh\nprintf 'apple-git %s\\n' \"$*\" >> \"{}\"\necho 'git version 2.39.5 (Apple Git-154)'\n",
                calls.display()
            ),
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(usr_bin.join("git"), std::fs::Permissions::from_mode(0o755))
                .unwrap();
        }
        shims.add("xcode-select", "exit 2");
        let mut host = shims.host(true);
        host.path = std::env::join_paths([shims.dir.path(), usr_bin.as_path()]).unwrap();
        assert_eq!(git_version(&host).await, None);
        assert!(!shims
            .calls()
            .iter()
            .any(|call| call.starts_with("apple-git")));
        shims.add("xcode-select", "echo /Library/Developer/CommandLineTools");
        assert_eq!(git_version(&host).await.as_deref(), Some("2.39.5"));
        // Homebrew's git, first on PATH, needs no tools.
        shims.add("git", "echo 'git version 2.51.0'");
        shims.add("xcode-select", "exit 2");
        assert_eq!(git_version(&host).await.as_deref(), Some("2.51.0"));
    }

    #[test]
    fn selinux_enforcing_is_read_from_its_file() {
        let shims = Shims::new();
        let host = shims.host(false);
        assert!(!selinux_enforcing(&host));
        let selinux = shims.root.path().join("sys/fs/selinux");
        std::fs::create_dir_all(&selinux).unwrap();
        std::fs::write(selinux.join("enforce"), "0").unwrap();
        assert!(!selinux_enforcing(&host));
        std::fs::write(selinux.join("enforce"), "1").unwrap();
        assert!(selinux_enforcing(&host));
    }
}
