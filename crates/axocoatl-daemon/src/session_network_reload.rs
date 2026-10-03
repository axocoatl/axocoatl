//! `axocoatl network reload` and `POST /api/network/reload`: apply the
//! configuration file's allowlists and egress routes to running Sessions.
//!
//! The daemon reads the file it was started with again and validates all of
//! it. Only these change while it runs: `sandbox.egress.allow`,
//! `sandbox.egress.private_destinations`, `sandbox.egress.routes`,
//! `credentials`, `browser.allow` and `browser.private_destinations`. Each
//! running decision point records its new policy (`policy`, `source:
//! config_reload`), new connections use it at once, and open connections
//! that no rule allows any more, or whose route changed, are closed. Every
//! other difference from the configuration the daemon started with is
//! reported under `restart_required` and not applied.
//!
//! A decision point that could not record a scope's new policy keeps that
//! scope's old lists, and a later reload, even of the same file, tries it
//! again: what each decision point has applied is compared with the new
//! lists, not only what the daemon held before.

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use axocoatl_config::{AxocoatlConfig, CredentialSourceYaml, EgressAllowYaml, EgressRouteYaml};
use axocoatl_session::network_record::EgressScope;

use crate::session_egress::{EgressPolicyConfig, SessionEgress};
use crate::session_egress_policy::CompiledPolicy;

/// The settings a reload applies, as dotted keys.
pub const LIVE_KEYS: [&str; 6] = [
    "sandbox.egress.allow",
    "sandbox.egress.private_destinations",
    "sandbox.egress.routes",
    "credentials",
    "browser.allow",
    "browser.private_destinations",
];

/// How deep `restart_required` names a changed setting.
const MAX_KEY_DEPTH: usize = 3;

/// What a reload did.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct NetworkReloadReport {
    /// Allowlists that changed, for new Sessions or for a running Session
    /// that had not applied them yet, and now apply.
    pub applied: Vec<String>,
    /// Allowlists that are the same as before.
    pub unchanged: Vec<String>,
    /// Settings that differ from the ones the daemon started with. They are
    /// not applied; restart the daemon for them.
    pub restart_required: Vec<String>,
    /// Each running Session policy that changed.
    pub revisions: Vec<ScopeRevision>,
    /// Running Sessions whose new policy could not be recorded. They keep
    /// that policy until a reload succeeds for them or the daemon restarts;
    /// the next reload tries them again.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub failed: Vec<ReloadFailure>,
    /// The entries each list gained and lost, compared with the lists in
    /// force before this reload, so a person sees what a reload widens.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub changes: Vec<ListChange>,
}

/// What one of [`LIVE_KEYS`] gained and lost. Allow entries are listed as
/// the rules they compile to (a preset as each of its hosts), private
/// ranges as ranges.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListChange {
    pub key: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub added: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub removed: Vec<String>,
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

/// A Session the reload could not change: one scope whose new policy could
/// not be recorded, or the whole Session (no `scope`) when its lists did not
/// compile.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReloadFailure {
    pub session_id: String,
    /// `session` or `browser`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
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

/// One scope of one decision point whose new policy could not be recorded.
/// It keeps its policy and its lists.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScopeReloadFailure {
    pub scope: EgressScope,
    pub error: String,
}

/// What [`SessionEgress::reload_config`] did to one decision point.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ConfigReload {
    pub changed: Vec<ScopeReload>,
    pub failed: Vec<ScopeReloadFailure>,
}

/// What reloading a set of decision points did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PointsReload {
    /// [`LIVE_KEYS`] that at least one decision point had not applied.
    pub lagging: Vec<String>,
    pub revisions: Vec<ScopeRevision>,
    pub failed: Vec<ReloadFailure>,
}

impl PointsReload {
    pub fn extend(&mut self, other: PointsReload) {
        for key in other.lagging {
            if !self.lagging.contains(&key) {
                self.lagging.push(key);
            }
        }
        self.revisions.extend(other.revisions);
        self.failed.extend(other.failed);
    }
}

