//! Finite, actual tree observations through the existing invocation authority.
use super::*;
use axocoatl_session::execution_content::{
    is_capture_baseline, ActivationRepositorySnapshot, ActivationRepositorySnapshotView,
    RepositoryComparison, RepositorySnapshotPhase, MAX_CHANGED_MANIFEST_BYTES,
    MAX_COMPARED_PATH_BYTES, MAX_PATCH_PREFIX_BYTES, MAX_RECORDED_PATCH_BYTES,
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
            // A capture the Session cannot record is unavailable, not a fault.
            let admitted = match self.admit_or_decline_invocation(activation, &request)? {
                Ok(admitted) => admitted,
                Err(reason) => {
                    observation.unavailable = Some(reason);
                    return self.retain_observation(observation);
                }
            };
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
        self.retain_observation(observation)
    }

    /// Retain one repository observation; an unavailable one keeps only its
    /// bounded reason.
    fn retain_observation(
        &self,
        mut observation: ActivationRepositorySnapshot,
    ) -> Result<Option<EvidenceRef>> {
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
        if state.tool_call_room() <= earlier as usize {
            return Some(record_full_message());
        }
        state
            .host_observation_shortfall(activation, TOOL_CALL_NEEDS.saturating_add(earlier))
            .map(reserve_message)
    }

    /// What this activation's grant still allows its Agent (see
    /// `DispatchState::agent_allowance`).
    pub(crate) fn agent_allowance(
        &self,
        activation: &ActivationRef,
    ) -> Option<axocoatl_llm::ProviderAllowance> {
        self.lock().ok()?.agent_allowance(activation)
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
    /// cannot change a file: it is offered no file-writing tool, and its
    /// shell, when it has one, runs under a write restriction the kernel
    /// enforces or does not run at all. Without a shell it is never judged;
    /// with one, a change its complete captures show (a permission bit, say)
    /// is still reported, but captures it could not complete (a Stop skips
    /// the After capture) are no reason to doubt it. Returns why the
    /// activation must not be accepted, or `None` when every change stayed in
    /// scope. A scope that cannot be read is itself a reason. A writer that
    /// was `stopped` before its captures could establish what it changed is
    /// told so in words the failure classes read as a stop. The captures
    /// compare complete manifests, whatever the index's flags; files the
    /// repository's ignore rules exclude are not judged, but the ignore files
    /// Git reads, and Git's own settings, hooks and exclude files, are.
    pub(crate) fn write_scope_violation(
        &self,
        activation: &ActivationRef,
        stopped: bool,
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
            if scope.is_read_only() {
                return Ok(None);
            }
            if stopped {
                return Ok(Some(format!(
                    "{STOPPED_BEFORE_CAPTURE}, so changes outside the paths this Agent may \
                     change ({}) cannot be ruled out; any change is kept for review",
                    scope.describe()
                )));
            }
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

/// How the failure of a writer stopped before its captures could establish
/// what it changed begins; `classify_activation_failure` reads it as a stop.
pub(crate) const STOPPED_BEFORE_CAPTURE: &str =
    axocoatl_session::execution_content::STOPPED_BEFORE_CAPTURE;

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

/// Why a tool call or capture cannot be recorded: this turn has used its
/// per-turn room for them.
pub(crate) const RECORD_FULL: &str = "This turn can record no more tool calls or captures";

/// Why an Agent's tool call is declined when the turn can record no more.
pub(crate) fn record_full_message() -> String {
    format!("{RECORD_FULL}. Do not call any more tools; write your final answer now.")
}

/// How a failure that the Session budget refused a model call begins.
pub(crate) const BUDGET_USED_UP: &str = "The Session budget for this Agent is used up: ";

/// The failure when the host's reserve refuses an Agent's next model call.
pub(crate) fn held_back_message(reserve: u32) -> String {
    format!(
        "{BUDGET_USED_UP}its last {reserve} invocation(s) are held for the host to observe its \
         changes and run required checks."
    )
}

/// The failure when a grant's own limit refuses a model call that would
/// reserve `reservation`, or `None` when no budget limit is the cause.
pub(crate) fn grant_exhausted_message(
    limits: &axocoatl_session::control_authority::GrantLimits,
    usage: &axocoatl_session::control_authority::GrantUsage,
    reservation: &axocoatl_session::control_authority::DispatchReservation,
) -> Option<String> {
    if usage.invocations >= limits.invocations {
        return Some(format!(
            "{BUDGET_USED_UP}it has made all {} model and tool calls its budget allows.",
            group_digits(u64::from(limits.invocations))
        ));
    }
    if reservation.tokens > limits.tokens {
        // Nothing was used up: the whole limit cannot pay for one call.
        return Some(format!(
            "{BUDGET_USED_UP}its whole token limit of {} cannot pay for one model call, which \
             reserves {}. Raise its token limit in Team and budget.",
            group_digits(limits.tokens),
            group_digits(reservation.tokens)
        ));
    }
    if usage.tokens.saturating_add(reservation.tokens) > limits.tokens {
        return Some(format!(
            "{BUDGET_USED_UP}{} of its {} tokens remain and the next model call needs {}.",
            group_digits(limits.tokens.saturating_sub(usage.tokens)),
            group_digits(limits.tokens),
            group_digits(reservation.tokens)
        ));
    }
    if usage
        .cost_microunits
        .saturating_add(reservation.cost_microunits)
        > limits.cost_microunits
    {
        return Some(format!(
            "{BUDGET_USED_UP}the next model call would pass its spending limit."
        ));
    }
    None
}

/// `1457714` as `1,457,714`.
pub(crate) fn group_digits(value: u64) -> String {
    let digits = value.to_string();
    let mut grouped = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            grouped.push(',');
        }
        grouped.push(digit);
    }
    grouped
}

