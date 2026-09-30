//! Finite, actual tree observations through the existing invocation authority.
use super::*;
use axocoatl_session::execution_content::{
    is_capture_baseline, ActivationRepositorySnapshot, ActivationRepositorySnapshotView,
    RepositoryComparison, RepositorySnapshotPhase, MAX_COMPARED_PATH_BYTES,
    REPOSITORY_CAPTURE_SCRIPT,
};
use base64::Engine as _;

pub(super) const CAPTURE: &str = axocoatl_session::execution_content::REPOSITORY_SNAPSHOT_COMMAND;

/// How one capture treats the complete manifest a write-scope judgement
/// compares (see [`REPOSITORY_CAPTURE_SCRIPT`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum CaptureMode {
    /// Report the tree and patch only.
    Observe,
    /// Also keep the complete manifest in the sandbox for the After capture.
    Keep,
    /// Verify that the kept Before manifest at `baseline` still has the
    /// digest the host recorded, then list every path whose entry changed.
    Compare { baseline: String, expected: String },
}

/// The exact capture command of a mode.
pub(super) fn capture_command(mode: &CaptureMode) -> String {
    match mode {
        CaptureMode::Observe => CAPTURE.to_owned(),
        CaptureMode::Keep => format!("capture_mode=keep\n{REPOSITORY_CAPTURE_SCRIPT}"),
        CaptureMode::Compare { baseline, expected } => format!(
            "capture_mode=compare\nbaseline={baseline}\nexpected={expected}\n\
             {REPOSITORY_CAPTURE_SCRIPT}"
        ),
    }
}

/// The mode of an exact capture command; `None` for any other command.
pub(super) fn capture_command_mode(command: &str) -> Option<CaptureMode> {
    if command == CAPTURE {
        return Some(CaptureMode::Observe);
    }
    let head = command.strip_suffix(REPOSITORY_CAPTURE_SCRIPT)?;
    if head == "capture_mode=keep\n" {
        return Some(CaptureMode::Keep);
    }
    let (baseline, expected) = head
        .strip_prefix("capture_mode=compare\nbaseline=")?
        .strip_suffix('\n')?
        .split_once("\nexpected=")?;
    (is_capture_baseline(baseline) && is_digest(expected)).then(|| CaptureMode::Compare {
        baseline: baseline.to_owned(),
        expected: expected.to_owned(),
    })
}

fn is_digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

impl SessionDispatchController {
    pub(crate) async fn capture_activation_repository(
        &self,
        activation: &ActivationRef,
        phase: RepositorySnapshotPhase,
    ) -> Result<Option<EvidenceRef>> {
        let (bound, mut observation, mode) = {
            let state = self.lock()?;
            let snapshot = state.canonical.snapshot(&state.turn_id).map_err(error)?;
            if let Some(previous) = state
                .content
                .repository_snapshots(&snapshot, activation)
                .map_err(error)?
                .iter()
                .find(|previous| previous.content.phase == phase)
            {
                return Ok(Some(previous.reference.clone()));
            }
            let Some(bound) = state
                .bound
                .get(&activation.activation_id)
                .filter(|bound| bound.activation == *activation)
                .cloned()
            else {
                return Ok(None);
            };
            let Some(repository) = &bound.repository else {
                return Ok(None);
            };
            let mut observation = empty_observation(activation, phase, repository.reference());
            let grant = state
                .authority
                .grant_status(bound.grant.grant_id.as_str())
                .map_err(error)?;
            let usage = state
                .authority
                .usage(bound.grant.grant_id.as_str())
                .map_err(error)?;
            // The exact live lease includes every delegated ancestor. A
            // child's own grant may remain retained and unrevoked after its
            // parent loses authority; that never permits a new capture claim.
            let lease_live = match state
                .authority
                .attest_control_source(&bound.lease, now_ms()?)
            {
                Ok(_) => true,
                Err(
                    axocoatl_session::control_authority::AuthorityError::Denied
                    | axocoatl_session::control_authority::AuthorityError::StaleLease,
                ) => false,
                Err(failure) => return Err(error(failure)),
            };
            if bound.control.is_cancelled()
                || !lease_live
                || grant.revoked_at_revision.is_some()
                || now_ms()? >= grant.policy.expires_at_ms
            {
                observation.unavailable = Some(
                    "Execution was stopped, revoked, or expired before this repository observation"
                        .into(),
                );
            } else if capture_tool(&bound.profile).is_none() {
                observation.unavailable =
                    Some("The approved Agent profile does not permit repository capture".into());
            } else if usage.invocations >= grant.policy.limits.invocations {
                observation.unavailable =
                    Some("The approved invocation allowance is exhausted".into());
            }
            // An activation whose writes are limited keeps its complete Before
            // manifest in the sandbox; its After capture compares with it.
            let restricted = state
                .admitted_write_scope(activation)
                .map_or(true, |scope| !scope.is_unrestricted());
            let mode = match phase {
                RepositorySnapshotPhase::Before if restricted => CaptureMode::Keep,
                RepositorySnapshotPhase::After if restricted => state
                    .content
                    .repository_snapshots(&snapshot, activation)
                    .map_err(error)?
                    .into_iter()
                    .find(|capture| capture.content.phase == RepositorySnapshotPhase::Before)
                    .and_then(|before| {
                        Some(CaptureMode::Compare {
                            baseline: before.content.baseline?,
                            expected: before.content.judged_sha256?,
                        })
                    })
                    .unwrap_or(CaptureMode::Observe),
                _ => CaptureMode::Observe,
            };
            (bound, observation, mode)
        };
        if observation.unavailable.is_none() {
            let group = match phase {
                RepositorySnapshotPhase::Before => u64::MAX - 1,
                RepositorySnapshotPhase::After => u64::MAX,
                RepositorySnapshotPhase::BeforeCheck { index } => {
                    u64::MAX - 2 - u64::from(index) * 3
                }
                RepositorySnapshotPhase::AfterCheck { index } => {
                    u64::MAX - 4 - u64::from(index) * 3
                }
            };
            let request = ToolInvocationRequest {
                actor_id: bound.actor_id.clone(),
                provider_id: bound.profile.provider.clone(),
                model_id: bound.profile.model.clone(),
                provider_response_group: group,
                provider_call_index: 0,
                provider_call_count: 1,
                tool_call: axocoatl_llm::ToolCall {
                    id: format!("repository-{phase:?}"),
                    name: capture_tool(&bound.profile)
                        .ok_or_else(|| error("Repository capture has no admitted tool"))?
                        .into(),
                    arguments: serde_json::json!({"command": capture_command(&mode)}),
                    provider_metadata: Default::default(),
                },
            };
            let invocation = host_invocation_id(activation, group)?;
            let admitted = self.admit_invocation(activation, &request)?;
            let repository = admitted
                .repository
                .as_ref()
                .ok_or_else(|| error("Repository capture has no owned invocation executor"))?;
            let returned = repository
                .capture_snapshot(&capture_command(&mode))
                .await
                .map_err(|error| error.to_string());
            Box::new(admitted)
                .record_outcome(&ToolInvocationOutcome::Returned(returned.clone()))
                .await
                .map_err(error)?;
            observation.invocation = Some(invocation.clone());
            {
                let state = self.lock()?;
                let snapshot = state.canonical.snapshot(&state.turn_id).map_err(error)?;
                if let Some(item) = snapshot
                    .contract()
                    .invocations()
                    .iter()
                    .find(|item| item.invocation_id == invocation)
                {
                    if let InvocationEvidence::Outcome { evidence, .. } = &item.evidence {
                        observation.outcome = Some(evidence.clone());
                    }
                }
            }
            match returned {
                Ok(value) => {
                    if let Err(failure) = parse_capture(&value, &mut observation, &mode) {
                        observation.unavailable = Some(failure.to_string());
                    }
                }
                Err(failure) => observation.unavailable = Some(failure),
            }
        }
        if let Some(reason) = &mut observation.unavailable {
            observation.tree_sha256 = None;
            observation.judged_sha256 = None;
            observation.baseline = None;
            observation.compared = None;
            let mut end = reason.len().min(4096);
            while !reason.is_char_boundary(end) {
                end -= 1;
            }
            reason.truncate(end);
        }
        let mut state = self.lock()?;
        let snapshot = state.canonical.snapshot(&state.turn_id).map_err(error)?;
        let result = state
            .content
            .retain_repository_snapshot(&snapshot, observation)
            .map(|receipt| Some(receipt.reference().clone()))
            .map_err(error);
        state.fail_closed(result)
    }

