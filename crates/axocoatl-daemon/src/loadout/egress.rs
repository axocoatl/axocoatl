//! A loadout Session's egress policy: the loadout's `egress.allow` and
//! `routes` (and the routes its external Agents and e2e checks need) added
//! to the daemon's own lists for that Session only. Owner: core.
//!
//! The additions never reach another Session: the daemon keeps them per
//! Session and applies them on top of the global lists whenever that
//! Session's decision point opens or the global lists reload. A route's
//! credential is a name; its value is read by the route broker per request
//! and never enters the container.

use axocoatl_config::loadout::{AgentRuntime, ResolvedLoadout};
use axocoatl_config::{CredentialSourceYaml, EgressAllowYaml, EgressRouteYaml};

use super::RunError;
use crate::egress_broker::RouteOrigin;
use crate::session_egress::EgressPolicyConfig;

/// What a loadout adds to a Session's egress policy, with every route's
/// credential resolved to where its value is read.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LoadoutEgressOverlay {
    pub allow: Vec<EgressAllowYaml>,
    pub private_destinations: Vec<String>,
    pub routes: Vec<EgressRouteYaml>,
    /// Where each of `routes` comes from: the loadout's own `routes`, or
    /// the ones Axocoatl adds for an external writer.
    pub route_origins: Vec<RouteOrigin>,
    /// Credentials the routes name that the configuration does not define:
    /// secrets stored with `axocoatl secret set`, read from their file.
    pub credentials: std::collections::BTreeMap<String, CredentialSourceYaml>,
}

impl LoadoutEgressOverlay {
    /// `base` with this overlay's entries added. A route whose host the base
    /// already routes keeps the base's route when both are the same and is
    /// refused otherwise, so a loadout cannot replace a configured route.
    pub fn apply(&self, base: &EgressPolicyConfig) -> Result<EgressPolicyConfig, RunError> {
        let mut policy = base.clone();
        for entry in &self.allow {
            if !policy.session_allow.contains(entry) {
                policy.session_allow.push(entry.clone());
            }
        }
        for range in &self.private_destinations {
            if !policy.session_private.contains(range) {
                policy.session_private.push(range.clone());
            }
        }
        for (index, route) in self.routes.iter().enumerate() {
            match policy
                .routes
                .iter()
                .find(|existing| existing.host == route.host)
            {
                Some(existing) if existing == route => {}
                Some(_) => {
                    return Err(RunError::Usage(format!(
                        "the loadout routes {} and so does the configuration, differently; \
                         remove one of the two routes",
                        route.host
                    )))
                }
                None => {
                    // The routes before this one keep their origins (the
                    // configured ones have none listed).
                    policy
                        .route_origins
                        .resize(policy.routes.len(), RouteOrigin::Configured);
                    policy.routes.push(route.clone());
                    policy.route_origins.push(
                        self.route_origins
                            .get(index)
                            .cloned()
                            .unwrap_or(RouteOrigin::Loadout),
                    );
                }
            }
        }
        for (name, source) in &self.credentials {
            policy
                .credentials
                .entry(name.clone())
                .or_insert_with(|| source.clone());
        }
        Ok(policy)
    }
}

/// The routes an external runtime needs, when the loadout's writer is one,
/// each with the runtime it is for.
fn external_routes(
    resolved: &ResolvedLoadout,
) -> Result<Vec<(EgressRouteYaml, RouteOrigin)>, RunError> {
    let mut routes = Vec::new();
    let mut seen = Vec::new();
    for agent in &resolved.loadout.file.agents {
        if agent.runtime == AgentRuntime::Native || seen.contains(&agent.runtime) {
            continue;
        }
        seen.push(agent.runtime);
        let needed = crate::external_agent::routes_for(agent.runtime)
            .map_err(|error| RunError::Infrastructure(error.to_string()))?;
        let origin = RouteOrigin::ExternalWriter {
            runtime: crate::external_agent::runtime_provider(agent.runtime)
                .unwrap_or("external")
                .to_string(),
        };
        routes.extend(needed.into_iter().map(|route| (route, origin.clone())));
    }
    Ok(routes)
}

