//! Host-side git introspection for provisioning a **remote** (E2B) sandbox.
//!
//! A remote microVM has no bind-mount of the local tree; it reproduces the repo
//! by `git clone` *inside* the VM. To drive that we read the local repo on the
//! host: its origin URL (normalized to a clean https URL), current branch, and
//! that it is on a clean, committed ref. Every command runs via `git -C <dir>`
//! on the host and is classified by **exit code** (stable across git versions),
//! never by stderr text. The Podman backend never calls this — it bind-mounts
//! the tree directly.

use std::path::{Path, PathBuf};
use std::process::Output;

use axocoatl_core::SecureDir;
use tokio::process::Command;

const HOST_GIT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

fn hardened_local_git_command(dir: &Path, args: &[&str]) -> Command {
    let mut command = Command::new("git");
    command
        .kill_on_drop(true)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_ASKPASS", "/usr/bin/false")
        .env("GIT_EXTERNAL_DIFF", "/usr/bin/false")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .arg("--no-optional-locks")
        .arg("-C")
        .arg(dir)
        // Repository-owned config must not turn read-only introspection into
        // host command execution. In particular, `status` consults
        // core.fsmonitor and Git credential operations accumulate helpers.
        .args(["-c", "core.fsmonitor=false"])
        .args(["-c", "core.hooksPath=/dev/null"])
        .args(["-c", "credential.helper="])
        .args(["-c", "diff.external="])
        .args(["-c", "protocol.ext.allow=never"])
        .args(["-c", "protocol.file.allow=never"])
        .args(["-c", "protocol.git.allow=never"])
        .args(["-c", "protocol.ssh.allow=never"])
        .args(args);
    command
}

fn hardened_remote_git_command(args: &[&str]) -> Command {
    let mut command = Command::new("git");
    command
        .kill_on_drop(true)
        // Do not discover the repository's local config at all. Global user
        // credential helpers remain available for private HTTPS repositories;
        // the agent-writable repo cannot replace them because this command has
        // no `-C` and runs from the filesystem root.
        .current_dir(Path::new("/"))
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_COMMON_DIR")
        .env_remove("GIT_INDEX_FILE")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_ASKPASS", "/usr/bin/false")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .arg("--no-optional-locks")
        .args(["-c", "protocol.ext.allow=never"])
        .args(["-c", "protocol.file.allow=never"])
        .args(["-c", "protocol.git.allow=never"])
        .args(["-c", "protocol.ssh.allow=never"])
        .args(args);
    command
}

/// What a remote VM needs to reproduce a local git repo.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteRepoSpec {
    /// Clean https clone URL — no embedded credentials.
    pub https_url: String,
    /// Branch to clone and track.
    pub branch: String,
    /// Exact local commit proven to be the current remote branch tip before a
    /// billable VM is created.
    pub commit: String,
    /// Credential-scope authority derived from the sanitized HTTPS URL.
    pub credential_authority: String,
    /// Directory name to clone into (repo basename, no `.git`).
    pub name: String,
}

async fn git(dir: &Path, args: &[&str]) -> Result<Output, String> {
    let mut command = hardened_local_git_command(dir, args);
    tokio::time::timeout(HOST_GIT_TIMEOUT, command.output())
        .await
        .map_err(|_| "git command timed out after 60 seconds".to_string())?
        .map_err(|e| format!("could not run git: {e}"))
}

async fn remote_git(args: &[&str]) -> Result<Output, String> {
    let mut command = hardened_remote_git_command(args);
    tokio::time::timeout(HOST_GIT_TIMEOUT, command.output())
        .await
        .map_err(|_| "git remote command timed out after 60 seconds".to_string())?
        .map_err(|error| format!("could not run remote git check: {error}"))
}

struct GitInspectionDir {
    parent: SecureDir,
    relative: PathBuf,
    root: SecureDir,
}