    /// Why an Agent's tool call must be declined before admission, if it
    /// would spend invocations the host holds back (see
    /// `DispatchState::host_observation_shortfall`). `earlier` calls of the
    /// same provider response already passed and will spend one each.
    pub(crate) fn host_observation_reserve_refusal(
        &self,
        activation: &ActivationRef,
        request: &ToolInvocationRequest,
        earlier: u32,
    ) -> Option<String> {
        if is_host_observation(request.provider_response_group) {
            return None;
        }
        let state = self.lock().ok()?;
        state
            .host_observation_shortfall(activation, TOOL_CALL_NEEDS.saturating_add(earlier))
            .map(reserve_message)
    }

    /// Digests of every cited path, read in fixed observations of at most
    /// `MAX_DIGEST_PATHS` files each (at most four). Paths the host cannot read
    /// as regular repository files are absent.
    pub(crate) async fn observe_cited_digests(
        &self,
        activation: &ActivationRef,
        paths: &[String],
    ) -> Result<std::collections::BTreeMap<String, String>> {
        let unique: Vec<String> = paths
            .iter()
            .filter(|path| digest_path_ok(path))
            .cloned()
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect();
        let mut digests = std::collections::BTreeMap::new();
        for chunk in unique.chunks(MAX_DIGEST_PATHS).take(4) {
            digests.extend(self.observe_file_digests(activation, chunk).await?);
        }
        Ok(digests)
    }

    /// SHA-256 of regular repository files the host reads itself. One fixed, read-only
    /// command per observation, admitted as a host observation on the
    /// activation's own grant and recorded like any invocation. A path that
    /// is missing, not a regular file, or reached through a symbolic link is
    /// absent from the result. Refused when the invocation reserve could not
    /// spare it.
    pub(crate) async fn observe_file_digests(
        &self,
        activation: &ActivationRef,
        paths: &[String],
    ) -> Result<std::collections::BTreeMap<String, String>> {
        let paths: Vec<&str> = paths
            .iter()
            .map(String::as_str)
            .filter(|path| digest_path_ok(path))
            .take(MAX_DIGEST_PATHS)
            .collect();
        if paths.is_empty() {
            return Ok(Default::default());
        }
        let command = digest_command(&paths);
        let (bound, group) = {
            let state = self.lock()?;
            let bound = state
                .bound
                .get(&activation.activation_id)
                .filter(|bound| bound.activation == *activation)
                .cloned()
                .ok_or_else(|| error("digest observation has no exact live activation"))?;
            if bound.repository.is_none() || !bound.profile.tools.iter().any(|tool| tool == "bash")
            {
                return Err(error("this Agent cannot run repository observations"));
            }
            if state
                .host_observation_shortfall(activation, TOOL_CALL_NEEDS)
                .is_some()
            {
                return Err(error(
                    "the invocation allowance cannot spare a digest observation",
                ));
            }
            let snapshot = state.canonical.snapshot(&state.turn_id).map_err(error)?;
            let used: std::collections::HashSet<&InvocationId> = snapshot
                .contract()
                .invocations()
                .iter()
                .filter(|item| item.activation == *activation)
                .map(|item| &item.invocation_id)
                .collect();
            let mut group = None;
            for offset in 0..MAX_DIGEST_OBSERVATIONS {
                let candidate = DIGEST_GROUP_BASE + offset;
                if !used.contains(&host_invocation_id(activation, candidate)?) {
                    group = Some(candidate);
                    break;
                }
            }
            let group =
                group.ok_or_else(|| error("this activation used every digest observation"))?;
            (bound, group)
        };
        let request = ToolInvocationRequest {
            actor_id: bound.actor_id.clone(),
            provider_id: bound.profile.provider.clone(),
            model_id: bound.profile.model.clone(),
            provider_response_group: group,
            provider_call_index: 0,
            provider_call_count: 1,
            tool_call: axocoatl_llm::ToolCall {
                id: format!("file-digests-{}", group - DIGEST_GROUP_BASE),
                name: "bash".into(),
                arguments: serde_json::json!({"command": command}),
                provider_metadata: Default::default(),
            },
        };
        let admitted = self.admit_invocation(activation, &request)?;
        let repository = admitted
            .repository
            .as_ref()
            .ok_or_else(|| error("Digest observation has no owned invocation executor"))?;
        let returned = repository
            .observe_file_digests(&command)
            .await
            .map_err(|error| error.to_string());
        Box::new(admitted)
            .record_outcome(&ToolInvocationOutcome::Returned(returned.clone()))
            .await
            .map_err(error)?;
        let value = returned.map_err(error)?;
        if value.get("exit_code").and_then(serde_json::Value::as_i64) != Some(0) {
            return Err(error("digest observation did not complete"));
        }
        let stdout = value
            .get("stdout")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| error("digest observation returned no bytes"))?;
        Ok(parse_digests(stdout, &paths))
    }

    /// An activation with a write scope may change only those paths. File
    /// tools refuse other paths before any effect, but a write through a
    /// hard link, or to a path swapped while it runs, can still reach another
    /// file, and a shell can write anywhere, so the exact Before and After
    /// captures of every such activation decide. A read-only activation
    /// without a shell is the exception: it can write nothing. Returns why
    /// the activation must not be accepted, or `None` when every change stayed
    /// in scope. A scope that cannot be read is itself a reason. The captures
    /// compare complete manifests, whatever the index's flags; files the
    /// repository's ignore rules exclude are not judged, but the ignore files
    /// Git reads, and Git's own settings, hooks and exclude files, are.
    pub(crate) fn write_scope_violation(
        &self,
        activation: &ActivationRef,
    ) -> Result<Option<String>> {
        let state = self.lock()?;
        let admitted = state
            .authority
            .activation_profile(activation)
            .map_err(error)
            .and_then(|profile| {
                Ok((
                    profile.tools.iter().any(|tool| tool == "bash"),
                    state.admitted_write_scope(activation)?,
                ))
            });
        let Ok((shell, scope)) = admitted else {
            return Ok(Some(
                "its admitted write scope cannot be read, so its changes cannot be judged; any \
                 change is kept for review"
                    .into(),
            ));
        };
        if scope.is_unrestricted() || (scope.is_read_only() && !shell) {
            return Ok(None);
        }
        let snapshot = state.canonical.snapshot(&state.turn_id).map_err(error)?;
        let captures = state
            .content
            .repository_snapshots(&snapshot, activation)
            .map_err(error)?;
        let Some(changed) = judged_changed_paths(&captures) else {
            return Ok(Some(format!(
                "its repository captures cannot establish which files it changed, so changes \
                 outside the paths this Agent may change ({}) cannot be ruled out; any change \
                 is kept for review",
                scope.describe()
            )));
        };
        let outside: Vec<String> = changed
            .into_iter()
            .filter(|path| !scope.allows_change(path))
            .collect();
        Ok((!outside.is_empty()).then(|| {
            format!(
                "it changed {} outside the paths this Agent may change ({}); the change is kept \
                 for review",
                outside.join(", "),
                scope.describe()
            )
        }))
    }
}

