//! Compatibility projections and cadence parsing for Automation schedules.
//!
//! The canonical trigger runtime lives in [`crate::automation_runtime`].
//! Legacy `schedules:` YAML is only used to seed the Automation store on first
//! boot; this module deliberately does not start runners from YAML.

use std::sync::{Arc, Mutex};

use axocoatl_config::ScheduleConfigYaml;

/// Live observation of one canonical scheduled Automation, exposed through
/// the compatibility `/api/schedules` API.
#[derive(Debug, Clone)]
pub struct ScheduleState {
    /// Exact id in `AutomationStore`. `config.id` may be the legacy-compatible
    /// id without the `sched:` prefix.
    pub automation_id: String,
    pub config: ScheduleConfigYaml,
    pub interval_secs: u64,
    pub next_fire_unix: Option<u64>,
    pub last_fired_unix: Option<u64>,
    pub last_outcome: Option<String>,
    pub last_error: Option<String>,
    pub run_count: u64,
}

impl ScheduleState {
    pub fn next_fire_unix(&self) -> Option<u64> {
        self.next_fire_unix
    }
}

/// Parse `30s` / `5m` / `2h` / `1d` into seconds.
pub fn parse_interval(s: &str) -> Result<u64, String> {
    let trimmed = s.trim();
    if trimmed.is_empty() {
        return Err("empty interval".into());
    }
    let (num_str, unit) = trimmed.split_at(trimmed.len() - 1);
    let n: u64 = num_str
        .parse()
        .map_err(|_| format!("invalid number in '{trimmed}'"))?;
    let mult = match unit {
        "s" => 1,
        "m" => 60,
        "h" => 3600,
        "d" => 86400,
        _ => {
            return Err(format!(
                "unknown unit '{unit}' in '{trimmed}' — use s/m/h/d"
            ))
        }
    };
    Ok(n.saturating_mul(mult))
}

/// Runtime observation table. It is a cache for compatibility and UI
/// observability, never a source of trigger configuration.
pub type ScheduleTable = Arc<Mutex<Vec<ScheduleState>>>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_supported_fixed_cadences() {
        assert_eq!(parse_interval("30s"), Ok(30));
        assert_eq!(parse_interval("5m"), Ok(300));
        assert_eq!(parse_interval("2h"), Ok(7_200));
        assert_eq!(parse_interval("1d"), Ok(86_400));
    }

    #[test]
    fn rejects_invalid_or_zeroish_shapes() {
        assert!(parse_interval("").is_err());
        assert!(parse_interval("1w").is_err());
        assert!(parse_interval("soon").is_err());
        assert_eq!(parse_interval("0s"), Ok(0));
    }
}
