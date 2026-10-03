//! Validation for the `browser` block.
//!
//! The network cross-field rules for `browser` (no E2B, no `allow` under
//! `network: none`) are enforced by [`crate::egress::validate_egress`].

use crate::egress::{validate_allow_list, ConfigWarning};
use crate::error::ConfigError;
use crate::types::AxocoatlConfig;

/// The image `axocoatl browser install` builds and the browser tools use by
/// default.
pub const DEFAULT_BROWSER_IMAGE: &str = "localhost/axocoatl-browser:pw1.60.0";

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

/// Validate the browser block. Returns warnings that do not stop the daemon.
pub fn validate_browser(config: &AxocoatlConfig) -> Result<Vec<ConfigWarning>, ConfigError> {
    let Some(browser) = &config.browser else {
        return Ok(Vec::new());
    };
    let mut warnings =
        validate_allow_list("browser", &browser.allow, &browser.private_destinations)?;
    if !(1024..=65536).contains(&browser.snapshot_max_bytes) {
        return Err(invalid(
            "browser.snapshot_max_bytes",
            browser.snapshot_max_bytes,
            "must be 1024-65536",
            "Omit it for the default 16384.",
        ));
    }
    if !(10..=170).contains(&browser.timeout_secs) {
        return Err(invalid(
            "browser.timeout_secs",
            browser.timeout_secs,
            "must be 10-170",
            "Omit it for the default 120.",
        ));
    }
    if !(1..=4).contains(&browser.max_parallel) {
        return Err(invalid(
            "browser.max_parallel",
            browser.max_parallel,
            "must be 1-4",
            "Omit it for the default 2.",
        ));
    }
    if let Some(image) = &browser.image {
        if image.is_empty()
            || image.len() > 512
            || image.starts_with('-')
            || image.chars().any(|c| c.is_whitespace() || c.is_control())
        {
            return Err(invalid(
                "browser.image",
                image,
                "must be an image reference without spaces",
                "Omit it to use the image `axocoatl browser install` builds.",
            ));
        }
        if image != DEFAULT_BROWSER_IMAGE && !image.contains("@sha256:") {
            warnings.push(ConfigWarning {
                field: "browser.image".into(),
                message:
                    "a tag can be moved to a different image; pin it by digest (image@sha256:...)"
                        .into(),
            });
        }
    }
    Ok(warnings)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{BrowserConfigYaml, EgressAllowYaml};

    fn reason(error: ConfigError) -> String {
        error.to_string()
    }

    fn with(change: impl Fn(&mut BrowserConfigYaml)) -> AxocoatlConfig {
        let mut browser = BrowserConfigYaml::default();
        change(&mut browser);
        AxocoatlConfig {
            browser: Some(browser),
            ..AxocoatlConfig::default()
        }
    }

    #[test]
    fn browser_bounds_image_and_allowlist_are_checked() {
        assert!(validate_browser(&AxocoatlConfig::default())
            .unwrap()
            .is_empty());
        assert!(validate_browser(&with(|_| {})).unwrap().is_empty());
        for (snapshot, ok) in [(1023, false), (1024, true), (65536, true), (65537, false)] {
            let config = with(|browser| browser.snapshot_max_bytes = snapshot);
            assert_eq!(validate_browser(&config).is_ok(), ok, "{snapshot}");
        }
        for (timeout, ok) in [(9, false), (10, true), (170, true), (171, false)] {
            let config = with(|browser| browser.timeout_secs = timeout);
            assert_eq!(validate_browser(&config).is_ok(), ok, "{timeout}");
        }
        for (parallel, ok) in [(0, false), (1, true), (4, true), (5, false)] {
            let config = with(|browser| browser.max_parallel = parallel);
            assert_eq!(validate_browser(&config).is_ok(), ok, "{parallel}");
        }
        for image in ["", "has space:1", "-flag", "a\nb"] {
            let config = with(|browser| browser.image = Some(image.into()));
            assert!(reason(validate_browser(&config).unwrap_err()).contains("browser.image"));
        }
        let config = with(|browser| browser.image = Some("example.org/team/browser:latest".into()));
        let warnings = validate_browser(&config).unwrap();
        assert!(
            warnings[0].message.contains("pin it by digest"),
            "{warnings:?}"
        );
        let config = with(|browser| {
            browser.image = Some(format!(
                "example.org/team/browser@sha256:{}",
                "a".repeat(64)
            ))
        });
        assert!(validate_browser(&config).unwrap().is_empty());
        let config = with(|browser| browser.image = Some(DEFAULT_BROWSER_IMAGE.into()));
        assert!(validate_browser(&config).unwrap().is_empty());

        // The shared allowlist rules apply under the browser field.
        let config =
            with(|browser| browser.allow = vec![EgressAllowYaml::Preset("no-such-preset".into())]);
        assert!(reason(validate_browser(&config).unwrap_err()).contains("browser.allow[0]"));
        let config = with(|browser| browser.allow = vec![EgressAllowYaml::Preset("npm".into())]);
        assert!(!validate_browser(&config).unwrap().is_empty());
    }
}