impl GitInspectionDir {
    fn create(parent: &SecureDir, commit: &str) -> Result<Self, String> {
        let relative = PathBuf::from("runtime/git-inspection")
            .join(format!("inspect-{}", uuid::Uuid::new_v4()));
        let root = parent.child(&relative).map_err(|error| {
            format!("could not create protected git inspection directory: {error}")
        })?;
        root.child("objects").map_err(|error| {
            format!("could not initialize protected git inspection objects: {error}")
        })?;
        root.child("refs").map_err(|error| {
            format!("could not initialize protected git inspection refs: {error}")
        })?;
        root.atomic_write("HEAD", format!("{commit}\n").as_bytes())
            .map_err(|error| {
                format!("could not initialize protected git inspection HEAD: {error}")
            })?;
        root.atomic_write(
            "config",
            b"[core]\n\trepositoryformatversion = 0\n\tbare = false\n\tfilemode = true\n",
        )
        .map_err(|error| {
            format!("could not initialize protected git inspection config: {error}")
        })?;
        Ok(Self {
            parent: parent.clone(),
            relative,
            root,
        })
    }
}

impl Drop for GitInspectionDir {
    fn drop(&mut self) {
        if let Err(error) = self.parent.remove_dir_all(&self.relative) {
            tracing::warn!(
                path = %self.parent.path().join(&self.relative).display(),
                %error,
                "could not remove protected git inspection directory"
            );
        }
    }
}

fn inspection_git_command(
    inspection: &SecureDir,
    workspace: &SecureDir,
    object_directory: &Path,
    args: &[&str],
) -> Command {
    let mut command = Command::new("git");
    command
        .kill_on_drop(true)
        .current_dir(Path::new("/"))
        .env_remove("GIT_COMMON_DIR")
        .env_remove("GIT_CONFIG_PARAMETERS")
        .env_remove("GIT_ALTERNATE_OBJECT_DIRECTORIES")
        .env_remove("GIT_CEILING_DIRECTORIES")
        .env("GIT_DIR", inspection.path())
        .env("GIT_WORK_TREE", workspace.path())
        .env("GIT_INDEX_FILE", inspection.path().join("index"))
        .env("GIT_OBJECT_DIRECTORY", object_directory)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_CONFIG_COUNT", "0")
        .env("GIT_ATTR_NOSYSTEM", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_ASKPASS", "/usr/bin/false")
        .env("GIT_EXTERNAL_DIFF", "/usr/bin/false")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .arg("--no-optional-locks")
        .args(["-c", "core.fsmonitor=false"])
        .args(["-c", "core.hooksPath=/dev/null"])
        .args(["-c", "credential.helper="])
        .args(["-c", "diff.external="])
        .args(args);
    command
}

async fn inspection_git(
    inspection: &SecureDir,
    workspace: &SecureDir,
    object_directory: &Path,
    args: &[&str],
) -> Result<Output, String> {
    let mut command = inspection_git_command(inspection, workspace, object_directory, args);
    tokio::time::timeout(HOST_GIT_TIMEOUT, command.output())
        .await
        .map_err(|_| "protected git inspection timed out after 60 seconds".to_string())?
        .map_err(|error| format!("could not run protected git inspection: {error}"))
}

/// Compare the Workspace to `commit` with a fresh index and a config directory
/// beneath Axocoatl's masked control-plane root. Repository-owned config never
/// enters this Git process, so `.gitattributes` cannot activate a local
/// `filter.*.clean`/`process`, fsmonitor, hook, or external diff on the host.
async fn protected_worktree_status(
    workspace: &SecureDir,
    control_root: &SecureDir,
    commit: &str,
) -> Result<Output, String> {
    if !matches!(commit.len(), 40 | 64) || !commit.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err("local HEAD is not a full hexadecimal object id".to_string());
    }
    workspace
        .verify_ambient_identity()
        .map_err(|error| format!("Workspace identity changed before git inspection: {error}"))?;
    control_root.verify_ambient_identity().map_err(|error| {
        format!("control-plane identity changed before git inspection: {error}")
    })?;

    let object_path = git(
        workspace.path(),
        &[
            "rev-parse",
            "--path-format=absolute",
            "--git-path",
            "objects",
        ],
    )
    .await?;
    if !object_path.status.success() {
        return Err("could not resolve the repository object directory".to_string());
    }
    let object_directory = PathBuf::from(out(&object_path));
    if !object_directory.is_absolute() {
        return Err("git returned a non-absolute object directory".to_string());
    }

    let inspection = GitInspectionDir::create(control_root, commit)?;
    let read_tree = inspection_git(
        &inspection.root,
        workspace,
        &object_directory,
        &["read-tree", commit],
    )
    .await?;
    if !read_tree.status.success() {
        return Err(format!(
            "could not build a protected index for local HEAD: {}",
            String::from_utf8_lossy(&read_tree.stderr).trim()
        ));
    }
    let status = inspection_git(
        &inspection.root,
        workspace,
        &object_directory,
        &[
            "status",
            "--porcelain=v1",
            "--untracked-files=all",
            "--ignore-submodules=all",
        ],
    )
    .await?;
    inspection.root.verify_ambient_identity().map_err(|error| {
        format!("protected git inspection directory changed while git ran: {error}")
    })?;
    workspace
        .verify_ambient_identity()
        .map_err(|error| format!("Workspace identity changed while git ran: {error}"))?;
    control_root
        .verify_ambient_identity()
        .map_err(|error| format!("control-plane identity changed while git ran: {error}"))?;
    Ok(status)
}