impl DispatchState {
    /// What this activation's grant still allows its Agent: the invocations
    /// left less those the host holds back, and the tokens and spending
    /// left. The Agent asks for its final answer, without tools, once this
    /// cannot pay for another tool round.
    pub(crate) fn agent_allowance(
        &self,
        activation: &ActivationRef,
    ) -> Option<axocoatl_llm::ProviderAllowance> {
        let bound = self
            .bound
            .get(&activation.activation_id)
            .filter(|bound| bound.activation == *activation)?;
        let grant = bound.grant.grant_id.as_str();
        let limits = self.authority.grant_status(grant).ok()?.policy.limits;
        let usage = self.authority.usage(grant).ok()?;
        let reserve = self.host_observation_reserve(activation).unwrap_or(0);
        Some(axocoatl_llm::ProviderAllowance {
            invocations: Some(u64::from(
                limits
                    .invocations
                    .saturating_sub(usage.invocations)
                    .saturating_sub(reserve),
            )),
            tokens: Some(limits.tokens.saturating_sub(usage.tokens)),
            cost_microunits: Some(limits.cost_microunits.saturating_sub(usage.cost_microunits)),
            tool_calls: Some(u64::try_from(self.tool_call_room()).unwrap_or(u64::MAX)),
        })
    }

    /// Tool calls this turn can still record for an Agent within its
    /// per-turn bounds (the turn contract's commands and bytes, and the
    /// authority's dispatch claims, one more of which each tool round's
    /// model call takes), less the After capture that each running
    /// activation observing its repository still needs. A Session's own
    /// history has no such bound: each new turn starts with full room.
    pub(crate) fn tool_call_room(&self) -> usize {
        let held =
            self.canonical
                .snapshot(&self.turn_id)
                .ok()
                .map_or(0, |snapshot| {
                    snapshot
                        .contract()
                        .activations()
                        .iter()
                        .filter(|item| {
                            item.state == ActivationState::Running
                                && self.bound.get(&item.activation.activation_id).is_some_and(
                                    |bound| {
                                        bound.activation == item.activation
                                            && bound.repository.is_some()
                                            && capture_tool(&bound.profile).is_some()
                                    },
                                )
                        })
                        .count()
                });
        self.turn_record_room().saturating_sub(held)
    }

