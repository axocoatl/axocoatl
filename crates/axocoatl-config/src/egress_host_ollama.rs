//! `sandbox.egress.host_ollama`: an explicit, opt-in route from Session
//! containers to an Ollama server on this computer's loopback interface.
//!
//! Under `network: egress` a container reaches
//! `https://ollama.host.axocoatl.internal` ([`HOST_OLLAMA_ROUTE_HOST`]): the
//! Session's decision point recognizes the name without DNS and answers its
//! `CONNECT` with a relay, the route broker ends TLS with the Session's own
//! certificate authority, records each request and response like any route,
//! and forwards plain HTTP to `127.0.0.1:<port>`. Nothing else on this
//! computer is reachable through it. It is off unless set; under `bridge`
//! and `none` there is no decision point, so no Session reaches it (a
//! warning says so, since a loadout Session always runs under `egress`).
//!
//! Owner: workstream `runtime`.

use std::collections::HashSet;

use crate::egress::ConfigWarning;
use crate::error::ConfigError;
use crate::types::{AxocoatlConfig, HostOllamaRouteYaml, RouteForYaml};

/// The host name containers use for the route (`OLLAMA_HOST`). It never
/// resolves in DNS; the decision point recognizes it by name.
pub const HOST_OLLAMA_ROUTE_HOST: &str = "ollama.host.axocoatl.internal";

/// The port containers connect to: the route is HTTPS, ended by the broker.
pub const HOST_OLLAMA_ROUTE_PORT: u16 = 443;

/// `OLLAMA_HOST` for a process the route serves.
pub const HOST_OLLAMA_URL: &str = "https://ollama.host.axocoatl.internal:443";

/// The suffix every name Axocoatl reserves for its own routes ends with; a
/// route or allow entry cannot claim one.
pub const RESERVED_ROUTE_SUFFIX: &str = ".axocoatl.internal";

/// Whether `host` (normalized, lowercase) is a name Axocoatl reserves.
pub fn is_reserved_route_host(host: &str) -> bool {
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    host == RESERVED_ROUTE_SUFFIX.trim_start_matches('.') || host.ends_with(RESERVED_ROUTE_SUFFIX)
}

/// The processes the route serves: `for`, or `[agent]` when omitted.
pub fn host_ollama_bindings(route: &HostOllamaRouteYaml) -> Vec<RouteForYaml> {
    route
        .bindings
        .clone()
        .unwrap_or_else(|| vec![RouteForYaml::Agent])
}

fn invalid(
    field: &str,
    value: impl std::fmt::Debug,
    reason: &str,
    suggestion: &str,
) -> ConfigError {
    ConfigError::InvalidField {
        field: field.to_string(),
        value: format!("{value:?}"),
        reason: reason.to_string(),
        suggestion: suggestion.to_string(),
    }
}

/// Validate `sandbox.egress.host_ollama`. Absent is always valid. The port
/// must be a TCP port other than the daemon's own, and `for` lists each of
/// `agent`, `terminal` and `setup` at most once, like a route's.
pub fn validate_host_ollama(config: &AxocoatlConfig) -> Result<(), ConfigError> {
    let Some(route) = config
        .sandbox
        .egress
        .as_ref()
        .and_then(|egress| egress.host_ollama.as_ref())
    else {
        return Ok(());
    };
    const FIELD: &str = "sandbox.egress.host_ollama";
    if route.port == 0 {
        return Err(invalid(
            &format!("{FIELD}.port"),
            route.port,
            "the port of the Ollama server on this computer's loopback interface is 1-65535",
            "Set port: 11434, or the port your Ollama server listens on.",
        ));
    }
    if route.port == config.server.port {
        return Err(invalid(
            &format!("{FIELD}.port"),
            route.port,
            "that is the Axocoatl daemon's own port; the route reaches only an Ollama server",
            "Set the port your Ollama server listens on, such as 11434.",
        ));
    }
    if let Some(bindings) = &route.bindings {
        if bindings.is_empty() {
            return Err(invalid(
                &format!("{FIELD}.for"),
                bindings,
                "for cannot be empty; omit it for [agent]",
                "List agent, terminal or setup.",
            ));
        }
        let unique: HashSet<RouteForYaml> = bindings.iter().copied().collect();
        if unique.len() != bindings.len() {
            return Err(invalid(
                &format!("{FIELD}.for"),
                bindings,
                "a kind is listed twice",
                "List each of agent, terminal and setup at most once.",
            ));
        }
    }
    Ok(())
}

