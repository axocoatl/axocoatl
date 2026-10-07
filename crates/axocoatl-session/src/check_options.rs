//! Per-check options of a Session team's required checks: a name, a
//! timeout, and the report the check writes.
//!
//! `SessionTeamEdit::check_options` is aligned by index with
//! `required_checks`; empty means every check takes the defaults, which
//! keeps the serialized shape of teams applied before 1.3.
//!
//! Owner: workstream `runtime` (timeouts reach the admitted definitions and
//! the authority bound); workstream `e2e` reads `report`.

use serde::{Deserialize, Serialize};

/// Timeout of a check that names none: three minutes (the 1.2 constant).
pub const DEFAULT_CHECK_TIMEOUT_MS: u64 = 180_000;
/// Longest timeout a check may have: thirty minutes.
pub const MAX_CHECK_TIMEOUT_MS: u64 = 30 * 60 * 1000;
/// Shortest timeout a check may have.
pub const MIN_CHECK_TIMEOUT_MS: u64 = 1_000;

/// Options of one required check.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RequiredCheckOptions {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub report: Option<CheckReportSpec>,
}

/// Where a check writes a machine-readable report inside the Session
/// container, and in which format.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CheckReportSpec {
    /// `junit` or `e2e_report_json`.
    pub format: String,
    /// Absolute path under `/tmp/axocoatl-check-reports/`.
    pub path: String,
}

/// Directory every check report must be under.
pub const CHECK_REPORT_DIR: &str = "/tmp/axocoatl-check-reports/";

impl RequiredCheckOptions {
    /// The check's timeout, defaults applied.
    pub fn timeout_ms(&self) -> u64 {
        self.timeout_ms.unwrap_or(DEFAULT_CHECK_TIMEOUT_MS)
    }
}

/// Validate options against the checks they belong to.
pub fn validate_check_options(
    checks: &[Vec<String>],
    options: &[RequiredCheckOptions],
) -> Result<(), String> {
    if options.is_empty() {
        return Ok(());
    }
    if options.len() != checks.len() {
        return Err(format!(
            "check_options has {} entries for {} required checks",
            options.len(),
            checks.len()
        ));
    }
    for (index, option) in options.iter().enumerate() {
        if let Some(timeout) = option.timeout_ms {
            if !(MIN_CHECK_TIMEOUT_MS..=MAX_CHECK_TIMEOUT_MS).contains(&timeout) {
                return Err(format!(
                    "check {} timeout must be {}s to {} minutes",
                    index + 1,
                    MIN_CHECK_TIMEOUT_MS / 1000,
                    MAX_CHECK_TIMEOUT_MS / 60_000
                ));
            }
        }
        if let Some(report) = &option.report {
            if !matches!(report.format.as_str(), "junit" | "e2e_report_json") {
                return Err(format!(
                    "check {} report format must be junit or e2e_report_json",
                    index + 1
                ));
            }
            if !report.path.starts_with(CHECK_REPORT_DIR) || report.path.contains("..") {
                return Err(format!(
                    "check {} report path must be under {CHECK_REPORT_DIR}",
                    index + 1
                ));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn options_are_aligned_and_bounded() {
        let checks = vec![vec!["npm".to_string(), "test".to_string()]];
        assert!(validate_check_options(&checks, &[]).is_ok());
        let long = RequiredCheckOptions {
            timeout_ms: Some(MAX_CHECK_TIMEOUT_MS + 1),
            ..RequiredCheckOptions::default()
        };
        assert!(validate_check_options(&checks, &[long]).is_err());
        let ok = RequiredCheckOptions {
            timeout_ms: Some(600_000),
            ..RequiredCheckOptions::default()
        };
        assert!(validate_check_options(&checks, std::slice::from_ref(&ok)).is_ok());
        assert_eq!(ok.timeout_ms(), 600_000);
        assert_eq!(
            RequiredCheckOptions::default().timeout_ms(),
            DEFAULT_CHECK_TIMEOUT_MS
        );
        assert!(validate_check_options(&checks, &[ok.clone(), ok]).is_err());
    }
}