/// What `resolved` adds to its Session's policy. Credentials named by
/// routes resolve from `credentials` in the configuration first, then from
/// the secret store under `data_dir` (`axocoatl secret set <name>`); a name
/// found in neither is a usage error, never a route without its credential.
pub fn loadout_overlay(
    base: &EgressPolicyConfig,
    resolved: &ResolvedLoadout,
    data_dir: &std::path::Path,
) -> Result<LoadoutEgressOverlay, RunError> {
    let file = &resolved.loadout.file;
    let mut routes = file.routes.clone();
    let mut route_origins = vec![RouteOrigin::Loadout; routes.len()];
    // A route the loadout lists for the same host replaces the one Axocoatl
    // would add for its external writer.
    for (route, origin) in external_routes(resolved)? {
        if !routes.iter().any(|existing| existing.host == route.host) {
            routes.push(route);
            route_origins.push(origin);
        }
    }
    if file.sandbox.network == "none" && !routes.is_empty() {
        return Err(RunError::Usage(
            "the loadout runs under network: none, which reaches no host, but its Agents need \
             routes; run it under network: egress"
                .into(),
        ));
    }
    let mut credentials = std::collections::BTreeMap::new();
    for route in &routes {
        let Some(name) = &route.credential else {
            continue;
        };
        if base.credentials.contains_key(name) || credentials.contains_key(name) {
            continue;
        }
        if !axocoatl_config::egress_routes::is_valid_credential_name(name) {
            return Err(RunError::Usage(format!(
                "the route to {} names credential {name:?}, which is not a valid name",
                route.host
            )));
        }
        let path = crate::secret_store::secret_path(data_dir, name);
        match std::fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.file_type().is_file() => {
                credentials.insert(
                    name.clone(),
                    CredentialSourceYaml {
                        env: None,
                        file: Some(path.display().to_string()),
                    },
                );
            }
            _ => {
                return Err(RunError::Usage(format!(
                    "the route to {} names credential {name:?}, which is neither in the \
                     configuration's credentials nor stored: {}",
                    route.host,
                    crate::secret_store::how_to_store(name)
                )))
            }
        }
    }
    let overlay = LoadoutEgressOverlay {
        allow: file.egress.allow.clone(),
        private_destinations: file.egress.private_destinations.clone(),
        routes,
        route_origins,
        credentials,
    };
    // Refuse a conflict with the configured routes now, not when the
    // Session's decision point opens.
    overlay.apply(base)?;
    Ok(overlay)
}