/// What a person should know about `sandbox.egress.host_ollama`: under
/// `bridge` or `none` no Session but a loadout's (which always runs under
/// `egress`) can reach it.
pub fn host_ollama_warnings(config: &AxocoatlConfig) -> Vec<ConfigWarning> {
    let Some(route) = config
        .sandbox
        .egress
        .as_ref()
        .and_then(|egress| egress.host_ollama.as_ref())
    else {
        return Vec::new();
    };
    let mut warnings = Vec::new();
    if config.sandbox.network != "egress" {
        warnings.push(ConfigWarning {
            field: "sandbox.egress.host_ollama".into(),
            message: format!(
                "the route to Ollama on 127.0.0.1:{} applies only to Sessions under network: \
                 egress; with sandbox.network {:?} only loadout Sessions, which always run under \
                 egress, reach it, and no other Session does",
                route.port, config.sandbox.network
            ),
        });
    }
    warnings
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::EgressConfigYaml;

    fn with_route(network: &str, port: u16, bindings: Option<Vec<RouteForYaml>>) -> AxocoatlConfig {
        let mut config = AxocoatlConfig::default();
        config.sandbox.network = network.into();
        config.sandbox.egress = Some(EgressConfigYaml {
            host_ollama: Some(HostOllamaRouteYaml { port, bindings }),
            ..EgressConfigYaml::default()
        });
        config
    }

    #[test]
    fn absent_is_valid_and_silent() {
        let config = AxocoatlConfig::default();
        assert!(validate_host_ollama(&config).is_ok());
        assert!(host_ollama_warnings(&config).is_empty());
    }

    #[test]
    fn a_loopback_port_other_than_the_daemons_is_valid() {
        let config = with_route("egress", 11434, None);
        validate_host_ollama(&config).unwrap();
        assert!(host_ollama_warnings(&config).is_empty());
        assert_eq!(
            host_ollama_bindings(
                config
                    .sandbox
                    .egress
                    .as_ref()
                    .unwrap()
                    .host_ollama
                    .as_ref()
                    .unwrap()
            ),
            [RouteForYaml::Agent]
        );
        let all = with_route(
            "egress",
            11436,
            Some(vec![
                RouteForYaml::Agent,
                RouteForYaml::Terminal,
                RouteForYaml::Setup,
            ]),
        );
        validate_host_ollama(&all).unwrap();
        // The whole configuration validates with it.
        crate::validate_config(&config).unwrap();
    }

    #[test]
    fn port_zero_the_daemon_port_and_bad_bindings_are_refused() {
        let error = validate_host_ollama(&with_route("egress", 0, None)).unwrap_err();
        assert!(error.to_string().contains("host_ollama.port"), "{error}");
        let mut daemon = with_route("egress", 11434, None);
        daemon.server.port = 11434;
        let error = validate_host_ollama(&daemon).unwrap_err();
        assert!(error.to_string().contains("daemon's own port"), "{error}");
        let error = validate_host_ollama(&with_route("egress", 11434, Some(vec![]))).unwrap_err();
        assert!(error.to_string().contains("for cannot be empty"), "{error}");
        let error = validate_host_ollama(&with_route(
            "egress",
            11434,
            Some(vec![RouteForYaml::Agent, RouteForYaml::Agent]),
        ))
        .unwrap_err();
        assert!(error.to_string().contains("listed twice"), "{error}");
    }

    #[test]
    fn bridge_and_none_warn_that_only_egress_sessions_reach_it() {
        for network in ["bridge", "none"] {
            let config = with_route(network, 11434, None);
            validate_host_ollama(&config).unwrap();
            let warnings = host_ollama_warnings(&config);
            assert_eq!(warnings.len(), 1, "{network}");
            assert!(
                warnings[0].message.contains("only loadout Sessions"),
                "{}",
                warnings[0].message
            );
            assert!(crate::network_warnings(&config)
                .iter()
                .any(|warning| warning.field == "sandbox.egress.host_ollama"));
        }
    }

    #[test]
    fn the_route_names_are_reserved() {
        assert!(is_reserved_route_host(HOST_OLLAMA_ROUTE_HOST));
        assert!(is_reserved_route_host("OLLAMA.host.axocoatl.internal."));
        assert!(is_reserved_route_host("other.axocoatl.internal"));
        assert!(is_reserved_route_host("axocoatl.internal"));
        assert!(!is_reserved_route_host("axocoatl.internal.example.com"));
        assert!(!is_reserved_route_host("notaxocoatl.internal"));
        assert!(HOST_OLLAMA_URL.starts_with(&format!("https://{HOST_OLLAMA_ROUTE_HOST}:")));
    }

    #[test]
    fn the_setting_parses_from_yaml_and_refuses_unknown_fields() {
        let parsed: EgressConfigYaml =
            serde_yaml::from_str("host_ollama: {port: 11434, for: [agent, terminal]}").unwrap();
        assert_eq!(
            parsed.host_ollama,
            Some(HostOllamaRouteYaml {
                port: 11434,
                bindings: Some(vec![RouteForYaml::Agent, RouteForYaml::Terminal]),
            })
        );
        assert!(serde_yaml::from_str::<EgressConfigYaml>(
            "host_ollama: {port: 11434, host: 10.0.0.1}"
        )
        .is_err());
    }
}
