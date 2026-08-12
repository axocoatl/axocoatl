//! Pure identity and path derivation for attempt sets.
//!
//! User-controlled session and set ids never become path or branch components
//! directly. Stable digest keys keep independently-created sessions and attempt
//! sets isolated without relying on callers to sanitise them first.

use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

const SESSION_KEY_HEX_LEN: usize = 16;
const SET_KEY_HEX_LEN: usize = 24;
const AGENT_KEY_HEX_LEN: usize = 16;

fn digest_key(domain: &[u8], value: &str, len: usize) -> String {
    let mut hasher = Sha256::new();
    hasher.update(domain);
    hasher.update([0]);
    hasher.update(value.as_bytes());
    let encoded = hex::encode(hasher.finalize());
    encoded[..len].to_string()
}

/// Stable, path-safe key for a session.
pub(crate) fn session_key(session_id: &str) -> String {
    digest_key(b"session", session_id, SESSION_KEY_HEX_LEN)
}

/// Stable, short key derived from the whole attempt-set id.
pub(crate) fn set_key(set_id: &str) -> String {
    digest_key(b"attempt-set", set_id, SET_KEY_HEX_LEN)
}

/// Globally set-scoped branch name for one lane.
pub(crate) fn branch_name(set_id: &str, index: usize) -> String {
    format!("axo/attempt-{}-{index}", set_key(set_id))
}

/// Protected ref keeping the hidden workspace snapshot reachable until the set
/// is resolved. It is never checked out in the primary workspace.
pub(crate) fn base_ref(set_id: &str) -> String {
    format!("refs/axo/attempts/{}/base", set_key(set_id))
}

/// Protected refs for the resumable Keep transaction. These keep the primary
/// workspace preimage and computed postimage reachable until the completion
/// receipt is durable and attempt cleanup succeeds.
pub(crate) fn keep_preimage_ref(set_id: &str) -> String {
    format!("refs/axo/attempts/{}/keep-preimage", set_key(set_id))
}

pub(crate) fn keep_postimage_ref(set_id: &str) -> String {
    format!("refs/axo/attempts/{}/keep-postimage", set_key(set_id))
}

/// Primary-repository ref protecting the exact candidate tree captured after
/// Checks. The lane clone can then be stopped or garbage-collected without
/// making Judge/Keep depend on mutable lane storage.
pub(crate) fn checked_candidate_ref(set_id: &str, index: usize) -> String {
    format!("refs/axo/attempts/{}/checked-{index}", set_key(set_id))
}

/// Temporary local branch advertised while independent lane clones are made.
/// Removed from the primary repository immediately after clone setup.
pub(crate) fn clone_branch(set_id: &str) -> String {
    format!("axo/attempt-base-{}", set_key(set_id))
}

/// Safe identifier passed to `SessionSandbox`, which adds its own `axo-ses-`
/// prefix to form the Podman container name.
pub(crate) fn container_id(session_id: &str, set_id: &str, index: usize) -> String {
    format!(
        "attempt-{}-{}-{index}",
        session_key(session_id),
        set_key(set_id)
    )
}

/// Root holding every attempt set for one session in this repository.
pub(crate) fn session_attempts_root(repo_root: &Path, session_id: &str) -> PathBuf {
    repo_root
        .join(".axo-variants")
        .join(session_key(session_id))
}

/// Root holding one attempt set's manifest, metadata, and worktrees.
pub(crate) fn attempt_root(repo_root: &Path, session_id: &str, set_id: &str) -> PathBuf {
    session_attempts_root(repo_root, session_id).join(set_key(set_id))
}

/// Durable completion receipt for an idempotent Keep retry after the set's
/// mutable worktrees and current pointer have been removed.
pub(crate) fn keep_receipt_path(repo_root: &Path, session_id: &str, set_id: &str) -> PathBuf {
    session_attempts_root(repo_root, session_id)
        .join("receipts")
        .join(format!("keep-{}.json", set_key(set_id)))
}

/// Filesystem staging area for the resumable Keep transaction. It lives below
/// the set root so atomic renames into the primary checkout stay on one device.
pub(crate) fn keep_apply_root(repo_root: &Path, session_id: &str, set_id: &str) -> PathBuf {
    attempt_root(repo_root, session_id, set_id).join("keep-apply")
}

/// Worktree path for one lane in an attempt set.
pub(crate) fn worktree_path(
    repo_root: &Path,
    session_id: &str,
    set_id: &str,
    index: usize,
) -> PathBuf {
    attempt_root(repo_root, session_id, set_id).join(index.to_string())
}

/// Legacy-compatible run id used by the stream protocol.
pub(crate) fn run_id(session_id: &str, index: usize) -> String {
    format!("{session_id}#{index}")
}

