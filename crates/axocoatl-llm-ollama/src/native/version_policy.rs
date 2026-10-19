//! Which Ollama server versions native execution accepts.
//!
//! The native guarantees (an over-long prompt is refused before generation
//! with `truncate:false`, a positive `num_predict` stops generation, and
//! `shift:false` never slides the context) were audited in the Ollama 0.40.6
//! source, where every GGUF model runs in llama.cpp's `llama-server`
//! (`llm/server.go` `NewLlamaServer`; `server/sched.go` uses it for every model
//! that is not safetensors). Later patch releases of the same minor line are
//! accepted, and each server is still checked by the contract probes on the
//! person's own model before a Team uses it. Pre-releases, development builds,
//! other minor lines and the releases on [`DENIED_OLLAMA_VERSIONS`] are refused.

/// The Ollama release whose source the native guarantees were audited
/// against. Native execution accepts this release and later patch releases
/// of its minor line (`0.40.P`, `P >= 6`).
pub const AUDITED_OLLAMA_VERSION: &str = "0.40.6";

const ACCEPTED_MAJOR: u64 = 0;
const ACCEPTED_MINOR: u64 = 40;
const FIRST_ACCEPTED_PATCH: u64 = 6;

/// The last release native execution accepted before 0.40, refused now with
/// its own explanation.
const PREVIOUS_AUDITED_VERSION: &str = "0.20.6";

/// Releases of the accepted line that are refused anyway, because one breaks a
/// guarantee native execution relies on. Empty: none is known.
pub const DENIED_OLLAMA_VERSIONS: &[&str] = &[];

/// Why native execution refuses an Ollama server that reports `version`, or
/// `None` when the version is accepted.
pub fn ollama_version_refusal(version: &str) -> Option<String> {
    refusal_with(version, DENIED_OLLAMA_VERSIONS)
}

/// Native execution accepts an Ollama server that reports `version`.
pub fn ollama_version_accepted(version: &str) -> bool {
    ollama_version_refusal(version).is_none()
}

fn refusal_with(version: &str, denied: &[&str]) -> Option<String> {
    let shown: String = version.chars().take(64).collect();
    if version == PREVIOUS_AUDITED_VERSION {
        return Some(format!(
            "Ollama {PREVIOUS_AUDITED_VERSION} is no longer supported: Ollama 0.40 runs models in \
             a different engine (llama.cpp's llama-server) and its registry serves models with new \
             digests. Install Ollama {AUDITED_OLLAMA_VERSION} or later, then review Team & budget \
             again"
        ));
    }
    if version.contains('-') || version.contains('+') {
        return Some(format!(
            "Ollama {shown} is a pre-release or a local build; native Sessions accept only a \
             released Ollama {AUDITED_OLLAMA_VERSION} or a later 0.{ACCEPTED_MINOR} release"
        ));
    }
    let Some([major, minor, patch]) = release_numbers(version) else {
        return Some(format!(
            "Ollama reported the version {shown:?}, which is not a release number; install Ollama \
             {AUDITED_OLLAMA_VERSION} or a later 0.{ACCEPTED_MINOR} release"
        ));
    };
    if (major, minor, patch) == (0, 0, 0) {
        return Some(format!(
            "Ollama 0.0.0 is a development build that does not say which release it is; install \
             a released Ollama {AUDITED_OLLAMA_VERSION} or later"
        ));
    }
    if (major, minor) > (ACCEPTED_MAJOR, ACCEPTED_MINOR) {
        return Some(format!(
            "Ollama {shown} is newer than this Axocoatl supports (Ollama {AUDITED_OLLAMA_VERSION} \
             or a later 0.{ACCEPTED_MINOR} release); upgrade Axocoatl, or install a 0.{ACCEPTED_MINOR} \
             release of Ollama"
        ));
    }
    if (major, minor, patch) < (ACCEPTED_MAJOR, ACCEPTED_MINOR, FIRST_ACCEPTED_PATCH) {
        return Some(format!(
            "Ollama {shown} is older than native Sessions support; upgrade Ollama to \
             {AUDITED_OLLAMA_VERSION} or later"
        ));
    }
    if denied.contains(&version) {
        return Some(format!(
            "Ollama {shown} is refused: this release is known to break a guarantee native \
             Sessions rely on; install another 0.{ACCEPTED_MINOR} release, {AUDITED_OLLAMA_VERSION} \
             or later"
        ));
    }
    None
}

