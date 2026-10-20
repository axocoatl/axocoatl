//! Who a commit the person makes from the workbench is by.
//!
//! Session Git runs inside the Session's sandbox with no configuration of the
//! person's, and every internal commit (Ways baselines, attempt refs under
//! `refs/axo/*`) is Axocoatl's. A commit the person makes from Source Control
//! is theirs: its author and committer are the `user.name` and `user.email`
//! host Git reads for the repository, the identity Keep as PR commits with,
//! and it is dated in the host's time zone. Without that identity the commit
//! is refused rather than made under another name. A repository that signs
//! its commits is refused too: the sandboxed Git cannot reach the person's
//! signing key, and an unsigned commit there would break the repository's
//! own rule.

use std::path::Path;

use serde::Serialize;

use crate::git_host::{plain_git, repository_git, run_bounded, HostTools, KEEP_LOCAL_TIMEOUT};

/// The longest name or email taken from a Git setting.
const MAX_IDENTITY_CHARS: usize = 256;

/// What host Git says a commit the person makes in this repository is by.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct CommitIdentity {
    /// `user.name`, when set.
    pub name: Option<String>,
    /// `user.email`, when set.
    pub email: Option<String>,
    /// The repository signs its commits (`commit.gpgSign`).
    pub signing: bool,
    /// Why a commit from the workbench is refused, when it is.
    pub problem: Option<String>,
}

/// The author and committer of one commit the person makes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitPerson {
    pub name: String,
    pub email: String,
    /// `<unix seconds> <+hhmm>` in the host's time zone, when host Git said.
    pub date: Option<String>,
}

/// Refusal when no identity is set.
pub const NO_IDENTITY: &str = "Git has no name and email for your commits in this repository. \
Set them in a terminal with `git config --global user.name \"Your Name\"` and \
`git config --global user.email you@example.com`, then commit again.";

/// Refusal for a repository that signs its commits.
pub const SIGNING_REPOSITORY: &str = "This repository signs its commits (commit.gpgSign), and a \
commit from the workbench cannot be signed. Commit from your terminal instead.";

fn setting(output: &std::process::Output) -> Option<String> {
    if !output.status.success() {
        return None;
    }
    let value = String::from_utf8_lossy(&output.stdout)
        .lines()
        .next()
        .unwrap_or("")
        .trim()
        .to_string();
    (!value.is_empty()
        && value.chars().count() <= MAX_IDENTITY_CHARS
        && !value.chars().any(char::is_control))
    .then_some(value)
}

impl CommitIdentity {
    /// Read the identity of `repo` through host Git: the repository's own,
    /// the person's global and the system configuration, as `git commit` in
    /// a terminal there would. Nothing a setting names can run.
    pub async fn read(tools: &HostTools, repo: &Path) -> Result<Self, String> {
        // `git config` reads settings and runs nothing. It goes without the
        // command-line overrides of other host Git here, one of which turns
        // commit.gpgSign off: that is the setting being asked about.
        let repo = repo.to_string_lossy();
        let get = |key: &'static [&'static str]| {
            let mut args = vec!["-C", repo.as_ref(), "config", "--get"];
            args.extend_from_slice(key);
            run_bounded(plain_git(tools, &args), KEEP_LOCAL_TIMEOUT, "git config")
        };
        let name = setting(&get(&["user.name"]).await?);
        let email = setting(&get(&["user.email"]).await?);
        let signing = setting(&get(&["--type=bool", "commit.gpgSign"]).await?)
            .is_some_and(|value| value == "true");
        let mut identity = Self {
            name,
            email,
            signing,
            problem: None,
        };
        identity.problem = identity.person(None).err();
        Ok(identity)
    }

    /// The person a commit is by, or why the workbench refuses to commit.
    pub fn person(&self, date: Option<String>) -> Result<GitPerson, String> {
        if self.signing {
            return Err(SIGNING_REPOSITORY.to_string());
        }
        match (&self.name, &self.email) {
            (Some(name), Some(email)) => Ok(GitPerson {
                name: name.clone(),
                email: email.clone(),
                date,
            }),
            _ => Err(NO_IDENTITY.to_string()),
        }
    }
}