/// The tool the host's repository captures of an activation run as: its own
/// `bash`, or, for an activation limited to named paths without a shell, the
/// host's capture port. `None` when the activation has neither.
pub(crate) fn capture_tool(profile: &ExecutionProfile) -> Option<&'static str> {
    if profile.tools.iter().any(|tool| tool == "bash") {
        Some("bash")
    } else if profile
        .write_scope
        .as_ref()
        .is_some_and(|scope| !scope.is_empty())
    {
        Some(axocoatl_session::control_authority::REPOSITORY_CAPTURE_PORT)
    } else {
        None
    }
}

/// Host observation groups for file digests: below every capture and check
/// group, inside the host range (`is_host_observation`).
const DIGEST_GROUP_BASE: u64 = u64::MAX - 4096;
const MAX_DIGEST_OBSERVATIONS: u64 = 1024;

/// A digest observation serves an Agent's finding, so unlike the host's own
/// captures and checks it must leave the Agent's reserve intact.
pub(crate) fn is_digest_group(group: u64) -> bool {
    (DIGEST_GROUP_BASE..DIGEST_GROUP_BASE + MAX_DIGEST_OBSERVATIONS).contains(&group)
}
const MAX_DIGEST_PATHS: usize = 32;

/// The fixed digest command around its base64 path list. Paths are data
/// decoded inside the command, never shell source. A path counts only when
/// its resolved location is exactly `<root>/<path>` (no symbolic link on the
/// way, nothing outside the repository), it is a regular file and it can be
/// read; any other path is left out without failing the rest. `./` keeps a
/// leading `-` from being read as an option by GNU, BusyBox or BSD realpath.
const DIGEST_HEAD: &str = "set -eu\nexport LC_ALL=C\nroot=$(pwd -P)\nprintf '%s' '";
const DIGEST_TAIL: &str = "' | base64 -d | while IFS= read -r path; do\n  \
    real=$(realpath \"./$path\" 2>/dev/null || true)\n  \
    if [ \"$real\" = \"$root/$path\" ] && [ -f \"$real\" ] && [ ! -L \"$real\" ]; then\n    \
    digest=$(sha256sum 2>/dev/null < \"$real\") || continue\n    \
    encoded=$(printf %s \"$path\" | base64 | tr -d '\\n')\n    \
    printf '%s\\t%s\\n' \"$encoded\" \"${digest%% *}\"\n  \
    fi\ndone\n";

/// A repository-relative path the digest command may be given: no empty,
/// `.` or `..` component, not absolute, no line breaks or NUL.
pub(crate) fn digest_path_ok(path: &str) -> bool {
    !path.is_empty()
        && path.len() <= 1024
        && !path.starts_with('/')
        && !path.contains(['\n', '\r', '\0'])
        && path
            .split('/')
            .all(|part| !part.is_empty() && part != "." && part != "..")
}

pub(super) fn digest_command(paths: &[&str]) -> String {
    let list: String = paths.iter().map(|path| format!("{path}\n")).collect();
    format!(
        "{DIGEST_HEAD}{}{DIGEST_TAIL}",
        base64::engine::general_purpose::STANDARD.encode(list)
    )
}

/// Only the exact fixed command with a well-formed path list is accepted.
pub(super) fn is_digest_command(command: &str) -> bool {
    let Some(encoded) = command
        .strip_prefix(DIGEST_HEAD)
        .and_then(|rest| rest.strip_suffix(DIGEST_TAIL))
    else {
        return false;
    };
    if !encoded
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'/' | b'='))
    {
        return false;
    }
    let Ok(list) = base64::engine::general_purpose::STANDARD.decode(encoded) else {
        return false;
    };
    let Ok(list) = String::from_utf8(list) else {
        return false;
    };
    let paths: Vec<&str> = list.strip_suffix('\n').unwrap_or("").split('\n').collect();
    !paths.is_empty()
        && paths.len() <= MAX_DIGEST_PATHS
        && paths.iter().all(|path| digest_path_ok(path))
        && digest_command(&paths) == command
}

/// The invocation identity a host observation in `group` receives.
fn host_invocation_id(activation: &ActivationRef, group: u64) -> Result<InvocationId> {
    InvocationId::new(format!(
        "tool-{:x}",
        Sha256::digest(serde_json::to_vec(&(activation, group, 0usize)).map_err(error)?)
    ))
    .map_err(error)
}

/// `base64(path)\tsha256` lines for requested paths only.
fn parse_digests(stdout: &str, requested: &[&str]) -> std::collections::BTreeMap<String, String> {
    let mut digests = std::collections::BTreeMap::new();
    for line in stdout.lines() {
        let Some((encoded, digest)) = line.split_once('\t') else {
            continue;
        };
        let Some(path) = base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .ok()
            .and_then(|bytes| String::from_utf8(bytes).ok())
        else {
            continue;
        };
        if requested.contains(&path.as_str())
            && digest.len() == 64
            && digest.bytes().all(|byte| byte.is_ascii_hexdigit())
        {
            digests.insert(path, digest.to_ascii_lowercase());
        }
    }
    digests
}

