//! Validation for `web_search` (SearXNG) and `web_fetch`.
//!
//! `web_search.provider: searxng` searches through a SearXNG instance that
//! Axocoatl runs (`managed: true`, the default) or one at `url`. The legacy
//! `tavily` provider still parses for configs that already use it; only
//! legacy Sessions use it. `web_fetch` is enabled by its block being present.

use crate::egress::ConfigWarning;
use crate::error::ConfigError;
use crate::types::AxocoatlConfig;

/// Smallest and largest `web_fetch.max_bytes`.
pub const MIN_FETCH_BYTES: u64 = 64 * 1024;
pub const MAX_FETCH_BYTES: u64 = 8 * 1024 * 1024;
/// Bounds for `web_fetch.timeout_secs` and `web_search.searxng.timeout_secs`.
pub const MIN_WEB_TIMEOUT_SECS: u64 = 1;
pub const MAX_WEB_TIMEOUT_SECS: u64 = 60;
/// Most engines `web_search.searxng.engines` may list.
pub const MAX_SEARXNG_ENGINES: usize = 64;

fn invalid(
    field: &str,
    value: impl std::fmt::Debug,
    reason: impl Into<String>,
    suggestion: impl Into<String>,
) -> ConfigError {
    ConfigError::InvalidField {
        field: field.to_string(),
        value: format!("{value:?}"),
        reason: reason.into(),
        suggestion: suggestion.into(),
    }
}

