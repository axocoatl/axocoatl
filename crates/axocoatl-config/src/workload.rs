//! `sandbox.workload`: which users run commands in a local Podman Session
//! container.
//!
//! `auto`, the default, is `hardened` under `network: egress` and `image`
//! under `bridge` and `none`, so a configuration written before this block
//! existed keeps its behaviour outside `egress`. `hardened` needs rootless
//! Podman: the daemon falls back from `auto` to `image` under rootful Podman
//! and warns, and an explicit `hardened` refuses to start a Session there.

use crate::error::ConfigError;
use crate::types::{AxocoatlConfig, SandboxConfigYaml, WorkloadConfigYaml};

/// The only values `sandbox.workload.mode` accepts.
pub const WORKLOAD_MODES: [&str; 3] = ["auto", "hardened", "image"];
/// Highest uid or gid a workload user may have. Rootless Podman maps a
/// container's ids from the subordinate range in `/etc/subuid`, which holds
/// 65,536 ids by default; 65534 and 65535 are `nobody` and the overflow id.
pub const MAX_WORKLOAD_ID: u32 = 65_533;

/// A validated `sandbox.workload.mode`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkloadMode {
    Auto,
    Hardened,
    Image,
}

/// A validated `sandbox.workload`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WorkloadSettings {
    pub mode: WorkloadMode,
    /// `(uid, gid)` for Agents' commands, setup commands and terminals.
    pub writer: (u32, u32),
    /// `(uid, gid)` for read-only helpers' processes.
    pub helper: (u32, u32),
}

/// How Session containers under one network mode run their commands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkloadPlan {
    /// Every command runs as the image's own user.
    Image,
    /// Writers and helpers run as separate non-root users. With
    /// `required: false` (from `auto`) a rootful Podman falls back to
    /// [`WorkloadPlan::Image`] with a warning; with `required: true` the
    /// Session does not start there.
    Hardened { required: bool },
}

impl WorkloadSettings {
    /// What Session containers under `network` get.
    pub fn plan(&self, network: &str) -> WorkloadPlan {
        match self.mode {
            WorkloadMode::Hardened => WorkloadPlan::Hardened { required: true },
            WorkloadMode::Auto if network == "egress" => WorkloadPlan::Hardened { required: false },
            WorkloadMode::Auto | WorkloadMode::Image => WorkloadPlan::Image,
        }
    }

    /// `uid:gid` of the writer user.
    pub fn writer_user(&self) -> String {
        format!("{}:{}", self.writer.0, self.writer.1)
    }

    /// `uid:gid` of the helper user.
    pub fn helper_user(&self) -> String {
        format!("{}:{}", self.helper.0, self.helper.1)
    }
}

fn invalid(field: &str, value: &str, reason: &str, suggestion: &str) -> ConfigError {
    ConfigError::InvalidField {
        field: field.to_string(),
        value: format!("{value:?}"),
        reason: reason.to_string(),
        suggestion: suggestion.to_string(),
    }
}

/// Parse a numeric `uid:gid` whose ids are 1 to [`MAX_WORKLOAD_ID`].
pub fn parse_workload_user(value: &str) -> Option<(u32, u32)> {
    let (uid, gid) = value.split_once(':')?;
    let id = |text: &str| -> Option<u32> {
        if text.is_empty() || text.len() > 5 || !text.bytes().all(|byte| byte.is_ascii_digit()) {
            return None;
        }
        let id = text.parse::<u32>().ok()?;
        (1..=MAX_WORKLOAD_ID).contains(&id).then_some(id)
    };
    Some((id(uid)?, id(gid)?))
}

fn user(field: &str, value: &str) -> Result<(u32, u32), ConfigError> {
    parse_workload_user(value).ok_or_else(|| {
        invalid(
            field,
            value,
            &format!(
                "must be a numeric uid:gid with each id from 1 to {MAX_WORKLOAD_ID} (root is not allowed)"
            ),
            "Use for example \"1000:1000\".",
        )
    })
}

/// Parse and validate `sandbox.workload`, defaults applied.
pub fn workload_settings(sandbox: &SandboxConfigYaml) -> Result<WorkloadSettings, ConfigError> {
    let workload = sandbox.workload.clone().unwrap_or_default();
    workload_settings_of(&workload, &sandbox.backend)
}

fn workload_settings_of(
    workload: &WorkloadConfigYaml,
    backend: &str,
) -> Result<WorkloadSettings, ConfigError> {
    let mode = match workload.mode.as_str() {
        "auto" => WorkloadMode::Auto,
        "hardened" => WorkloadMode::Hardened,
        "image" => WorkloadMode::Image,
        other => {
            return Err(invalid(
                "sandbox.workload.mode",
                other,
                "sandbox.workload.mode accepts only \"auto\", \"hardened\" or \"image\"",
                "Omit it for auto: hardened under network: egress, the image's user otherwise.",
            ))
        }
    };
    let writer = user("sandbox.workload.writer_user", &workload.writer_user)?;
    let helper = user("sandbox.workload.helper_user", &workload.helper_user)?;
    // A helper that shared the writer's uid could read its processes'
    // environment and signal them; one in its group could read what the
    // group may read.
    if [helper.0, helper.1]
        .iter()
        .any(|id| *id == writer.0 || *id == writer.1)
    {
        return Err(invalid(
            "sandbox.workload.helper_user",
            &workload.helper_user,
            "the helper user must share no uid or gid with writer_user",
            "Use for example writer_user: \"1000:1000\" and helper_user: \"1001:1001\".",
        ));
    }
    if mode == WorkloadMode::Hardened && backend == "e2b" {
        return Err(invalid(
            "sandbox.workload.mode",
            &workload.mode,
            "hardened workload users need a local Podman container; the E2B backend runs its own users",
            "Use backend: podman, or omit sandbox.workload.",
        ));
    }
    Ok(WorkloadSettings {
        mode,
        writer,
        helper,
    })
}