/// Invocations an Agent's tool call must leave room for: the call itself, the
/// provider call that reads its result, and one more provider call so that a
/// model whose next tool round is declined can still answer.
pub(crate) const TOOL_CALL_NEEDS: u32 = 3;

/// The message an Agent sees when its remaining allowance is held back.
pub(crate) fn reserve_message(reserve: u32) -> String {
    format!(
        "The invocation allowance is nearly spent: {reserve} remaining invocation(s) are held \
         for the host to observe your changes and run required checks. Do not call any more \
         tools; write your final answer now."
    )
}

impl DispatchState {
    /// Invocations the host holds back for this activation: its After
    /// capture and, when required checks exist and this grant is the one that
    /// pays for them, one shared Before capture, each check, and one shared
    /// After capture. Returns the reserve when spending `needed` more
    /// invocations would cut into it.
    pub(crate) fn host_observation_shortfall(
        &self,
        activation: &ActivationRef,
        needed: u32,
    ) -> Option<u32> {
        let bound = self
            .bound
            .get(&activation.activation_id)
            .filter(|bound| bound.activation == *activation)?;
        if bound.repository.is_none() || capture_tool(&bound.profile).is_none() {
            return None;
        }
        let grant = bound.grant.grant_id.as_str();
        let pays_checks = self.authority.grant_pays_standing_checks(grant).ok()?
            || self.authority.grant_pays_required_checks(grant).ok()?;
        let snapshot = self.canonical.snapshot(&self.turn_id).ok()?;
        let group = snapshot
            .contract()
            .graph()
            .and_then(axocoatl_session::turn_checks::group_of);
        let checks = paid_checks(pays_checks, group.map(|(_, checks)| checks));
        let limit = self
            .authority
            .grant_status(grant)
            .ok()?
            .policy
            .limits
            .invocations;
        let used = self.authority.usage(grant).ok()?.invocations;
        reserve_shortfall(used, needed, host_reserve(checks), limit)
    }
}

/// How many of the turn's `checks` commands a grant pays for: all of them
/// when it is the paying grant, else none.
fn paid_checks(pays: bool, checks: Option<usize>) -> u32 {
    match checks {
        Some(checks) if pays => u32::try_from(checks).unwrap_or(u32::MAX),
        _ => 0,
    }
}

/// The After capture, plus a shared Before capture, each check and a shared
/// After capture when `checks` required checks are paid from this grant.
fn host_reserve(checks: u32) -> u32 {
    let condition_runs = if checks == 0 {
        0
    } else {
        checks.saturating_add(2)
    };
    condition_runs.saturating_add(1)
}

/// The reserve, when spending `needed` more of `limit` would cut into it.
fn reserve_shortfall(used: u32, needed: u32, reserve: u32, limit: u32) -> Option<u32> {
    (used.saturating_add(needed).saturating_add(reserve) > limit).then_some(reserve)
}

#[cfg(test)]
mod digest_tests {
    use super::*;

    #[test]
    fn only_the_fixed_command_with_safe_paths_is_accepted() {
        let command = digest_command(&["lib/a.js", "src/deep/b c.rs"]);
        assert!(is_digest_command(&command));
        assert!(!is_digest_command(&command.replace("sha256sum", "cat")));
        assert!(!is_digest_command(&format!("{command}; rm -rf .")));
        assert!(!is_digest_command(super::CAPTURE));
        for bad in [
            "",
            "/etc/passwd",
            "../x",
            "a/../b",
            "a//b",
            "./a",
            "a\nb",
            "a\0b",
        ] {
            assert!(!digest_path_ok(bad), "{bad:?}");
            if bad.contains('\n') {
                // In the list a line break separates two paths, each checked.
                continue;
            }
            let smuggled = format!(
                "{DIGEST_HEAD}{}{DIGEST_TAIL}",
                base64::engine::general_purpose::STANDARD.encode(format!("{bad}\n"))
            );
            assert!(!is_digest_command(&smuggled), "{bad:?}");
        }
        let too_many: Vec<String> = (0..=MAX_DIGEST_PATHS).map(|n| format!("f{n}")).collect();
        let too_many: Vec<&str> = too_many.iter().map(String::as_str).collect();
        assert!(!is_digest_command(&digest_command(&too_many)));
    }

    /// Runs the exact command against a repository with every kind of path it
    /// must refuse.
    #[cfg(unix)]
    #[test]
    fn the_fixed_command_reads_only_regular_files_inside_the_repository() {
        use std::os::unix::fs::{symlink, PermissionsExt};
        let tools = ["sha256sum", "realpath", "base64", "mkfifo"];
        if tools.iter().any(|tool| {
            std::process::Command::new("sh")
                .args(["-c", &format!("command -v {tool}")])
                .output()
                .map(|out| !out.status.success())
                .unwrap_or(true)
        }) {
            eprintln!("skipping: shell tools unavailable");
            return;
        }
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("x.js"), "secret").unwrap();
        let repo = tempfile::tempdir().unwrap();
        let root = repo.path().canonicalize().unwrap();
        std::fs::create_dir_all(root.join("lib")).unwrap();
        std::fs::write(root.join("lib/a.js"), "hello\n").unwrap();
        std::fs::write(root.join("-dash.js"), "dash\n").unwrap();
        std::fs::write(root.join("locked.js"), "locked\n").unwrap();
        std::fs::set_permissions(
            root.join("locked.js"),
            std::fs::Permissions::from_mode(0o000),
        )
        .unwrap();
        symlink("a.js", root.join("lib/link.js")).unwrap();
        symlink(outside.path(), root.join("dirlink")).unwrap();
        symlink("/etc/hosts", root.join("hosts")).unwrap();
        assert!(std::process::Command::new("mkfifo")
            .arg(root.join("pipe"))
            .status()
            .unwrap()
            .success());
        let paths = [
            "lib/a.js",
            "-dash.js",
            "locked.js",
            "lib/link.js",
            "dirlink/x.js",
            "hosts",
            "pipe",
            "lib/missing.js",
            "lib",
        ];
        let output = std::process::Command::new("sh")
            .args(["-c", &digest_command(&paths)])
            .current_dir(&root)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let digests = parse_digests(&String::from_utf8(output.stdout).unwrap(), &paths);
        let sha = |text: &str| format!("{:x}", Sha256::digest(text.as_bytes()));
        let root_user = std::process::Command::new("id")
            .arg("-u")
            .output()
            .map(|out| String::from_utf8_lossy(&out.stdout).trim() == "0")
            .unwrap_or(false);
        let mut expected = std::collections::BTreeMap::from([
            ("lib/a.js".to_string(), sha("hello\n")),
            ("-dash.js".to_string(), sha("dash\n")),
        ]);
        if root_user {
            // Root can read a mode-000 file; it is still a regular file inside.
            expected.insert("locked.js".into(), sha("locked\n"));
        }
        assert_eq!(digests, expected);
    }

    #[test]
    fn digests_are_read_only_for_requested_paths() {
        let line = |path: &str, digest: &str| {
            format!(
                "{}\t{digest}\n",
                base64::engine::general_purpose::STANDARD.encode(path)
            )
        };
        let stdout = [
            line("lib/a.js", &"A".repeat(64)),
            line("lib/other.js", &"b".repeat(64)),
            line("lib/b.js", "missing"),
        ]
        .concat();
        let parsed = parse_digests(&stdout, &["lib/a.js", "lib/b.js"]);
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed["lib/a.js"], "a".repeat(64));
    }

    #[test]
    fn digest_groups_are_host_observations_below_the_capture_groups() {
        assert!(is_host_observation(DIGEST_GROUP_BASE));
        assert!(is_host_observation(
            DIGEST_GROUP_BASE + MAX_DIGEST_OBSERVATIONS - 1
        ));
        // Check groups count down from u64::MAX - 2 in steps of 3.
        const { assert!(DIGEST_GROUP_BASE + MAX_DIGEST_OBSERVATIONS < u64::MAX - 2 - 3 * 1000) };
    }
}

