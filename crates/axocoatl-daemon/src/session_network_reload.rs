//! `axocoatl network reload` and `POST /api/network/reload`: apply the
//! configuration file's allowlists to running Sessions.
//!
//! The daemon reads the file it was started with again and validates all of
//! it. Only four lists change while it runs: `sandbox.egress.allow`,
//! `sandbox.egress.private_destinations`, `browser.allow` and
//! `browser.private_destinations`. Each running decision point records its
//! new policy (`policy`, `source: config_reload`), new connections use it at
//! once, and open connections that no rule allows any more are closed. Every
//! other difference from the configuration the daemon started with is
//! reported under `restart_required` and not applied.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use axocoatl_config::AxocoatlConfig;
use axocoatl_session::network_record::EgressScope;

use crate::session_egress::EgressPolicyConfig;

/// The settings a reload applies, as dotted keys.
pub const LIVE_KEYS: [&str; 4] = [
    "sandbox.egress.allow",
    "sandbox.egress.private_destinations",
    "browser.allow",
    "browser.private_destinations",
];

/// How deep `restart_required` names a changed setting.
const MAX_KEY_DEPTH: usize = 3;

/// What a reload did.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct NetworkReloadReport {
    /// Allowlists that changed and now apply.
    pub applied: Vec<String>,
    /// Allowlists that are the same as before.
    pub unchanged: Vec<String>,
    /// Settings that differ from the ones the daemon started with. They are
    /// not applied; restart the daemon for them.
    pub restart_required: Vec<String>,
    /// Each running Session policy that changed.
    pub revisions: Vec<ScopeRevision>,
    /// Running Sessions whose new policy could not be recorded. They keep
    /// their policy until the next reload or restart.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub failed: Vec<ReloadFailure>,
}

/// One Session policy a reload changed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScopeRevision {
    pub session_id: String,
    /// `session` or `browser`.
    pub scope: String,
    pub revision: u64,
    pub digest: String,
    /// Open connections closed because no rule allows them any more.
    pub closed: usize,
}

/// A Session the reload could not change.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReloadFailure {
    pub session_id: String,
    pub error: String,
}

/// One scope of one decision point after a reload changed it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScopeReload {
    pub scope: EgressScope,
    pub revision: u64,
    pub digest: String,
    pub closed: usize,
}

/// The allowlists of `next` that a daemon started with `started` can take
/// while it runs. The browser's lists apply only when both have a `browser`
/// block: adding or removing the block turns the tools on or off, which
/// needs a restart.
pub fn applicable_policy(
    started: &AxocoatlConfig,
    current: &EgressPolicyConfig,
    next: &AxocoatlConfig,
) -> EgressPolicyConfig {
    let mut policy = EgressPolicyConfig::from_config(next);
    if started.browser.is_none() || next.browser.is_none() {
        policy.browser = current.browser.clone();
    }
    policy
}

/// Which of [`LIVE_KEYS`] differ between two policies: `(changed, same)`.
pub fn live_changes(
    current: &EgressPolicyConfig,
    next: &EgressPolicyConfig,
) -> (Vec<String>, Vec<String>) {
    let (current_browser, next_browser) = (
        current.browser.clone().unwrap_or_default(),
        next.browser.clone().unwrap_or_default(),
    );
    let pairs = [
        (current.session_allow != next.session_allow),
        (current.session_private != next.session_private),
        (current_browser.0 != next_browser.0),
        (current_browser.1 != next_browser.1),
    ];
    let mut changed = Vec::new();
    let mut same = Vec::new();
    for (key, differs) in LIVE_KEYS.iter().zip(pairs) {
        if differs {
            changed.push((*key).to_string());
        } else {
            same.push((*key).to_string());
        }
    }
    (changed, same)
}

/// The config as JSON with the live lists emptied, so only settings that
/// need a restart remain to compare. A missing `sandbox.egress` block is the
/// same as an empty one.
fn without_live_lists(config: &AxocoatlConfig) -> Value {
    let mut config = config.clone();
    let egress = config.sandbox.egress.get_or_insert_with(Default::default);
    egress.allow.clear();
    egress.private_destinations.clear();
    if let Some(browser) = config.browser.as_mut() {
        browser.allow.clear();
        browser.private_destinations.clear();
    }
    serde_json::to_value(&config).unwrap_or(Value::Null)
}

fn differing_keys(path: &str, left: &Value, right: &Value, depth: usize, out: &mut Vec<String>) {
    if left == right {
        return;
    }
    match (left, right) {
        (Value::Object(left), Value::Object(right)) if depth < MAX_KEY_DEPTH => {
            let mut keys: Vec<&String> = left.keys().chain(right.keys()).collect();
            keys.sort();
            keys.dedup();
            for key in keys {
                let child = if path.is_empty() {
                    key.clone()
                } else {
                    format!("{path}.{key}")
                };
                differing_keys(
                    &child,
                    left.get(key).unwrap_or(&Value::Null),
                    right.get(key).unwrap_or(&Value::Null),
                    depth + 1,
                    out,
                );
            }
        }
        _ => out.push(if path.is_empty() {
            "(configuration)".to_string()
        } else {
            path.to_string()
        }),
    }
}