    /// Tool calls, captures included, this turn can still record.
    pub(crate) fn turn_record_room(&self) -> usize {
        let contract = self
            .canonical
            .turn_room(&self.turn_id)
            .ok()
            .flatten()
            .map_or(0, |room| room.tool_calls());
        // A tool call and the model call that reads its result, with one
        // claim kept for the final answer.
        let claims = self
            .authority
            .remaining_claims()
            .map_or(0, |claims| claims.saturating_sub(1) / 2);
        contract.min(claims)
    }

    /// Invocations the host holds back for this activation, when it can run
    /// commands in a repository: its After capture and, when required checks
    /// exist and this grant is the one that pays for them, what is left of
    /// their allowance: two passes of a shared Before capture, each check and
    /// a shared After capture, one for the turn and one for a Continue, less
    /// the check runs this turn already paid for, and never less than one
    /// pass.
    pub(crate) fn host_observation_reserve(&self, activation: &ActivationRef) -> Option<u32> {
        let bound = self
            .bound
            .get(&activation.activation_id)
            .filter(|bound| bound.activation == *activation)?;
        if bound.repository.is_none() || capture_tool(&bound.profile).is_none() {
            return None;
        }
        let grant = bound.grant.grant_id.as_str();
        let pays_checks = self.authority.grant_pays_required_checks(grant).ok()?;
        let snapshot = self.canonical.snapshot(&self.turn_id).ok()?;
        let group = snapshot
            .contract()
            .graph()
            .and_then(axocoatl_session::turn_checks::group_of);
        let spent = group.as_ref().map_or(0, |(group, _)| {
            snapshot
                .contract()
                .condition_runs()
                .iter()
                .filter(|run| group.contains(&run.run.condition_id))
                .count()
        });
        let checks = paid_checks(pays_checks, group.map(|(_, checks)| checks));
        Some(host_reserve(checks, spent))
    }

    /// The host observation reserve, when spending `needed` more invocations
    /// would cut into it.
    pub(crate) fn host_observation_shortfall(
        &self,
        activation: &ActivationRef,
        needed: u32,
    ) -> Option<u32> {
        let reserve = self.host_observation_reserve(activation)?;
        let grant = self
            .bound
            .get(&activation.activation_id)?
            .grant
            .grant_id
            .as_str();
        let limit = self
            .authority
            .grant_status(grant)
            .ok()?
            .policy
            .limits
            .invocations;
        let used = self.authority.usage(grant).ok()?.invocations;
        reserve_shortfall(used, needed, reserve, limit)
    }
}

/// How many of the turn's `checks` commands a grant pays for: all of them
/// when it is the paying grant, else none.
fn paid_checks(pays: bool, checks: Option<usize>) -> usize {
    match checks {
        Some(checks) if pays => checks,
        _ => 0,
    }
}

/// The After capture, plus what is left of the allowance of `checks`
/// required checks paid from this grant (`turn_checks::check_allowance`)
/// after `spent` of their runs, and at least one more pass of them.
fn host_reserve(checks: usize, spent: usize) -> u32 {
    use axocoatl_session::turn_checks::{check_allowance, check_pass_invocations};
    let checks = if checks == 0 {
        0
    } else {
        check_allowance(checks)
            .saturating_sub(u32::try_from(spent).unwrap_or(u32::MAX))
            .max(check_pass_invocations(checks))
    };
    checks.saturating_add(1)
}

/// The reserve, when spending `needed` more of `limit` would cut into it.
fn reserve_shortfall(used: u32, needed: u32, reserve: u32, limit: u32) -> Option<u32> {
    (used.saturating_add(needed).saturating_add(reserve) > limit).then_some(reserve)
}