#[cfg(test)]
mod reserve_tests {
    use super::*;

    #[test]
    fn a_tight_allowance_still_lets_the_model_answer_a_declined_round() {
        // Ten invocations, one required check: the host keeps 4 (After, and
        // Before + check + After for the check run).
        let (limit, reserve) = (10, host_reserve(1));
        assert_eq!(reserve, 4);
        assert_eq!(host_reserve(0), 1);
        let provider = |used| reserve_shortfall(used, 1, reserve, limit).is_none();
        let tool = |used, earlier| {
            reserve_shortfall(used, TOOL_CALL_NEEDS + earlier, reserve, limit).is_none()
        };
        assert!(provider(0));
        // After one provider call, each call of one response counts the
        // earlier ones: three fit, a fourth would eat the reserve.
        assert!(tool(1, 0));
        assert!(tool(1, 2));
        assert!(
            !tool(1, 3),
            "a fourth call of the same response is declined"
        );
        // Three tools admitted, then the provider call that reads them.
        assert!(provider(4));
        // The model asks for more tools: declined, and the provider call that
        // lets it answer is still admissible.
        assert!(!tool(5, 0));
        assert!(provider(5));
        // Only a model that ignores the refusal again runs out.
        assert!(!provider(6));
    }

    /// Every activation limited to named paths is captured, with its own
    /// shell or through the host's port; one that can write nothing, or may
    /// write anything, without a shell is not.
    #[test]
    fn captures_run_for_every_path_scoped_writer() {
        let profile = |tools: &[&str], scope: Option<&[&str]>| ExecutionProfile {
            definition: "d".into(),
            provider: "p".into(),
            model: "m".into(),
            isolation: "in-process".into(),
            tools: tools.iter().map(|tool| (*tool).into()).collect(),
            write_scope: scope.map(|scope| scope.iter().map(|path| (*path).into()).collect()),
        };
        let port = axocoatl_session::control_authority::REPOSITORY_CAPTURE_PORT;
        assert_eq!(capture_tool(&profile(&["bash"], None)), Some("bash"));
        assert_eq!(capture_tool(&profile(&["bash"], Some(&[]))), Some("bash"));
        assert_eq!(
            capture_tool(&profile(&["write_file"], Some(&["lib/"]))),
            Some(port)
        );
        assert_eq!(capture_tool(&profile(&["write_file"], Some(&[]))), None);
        assert_eq!(capture_tool(&profile(&["write_file"], None)), None);
    }

    #[test]
    fn required_checks_reserve_only_on_the_paying_grant() {
        // Two required checks: the paying grant holds back its After capture,
        // both captures around the checks and each check.
        assert_eq!(host_reserve(paid_checks(true, Some(2))), 5);
        // Every other grant of the turn holds back only its own After capture.
        assert_eq!(host_reserve(paid_checks(false, Some(2))), 1);
        // Without checks the payer holds back nothing more either.
        assert_eq!(host_reserve(paid_checks(true, None)), 1);
        // With ten invocations and two checks, the payer's Agent may start a
        // tool call only while three plus five still fit; any other Agent's
        // grant is unaffected by the checks.
        let reserve = host_reserve(paid_checks(true, Some(2)));
        assert!(reserve_shortfall(2, TOOL_CALL_NEEDS, reserve, 10).is_none());
        assert_eq!(
            reserve_shortfall(3, TOOL_CALL_NEEDS, reserve, 10),
            Some(reserve)
        );
        let other = host_reserve(paid_checks(false, Some(2)));
        assert!(reserve_shortfall(6, TOOL_CALL_NEEDS, other, 10).is_none());
    }
}

/// Provider response groups at the top of the range are the host's own
/// repository captures and check observations, never an Agent's tool round.
pub(crate) fn is_host_observation(group: u64) -> bool {
    group >= u64::MAX - 4096
}

/// Every repository path one activation changed, as its After capture
/// listed them after verifying the complete Before manifest kept in the
/// sandbox against the digest the host recorded. `None` when its captures
/// cannot establish that list, so a write scope is never judged on less.
/// Beside the files it names ignore files Git reads and paths inside `.git`.
pub(crate) fn judged_changed_paths(
    captures: &[ActivationRepositorySnapshotView],
) -> Option<Vec<String>> {
    let usable = |phase: RepositorySnapshotPhase| {
        captures
            .iter()
            .map(|capture| &capture.content)
            .find(|capture| capture.phase == phase)
            .filter(|capture| capture.unavailable.is_none() && capture.tree_sha256.is_some())
    };
    let before = usable(RepositorySnapshotPhase::Before)?
        .judged_sha256
        .as_ref()?;
    let after = usable(RepositorySnapshotPhase::After)?;
    after.judged_sha256.as_ref()?;
    let compared = after.compared.as_ref()?;
    (compared.before_sha256 == *before).then(|| compared.changed_paths.clone())
}

/// Repository paths one activation changed, from its own Before and After
/// captures, or `None` when they cannot establish it. This serves reports of
/// what changed; a write scope is judged by [`judged_changed_paths`]. An unchanged tree digest
/// proves no change; complete manifests are compared exactly; otherwise the
/// retained patches against the same HEAD are compared file by file. Ignored
/// files are not captured and never appear.
pub(crate) fn activation_changed_paths(
    captures: &[ActivationRepositorySnapshotView],
) -> Option<Vec<String>> {
    let usable = |phase: RepositorySnapshotPhase| {
        captures
            .iter()
            .map(|capture| &capture.content)
            .find(|capture| capture.phase == phase)
            .filter(|capture| capture.unavailable.is_none() && capture.tree_sha256.is_some())
    };
    let before = usable(RepositorySnapshotPhase::Before)?;
    let after = usable(RepositorySnapshotPhase::After)?;
    if before.tree_sha256 == after.tree_sha256 {
        return Some(Vec::new());
    }
    if before.manifest_complete && after.manifest_complete {
        return Some(manifest_changes(
            &before.manifest_prefix,
            &after.manifest_prefix,
        ));
    }
    if before.head != after.head || !before.patch_complete || !after.patch_complete {
        return None;
    }
    let patch = |capture: &ActivationRepositorySnapshot| {
        base64::engine::general_purpose::STANDARD
            .decode(capture.patch_base64.as_deref()?)
            .ok()
    };
    let (before, after) = (
        patch_sections(&patch(before)?)?,
        patch_sections(&patch(after)?)?,
    );
    let mut changed: Vec<String> = before
        .iter()
        .filter(|(path, section)| after.get(*path) != Some(section))
        .map(|(path, _)| path.clone())
        .chain(
            after
                .keys()
                .filter(|path| !before.contains_key(*path))
                .cloned(),
        )
        .collect();
    changed.sort();
    changed.dedup();
    Some(changed)
}