/// Reload each decision point with its lists, `(session id, decision point,
/// lists)`. A decision point that has applied them already is left alone;
/// one that has not, because it was opened before them or a reload could
/// not record one of its scopes, records and applies them now.
pub async fn reload_points(
    points: Vec<(String, Arc<SessionEgress>, EgressPolicyConfig)>,
    actor: &str,
) -> PointsReload {
    let mut done = PointsReload::default();
    for (session_id, egress, config) in points {
        let (lagging, _) = live_changes(&egress.policy_config(), &config);
        if lagging.is_empty() {
            continue;
        }
        done.extend(PointsReload {
            lagging,
            ..PointsReload::default()
        });
        match egress.reload_config(config, actor).await {
            Ok(reload) => {
                done.revisions
                    .extend(reload.changed.into_iter().map(|scope| ScopeRevision {
                        session_id: session_id.clone(),
                        scope: scope.scope.as_str().to_string(),
                        revision: scope.revision,
                        digest: scope.digest,
                        closed: scope.closed,
                    }));
                done.failed
                    .extend(reload.failed.into_iter().map(|failure| ReloadFailure {
                        session_id: session_id.clone(),
                        scope: Some(failure.scope.as_str().to_string()),
                        error: failure.error,
                    }));
            }
            Err(error) => done.failed.push(ReloadFailure {
                session_id,
                scope: None,
                error: error.to_string(),
            }),
        }
    }
    done
}

/// The rules one allow list compiles to and its private ranges, as text.
/// Lists the daemon already validated compile; anything else is shown as
/// written.
fn list_entries(
    scope: EgressScope,
    allow: &[EgressAllowYaml],
    private: &[String],
) -> (Vec<String>, Vec<String>) {
    match CompiledPolicy::compile(scope, allow, private, &[]) {
        Ok(policy) => (
            policy.rendered(),
            policy
                .private_destinations()
                .iter()
                .map(ToString::to_string)
                .collect(),
        ),
        Err(_) => (
            allow
                .iter()
                .map(|entry| serde_json::to_string(entry).unwrap_or_default())
                .collect(),
            private.to_vec(),
        ),
    }
}

/// Each route as its configuration, in JSON.
fn route_entries(routes: &[EgressRouteYaml]) -> Vec<String> {
    routes
        .iter()
        .map(|route| serde_json::to_string(route).unwrap_or_default())
        .collect()
}

/// Each credential as its name and where it is read; there is no value to
/// show.
fn credential_entries(
    credentials: &std::collections::BTreeMap<String, CredentialSourceYaml>,
) -> Vec<String> {
    credentials
        .iter()
        .map(|(name, source)| match (&source.env, &source.file) {
            (Some(variable), _) => format!("{name}: env {variable}"),
            (None, Some(path)) => format!("{name}: file {path}"),
            (None, None) => name.clone(),
        })
        .collect()
}