/// The policy of the run's Session: `base` plus the loadout's entries.
/// Credentials named by routes resolve from `credentials` first, then from
/// the secret store under `data_dir` (`crate::secret_store`).
pub fn loadout_policy(
    base: &EgressPolicyConfig,
    resolved: &ResolvedLoadout,
    data_dir: &std::path::Path,
) -> Result<EgressPolicyConfig, RunError> {
    loadout_overlay(base, resolved, data_dir)?.apply(base)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axocoatl_config::loadout::{parse_loadout, resolve_loadout, LoadoutSource, ParamValues};

    const CUSTOM: &str = r#"
schema: axocoatl.loadout/1
id: docs-fix
version: 1
name: Docs fix
kind: custom
agents:
  - id: writer
    role: writer
    model: { provider: openrouter, model: qwen/qwen3-coder }
    tools: [read_file, write_file]
egress:
  allow:
    - host: registry.npmjs.org
routes:
  - host: api.example.com
    credential: example-token
    inject: { header: Authorization, format: "Bearer {}" }
    access: read-only
budgets:
  agent: { activations: 1, invocations: 10, tokens: 100000, cost_usd: 1 }
  wall_clock: 10m
prompt: "{task}"
"#;

    fn resolved(text: &str) -> ResolvedLoadout {
        let loadout = parse_loadout(text, LoadoutSource::Builtin).unwrap();
        resolve_loadout(&loadout, &ParamValues::new(), "task", "/repo").unwrap()
    }

    #[test]
    fn a_loadout_adds_its_hosts_routes_and_stored_credentials() {
        let data = tempfile::tempdir().unwrap();
        let resolved = resolved(CUSTOM);
        let base = EgressPolicyConfig::default();
        let error = loadout_policy(&base, &resolved, data.path()).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("axocoatl secret set example-token"),
            "{error}"
        );
        // `axocoatl secret set` refuses a terminal: the advice is a pipe.
        assert!(error.to_string().contains("pipe the value"), "{error}");
        let secret = crate::secret_store::secret_path(data.path(), "example-token");
        std::fs::create_dir_all(secret.parent().unwrap()).unwrap();
        std::fs::write(&secret, "value").unwrap();
        let policy = loadout_policy(&base, &resolved, data.path()).unwrap();
        assert_eq!(policy.session_allow.len(), 1);
        assert_eq!(policy.routes.len(), 1);
        assert_eq!(
            policy.credentials.get("example-token"),
            Some(&CredentialSourceYaml {
                env: None,
                file: Some(secret.display().to_string()),
            })
        );
        // A configured credential of the same name wins over the store.
        let mut base = EgressPolicyConfig::default();
        base.credentials.insert(
            "example-token".into(),
            CredentialSourceYaml {
                env: Some("EXAMPLE_TOKEN".into()),
                file: None,
            },
        );
        let policy = loadout_policy(&base, &resolved, data.path()).unwrap();
        assert_eq!(
            policy.credentials["example-token"].env.as_deref(),
            Some("EXAMPLE_TOKEN")
        );
    }

    #[test]
    fn a_loadout_cannot_replace_a_configured_route() {
        let data = tempfile::tempdir().unwrap();
        let resolved = resolved(CUSTOM);
        let secret = crate::secret_store::secret_path(data.path(), "example-token");
        std::fs::create_dir_all(secret.parent().unwrap()).unwrap();
        std::fs::write(&secret, "value").unwrap();
        let mut base = EgressPolicyConfig::default();
        let mut configured = resolved.loadout.file.routes[0].clone();
        configured.access = None;
        base.routes.push(configured);
        assert!(loadout_policy(&base, &resolved, data.path()).is_err());
        let mut base = EgressPolicyConfig::default();
        base.routes.push(resolved.loadout.file.routes[0].clone());
        let policy = loadout_policy(&base, &resolved, data.path()).unwrap();
        assert_eq!(policy.routes.len(), 1, "the same route is not added twice");
    }

    /// Each route the loadout adds says whose it is, after the configured
    /// ones, so a refusal names what to change: the loadout's own route, or
    /// the one Axocoatl adds for its external writer, which a route the
    /// loadout lists for the same host replaces.
    #[test]
    fn a_loadouts_routes_say_where_they_come_from() {
        let data = tempfile::tempdir().unwrap();
        for name in ["example-token", "claude-code-oauth"] {
            let secret = crate::secret_store::secret_path(data.path(), name);
            std::fs::create_dir_all(secret.parent().unwrap()).unwrap();
            std::fs::write(&secret, "value").unwrap();
        }
        let text = CUSTOM.replace(
            "    tools: [read_file, write_file]\n",
            "    runtime: claude-code\n",
        );
        let external = resolved(&text);
        let mut base = EgressPolicyConfig::default();
        let configured: EgressRouteYaml =
            serde_yaml::from_str("{host: configured.test, access: read-only}").unwrap();
        base.routes.push(configured);
        let policy = loadout_policy(&base, &external, data.path()).unwrap();
        let hosts: Vec<&str> = policy
            .routes
            .iter()
            .map(|route| route.host.as_str())
            .collect();
        assert_eq!(
            hosts,
            ["configured.test", "api.example.com", "api.anthropic.com"]
        );
        assert_eq!(
            policy.route_origins,
            [
                RouteOrigin::Configured,
                RouteOrigin::Loadout,
                RouteOrigin::ExternalWriter {
                    runtime: "claude-code".into()
                }
            ]
        );
        // A route the loadout lists for the program's host replaces the one
        // Axocoatl would add, and is the loadout's.
        let own = text.replace("host: api.example.com", "host: api.anthropic.com");
        let policy =
            loadout_policy(&EgressPolicyConfig::default(), &resolved(&own), data.path()).unwrap();
        assert_eq!(policy.routes.len(), 1);
        assert_eq!(policy.route_origins, [RouteOrigin::Loadout]);
    }

    #[test]
    fn an_external_writer_waits_for_its_routes() {
        let text = CUSTOM
            .replace(
                "    tools: [read_file, write_file]\n",
                "    runtime: claude-code\n",
            )
            .replace("egress:\n  allow:\n    - host: registry.npmjs.org\n", "");
        let resolved = resolved(&text);
        let data = tempfile::tempdir().unwrap();
        let result = loadout_policy(&EgressPolicyConfig::default(), &resolved, data.path());
        // Until workstream `agents` provides the routes this is not
        // implemented, never a run without them.
        match result {
            Err(RunError::NotImplemented(_)) | Err(RunError::Usage(_)) | Ok(_) => {}
            Err(other) => panic!("{other}"),
        }
    }
}