fn out(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).trim().to_string()
}

/// Read the local repo at `dir` and derive an https clone spec, or a clear
/// error string explaining exactly what to fix. Runs on the host.
///
/// Ordering matters: the guard / dirty / no-commit / detached checks run before
/// deriving the ref because a remote sandbox reproduces from a clean committed
/// branch.
pub async fn remote_repo_spec(
    workspace: &SecureDir,
    control_root: &SecureDir,
) -> Result<RemoteRepoSpec, String> {
    let dir = workspace.path();
    let d = dir.display();

    // 0. Is this a git work tree at all?
    if !git(dir, &["rev-parse", "--is-inside-work-tree"])
        .await?
        .status
        .success()
    {
        return Err(format!(
            "'{d}' is not a git repository. Point the session at a directory that contains one."
        ));
    }

    // 1. At least one commit (unborn repo → `rev-parse HEAD` exits non-zero).
    let head = git(dir, &["rev-parse", "HEAD"]).await?;
    if !head.status.success() {
        return Err(format!(
            "the repo at '{d}' has no commits yet. Make at least one commit before a remote \
             sandbox can clone it."
        ));
    }
    let commit = out(&head);

    // 2. Clean tree — build the comparison under protected Axocoatl state,
    //    without loading repository-owned config. Untracked files count as
    //    dirty because a remote clone would otherwise omit them.
    let status = protected_worktree_status(workspace, control_root, &commit).await?;
    if !status.status.success() {
        return Err(format!(
            "could not inspect the repo at '{d}' without trusting its executable Git config: {}",
            String::from_utf8_lossy(&status.stderr).trim()
        ));
    }
    let porcelain = out(&status);
    if !porcelain.is_empty() {
        return Err(format!(
            "the repo at '{d}' has uncommitted changes; a remote sandbox reproduces from a \
             committed ref. Commit or stash first. Changed:\n{porcelain}"
        ));
    }

    // 3. On a branch — `symbolic-ref -q` exits 1 (quietly) when detached.
    let branch_out = git(dir, &["symbolic-ref", "-q", "--short", "HEAD"]).await?;
    if !branch_out.status.success() {
        return Err(format!(
            "the repo at '{d}' is in detached-HEAD state (not on a branch). Check out a branch \
             (git switch <branch>) so the remote sandbox can clone and track it."
        ));
    }
    let branch = out(&branch_out);

    // 4. Origin remote → normalize to https.
    let origin = git(dir, &["remote", "get-url", "origin"]).await?;
    if !origin.status.success() {
        let others = out(&git(dir, &["remote"]).await?);
        return Err(if others.is_empty() {
            format!(
                "the repo at '{d}' has no remotes. A remote sandbox clones over https — add one \
                 (git remote add origin https://...)."
            )
        } else {
            format!(
                "the repo at '{d}' has no 'origin' remote (found: {}). Add one with \
                 `git remote add origin <https-url>`.",
                others.replace('\n', ", ")
            )
        });
    }
    let raw_url = out(&origin);
    let https_url = normalize_to_https(&raw_url).ok_or_else(|| {
        format!(
            "origin '{raw_url}' uses a scheme the sandbox's token auth can't use. A remote sandbox \
             clones over https — set an https origin, e.g. https://github.com/owner/repo.git."
        )
    })?;
    let name = repo_name(&https_url);
    let credential_authority = https_authority(&https_url)
        .ok_or_else(|| format!("origin '{raw_url}' does not contain a safe HTTPS authority"))?;
    // The name becomes the in-VM clone dir and the sandbox root; keep it a plain
    // identifier so it can't inject into a later shell command or traverse paths,
    // even though those sinks run inside the sandbox.
    if !is_safe_repo_name(&name) {
        return Err(format!(
            "origin '{raw_url}' yields an unusable repository name '{name}' — expected a plain \
             name like 'my-repo' (letters, digits, '.', '_', '-')."
        ));
    }
    let remote_ref = format!("refs/heads/{branch}");
    let remote = remote_git(&["ls-remote", "--exit-code", &https_url, &remote_ref]).await?;
    if !remote.status.success() {
        return Err(format!(
            "branch '{branch}' is not readable from origin. Push the branch and verify remote access before starting an E2B Session."
        ));
    }
    let remote_commit = out(&remote)
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .to_string();
    if remote_commit != commit {
        return Err(format!(
            "local HEAD {commit} is not the current origin/{branch} tip ({remote_commit}). Push the exact commit before starting an E2B Session."
        ));
    }
    Ok(RemoteRepoSpec {
        https_url,
        branch,
        commit,
        credential_authority,
        name,
    })
}