/// What each of [`LIVE_KEYS`] gains and loses from `current` to `next`;
/// only lists that change are named.
pub fn list_changes(current: &EgressPolicyConfig, next: &EgressPolicyConfig) -> Vec<ListChange> {
    let (current_browser, next_browser) = (
        current.browser.clone().unwrap_or_default(),
        next.browser.clone().unwrap_or_default(),
    );
    let session_before = list_entries(
        EgressScope::Session,
        &current.session_allow,
        &current.session_private,
    );
    let session_after = list_entries(
        EgressScope::Session,
        &next.session_allow,
        &next.session_private,
    );
    let browser_before = list_entries(EgressScope::Browser, &current_browser.0, &current_browser.1);
    let browser_after = list_entries(EgressScope::Browser, &next_browser.0, &next_browser.1);
    let routes = (route_entries(&current.routes), route_entries(&next.routes));
    let credentials = (
        credential_entries(&current.credentials),
        credential_entries(&next.credentials),
    );
    let pairs = [
        (&session_before.0, &session_after.0),
        (&session_before.1, &session_after.1),
        (&routes.0, &routes.1),
        (&credentials.0, &credentials.1),
        (&browser_before.0, &browser_after.0),
        (&browser_before.1, &browser_after.1),
    ];
    LIVE_KEYS
        .iter()
        .zip(pairs)
        .filter_map(|(key, (old, new))| {
            let added: Vec<String> = new
                .iter()
                .filter(|entry| !old.contains(entry))
                .cloned()
                .collect();
            let removed: Vec<String> = old
                .iter()
                .filter(|entry| !new.contains(entry))
                .cloned()
                .collect();
            (!added.is_empty() || !removed.is_empty()).then(|| ListChange {
                key: (*key).to_string(),
                added,
                removed,
            })
        })
        .collect()
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
        (current.routes != next.routes),
        (current.credentials != next.credentials),
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
    egress.routes.clear();
    config.credentials.clear();
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
    fn only_the_live_lists_are_live_and_everything_else_needs_a_restart() {
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
                "sandbox.egress.routes",
                "credentials",
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

    fn route(yaml: &str) -> EgressRouteYaml {
        serde_yaml::from_str(yaml).unwrap()
    }

    /// Routes and credentials reload like the allowlists: they need no
    /// restart, a change is named entry by entry, and a credential is named
    /// with where it is read, never a value (the configuration has none).
    #[test]
    fn routes_and_credentials_reload_live_and_are_named_without_values() {
        let started = config(&["a.example"], None);
        let mut next = config(&["a.example"], None);
        next.sandbox.egress.as_mut().unwrap().routes = vec![
            route("{host: api.example, credential: api, inject: {header: Authorization, format: 'Bearer {}'}, rules: [{methods: [GET], path: /v1/**}]}"),
            route("{host: registry.example, access: read-only}"),
        ];
        next.credentials.insert(
            "api".into(),
            CredentialSourceYaml {
                env: Some("API_TOKEN".into()),
                file: None,
            },
        );
        assert!(restart_required(&started, &next).is_empty());
        let current = EgressPolicyConfig::from_config(&started);
        let policy = applicable_policy(&started, &current, &next);
        assert_eq!(policy.routes, next.sandbox.egress.as_ref().unwrap().routes);
        assert_eq!(policy.credentials, next.credentials);
        let (changed, _) = live_changes(&current, &policy);
        assert_eq!(changed, ["sandbox.egress.routes", "credentials"]);
        let changes = list_changes(&current, &policy);
        let keys: Vec<&str> = changes.iter().map(|change| change.key.as_str()).collect();
        assert_eq!(keys, ["sandbox.egress.routes", "credentials"]);
        assert_eq!(changes[0].added.len(), 2);
        assert!(changes[0].added[0].contains("\"host\":\"api.example\""));
        assert!(changes[0].added[1].contains("read-only"));
        assert_eq!(changes[1].added, ["api: env API_TOKEN"]);

        // A rule changed is the route removed and added again.
        let mut narrower = next.clone();
        narrower.sandbox.egress.as_mut().unwrap().routes[0] =
            route("{host: api.example, credential: api, inject: {header: Authorization, format: 'Bearer {}'}, rules: [{methods: [GET], path: /v1/repos}]}");
        let changes = list_changes(
            &EgressPolicyConfig::from_config(&next),
            &EgressPolicyConfig::from_config(&narrower),
        );
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].key, "sandbox.egress.routes");
        assert!(changes[0].removed[0].contains("/v1/**"));
        assert!(changes[0].added[0].contains("/v1/repos"));
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
            changes: vec![ListChange {
                key: "browser.allow".into(),
                added: vec!["fonts.example:443 (config)".into()],
                removed: Vec::new(),
            }],
        };
        let wire = serde_json::to_value(IpcResponse::NetworkReloaded {
            report: report.clone(),
        })
        .unwrap();
        assert_eq!(wire["type"], "network_reloaded");
        assert_eq!(wire["report"]["revisions"][0]["closed"], 1);
        assert!(wire["report"].get("failed").is_none());
        assert_eq!(
            wire["report"]["changes"],
            serde_json::json!([{"key": "browser.allow", "added": ["fonts.example:443 (config)"]}])
        );
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

    #[test]
    fn a_reload_names_each_entry_a_list_gains_and_loses() {
        let mut started = config(&["a.example", "b.example"], Some(&["fonts.example"]));
        started
            .sandbox
            .egress
            .as_mut()
            .unwrap()
            .private_destinations = vec!["10.0.0.0/8".into()];
        let mut next = config(&["b.example", "c.example"], Some(&["fonts.example"]));
        next.sandbox
            .egress
            .as_mut()
            .unwrap()
            .allow
            .push(EgressAllowYaml::Preset("npm".into()));
        next.sandbox.egress.as_mut().unwrap().private_destinations = vec!["192.168.1.0/24".into()];
        next.browser.as_mut().unwrap().private_destinations = vec!["10.9.0.0/16".into()];
        let current = EgressPolicyConfig::from_config(&started);
        let policy = applicable_policy(&started, &current, &next);
        let changes = list_changes(&current, &policy);
        let keys: Vec<&str> = changes.iter().map(|change| change.key.as_str()).collect();
        assert_eq!(
            keys,
            [
                "sandbox.egress.allow",
                "sandbox.egress.private_destinations",
                "browser.private_destinations"
            ]
        );
        // A preset is named by each host it allows, and a host whose
        // position moved is not a change.
        assert!(changes[0]
            .added
            .contains(&"c.example:443 (config)".to_string()));
        assert!(changes[0]
            .added
            .contains(&"registry.npmjs.org:443 (preset npm)".to_string()));
        assert!(!changes[0]
            .added
            .iter()
            .any(|entry| entry.starts_with("b.example")));
        assert_eq!(changes[0].removed, ["a.example:443 (config)"]);
        assert_eq!(changes[1].added, ["192.168.1.0/24"]);
        assert_eq!(changes[1].removed, ["10.0.0.0/8"]);
        assert_eq!(changes[2].added, ["10.9.0.0/16"]);
        assert!(changes[2].removed.is_empty());
        assert!(list_changes(&policy, &policy).is_empty());
    }
}
