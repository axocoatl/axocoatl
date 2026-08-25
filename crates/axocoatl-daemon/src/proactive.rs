//! Compatibility observations for event- and Skill-triggered Automations.
//!
//! Trigger matching and execution live in [`crate::automation_runtime`].
//! Legacy `proactive:` YAML is only a first-boot seed for `AutomationStore`.

use std::sync::{Arc, Mutex};

use axocoatl_config::ProactiveConfigYaml;

/// Live observation of one canonical event- or Skill-triggered Automation.
#[derive(Debug, Clone)]
pub struct ProactiveState {
    /// Exact id in `AutomationStore`. `config.id` may omit the legacy `pro:`
    /// prefix for compatibility callers.
    pub automation_id: String,
    pub config: ProactiveConfigYaml,
    /// Canonical trigger kind (`event` or `skill`). This avoids pretending an
    /// `OnSkill` trigger is a legacy `OnEvent` configuration.
    pub trigger_kind: String,
    pub trigger_detail: String,
    pub last_fired_unix: Option<u64>,
    pub last_outcome: Option<String>,
    pub last_error: Option<String>,
    pub run_count: u64,
}

/// Runtime observation table. Configuration always comes from
/// `AutomationStore`; this table can be rebuilt at any time.
pub type ProactiveTable = Arc<Mutex<Vec<ProactiveState>>>;