/// The current time in the host's time zone, as Git writes it
/// (`<unix seconds> <+hhmm>`), read from `git var GIT_COMMITTER_IDENT` on
/// the host. `None` when host Git cannot say.
pub async fn host_commit_date(tools: &HostTools, repo: &Path) -> Option<String> {
    let output = run_bounded(
        repository_git(tools, repo, &["var", "GIT_COMMITTER_IDENT"]),
        KEEP_LOCAL_TIMEOUT,
        "git var",
    )
    .await
    .ok()?;
    if !output.status.success() {
        return None;
    }
    commit_date_of_ident(&String::from_utf8_lossy(&output.stdout))
}

/// The `<seconds> <offset>` that ends a Git ident line.
fn commit_date_of_ident(ident: &str) -> Option<String> {
    let mut fields = ident.trim().rsplitn(3, ' ');
    let offset = fields.next()?;
    let seconds = fields.next()?;
    let offset_ok = offset.len() == 5
        && matches!(offset.as_bytes()[0], b'+' | b'-')
        && offset[1..].bytes().all(|byte| byte.is_ascii_digit());
    (offset_ok && !seconds.is_empty() && seconds.bytes().all(|byte| byte.is_ascii_digit()))
        .then(|| format!("{seconds} {offset}"))
}

/// The `env` assignments that make one Git command's author and committer
/// `person`. Environment outranks every configuration file.
pub fn person_environment(person: &GitPerson) -> Vec<String> {
    let mut assignments = vec![
        format!("GIT_AUTHOR_NAME={}", person.name),
        format!("GIT_AUTHOR_EMAIL={}", person.email),
        format!("GIT_COMMITTER_NAME={}", person.name),
        format!("GIT_COMMITTER_EMAIL={}", person.email),
    ];
    if let Some(date) = &person.date {
        assignments.push(format!("GIT_AUTHOR_DATE={date}"));
        assignments.push(format!("GIT_COMMITTER_DATE={date}"));
    }
    assignments
}

/// What one `git commit` did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommitOutcome {
    Committed,
    /// Nothing was staged: no commit was made.
    NothingToCommit,
    /// Git refused or failed; its own message.
    Failed(String),
}