fn is_http_url_with_host(value: &str) -> bool {
    let Some(rest) = value
        .strip_prefix("http://")
        .or_else(|| value.strip_prefix("https://"))
    else {
        return false;
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    !authority.is_empty() && !authority.contains('@') && !value.contains(char::is_whitespace)
}

/// Validate the web tool blocks. Returns warnings that do not stop the daemon.
pub fn validate_web(config: &AxocoatlConfig) -> Result<Vec<ConfigWarning>, ConfigError> {
    let mut warnings = Vec::new();
    let network_none = config.sandbox.network == "none";

    if let Some(search) = &config.web_search {
        match search.provider.as_str() {
            "searxng" => {}
            "tavily" => warnings.push(ConfigWarning {
                field: "web_search.provider".into(),
                message: "tavily is a hosted search API kept only for legacy (1.0-format) \
                          Sessions; native Sessions refuse it. Use provider: searxng"
                    .into(),
            }),
            "" => warnings.push(ConfigWarning {
                field: "web_search.provider".into(),
                message: "web_search is present without a provider, so no Agent gets \
                          web_search. Set provider: searxng"
                    .into(),
            }),
            other => {
                return Err(invalid(
                    "web_search.provider",
                    other,
                    "web_search.provider accepts only searxng (or the legacy tavily)",
                    "Set provider: searxng to search through a SearXNG instance Axocoatl runs.",
                ))
            }
        }
        if let Some(searxng) = &search.searxng {
            if search.provider != "searxng" {
                warnings.push(ConfigWarning {
                    field: "web_search.searxng".into(),
                    message: format!(
                        "ignored because web_search.provider is {:?}; it applies only to \
                         provider: searxng",
                        search.provider
                    ),
                });
            }
            match (searxng.managed, searxng.url.as_deref()) {
                (true, Some(url)) => {
                    return Err(invalid(
                        "web_search.searxng.url",
                        url,
                        "url names an instance you run; it cannot be combined with managed: true",
                        "Remove url to let Axocoatl run SearXNG, or set managed: false.",
                    ))
                }
                (false, None) => {
                    return Err(invalid(
                        "web_search.searxng.url",
                        "",
                        "managed: false needs the url of the SearXNG instance to query",
                        "Set url: http://127.0.0.1:8888 (or wherever your instance answers), \
                         or remove managed: false.",
                    ))
                }
                (false, Some(url)) if !is_http_url_with_host(url) => {
                    return Err(invalid(
                        "web_search.searxng.url",
                        url,
                        "the url must be an http or https URL with a host and no user:password@",
                        "Use a URL such as http://127.0.0.1:8888.",
                    ))
                }
                _ => {}
            }
            if let Some(image) = &searxng.image {
                if image.trim().is_empty()
                    || image.len() > 512
                    || image.contains(char::is_whitespace)
                {
                    return Err(invalid(
                        "web_search.searxng.image",
                        image,
                        "the image must be one container image reference without spaces",
                        "Remove image to use the pinned SearXNG image.",
                    ));
                }
                if !searxng.managed {
                    warnings.push(ConfigWarning {
                        field: "web_search.searxng.image".into(),
                        message: "ignored because managed is false".into(),
                    });
                }
            }
            if searxng.engines.len() > MAX_SEARXNG_ENGINES {
                return Err(invalid(
                    "web_search.searxng.engines",
                    searxng.engines.len(),
                    format!("at most {MAX_SEARXNG_ENGINES} engines"),
                    "List fewer engines, or none to keep SearXNG's defaults.",
                ));
            }
            let mut seen = std::collections::HashSet::new();
            for engine in &searxng.engines {
                let valid = !engine.trim().is_empty()
                    && engine.len() <= 64
                    && engine
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || matches!(c, ' ' | '_' | '-' | '.'));
                if !valid {
                    return Err(invalid(
                        "web_search.searxng.engines",
                        engine,
                        "an engine name is 1-64 letters, digits, spaces, '_', '-' or '.'",
                        "Use SearXNG engine names such as duckduckgo, wikipedia or brave.",
                    ));
                }
                if !seen.insert(engine.as_str()) {
                    return Err(invalid(
                        "web_search.searxng.engines",
                        engine,
                        "an engine is listed twice",
                        "List each engine once.",
                    ));
                }
            }
            let language_ok = !searxng.language.is_empty()
                && searxng.language.len() <= 16
                && searxng
                    .language
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
            if !language_ok {
                return Err(invalid(
                    "web_search.searxng.language",
                    &searxng.language,
                    "the language is \"all\" or a SearXNG language code such as en or pt-BR",
                    "Use language: all.",
                ));
            }
            if searxng.safesearch > 2 {
                return Err(invalid(
                    "web_search.searxng.safesearch",
                    searxng.safesearch,
                    "safesearch is 0 (off), 1 (moderate) or 2 (strict)",
                    "Use safesearch: 0, 1 or 2.",
                ));
            }
            if !(MIN_WEB_TIMEOUT_SECS..=MAX_WEB_TIMEOUT_SECS).contains(&searxng.timeout_secs) {
                return Err(invalid(
                    "web_search.searxng.timeout_secs",
                    searxng.timeout_secs,
                    format!("the timeout is {MIN_WEB_TIMEOUT_SECS}-{MAX_WEB_TIMEOUT_SECS} seconds"),
                    "Use timeout_secs: 15.",
                ));
            }
        }
        if search.provider == "searxng" && network_none {
            warnings.push(ConfigWarning {
                field: "web_search".into(),
                message: "Agents that list web_search are refused while sandbox.network is \
                          none: web tools reach the internet from your computer"
                    .into(),
            });
        }
    }

    if let Some(fetch) = &config.web_fetch {
        if !(MIN_FETCH_BYTES..=MAX_FETCH_BYTES).contains(&fetch.max_bytes) {
            return Err(invalid(
                "web_fetch.max_bytes",
                fetch.max_bytes,
                format!("max_bytes is {MIN_FETCH_BYTES}-{MAX_FETCH_BYTES} (64 KiB to 8 MiB)"),
                "Use max_bytes: 4194304 (4 MiB), the default.",
            ));
        }
        if !(MIN_WEB_TIMEOUT_SECS..=MAX_WEB_TIMEOUT_SECS).contains(&fetch.timeout_secs) {
            return Err(invalid(
                "web_fetch.timeout_secs",
                fetch.timeout_secs,
                format!("the timeout is {MIN_WEB_TIMEOUT_SECS}-{MAX_WEB_TIMEOUT_SECS} seconds"),
                "Use timeout_secs: 20, the default.",
            ));
        }
        if network_none {
            warnings.push(ConfigWarning {
                field: "web_fetch".into(),
                message: "Agents that list web_fetch are refused while sandbox.network is \
                          none: web tools reach the internet from your computer"
                    .into(),
            });
        }
    }
    Ok(warnings)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn parse(yaml: &str) -> Result<AxocoatlConfig, ConfigError> {
        crate::parse_config(yaml, &PathBuf::from("test.yaml"))
    }

    fn warnings(yaml: &str) -> Vec<String> {
        validate_web(&parse(yaml).unwrap())
            .unwrap()
            .iter()
            .map(ToString::to_string)
            .collect()
    }

    #[test]
    fn managed_and_unmanaged_searxng_and_fetch_validate() {
        for valid in [
            "web_search:\n  provider: searxng\n",
            "web_search:\n  provider: searxng\n  searxng: {}\n",
            "web_search:\n  provider: searxng\n  searxng:\n    engines: [duckduckgo, wikipedia, google images]\n    language: pt-BR\n    safesearch: 2\n    timeout_secs: 60\n",
            "web_search:\n  provider: searxng\n  searxng:\n    managed: false\n    url: http://127.0.0.1:8888\n",
            "web_search:\n  provider: searxng\n  searxng:\n    image: docker.io/searxng/searxng:2026.10.2-19ffbcd30\n",
            "web_fetch: {}\n",
            "web_fetch:\n  max_bytes: 65536\n  timeout_secs: 1\n",
            "web_fetch:\n  max_bytes: 8388608\n  timeout_secs: 60\n",
        ] {
            assert!(parse(valid).is_ok(), "{valid}");
            assert!(warnings(valid).is_empty(), "{valid}: {:?}", warnings(valid));
        }
    }

    #[test]
    fn invalid_web_settings_are_refused_with_the_field() {
        for (yaml, field) in [
            ("web_search:\n  provider: bing\n", "web_search.provider"),
            (
                "web_search:\n  provider: searxng\n  searxng:\n    url: http://127.0.0.1:8888\n",
                "web_search.searxng.url",
            ),
            (
                "web_search:\n  provider: searxng\n  searxng:\n    managed: false\n",
                "web_search.searxng.url",
            ),
            (
                "web_search:\n  provider: searxng\n  searxng:\n    managed: false\n    url: file:///tmp/x\n",
                "web_search.searxng.url",
            ),
            (
                "web_search:\n  provider: searxng\n  searxng:\n    managed: false\n    url: http://u:p@host/\n",
                "web_search.searxng.url",
            ),
            (
                "web_search:\n  provider: searxng\n  searxng:\n    image: \"a b\"\n",
                "web_search.searxng.image",
            ),
            (
                "web_search:\n  provider: searxng\n  searxng:\n    engines: [\"x;rm\"]\n",
                "web_search.searxng.engines",
            ),
            (
                "web_search:\n  provider: searxng\n  searxng:\n    engines: [a, a]\n",
                "web_search.searxng.engines",
            ),
            (
                "web_search:\n  provider: searxng\n  searxng:\n    language: \"en us\"\n",
                "web_search.searxng.language",
            ),
            (
                "web_search:\n  provider: searxng\n  searxng:\n    safesearch: 3\n",
                "web_search.searxng.safesearch",
            ),
            (
                "web_search:\n  provider: searxng\n  searxng:\n    timeout_secs: 0\n",
                "web_search.searxng.timeout_secs",
            ),
            (
                "web_search:\n  provider: searxng\n  searxng:\n    timeout_secs: 61\n",
                "web_search.searxng.timeout_secs",
            ),
            ("web_fetch:\n  max_bytes: 65535\n", "web_fetch.max_bytes"),
            ("web_fetch:\n  max_bytes: 8388609\n", "web_fetch.max_bytes"),
            ("web_fetch:\n  timeout_secs: 0\n", "web_fetch.timeout_secs"),
            ("web_fetch:\n  timeout_secs: 61\n", "web_fetch.timeout_secs"),
        ] {
            match parse(yaml) {
                Err(ConfigError::InvalidField { field: got, .. }) => {
                    assert_eq!(got, field, "{yaml}")
                }
                other => panic!("{yaml}: expected an error for {field}, got {other:?}"),
            }
        }
    }

    #[test]
    fn legacy_tavily_empty_provider_ignored_block_and_network_none_warn() {
        let tavily = warnings("web_search:\n  provider: tavily\n  api_key: k\n");
        assert!(
            tavily
                .iter()
                .any(|w| w.starts_with("web_search.provider") && w.contains("legacy")),
            "{tavily:?}"
        );
        let empty = warnings("web_search: {}\n");
        assert!(
            empty.iter().any(|w| w.contains("without a provider")),
            "{empty:?}"
        );
        let ignored = warnings("web_search:\n  provider: tavily\n  searxng: {}\n");
        assert!(
            ignored
                .iter()
                .any(|w| w.starts_with("web_search.searxng") && w.contains("ignored")),
            "{ignored:?}"
        );
        let none = warnings(
            "sandbox:\n  network: none\nweb_search:\n  provider: searxng\nweb_fetch: {}\n",
        );
        assert_eq!(
            none.iter()
                .filter(|w| w.contains("refused while sandbox.network is none"))
                .count(),
            2,
            "{none:?}"
        );
        let unmanaged_image = warnings(
            "web_search:\n  provider: searxng\n  searxng:\n    managed: false\n    url: https://search.example\n    image: x\n",
        );
        assert!(
            unmanaged_image
                .iter()
                .any(|w| w.starts_with("web_search.searxng.image")),
            "{unmanaged_image:?}"
        );
        // The warnings reach validate, doctor and daemon start.
        let config = parse("web_search:\n  provider: tavily\n  api_key: k\n").unwrap();
        assert!(crate::network_warnings(&config)
            .iter()
            .any(|w| w.field == "web_search.provider"));
    }
}