/// Settings in `next` that differ from the ones the daemon started with,
/// apart from the live lists, as dotted keys (`sandbox.network`, `agents`).
/// Only key names are reported, never values.
pub fn restart_required(started: &AxocoatlConfig, next: &AxocoatlConfig) -> Vec<String> {
    let mut keys = Vec::new();
    differing_keys(
        "",
        &without_live_lists(started),
        &without_live_lists(next),
        0,
        &mut keys,
    );
    keys
}

#[cfg(test)]
mod tests {
    use super::*;
    use axocoatl_config::{BrowserConfigYaml, EgressAllowYaml, EgressConfigYaml, EgressHostYaml};

    fn host(name: &str) -> EgressAllowYaml {
        EgressAllowYaml::Host(EgressHostYaml {
            host: name.into(),
            ports: None,
        })
    }

    fn config(allow: &[&str], browser: Option<&[&str]>) -> AxocoatlConfig {
        let mut config = AxocoatlConfig::default();
        config.sandbox.network = "egress".into();
        config.sandbox.egress = Some(EgressConfigYaml {
            allow: allow.iter().map(|name| host(name)).collect(),
            ..Default::default()
        });
        config.browser = browser.map(|hosts| BrowserConfigYaml {
            allow: hosts.iter().map(|name| host(name)).collect(),
            ..Default::default()
        });
        config
    }

    #[test]
    fn only_the_four_lists_are_live_and_everything_else_needs_a_restart() {
        let started = config(&["a.example"], Some(&["fonts.example"]));
        let mut next = config(&["a.example", "b.example"], Some(&[]));
        assert!(restart_required(&started, &next).is_empty());
        let current = EgressPolicyConfig::from_config(&started);
        let policy = applicable_policy(&started, &current, &next);
        assert_eq!(policy.session_allow.len(), 2);
        assert_eq!(policy.browser, Some((Vec::new(), Vec::new())));
        let (changed, same) = live_changes(&current, &policy);
        assert_eq!(changed, ["sandbox.egress.allow", "browser.allow"]);
        assert_eq!(
            same,
            [
                "sandbox.egress.private_destinations",
                "browser.private_destinations"
            ]
        );

        next.sandbox.network = "bridge".into();
        next.sandbox.egress.as_mut().unwrap().max_connections = 64;
        next.browser.as_mut().unwrap().max_parallel = 4;
        next.server.port = 9999;
        next.consolidation.enabled = !started.consolidation.enabled;
        assert_eq!(
            restart_required(&started, &next),
            [
                "browser.max_parallel",
                "consolidation.enabled",
                "sandbox.egress.max_connections",
                "sandbox.network",
                "server.port"
            ]
        );
    }

    #[test]
    fn adding_or_removing_the_browser_block_needs_a_restart_and_keeps_its_lists() {
        let started = config(&[], Some(&["fonts.example"]));
        let current = EgressPolicyConfig::from_config(&started);
        let without = config(&[], None);
        assert_eq!(restart_required(&started, &without), ["browser"]);
        let policy = applicable_policy(&started, &current, &without);
        assert_eq!(policy.browser, current.browser);

        let started = config(&[], None);
        let current = EgressPolicyConfig::from_config(&started);
        let with = config(&[], Some(&["fonts.example"]));
        assert_eq!(restart_required(&started, &with), ["browser"]);
        assert_eq!(applicable_policy(&started, &current, &with).browser, None);
    }

    #[test]
    fn the_ipc_request_and_answer_have_their_wire_names() {
        use crate::ipc::{IpcRequest, IpcResponse};
        assert_eq!(
            serde_json::to_value(IpcRequest::ReloadNetworkPolicy).unwrap(),
            serde_json::json!({"type": "reload_network_policy"})
        );
        let report = NetworkReloadReport {
            applied: vec!["browser.allow".into()],
            unchanged: vec!["sandbox.egress.allow".into()],
            restart_required: vec!["sandbox.network".into()],
            revisions: vec![ScopeRevision {
                session_id: "ses-1".into(),
                scope: "browser".into(),
                revision: 3,
                digest: "a".repeat(64),
                closed: 1,
            }],
            failed: Vec::new(),
        };
        let wire = serde_json::to_value(IpcResponse::NetworkReloaded {
            report: report.clone(),
        })
        .unwrap();
        assert_eq!(wire["type"], "network_reloaded");
        assert_eq!(wire["report"]["revisions"][0]["closed"], 1);
        assert!(wire["report"].get("failed").is_none());
        match serde_json::from_value::<IpcResponse>(wire).unwrap() {
            IpcResponse::NetworkReloaded { report: parsed } => assert_eq!(parsed, report),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_missing_egress_block_equals_an_empty_one_and_no_value_is_reported() {
        let mut started = AxocoatlConfig::default();
        started.sandbox.egress = None;
        let mut next = config(&["a.example"], None);
        next.sandbox.network = started.sandbox.network.clone();
        assert!(restart_required(&started, &next).is_empty());
        // A changed secret is named by its key, never by its value.
        let key = |value: &str| axocoatl_config::ProviderCredentials {
            api_key: value.into(),
            base_url: None,
            fallback: None,
        };
        let mut old = started.clone();
        old.providers.openai = Some(key("sk-old-value"));
        let mut new = started.clone();
        new.providers.openai = Some(key("sk-new-value"));
        let keys = restart_required(&old, &new);
        assert_eq!(keys, ["providers.openai.api_key"]);
        assert!(!keys.concat().contains("sk-"));
    }
}