/// Each path's sections of a `git diff --binary` patch. `None` for a patch
/// whose file headers cannot be read exactly (for example quoted paths).
fn patch_sections(patch: &[u8]) -> Option<std::collections::BTreeMap<String, Vec<u8>>> {
    let mut sections = std::collections::BTreeMap::<String, Vec<u8>>::new();
    let mut current: Option<(Vec<String>, Vec<u8>)> = None;
    let mut flush = |current: Option<(Vec<String>, Vec<u8>)>| {
        if let Some((paths, body)) = current {
            for path in paths {
                sections.entry(path).or_default().extend_from_slice(&body);
            }
        }
    };
    for line in patch.split_inclusive(|byte| *byte == b'\n') {
        if let Some(header) = line.strip_prefix(b"diff --git ") {
            flush(current.take());
            let header = std::str::from_utf8(header).ok()?.trim_end_matches('\n');
            current = Some((patch_header_paths(header)?, line.to_vec()));
        } else if let Some((_, body)) = current.as_mut() {
            body.extend_from_slice(line);
        } else if !line.iter().all(u8::is_ascii_whitespace) {
            return None;
        }
    }
    flush(current);
    Some(sections)
}

/// Paths named by `a/<path> b/<path>`; both sides of a rename or copy.
fn patch_header_paths(header: &str) -> Option<Vec<String>> {
    if header.contains('"') {
        return None;
    }
    let rest = header.strip_prefix("a/")?;
    let splits: Vec<usize> = rest.match_indices(" b/").map(|(index, _)| index).collect();
    if let Some(index) = splits
        .iter()
        .find(|index| rest[..**index] == rest[**index + 3..])
    {
        return Some(vec![rest[..*index].to_owned()]);
    }
    match splits.as_slice() {
        [index] => Some(vec![
            rest[..*index].to_owned(),
            rest[*index + 3..].to_owned(),
        ]),
        _ => None,
    }
}

/// Paths whose mode, kind or digest differ between two capture manifests of
/// `base64(path)\tmode\tkind\tsha256` lines, including added and removed paths.
fn manifest_changes(before: &str, after: &str) -> Vec<String> {
    let entries = |manifest: &str| {
        manifest
            .lines()
            .filter_map(|line| {
                let (encoded, entry) = line.split_once('\t')?;
                let path = base64::engine::general_purpose::STANDARD
                    .decode(encoded)
                    .ok()?;
                Some((
                    String::from_utf8_lossy(&path).into_owned(),
                    entry.to_string(),
                ))
            })
            .collect::<std::collections::BTreeMap<_, _>>()
    };
    let (before, after) = (entries(before), entries(after));
    let mut changed: Vec<String> = before
        .iter()
        .filter(|(path, entry)| after.get(*path) != Some(entry))
        .map(|(path, _)| path.clone())
        .chain(
            after
                .keys()
                .filter(|path| !before.contains_key(*path))
                .cloned(),
        )
        .collect();
    changed.sort();
    changed
}

pub(super) fn parse_capture(
    value: &serde_json::Value,
    observation: &mut ActivationRepositorySnapshot,
    mode: &CaptureMode,
) -> Result<()> {
    if value.get("exit_code").and_then(serde_json::Value::as_i64) != Some(0)
        || value
            .get("stdout_truncated")
            .and_then(serde_json::Value::as_bool)
            != Some(false)
    {
        return Err(error(
            "Repository capture failed or its result was truncated; the candidate is not verified",
        ));
    }
    let stdout = value
        .get("stdout")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| error("Repository capture returned no bytes"))?;
    let mut fields = HashMap::new();
    for line in stdout.lines() {
        let (key, value) = line
            .split_once('=')
            .ok_or_else(|| error("Invalid repository capture record"))?;
        if fields.insert(key, value).is_some() {
            return Err(error("Duplicate repository capture field"));
        }
    }
    let field = |name| {
        fields
            .get(name)
            .copied()
            .ok_or_else(|| error("Incomplete repository capture"))
    };
    let judgement_fields = match mode {
        CaptureMode::Observe => 0,
        CaptureMode::Keep => 2,
        CaptureMode::Compare { .. } => 3,
    };
    if field("format")? != "1" || fields.len() != 8 + judgement_fields {
        return Err(error("Unsupported repository capture"));
    }
    let head = field("head")?;
    if head != "unborn"
        && !((head.len() == 40 || head.len() == 64)
            && head.bytes().all(|byte| byte.is_ascii_hexdigit()))
    {
        return Err(error("Invalid captured repository HEAD"));
    }
    let manifest = base64::engine::general_purpose::STANDARD
        .decode(field("manifest_b64")?)
        .map_err(error)?;
    let patch = base64::engine::general_purpose::STANDARD
        .decode(field("patch_b64")?)
        .map_err(error)?;
    if manifest.len() > 8192 || patch.len() > 512 * 1024 {
        return Err(error("Repository capture exceeds its declared bounds"));
    }
    observation.head = (head != "unborn").then(|| head.to_owned());
    observation.tree_sha256 = Some(field("tree")?.to_owned());
    observation.manifest_bytes = field("manifest_bytes")?.parse().map_err(error)?;
    observation.manifest_complete = observation.manifest_bytes == manifest.len() as u64;
    observation.manifest_prefix = String::from_utf8(manifest).map_err(error)?;
    observation.patch_sha256 = Some(field("patch_sha256")?.to_owned());
    observation.patch_bytes = field("patch_bytes")?.parse().map_err(error)?;
    observation.patch_complete = observation.patch_bytes == patch.len() as u64;
    observation.patch_base64 = Some(base64::engine::general_purpose::STANDARD.encode(&patch));
    observation.patch_prefix =
        String::from_utf8_lossy(&patch[..patch.len().min(12288)]).into_owned();
    let mut prefix_end = observation.patch_prefix.len().min(12288);
    while !observation.patch_prefix.is_char_boundary(prefix_end) {
        prefix_end -= 1;
    }
    observation.patch_prefix.truncate(prefix_end);
    if observation.manifest_complete
        && observation.tree_sha256.as_deref()
            != Some(
                format!(
                    "{:x}",
                    Sha256::digest(observation.manifest_prefix.as_bytes())
                )
                .as_str(),
            )
    {
        return Err(error(
            "Captured tree digest differs from its exact manifest",
        ));
    }
    if observation.patch_complete
        && observation.patch_sha256.as_deref()
            != Some(format!("{:x}", Sha256::digest(&patch)).as_str())
    {
        return Err(error("Captured patch digest differs from its exact bytes"));
    }
    if matches!(mode, CaptureMode::Observe) {
        return Ok(());
    }
    let judged = field("judged")?;
    if !is_digest(judged) {
        return Err(error("Invalid captured manifest digest"));
    }
    observation.judged_sha256 = Some(judged.to_owned());
    match mode {
        CaptureMode::Observe => {}
        CaptureMode::Keep => {
            let baseline = field("baseline")?;
            if !is_capture_baseline(baseline) {
                return Err(error("Invalid kept manifest location"));
            }
            observation.baseline = Some(baseline.to_owned());
        }
        CaptureMode::Compare { expected, .. } => {
            if field("compared")? != expected {
                return Err(error(
                    "Repository capture compared with another Before manifest",
                ));
            }
            let listed = field("changed")?;
            if listed.len() > MAX_COMPARED_PATH_BYTES * 2 {
                return Err(error("Repository capture changed too many paths to record"));
            }
            let mut changed_paths = Vec::new();
            for encoded in listed.split(',').filter(|encoded| !encoded.is_empty()) {
                let path = base64::engine::general_purpose::STANDARD
                    .decode(encoded)
                    .map_err(|_| error("Invalid changed repository path"))?;
                changed_paths.push(String::from_utf8_lossy(&path).into_owned());
            }
            changed_paths.sort();
            changed_paths.dedup();
            if changed_paths.iter().map(String::len).sum::<usize>() > MAX_COMPARED_PATH_BYTES {
                return Err(error("Repository capture changed too many paths to record"));
            }
            observation.compared = Some(RepositoryComparison {
                before_sha256: expected.clone(),
                changed_paths,
            });
        }
    }
    Ok(())
}