/// `MAJOR.MINOR.PATCH`, each plain decimal digits without a leading zero.
fn release_numbers(version: &str) -> Option<[u64; 3]> {
    let mut numbers = [0u64; 3];
    let mut parts = version.split('.');
    for number in &mut numbers {
        let part = parts.next()?;
        if part.is_empty()
            || part.len() > 9
            || !part.bytes().all(|byte| byte.is_ascii_digit())
            || (part.len() > 1 && part.starts_with('0'))
        {
            return None;
        }
        *number = part.parse().ok()?;
    }
    parts.next().is_none().then_some(numbers)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_audited_release_and_later_patches_of_its_line_are_accepted() {
        for version in [AUDITED_OLLAMA_VERSION, "0.40.6", "0.40.7", "0.40.12"] {
            assert_eq!(ollama_version_refusal(version), None, "{version}");
            assert!(ollama_version_accepted(version));
        }
    }

    #[test]
    fn older_newer_pre_release_and_development_builds_are_refused_with_their_reason() {
        for (version, reason) in [
            (
                "0.40.5",
                "older than native Sessions support; upgrade Ollama to 0.40.6",
            ),
            ("0.39.9", "older than native Sessions support"),
            ("0.12.3", "older than native Sessions support"),
            ("0.41.0", "newer than this Axocoatl supports"),
            ("1.0.0", "newer than this Axocoatl supports"),
            ("0.40.7-rc0", "pre-release"),
            ("0.40.6-rc1", "pre-release"),
            ("0.40.6+local", "pre-release or a local build"),
            ("0.0.0", "development build"),
            ("", "not a release number"),
            ("0.40", "not a release number"),
            ("0.40.06", "not a release number"),
            ("0.40.6.1", "not a release number"),
            ("v0.40.6", "not a release number"),
            ("0.40.x", "not a release number"),
        ] {
            let refusal = ollama_version_refusal(version)
                .unwrap_or_else(|| panic!("{version:?} must be refused"));
            assert!(refusal.contains(reason), "{version:?}: {refusal}");
            assert!(!ollama_version_accepted(version));
        }
    }

    #[test]
    fn the_previous_audited_release_says_why_and_what_to_do() {
        let refusal = ollama_version_refusal("0.20.6").unwrap();
        assert!(refusal.contains("no longer supported"), "{refusal}");
        assert!(refusal.contains("llama-server"), "{refusal}");
        assert!(refusal.contains("new digests"), "{refusal}");
        assert!(
            refusal.contains("Install Ollama 0.40.6 or later"),
            "{refusal}"
        );
        assert!(refusal.contains("review Team & budget again"), "{refusal}");
    }

    #[test]
    fn the_denylist_refuses_an_otherwise_accepted_release() {
        assert!(DENIED_OLLAMA_VERSIONS.is_empty());
        let denied = ["0.40.8"];
        assert_eq!(refusal_with("0.40.7", &denied), None);
        let refusal = refusal_with("0.40.8", &denied).unwrap();
        assert!(refusal.contains("known to break a guarantee"), "{refusal}");
        // The denylist never turns a refused version into an accepted one.
        assert!(refusal_with("0.41.0", &denied).is_some());
    }

    #[test]
    fn a_long_or_hostile_report_is_shortened_in_the_refusal() {
        let refusal = ollama_version_refusal(&"9".repeat(500)).unwrap();
        assert!(refusal.len() < 400, "{refusal}");
    }
}