/// Read a `git commit` result. Git reports "nothing to commit" on stdout
/// with exit status 1; any other non-zero status is a failure whose message
/// is Git's own.
pub fn commit_outcome(exit_code: i32, stdout: &str, stderr: &str) -> CommitOutcome {
    if exit_code == 0 {
        return CommitOutcome::Committed;
    }
    let both = format!("{stdout}\n{stderr}");
    if both.contains("nothing to commit")
        || both.contains("no changes added to commit")
        || both.contains("nothing added to commit")
    {
        return CommitOutcome::NothingToCommit;
    }
    let message = [stderr.trim(), stdout.trim()]
        .into_iter()
        .find(|text| !text.is_empty())
        .unwrap_or("git commit failed")
        .lines()
        .take(6)
        .collect::<Vec<_>>()
        .join("\n");
    CommitOutcome::Failed(message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    fn git(repo: &Path, global: &Path, args: &[&str]) {
        let status = Command::new("git")
            .args(args)
            .current_dir(repo)
            .env("GIT_CONFIG_GLOBAL", global)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?}");
    }

    fn tools(global: &Path) -> HostTools {
        HostTools {
            path: None,
            env: vec![
                ("GIT_CONFIG_GLOBAL".into(), global.as_os_str().to_owned()),
                ("GIT_CONFIG_NOSYSTEM".into(), "1".into()),
            ],
        }
    }

    #[tokio::test]
    async fn identity_comes_from_the_repository_before_the_global_settings() {
        let root = tempfile::tempdir().unwrap();
        let global = root.path().join("gitconfig");
        std::fs::write(
            &global,
            "[user]\n\tname = Global Person\n\temail = global@example.com\n",
        )
        .unwrap();
        let repo = root.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        git(&repo, &global, &["init", "-q"]);
        let tools = tools(&global);

        let identity = CommitIdentity::read(&tools, &repo).await.unwrap();
        assert_eq!(identity.name.as_deref(), Some("Global Person"));
        assert_eq!(identity.email.as_deref(), Some("global@example.com"));
        assert_eq!(identity.problem, None);

        git(&repo, &global, &["config", "user.name", "Repo Person"]);
        git(
            &repo,
            &global,
            &["config", "user.email", "repo@example.com"],
        );
        let identity = CommitIdentity::read(&tools, &repo).await.unwrap();
        let person = identity.person(Some("1700000000 +0200".into())).unwrap();
        assert_eq!(person.name, "Repo Person");
        assert_eq!(person.email, "repo@example.com");
        assert_eq!(
            person_environment(&person),
            [
                "GIT_AUTHOR_NAME=Repo Person",
                "GIT_AUTHOR_EMAIL=repo@example.com",
                "GIT_COMMITTER_NAME=Repo Person",
                "GIT_COMMITTER_EMAIL=repo@example.com",
                "GIT_AUTHOR_DATE=1700000000 +0200",
                "GIT_COMMITTER_DATE=1700000000 +0200",
            ]
        );
    }

    #[tokio::test]
    async fn no_identity_and_a_signing_repository_are_refused() {
        let root = tempfile::tempdir().unwrap();
        let global = root.path().join("gitconfig");
        std::fs::write(&global, "").unwrap();
        let repo = root.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        git(&repo, &global, &["init", "-q"]);
        let tools = tools(&global);

        let identity = CommitIdentity::read(&tools, &repo).await.unwrap();
        assert_eq!(identity.name, None);
        assert_eq!(identity.problem.as_deref(), Some(NO_IDENTITY));
        assert_eq!(identity.person(None), Err(NO_IDENTITY.to_string()));

        git(&repo, &global, &["config", "user.name", "Signer"]);
        git(
            &repo,
            &global,
            &["config", "user.email", "signer@example.com"],
        );
        git(&repo, &global, &["config", "commit.gpgSign", "true"]);
        let identity = CommitIdentity::read(&tools, &repo).await.unwrap();
        assert!(identity.signing);
        assert_eq!(identity.problem.as_deref(), Some(SIGNING_REPOSITORY));
    }

    #[tokio::test]
    async fn the_host_date_carries_the_host_time_zone() {
        let root = tempfile::tempdir().unwrap();
        let global = root.path().join("gitconfig");
        std::fs::write(
            &global,
            "[user]\n\tname = Dated\n\temail = dated@example.com\n",
        )
        .unwrap();
        let repo = root.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        git(&repo, &global, &["init", "-q"]);
        let date = host_commit_date(&tools(&global), &repo).await.unwrap();
        let (seconds, offset) = date.split_once(' ').unwrap();
        assert!(seconds.parse::<u64>().unwrap() > 1_600_000_000);
        assert_eq!(offset.len(), 5);
    }

    #[test]
    fn ident_dates_are_parsed_strictly() {
        assert_eq!(
            commit_date_of_ident("A Person <a@example.com> 1700000000 -0730\n").as_deref(),
            Some("1700000000 -0730")
        );
        assert_eq!(commit_date_of_ident("A Person <a@example.com>"), None);
        assert_eq!(commit_date_of_ident("x <y> 17x 0200"), None);
    }

    #[test]
    fn commit_results_tell_nothing_to_commit_from_failure() {
        assert_eq!(commit_outcome(0, "", ""), CommitOutcome::Committed);
        assert_eq!(
            commit_outcome(
                1,
                "On branch main\nnothing to commit, working tree clean\n",
                ""
            ),
            CommitOutcome::NothingToCommit
        );
        assert_eq!(
            commit_outcome(1, "no changes added to commit (use \"git add\")", ""),
            CommitOutcome::NothingToCommit
        );
        assert_eq!(
            commit_outcome(
                128,
                "",
                "fatal: Unable to create '/w/.git/index.lock': File exists.\n\nAnother git process seems to be running"
            ),
            CommitOutcome::Failed(
                "fatal: Unable to create '/w/.git/index.lock': File exists.\n\nAnother git process seems to be running".into()
            )
        );
        assert_eq!(
            commit_outcome(1, "", ""),
            CommitOutcome::Failed("git commit failed".into())
        );
    }
}