pub(super) fn empty_observation(
    activation: &ActivationRef,
    phase: RepositorySnapshotPhase,
    repository: &EvidenceRef,
) -> ActivationRepositorySnapshot {
    ActivationRepositorySnapshot {
        activation: activation.clone(),
        phase,
        repository: repository.clone(),
        invocation: None,
        condition_run: None,
        outcome: None,
        head: None,
        tree_sha256: None,
        manifest_bytes: 0,
        manifest_prefix: String::new(),
        manifest_complete: false,
        patch_sha256: None,
        patch_bytes: 0,
        patch_prefix: String::new(),
        patch_complete: false,
        patch_base64: None,
        unavailable: None,
        judged_sha256: None,
        baseline: None,
        compared: None,
    }
}

#[cfg(test)]
mod manifest_change_tests {
    use super::manifest_changes;
    use base64::Engine as _;

    fn line(path: &str, mode: &str, kind: &str, digest: &str) -> String {
        let encoded = base64::engine::general_purpose::STANDARD.encode(path);
        format!("{encoded}\t{mode}\t{kind}\t{digest}\n")
    }

    #[test]
    fn reports_changed_added_removed_and_mode_changes_only() {
        let a = "a".repeat(64);
        let b = "b".repeat(64);
        let before = [
            line("lib/manifest.js", "644", "file", &a),
            line("lib/paths.js", "644", "file", &a),
            line("run.sh", "644", "file", &a),
            line("gone.txt", "644", "file", &a),
        ]
        .concat();
        let after = [
            line("lib/manifest.js", "644", "file", &a),
            line("lib/paths.js", "644", "file", &b),
            line("run.sh", "755", "file", &a),
            line("new.txt", "644", "file", &a),
        ]
        .concat();
        assert_eq!(
            manifest_changes(&before, &after),
            ["gone.txt", "lib/paths.js", "new.txt", "run.sh"]
        );
        assert!(manifest_changes(&before, &before).is_empty());
    }

    /// The capture's patch: tracked changes against HEAD, then untracked files.
    fn capture_patch(root: &std::path::Path) -> Vec<u8> {
        let git = |args: &[&str]| {
            std::process::Command::new("git")
                .current_dir(root)
                .args(["-c", "core.fsmonitor=false"])
                .args(args)
                .output()
                .unwrap()
                .stdout
        };
        let mut patch = git(&[
            "diff",
            "--no-ext-diff",
            "--no-textconv",
            "--binary",
            "HEAD",
            "--",
        ]);
        let untracked = git(&["ls-files", "--others", "--exclude-standard", "-z"]);
        for path in untracked
            .split(|byte| *byte == 0)
            .filter(|path| !path.is_empty())
        {
            let path = std::str::from_utf8(path).unwrap();
            patch.extend(git(&[
                "diff",
                "--no-ext-diff",
                "--no-textconv",
                "--binary",
                "--no-index",
                "--",
                "/dev/null",
                path,
            ]));
        }
        patch
    }

    #[test]
    fn patch_sections_name_exactly_the_files_a_step_changed() {
        use super::patch_sections;
        let repo = tempfile::tempdir().unwrap();
        let root = repo.path();
        let run = |args: &[&str]| {
            assert!(std::process::Command::new("git")
                .current_dir(root)
                .args(args)
                .status()
                .unwrap()
                .success());
        };
        run(&["init", "-q"]);
        std::fs::create_dir_all(root.join("lib")).unwrap();
        std::fs::write(root.join("a.txt"), "a\n").unwrap();
        std::fs::write(root.join("lib/x.js"), "x\n").unwrap();
        std::fs::write(root.join("run.sh"), "echo\n").unwrap();
        run(&["add", "-A"]);
        run(&[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@example.invalid",
            "commit",
            "-qm",
            "base",
        ]);
        // Earlier work already changed a.txt; this step leaves it alone.
        std::fs::write(root.join("a.txt"), "a2\n").unwrap();
        let before = patch_sections(&capture_patch(root)).unwrap();
        std::fs::write(root.join("lib/x.js"), "x2\n").unwrap();
        std::fs::write(root.join("new file.txt"), "n\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(root.join("run.sh"), std::fs::Permissions::from_mode(0o755))
                .unwrap();
        }
        let after = patch_sections(&capture_patch(root)).unwrap();
        let mut changed: Vec<&String> = after
            .iter()
            .filter(|(path, section)| before.get(*path) != Some(section))
            .map(|(path, _)| path)
            .collect();
        changed.sort();
        let mut expected = vec!["lib/x.js", "new file.txt"];
        if cfg!(unix) {
            expected.push("run.sh");
        }
        expected.sort();
        assert_eq!(changed, expected);
        assert_eq!(before.get("a.txt"), after.get("a.txt"));
    }
}

#[cfg(test)]
mod judgement_tests {
    use super::*;

    fn digest(byte: char) -> String {
        byte.to_string().repeat(64)
    }