#[cfg(test)]
impl SessionDispatchController {
    /// A bound activation's host observation reserve, read from the real
    /// authority, and the reserve when spending `needed` more would cut into
    /// it.
    pub(crate) fn host_observation_for_test(
        &self,
        activation: &ActivationRef,
        needed: u32,
    ) -> (Option<u32>, Option<u32>) {
        let state = self.lock().unwrap();
        (
            state.host_observation_reserve(activation),
            state.host_observation_shortfall(activation, needed),
        )
    }
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
        // Thirteen invocations, one required check: the host keeps 7 (After,
        // and Before + check + After twice: the turn's run and a Continue).
        let (limit, reserve) = (13, host_reserve(1, 0));
        assert_eq!(reserve, 7);
        assert_eq!(host_reserve(0, 0), 1);
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
        // Two required checks: the paying grant holds back its After capture
        // and, twice, both captures around the checks and each check.
        assert_eq!(host_reserve(paid_checks(true, Some(2)), 0), 9);
        // Every other grant of the turn holds back only its own After capture.
        assert_eq!(host_reserve(paid_checks(false, Some(2)), 0), 1);
        // Without checks the payer holds back nothing more either.
        assert_eq!(host_reserve(paid_checks(true, None), 0), 1);
        // Once a pass was paid for, a retried payer keeps only what is left,
        // and never less than one more pass.
        assert_eq!(host_reserve(paid_checks(true, Some(2)), 2), 7);
        assert_eq!(host_reserve(paid_checks(true, Some(2)), 4), 5);
        assert_eq!(host_reserve(paid_checks(true, Some(2)), 12), 5);
        // With fourteen invocations and two checks, the payer's Agent may
        // start a tool call only while three plus nine still fit; any other
        // Agent's grant is unaffected by the checks.
        let reserve = host_reserve(paid_checks(true, Some(2)), 0);
        assert!(reserve_shortfall(2, TOOL_CALL_NEEDS, reserve, 14).is_none());
        assert_eq!(
            reserve_shortfall(3, TOOL_CALL_NEEDS, reserve, 14),
            Some(reserve)
        );
        let other = host_reserve(paid_checks(false, Some(2)), 0);
        assert!(reserve_shortfall(10, TOOL_CALL_NEEDS, other, 14).is_none());
        // The smallest limit Apply accepts for the payer leaves it, after its
        // Before capture, one model call beside the reserve.
        let minimum = axocoatl_session::turn_checks::payer_minimum_invocations(2);
        assert_eq!(minimum, reserve + 2);
        assert!(reserve_shortfall(1, 1, reserve, minimum).is_none());
        assert!(reserve_shortfall(2, 1, reserve, minimum).is_some());
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

/// Why a capture stopped at its time limit (`limit`) recorded nothing. It
/// reads every file of the repository, so the time grows with the files the
/// repository and the change hold: on a 6-CPU Podman machine a capture of
/// 500,000 files took 53 seconds.
pub(super) fn capture_timed_out(limit: std::time::Duration) -> String {
    format!(
        "The repository capture did not finish within its {} seconds: it reads every file in \
         the repository, so a very large repository or change (a million files or more), or \
         a busy computer, can take longer",
        limit.as_secs()
    )
}

/// Why a capture that could not read a file it records (exit 65) recorded
/// nothing: a file in the repository or its Git directory that the Agents'
/// user cannot read, or that changed as the capture read it.
pub(super) const CAPTURE_UNREADABLE: &str = "The repository capture could not read every file \
     it records, so the candidate is not verified: a file in the repository or in its .git \
     directory that the Agents' user cannot read (its permissions, say), or one that changed \
     while the capture read it";

/// Why a capture whose listing of the repository's files reached the limit
/// on its temporary files (exit 69) recorded nothing.
pub(super) const CAPTURE_LISTING_TOO_LARGE: &str = "The repository capture's listing of the \
     repository's files reached the 1 GiB a capture may write, so the candidate is not \
     verified: the repository holds more files, or longer names, than one capture lists";

/// Read one capture's result. A capture around the checks that ran past its
/// time limit carries that limit as `timed_out_after_ms`.
pub(super) fn parse_capture(
    value: &serde_json::Value,
    observation: &mut ActivationRepositorySnapshot,
    mode: &CaptureMode,
) -> Result<()> {
    if let Some(limit) = value
        .get("timed_out_after_ms")
        .and_then(serde_json::Value::as_u64)
    {
        return Err(error(capture_timed_out(std::time::Duration::from_millis(
            limit,
        ))));
    }
    let exit_code = value.get("exit_code").and_then(serde_json::Value::as_i64);
    // A comparison whose list of changed paths is longer than one capture
    // records (`MAX_COMPARED_PATH_BYTES` in base64) refuses it.
    if matches!(mode, CaptureMode::Compare { .. }) && exit_code == Some(67) {
        return Err(error("Repository capture changed too many paths to record"));
    }
    match exit_code {
        Some(65) => return Err(error(CAPTURE_UNREADABLE)),
        Some(69) => return Err(error(CAPTURE_LISTING_TOO_LARGE)),
        _ => {}
    }
    if exit_code != Some(0)
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
    // Format 2 adds the lines of the paths that differ from HEAD; format 1
    // is the capture a check group admitted before it keeps running.
    let base_fields = match field("format")? {
        "1" => 8,
        "2" => 10,
        _ => return Err(error("Unsupported repository capture")),
    };
    if fields.len() != base_fields + judgement_fields {
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
    if manifest.len() > 8192 || patch.len() > MAX_RECORDED_PATCH_BYTES {
        return Err(error("Repository capture exceeds its declared bounds"));
    }
    observation.head = (head != "unborn").then(|| head.to_owned());
    observation.tree_sha256 = Some(field("tree")?.to_owned());
    observation.manifest_bytes = field("manifest_bytes")?.parse().map_err(error)?;
    observation.manifest_complete = observation.manifest_bytes == manifest.len() as u64;
    observation.manifest_prefix = String::from_utf8(manifest).map_err(error)?;
    observation.patch_sha256 = Some(field("patch_sha256")?.to_owned());
    observation.patch_bytes = field("patch_bytes")?.parse().map_err(error)?;
    if !is_digest(field("patch_sha256")?) {
        return Err(error("Invalid captured patch digest"));
    }
    if patch.len() as u64 > observation.patch_bytes {
        return Err(error("Captured patch is longer than its recorded size"));
    }
    // A patch too large to record whole arrives as its first bytes beside
    // the whole patch's digest and size: it is shown, never kept as the
    // protected patch, so nothing can apply or read files from part of it.
    observation.patch_complete = observation.patch_bytes == patch.len() as u64;
    observation.patch_prefix =
        String::from_utf8_lossy(&patch[..patch.len().min(MAX_PATCH_PREFIX_BYTES)]).into_owned();
    let mut prefix_end = observation.patch_prefix.len().min(MAX_PATCH_PREFIX_BYTES);
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
    observation.patch_base64 = observation
        .patch_complete
        .then(|| base64::engine::general_purpose::STANDARD.encode(&patch));
    if base_fields == 10 {
        observation.changed_manifest =
            changed_manifest(field("changed_complete")?, field("changed_b64")?)?;
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

/// The lines of the paths that differ from HEAD a capture recorded: each
/// `<base64 path>\t<mode>\t<kind>\t<digest>`, of a file or link with its
/// SHA-256, a deleted path, or a submodule or nested checkout
/// ([`axocoatl_session::turn_review::REPOSITORY`]) with the digest of all of
/// it. `None` when the capture left them out.
fn changed_manifest(complete: &str, encoded: &str) -> Result<Option<String>> {
    match complete {
        "0" if encoded.is_empty() => return Ok(None),
        "1" => {}
        _ => return Err(error("Invalid record of the changed paths")),
    }
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .map_err(|_| error("Invalid record of the changed paths"))?;
    if bytes.len() > MAX_CHANGED_MANIFEST_BYTES {
        return Err(error("Repository capture exceeds its declared bounds"));
    }
    let text =
        String::from_utf8(bytes).map_err(|_| error("Invalid record of the changed paths"))?;
    for line in text.lines() {
        let fields: Vec<&str> = line.split('\t').collect();
        let valid = match fields.as_slice() {
            [path, mode, kind, digest] => {
                base64::engine::general_purpose::STANDARD
                    .decode(path)
                    .is_ok_and(|path| !path.is_empty())
                    && mode.len() <= 4
                    && mode.bytes().all(|byte| (b'0'..=b'7').contains(&byte))
                    && match *kind {
                        "file" | "link" | axocoatl_session::turn_review::REPOSITORY => {
                            is_digest(digest)
                        }
                        "deleted" => *digest == "missing",
                        _ => false,
                    }
            }
            _ => false,
        };
        if !valid {
            return Err(error("Invalid record of the changed paths"));
        }
    }
    Ok(Some(text))
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
        changed_manifest: None,
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

    /// Format 2 records the lines of the paths that differ from HEAD, a
    /// submodule's or nested checkout's among them (or says it left them
    /// out); format 1, which a check group admitted before still runs, has
    /// none; a malformed line refuses the capture.
    #[test]
    fn the_lines_of_the_changed_paths_are_read_from_format_2() {
        let encode = |text: &str| base64::engine::general_purpose::STANDARD.encode(text);
        let lines = format!(
            "{}\t644\tfile\t{}\n{}\t0\tdeleted\tmissing\n{}\t777\tlink\t{}\n{}\t755\trepository\t{}\n",
            encode("lib/a.rs"),
            digest('1'),
            encode("old.rs"),
            encode("docs/link"),
            digest('2'),
            encode("vendor/sub"),
            digest('4')
        );
        let format2 = |complete: &str, encoded: &str| {
            let mut value = output(&format!(
                "changed_complete={complete}
changed_b64={encoded}
"
            ));
            let stdout = value["stdout"]
                .as_str()
                .unwrap()
                .replacen("format=1", "format=2", 1);
            value["stdout"] = serde_json::Value::String(stdout);
            value
        };
        let mut recorded = observation();
        parse_capture(
            &format2("1", &encode(&lines)),
            &mut recorded,
            &CaptureMode::Observe,
        )
        .unwrap();
        assert_eq!(recorded.changed_manifest.as_deref(), Some(lines.as_str()));
        let mut left_out = observation();
        parse_capture(&format2("0", ""), &mut left_out, &CaptureMode::Observe).unwrap();
        assert_eq!(left_out.changed_manifest, None);
        let mut earlier = observation();
        parse_capture(&output(""), &mut earlier, &CaptureMode::Observe).unwrap();
        assert_eq!(earlier.changed_manifest, None);
        for bad in [
            format2("0", &encode(&lines)),
            format2("2", ""),
            format2("1", &encode("lib/a.rs\t644\tfile\tabc\n")),
            format2(
                "1",
                &encode(&format!("{}\t644\tfile\tnot-hex\n", encode("a"))),
            ),
            format2(
                "1",
                &encode(&format!("{}\t0\tdeleted\t{}\n", encode("a"), digest('3'))),
            ),
            format2(
                "1",
                &encode(&format!("{}\t755\trepository\tmissing\n", encode("a"))),
            ),
            format2(
                "1",
                &encode(&format!("{}\t0\tspecial\t{}\n", encode("a"), digest('3'))),
            ),
            format2("1", "!!"),
            // Format 2 without its fields, format 1 with them.
            {
                let mut value = output("");
                let stdout = value["stdout"]
                    .as_str()
                    .unwrap()
                    .replacen("format=1", "format=2", 1);
                value["stdout"] = serde_json::Value::String(stdout);
                value
            },
            output(&format!(
                "changed_complete=1\nchanged_b64={}\n",
                encode(&lines)
            )),
        ] {
            assert!(
                parse_capture(&bad, &mut observation(), &CaptureMode::Observe).is_err(),
                "{bad}"
            );
        }
    }

    /// A capture whose patch is too large to record whole prints its first
    /// bytes beside the whole patch's digest and size: the host records the
    /// patch as incomplete, keeps the prefix only as text to show and keeps
    /// no protected patch, so nothing can apply part of it. A prefix that
    /// claims more than the patch, or a digest that is not one, refuses the
    /// capture, as does a whole patch whose digest differs.
    #[test]
    fn a_patch_too_large_to_record_is_kept_as_its_digest_size_and_prefix() {
        let prefix = "diff --git a/src/lib.rs b/src/lib.rs\n".repeat(400);
        assert!(prefix.len() > MAX_PATCH_PREFIX_BYTES);
        let with_patch = |patch: &[u8], bytes: usize, sha: &str| {
            let mut value = output("");
            let stdout = value["stdout"].as_str().unwrap().to_owned();
            let empty_sha = format!("{:x}", Sha256::digest(b""));
            let stdout = stdout
                .replacen(
                    &format!("patch_sha256={empty_sha}"),
                    &format!("patch_sha256={sha}"),
                    1,
                )
                .replacen("patch_bytes=0", &format!("patch_bytes={bytes}"), 1)
                .replacen(
                    "patch_b64=\n",
                    &format!(
                        "patch_b64={}\n",
                        base64::engine::general_purpose::STANDARD.encode(patch)
                    ),
                    1,
                );
            value["stdout"] = serde_json::Value::String(stdout);
            value
        };
        let whole_sha = digest('9');
        let mut partial = observation();
        parse_capture(
            &with_patch(prefix.as_bytes(), 9 * 1024 * 1024, &whole_sha),
            &mut partial,
            &CaptureMode::Observe,
        )
        .unwrap();
        assert!(!partial.patch_complete);
        assert_eq!(partial.patch_base64, None);
        assert_eq!(partial.patch_bytes, 9 * 1024 * 1024);
        assert_eq!(partial.patch_sha256.as_deref(), Some(whole_sha.as_str()));
        assert_eq!(partial.patch_prefix, prefix[..MAX_PATCH_PREFIX_BYTES]);
        assert!(partial.tree_sha256.is_some());

        let whole = b"diff --git a/a b/a\n".to_vec();
        let sha = format!("{:x}", Sha256::digest(&whole));
        let mut recorded = observation();
        parse_capture(
            &with_patch(&whole, whole.len(), &sha),
            &mut recorded,
            &CaptureMode::Observe,
        )
        .unwrap();
        assert!(recorded.patch_complete);
        assert_eq!(
            recorded.patch_base64,
            Some(base64::engine::general_purpose::STANDARD.encode(&whole))
        );

        for (patch, bytes, sha) in [
            // More bytes than the patch it claims to begin.
            (prefix.as_bytes(), 10, whole_sha.clone()),
            // Not a digest.
            (
                prefix.as_bytes(),
                9 * 1024 * 1024,
                "not-a-digest".to_owned(),
            ),
            // A whole patch whose digest differs.
            (whole.as_slice(), whole.len(), whole_sha.clone()),
        ] {
            let mut refused = observation();
            assert!(parse_capture(
                &with_patch(patch, bytes, &sha),
                &mut refused,
                &CaptureMode::Observe
            )
            .is_err());
            assert_eq!(refused.patch_base64, None);
        }
        // A prefix beyond what one capture records is not a prefix.
        let oversized = vec![b'x'; MAX_RECORDED_PATCH_BYTES + 1];
        assert!(parse_capture(
            &with_patch(&oversized, oversized.len() + 1, &whole_sha),
            &mut observation(),
            &CaptureMode::Observe
        )
        .is_err());
        // A comparison that listed more changed paths than one capture
        // records says so.
        let compare = CaptureMode::Compare {
            baseline: "/tmp/axocoatl-baseline.Qx12ab".into(),
            expected: digest('d'),
        };
        let refused = serde_json::json!({"stdout": "", "exit_code": 67, "stdout_truncated": false});
        let failure = parse_capture(&refused, &mut observation(), &compare)
            .unwrap_err()
            .to_string();
        assert!(
            failure.contains("changed too many paths to record"),
            "{failure}"
        );
        let failure = parse_capture(&refused, &mut observation(), &CaptureMode::Observe)
            .unwrap_err()
            .to_string();
        assert!(
            failure.contains("failed or its result was truncated"),
            "{failure}"
        );
        // The capture reads a patch of any size without storing it, so no
        // exit code of its own names a change too large to capture: 68, which
        // a capture that stored the patch used for that, is a failure like
        // any other.
        let other = serde_json::json!({"stdout": "", "exit_code": 68, "stdout_truncated": false});
        for mode in [&CaptureMode::Observe, &CaptureMode::Keep, &compare] {
            let failure = parse_capture(&other, &mut observation(), mode)
                .unwrap_err()
                .to_string();
            assert!(
                failure.contains("failed or its result was truncated"),
                "{failure}"
            );
        }
        // A capture that could not read a file it records, and one whose
        // listing reached the limit on its temporary files, say so in every
        // mode.
        for (code, reason) in [(65, CAPTURE_UNREADABLE), (69, CAPTURE_LISTING_TOO_LARGE)] {
            let failed =
                serde_json::json!({"stdout": "", "exit_code": code, "stdout_truncated": false});
            for mode in [&CaptureMode::Observe, &CaptureMode::Keep, &compare] {
                let failure = parse_capture(&failed, &mut observation(), mode)
                    .unwrap_err()
                    .to_string();
                assert_eq!(failure, format!("Session dispatch: {reason}"));
            }
        }
        assert!(CAPTURE_UNREADABLE.starts_with(
            "The repository capture could not read every file it records, so the candidate is \
             not verified: "
        ));
        assert!(CAPTURE_LISTING_TOO_LARGE.contains("reached the 1 GiB a capture may write"));
        // The capture names those failures with those codes, and the limit
        // is 1 GiB of 512-byte blocks.
        let script = axocoatl_session::execution_content::REPOSITORY_SNAPSHOT_COMMAND;
        for said in [
            "file_limit=2097152\nulimit -f \"$file_limit\"\n",
            "    exit 69\n",
            "    exit 65\n",
        ] {
            assert!(script.contains(said), "{said}");
        }
        // A capture around the checks that ran past its time limit says so,
        // with the limit, in every mode.
        let timed_out = serde_json::json!({"stdout": "", "exit_code": null,
            "stdout_truncated": false, "timed_out_after_ms": 180_000});
        for mode in [&CaptureMode::Observe, &CaptureMode::Keep, &compare] {
            let failure = parse_capture(&timed_out, &mut observation(), mode)
                .unwrap_err()
                .to_string();
            assert_eq!(
                failure,
                "Session dispatch: The repository capture did not finish within its 180 \
                 seconds: it reads every file in the repository, so a very large repository or \
                 change (a million files or more), or a busy computer, can take longer"
            );
        }
        let finished = serde_json::json!({"stdout": "", "exit_code": 1,
            "stdout_truncated": false, "timed_out_after_ms": null});
        let failure = parse_capture(&finished, &mut observation(), &CaptureMode::Observe)
            .unwrap_err()
            .to_string();
        assert!(
            failure.contains("failed or its result was truncated"),
            "{failure}"
        );
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