/// Validate `sandbox.workload`.
pub fn validate_workload(config: &AxocoatlConfig) -> Result<(), ConfigError> {
    workload_settings(&config.sandbox).map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sandbox(yaml: &str) -> SandboxConfigYaml {
        serde_yaml::from_str(yaml).unwrap()
    }

    fn reason(error: ConfigError) -> String {
        match error {
            ConfigError::InvalidField { field, reason, .. } => format!("{field}: {reason}"),
            other => other.to_string(),
        }
    }

    #[test]
    fn defaults_are_auto_with_two_separate_users() {
        let settings = workload_settings(&SandboxConfigYaml::default()).unwrap();
        assert_eq!(
            settings,
            WorkloadSettings {
                mode: WorkloadMode::Auto,
                writer: (1000, 1000),
                helper: (1001, 1001),
            }
        );
        assert_eq!(settings.writer_user(), "1000:1000");
        assert_eq!(settings.helper_user(), "1001:1001");
        // Auto is hardened only under egress, so bridge and none keep the
        // image's user as before.
        assert_eq!(
            settings.plan("egress"),
            WorkloadPlan::Hardened { required: false }
        );
        assert_eq!(settings.plan("bridge"), WorkloadPlan::Image);
        assert_eq!(settings.plan("none"), WorkloadPlan::Image);
    }

    #[test]
    fn explicit_modes_apply_in_every_network_mode() {
        for network in ["bridge", "none", "egress"] {
            let hardened = workload_settings(&sandbox(&format!(
                "network: {network}\nworkload:\n  mode: hardened\n"
            )))
            .unwrap();
            assert_eq!(
                hardened.plan(network),
                WorkloadPlan::Hardened { required: true }
            );
            let image = workload_settings(&sandbox(&format!(
                "network: {network}\nworkload: {{mode: image}}\n"
            )))
            .unwrap();
            assert_eq!(image.plan(network), WorkloadPlan::Image);
        }
        let custom = workload_settings(&sandbox(
            "workload:\n  writer_user: \"2000:2100\"\n  helper_user: \"3000:3100\"\n",
        ))
        .unwrap();
        assert_eq!((custom.writer, custom.helper), ((2000, 2100), (3000, 3100)));
    }

    #[test]
    fn users_must_be_numeric_non_root_and_apart() {
        for (yaml, expected) in [
            ("workload: {mode: strict}", "sandbox.workload.mode"),
            ("workload: {mode: Hardened}", "sandbox.workload.mode"),
            ("workload: {writer_user: \"0:0\"}", "root is not allowed"),
            ("workload: {writer_user: \"1000:0\"}", "root is not allowed"),
            ("workload: {writer_user: \"1000\"}", "numeric uid:gid"),
            ("workload: {writer_user: \"node:node\"}", "numeric uid:gid"),
            ("workload: {writer_user: \"+1000:1000\"}", "numeric uid:gid"),
            (
                "workload: {writer_user: \"1000:1000:1\"}",
                "numeric uid:gid",
            ),
            (
                "workload: {helper_user: \"65534:65534\"}",
                "sandbox.workload.helper_user",
            ),
            (
                "workload: {helper_user: \"70000:70000\"}",
                "sandbox.workload.helper_user",
            ),
            (
                "workload: {helper_user: \"1000:1001\"}",
                "share no uid or gid",
            ),
            (
                "workload: {helper_user: \"1001:1000\"}",
                "share no uid or gid",
            ),
            (
                "workload: {writer_user: \"1000:1001\", helper_user: \"1001:1002\"}",
                "share no uid or gid",
            ),
        ] {
            let error = workload_settings(&sandbox(yaml)).unwrap_err();
            assert!(reason(error).contains(expected), "{yaml}");
        }
        assert!(serde_yaml::from_str::<SandboxConfigYaml>("workload: {users: 1}").is_err());
    }

    #[test]
    fn e2b_refuses_hardened_but_keeps_auto_and_image() {
        let error =
            workload_settings(&sandbox("backend: e2b\nworkload: {mode: hardened}\n")).unwrap_err();
        assert!(reason(error).contains("E2B"));
        for mode in ["auto", "image"] {
            let settings = workload_settings(&sandbox(&format!(
                "backend: e2b\nworkload: {{mode: {mode}}}\n"
            )))
            .unwrap();
            assert_eq!(settings.plan("bridge"), WorkloadPlan::Image);
        }
    }

    #[test]
    fn whole_config_validation_reports_the_workload_field() {
        let path = std::path::PathBuf::from("test.yaml");
        let error =
            crate::parse_config("sandbox:\n  workload: {writer_user: root}\n", &path).unwrap_err();
        assert!(
            reason(error).starts_with("sandbox.workload.writer_user"),
            "the workload check runs in validate_config"
        );
        let config = crate::parse_config(
            "sandbox:\n  network: egress\n  workload:\n    mode: hardened\n    writer_user: \"1200:1200\"\n",
            &path,
        )
        .unwrap();
        let settings = workload_settings(&config.sandbox).unwrap();
        assert_eq!(
            (settings.mode, settings.writer),
            (WorkloadMode::Hardened, (1200, 1200))
        );
        assert!(validate_workload(&config).is_ok());
    }
}