fn https_authority(url: &str) -> Option<String> {
    let authority = url.strip_prefix("https://")?.split('/').next()?;
    (!authority.is_empty()
        && authority
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '.' | '-' | ':' | '[' | ']')))
    .then(|| authority.to_string())
}

/// A plain repo identifier safe to use as a directory name and shell argument:
/// non-empty, not `.`/`..`, only `[A-Za-z0-9._-]`.
fn is_safe_repo_name(name: &str) -> bool {
    !name.is_empty()
        && name != "."
        && name != ".."
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

/// Strip a `user[:pass]@` prefix from an authority, but only when the `@` is in
/// the authority (before the first `/`) — so a `@` inside the path is untouched.
fn strip_userinfo(host_path: &str) -> &str {
    let authority_end = host_path.find('/').unwrap_or(host_path.len());
    match host_path[..authority_end].rfind('@') {
        Some(at) => &host_path[at + 1..],
        None => host_path,
    }
}

/// Normalize a git remote URL to a clean https URL with no embedded credentials.
/// Returns `None` for schemes a token-auth https clone cannot use (git://,
/// file://, bare local paths).
pub fn normalize_to_https(url: &str) -> Option<String> {
    let u = url.trim();
    if u.is_empty() {
        return None;
    }

    // scp-like shorthand: [user@]host:owner/repo(.git) — no scheme, a ':' before
    // any '/', and a non-absolute right-hand side. Userinfo is optional (git
    // accepts `host:owner/repo`), so we do NOT require '@'.
    if !u.contains("://") {
        let colon = u.find(':')?;
        let first_slash = u.find('/').unwrap_or(usize::MAX);
        if colon > first_slash {
            return None; // ':' is inside a path segment → a local path, not scp
        }
        let (authority, path) = (&u[..colon], &u[colon + 1..]);
        // Reject bare/absolute local paths (`/abs`, `C:/abs`, `host:/abs`).
        if authority.is_empty() || path.starts_with('/') {
            return None;
        }
        let host = authority.rsplit('@').next().unwrap_or(authority);
        if host.is_empty() {
            return None;
        }
        return Some(format!("https://{host}/{path}"));
    }

    // ssh://[user@]host[:port]/path
    if let Some(rest) = u.strip_prefix("ssh://") {
        let (authority, path) = rest.split_once('/')?;
        let host = authority.rsplit('@').next().unwrap_or(authority);
        let host = host.split(':').next().unwrap_or(host); // drop :port
        return Some(format!("https://{host}/{path}"));
    }

    // https:// or http:// — strip any embedded userinfo, force https.
    for scheme in ["https://", "http://"] {
        if let Some(rest) = u.strip_prefix(scheme) {
            return Some(format!("https://{}", strip_userinfo(rest)));
        }
    }

    None // git://, file://, or anything else
}

/// The repo basename from an https URL, without a trailing `.git`.
fn repo_name(https_url: &str) -> String {
    let name = https_url
        .trim_end_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or("repo")
        .trim_end_matches(".git");
    if name.is_empty() {
        "repo".to_string()
    } else {
        name.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run_git(dir: &Path, args: &[&str]) {
        let status = std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .status()
            .unwrap();
        assert!(status.success(), "git command failed: {args:?}");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn repository_config_and_attributes_cannot_execute_host_helpers() {
        use std::os::unix::fs::PermissionsExt;
        use tokio::io::AsyncWriteExt;

        let repo = tempfile::tempdir().unwrap();
        let hostile = tempfile::tempdir().unwrap();
        run_git(repo.path(), &["init", "-q"]);
        run_git(
            repo.path(),
            &["config", "user.email", "test@example.invalid"],
        );
        run_git(repo.path(), &["config", "user.name", "Test"]);
        std::fs::write(repo.path().join("tracked.txt"), "clean\n").unwrap();
        std::fs::write(repo.path().join(".gitattributes"), "*.txt filter=evil\n").unwrap();
        run_git(repo.path(), &["add", "tracked.txt", ".gitattributes"]);
        run_git(repo.path(), &["commit", "-q", "-m", "initial"]);

        let sentinel = hostile.path().join("host-command-ran");
        let helper = hostile.path().join("malicious-helper.sh");
        std::fs::write(
            &helper,
            format!(
                "#!/bin/sh\ntouch '{}'\nprintf 'username=attacker\\npassword=stolen\\n'\n",
                sentinel.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&helper, std::fs::Permissions::from_mode(0o700)).unwrap();
        run_git(
            repo.path(),
            &["config", "core.fsmonitor", helper.to_str().unwrap()],
        );
        run_git(
            repo.path(),
            &[
                "config",
                "credential.helper",
                &format!("!{}", helper.display()),
            ],
        );
        run_git(
            repo.path(),
            &["config", "filter.evil.clean", helper.to_str().unwrap()],
        );
        run_git(repo.path(), &["config", "filter.evil.required", "true"]);

        let control = tempfile::tempdir().unwrap();
        let workspace = SecureDir::open(repo.path()).unwrap();
        let control_root = SecureDir::open(control.path()).unwrap();
        let commit = out(&git(repo.path(), &["rev-parse", "HEAD"]).await.unwrap());
        let status = protected_worktree_status(&workspace, &control_root, &commit)
            .await
            .unwrap();
        assert!(status.status.success());
        assert!(out(&status).is_empty());
        assert!(
            !sentinel.exists(),
            "repository fsmonitor or clean filter executed on the host"
        );

        let mut command = hardened_local_git_command(repo.path(), &["credential", "fill"]);
        command
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        let mut child = command.spawn().unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(b"protocol=https\nhost=example.invalid\n\n")
            .await
            .unwrap();
        let _ = tokio::time::timeout(HOST_GIT_TIMEOUT, child.wait_with_output())
            .await
            .expect("credential probe timed out")
            .unwrap();
        assert!(
            !sentinel.exists(),
            "repository credential.helper executed on the host"
        );
    }

    #[test]
    fn remote_git_never_discovers_the_repository_local_config() {
        let command = hardened_remote_git_command(&[
            "ls-remote",
            "--exit-code",
            "https://example.invalid/owner/repo.git",
            "refs/heads/main",
        ]);
        let command = command.as_std();
        assert_eq!(command.get_current_dir(), Some(Path::new("/")));
        let arguments = command
            .get_args()
            .map(|argument| argument.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        assert!(arguments.contains(&"https://example.invalid/owner/repo.git".to_string()));
        assert!(!arguments.iter().any(|argument| argument == "origin"));
        assert!(!arguments.iter().any(|argument| argument == "-C"));
    }

    #[test]
    fn normalizes_scp_form() {
        assert_eq!(
            normalize_to_https("git@github.com:owner/repo.git").as_deref(),
            Some("https://github.com/owner/repo.git")
        );
    }

    #[test]
    fn normalizes_ssh_scheme_dropping_user_and_port() {
        assert_eq!(
            normalize_to_https("ssh://git@github.com:22/owner/repo.git").as_deref(),
            Some("https://github.com/owner/repo.git")
        );
    }

    #[test]
    fn passes_through_clean_https() {
        assert_eq!(
            normalize_to_https("https://github.com/owner/repo.git").as_deref(),
            Some("https://github.com/owner/repo.git")
        );
    }

    #[test]
    fn strips_embedded_credentials_from_https() {
        assert_eq!(
            normalize_to_https("https://x-access-token:ghp_secret@github.com/owner/repo.git")
                .as_deref(),
            Some("https://github.com/owner/repo.git")
        );
    }

    #[test]
    fn does_not_strip_at_sign_in_path() {
        assert_eq!(
            normalize_to_https("https://github.com/owner/repo@thing.git").as_deref(),
            Some("https://github.com/owner/repo@thing.git")
        );
    }

    #[test]
    fn upgrades_http_to_https() {
        assert_eq!(
            normalize_to_https("http://gitlab.local/owner/repo.git").as_deref(),
            Some("https://gitlab.local/owner/repo.git")
        );
    }

    #[test]
    fn rejects_unclonable_schemes() {
        assert_eq!(normalize_to_https("git://github.com/owner/repo.git"), None);
        assert_eq!(normalize_to_https("file:///srv/repo.git"), None);
        assert_eq!(normalize_to_https("/Users/me/local/repo"), None);
        assert_eq!(normalize_to_https("./relative/repo"), None);
    }

    #[test]
    fn repo_name_strips_git_suffix_and_path() {
        assert_eq!(repo_name("https://github.com/owner/repo.git"), "repo");
        assert_eq!(repo_name("https://github.com/owner/repo"), "repo");
        assert_eq!(repo_name("https://github.com/owner/repo.git/"), "repo");
    }

    #[test]
    fn normalizes_userless_scp_form() {
        // git accepts `host:owner/repo` with no user — must not be rejected.
        assert_eq!(
            normalize_to_https("github.com:owner/repo.git").as_deref(),
            Some("https://github.com/owner/repo.git")
        );
    }

    #[test]
    fn rejects_colon_after_slash_as_local_path() {
        // A ':' inside a path segment is not scp shorthand.
        assert_eq!(normalize_to_https("/srv/repos:mirror/x.git"), None);
        assert_eq!(normalize_to_https("host:/absolute/path"), None);
    }

    #[test]
    fn safe_repo_name_accepts_plain_and_rejects_metachars() {
        assert!(is_safe_repo_name("my-repo.git"));
        assert!(is_safe_repo_name("Repo_2"));
        assert!(!is_safe_repo_name(""));
        assert!(!is_safe_repo_name("."));
        assert!(!is_safe_repo_name(".."));
        assert!(!is_safe_repo_name("x';id;'"));
        assert!(!is_safe_repo_name("a b"));
        assert!(!is_safe_repo_name("$(whoami)"));
    }
}
