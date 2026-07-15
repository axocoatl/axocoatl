//! Host-side git introspection for provisioning a **remote** (E2B) sandbox.
//!
//! A remote microVM has no bind-mount of the local tree; it reproduces the repo
//! by `git clone` *inside* the VM. To drive that we read the local repo on the
//! host: its origin URL (normalized to a clean https URL), current branch, and
//! that it is on a clean, committed ref. Every command runs via `git -C <dir>`
//! on the host and is classified by **exit code** (stable across git versions),
//! never by stderr text. The Podman backend never calls this — it bind-mounts
//! the tree directly.

use std::path::Path;
use std::process::Output;

use tokio::process::Command;

/// What a remote VM needs to reproduce a local git repo.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteRepoSpec {
    /// Clean https clone URL — no embedded credentials.
    pub https_url: String,
    /// Branch to clone and track.
    pub branch: String,
    /// Directory name to clone into (repo basename, no `.git`).
    pub name: String,
}

async fn git(dir: &Path, args: &[&str]) -> Result<Output, String> {
    Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .await
        .map_err(|e| format!("could not run git: {e}"))
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
pub async fn remote_repo_spec(dir: &Path) -> Result<RemoteRepoSpec, String> {
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

    // 1. Clean tree — a remote VM reproduces from a committed ref (untracked
    //    files count as dirty: a file you expect would otherwise be missing).
    let porcelain = out(&git(dir, &["status", "--porcelain"]).await?);
    if !porcelain.is_empty() {
        return Err(format!(
            "the repo at '{d}' has uncommitted changes; a remote sandbox reproduces from a \
             committed ref. Commit or stash first. Changed:\n{porcelain}"
        ));
    }

    // 2. At least one commit (unborn repo → `rev-parse HEAD` exits non-zero).
    if !git(dir, &["rev-parse", "HEAD"]).await?.status.success() {
        return Err(format!(
            "the repo at '{d}' has no commits yet. Make at least one commit before a remote \
             sandbox can clone it."
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
    // The name becomes the in-VM clone dir and the sandbox root; keep it a plain
    // identifier so it can't inject into a later shell command or traverse paths,
    // even though those sinks run inside the sandbox.
    if !is_safe_repo_name(&name) {
        return Err(format!(
            "origin '{raw_url}' yields an unusable repository name '{name}' — expected a plain \
             name like 'my-repo' (letters, digits, '.', '_', '-')."
        ));
    }
    Ok(RemoteRepoSpec {
        https_url,
        branch,
        name,
    })
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