/// Unique actor/checkpoint scope for one agent in one attempt-set lane.
///
/// [`crate::stream::run_of_scoped_agent`] splits on the final `:`. Keeping all
/// set and agent identity in the suffix means the prefix remains the fixed
/// `{session}#{index}` run id expected by existing stream consumers.
pub(crate) fn actor_scope(session_id: &str, set_id: &str, index: usize, agent_id: &str) -> String {
    let agent_key = digest_key(b"agent", agent_id, AGENT_KEY_HEX_LEN);
    format!(
        "{}:{}-{agent_key}",
        run_id(session_id, index),
        set_key(set_id)
    )
}

#[cfg(test)]
mod tests {
    use std::path::{Component, Path};

    use super::*;
    use crate::stream::run_of_scoped_agent;

    fn is_lower_hex(value: &str) -> bool {
        !value.is_empty()
            && value
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    }

    #[test]
    fn same_lane_in_different_sets_never_reuses_identity_or_storage() {
        let repo = Path::new("/repo");
        let first = "018f8f79-44ea-7e74-a445-112233445566";
        let second = "018f8f79-44ea-7e74-a445-665544332211";

        assert_ne!(branch_name(first, 0), branch_name(second, 0));
        assert_ne!(base_ref(first), base_ref(second));
        assert_ne!(keep_preimage_ref(first), keep_preimage_ref(second));
        assert_ne!(keep_postimage_ref(first), keep_postimage_ref(second));
        assert_ne!(
            checked_candidate_ref(first, 0),
            checked_candidate_ref(second, 0)
        );
        assert_ne!(
            checked_candidate_ref(first, 0),
            checked_candidate_ref(first, 1)
        );
        assert_ne!(
            container_id("session", first, 0),
            container_id("session", second, 0)
        );
        assert_ne!(
            worktree_path(repo, "session", first, 0),
            worktree_path(repo, "session", second, 0)
        );
        assert_ne!(
            actor_scope("session", first, 0, "builder"),
            actor_scope("session", second, 0, "builder")
        );
        assert_eq!(run_id("session", 0), "session#0");
    }

    #[test]
    fn sessions_sharing_a_repo_have_disjoint_attempt_roots() {
        let repo = Path::new("/repo");
        let first = session_attempts_root(repo, "workspace/a");
        let second = session_attempts_root(repo, "workspace/../a");

        assert_ne!(first, second);
        assert_ne!(
            attempt_root(repo, "workspace/a", "set-1"),
            attempt_root(repo, "workspace/../a", "set-1")
        );
        assert_ne!(
            keep_receipt_path(repo, "workspace/a", "set-1"),
            keep_receipt_path(repo, "workspace/../a", "set-1")
        );
        assert_eq!(first.parent(), Some(repo.join(".axo-variants").as_path()));
        assert_eq!(second.parent(), Some(repo.join(".axo-variants").as_path()));
    }

    #[test]
    fn user_ids_cannot_create_traversal_or_unsafe_branch_components() {
        let repo = Path::new("/repo");
        let session = "../../outside/session with spaces";
        let set = "../set:/\\?*\nrefs/heads/main";

        let session_component = session_key(session);
        let set_component = set_key(set);
        assert_eq!(session_component.len(), SESSION_KEY_HEX_LEN);
        assert_eq!(set_component.len(), SET_KEY_HEX_LEN);
        assert!(is_lower_hex(&session_component));
        assert!(is_lower_hex(&set_component));

        let root = attempt_root(repo, session, set);
        let relative = root.strip_prefix(repo).expect("attempt stays below repo");
        let components: Vec<_> = relative.components().collect();
        assert_eq!(components.len(), 3);
        assert!(components
            .iter()
            .all(|component| matches!(component, Component::Normal(_))));
        assert!(!root.to_string_lossy().contains(".."));
        assert!(!root.to_string_lossy().contains(session));
        assert!(!root.to_string_lossy().contains(set));

        let receipt = keep_receipt_path(repo, session, set);
        assert!(receipt.starts_with(repo.join(".axo-variants")));
        assert!(!receipt.to_string_lossy().contains(".."));
        assert!(!receipt.to_string_lossy().contains(session));
        assert!(!receipt.to_string_lossy().contains(set));

        let branch = branch_name(set, 42);
        assert!(branch.starts_with("axo/attempt-"));
        assert!(!branch.contains(".."));
        assert!(branch.bytes().all(|byte| byte.is_ascii_lowercase()
            || byte.is_ascii_digit()
            || b"/-".contains(&byte)));
        for value in [
            base_ref(set),
            keep_preimage_ref(set),
            keep_postimage_ref(set),
            checked_candidate_ref(set, 42),
            clone_branch(set),
            container_id(session, set, 42),
        ] {
            assert!(!value.contains(".."));
            assert!(!value.contains(session));
            assert!(!value.contains(set));
        }
    }

    #[test]
    fn actor_scope_retains_the_fixed_stream_run_id() {
        let scope = actor_scope(
            "session-7",
            "0190aabb-ccdd-7eef-8899-aabbccddeeff",
            3,
            "agent:with:colons",
        );
        assert_eq!(run_of_scoped_agent(&scope), Some(run_id("session-7", 3)));
        assert_eq!(scope.matches(':').count(), 1);
    }
}