    #[test]
    fn only_exact_capture_commands_are_run() {
        let compare = CaptureMode::Compare {
            baseline: "/tmp/axocoatl-baseline.AbC123".into(),
            expected: digest('a'),
        };
        for mode in [CaptureMode::Observe, CaptureMode::Keep, compare] {
            assert_eq!(capture_command_mode(&capture_command(&mode)), Some(mode));
        }
        let script = REPOSITORY_CAPTURE_SCRIPT;
        for refused in [
            format!("capture_mode=keep\nrm -rf .\n{script}"),
            format!("capture_mode=compare\nbaseline=/tmp/axocoatl-baseline.x/../y\nexpected={}\n{script}", digest('a')),
            format!("capture_mode=compare\nbaseline=/home/agent/kept\nexpected={}\n{script}", digest('a')),
            format!("capture_mode=compare\nbaseline=/tmp/axocoatl-baseline.x\nexpected={}\n{script}", digest('A')),
            format!("capture_mode=compare\nbaseline=/tmp/axocoatl-baseline.x\nexpected=abc\n{script}"),
            format!("capture_mode=observe\n{script}; true"),
            script.to_owned(),
        ] {
            assert_eq!(capture_command_mode(&refused), None, "{refused:.80}");
        }
    }

    fn output(extra: &str) -> serde_json::Value {
        let manifest = "bGliL2E=\t644\tfile\t".to_owned() + &digest('b') + "\n";
        let tree = format!("{:x}", Sha256::digest(manifest.as_bytes()));
        let patch_sha = format!("{:x}", Sha256::digest(b""));
        let stdout = format!(
            "format=1\nhead=unborn\ntree={tree}\nmanifest_bytes={}\nmanifest_b64={}\n\
             patch_sha256={patch_sha}\npatch_bytes=0\npatch_b64=\n{extra}",
            manifest.len(),
            base64::engine::general_purpose::STANDARD.encode(&manifest),
        );
        serde_json::json!({"stdout": stdout, "exit_code": 0, "stdout_truncated": false})
    }

    fn observation() -> ActivationRepositorySnapshot {
        empty_observation(
            &ActivationRef {
                session_id: SessionId::new("s").unwrap(),
                turn_id: LogicalTurnId::new("t").unwrap(),
                execution_epoch_id: ExecutionEpochId::new("e").unwrap(),
                node_id: TurnNodeId::new("n").unwrap(),
                generation: 1,
                activation_id: ActivationId::new("a").unwrap(),
            },
            RepositorySnapshotPhase::After,
            &EvidenceRef::new("repository").unwrap(),
        )
    }

    #[test]
    fn judgement_fields_are_read_only_in_the_mode_that_asked_for_them() {
        let keep = format!(
            "judged={}\nbaseline=/tmp/axocoatl-baseline.Qx12ab\n",
            digest('c')
        );
        let mut kept = observation();
        parse_capture(&output(&keep), &mut kept, &CaptureMode::Keep).unwrap();
        assert_eq!(kept.judged_sha256, Some(digest('c')));
        assert_eq!(
            kept.baseline.as_deref(),
            Some("/tmp/axocoatl-baseline.Qx12ab")
        );
        // An observation that reports more than it was asked for is refused.
        assert!(parse_capture(&output(&keep), &mut observation(), &CaptureMode::Observe).is_err());
        let bad = format!("judged={}\nbaseline=/home/agent\n", digest('c'));
        assert!(parse_capture(&output(&bad), &mut observation(), &CaptureMode::Keep).is_err());

        let expected = digest('d');
        let compare = CaptureMode::Compare {
            baseline: "/tmp/axocoatl-baseline.Qx12ab".into(),
            expected: expected.clone(),
        };
        let encode = |path: &str| base64::engine::general_purpose::STANDARD.encode(path);
        let changed = format!(
            "judged={}\ncompared={expected}\nchanged={},{}\n",
            digest('e'),
            encode("config/x"),
            encode(".git/hooks/pre-commit")
        );
        let mut compared = observation();
        parse_capture(&output(&changed), &mut compared, &compare).unwrap();
        assert_eq!(
            compared.compared,
            Some(RepositoryComparison {
                before_sha256: expected.clone(),
                changed_paths: vec![".git/hooks/pre-commit".into(), "config/x".into()],
            })
        );
        let nothing = format!("judged={}\ncompared={expected}\nchanged=\n", digest('e'));
        let mut unchanged = observation();
        parse_capture(&output(&nothing), &mut unchanged, &compare).unwrap();
        assert_eq!(
            unchanged.compared.unwrap().changed_paths,
            Vec::<String>::new()
        );
        // A comparison with another Before manifest is not this one's.
        let other = format!(
            "judged={}\ncompared={}\nchanged=\n",
            digest('e'),
            digest('f')
        );
        assert!(parse_capture(&output(&other), &mut observation(), &compare).is_err());
    }

    fn view(content: ActivationRepositorySnapshot) -> ActivationRepositorySnapshotView {
        ActivationRepositorySnapshotView {
            reference: EvidenceRef::new("capture").unwrap(),
            content,
        }
    }

    #[test]
    fn a_write_scope_is_judged_only_on_a_verified_complete_comparison() {
        let mut before = observation();
        before.phase = RepositorySnapshotPhase::Before;
        before.tree_sha256 = Some(digest('1'));
        before.judged_sha256 = Some(digest('2'));
        before.baseline = Some("/tmp/axocoatl-baseline.Qx12ab".into());
        let mut after = observation();
        after.tree_sha256 = Some(digest('3'));
        after.judged_sha256 = Some(digest('4'));
        after.compared = Some(RepositoryComparison {
            before_sha256: digest('2'),
            changed_paths: vec!["config/x".into()],
        });
        let judged = |before: &ActivationRepositorySnapshot,
                      after: &ActivationRepositorySnapshot| {
            judged_changed_paths(&[view(before.clone()), view(after.clone())])
        };
        assert_eq!(judged(&before, &after), Some(vec!["config/x".to_owned()]));
        // Compared with another manifest, or without one kept, nothing is established.
        let mut other = after.clone();
        other.compared.as_mut().unwrap().before_sha256 = digest('5');
        assert_eq!(judged(&before, &other), None);
        let mut unkept = before.clone();
        unkept.judged_sha256 = None;
        assert_eq!(judged(&unkept, &after), None);
        let mut uncompared = after.clone();
        uncompared.compared = None;
        assert_eq!(judged(&before, &uncompared), None);
        let mut unavailable = after.clone();
        unavailable.unavailable = Some("the kept Before manifest differs".into());
        assert_eq!(judged(&before, &unavailable), None);
        // Equal trees are not taken as proof: the comparison decides.
        let mut same_tree = after.clone();
        same_tree.tree_sha256 = before.tree_sha256.clone();
        assert_eq!(
            judged(&before, &same_tree),
            Some(vec!["config/x".to_owned()])
        );
    }
}
