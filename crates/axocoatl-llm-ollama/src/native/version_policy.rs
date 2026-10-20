//! Which Ollama server versions native execution accepts.
//!
//! The native guarantees (an over-long prompt is refused before generation
//! with `truncate:false`, a positive `num_predict` stops generation, and
//! `shift:false` never slides the context) were audited in the Ollama 0.40.6
//! source, where every GGUF model runs in llama.cpp's `llama-server`
//! (`llm/server.go` `NewLlamaServer`; `server/sched.go` uses it for every model
//! that is not safetensors). Later releases of 0.40 and later 0.x minor
//! releases are accepted as well, because native execution never trusts the
//! audit alone: before a Team or a run uses a model, the contract probes
//! (`contract_probe`) check both guarantees on the person's own server and
//! model, and a server whose version changed is probed again before it
//! serves. A release newer than the audited minor line is accepted only
//! through those probes and is reported as unaudited
//! ([`ollama_version_unaudited`]): `doctor` warns about it, onboarding says
//! so, and a run's receipt records it. Pre-releases, development builds,
//! releases older than the audited one, a new major release and the releases
//! on [`DENIED_OLLAMA_VERSIONS`] are refused.

/// The Ollama release whose source the native guarantees were audited
/// against. Native execution accepts this release, later patch releases of
/// its minor line (`0.40.P`, `P >= 6`) and, through the contract probes,
/// later 0.x minor releases.
pub const AUDITED_OLLAMA_VERSION: &str = "0.40.6";

const ACCEPTED_MAJOR: u64 = 0;
const AUDITED_MINOR: u64 = 40;
const FIRST_ACCEPTED_PATCH: u64 = 6;

/// The last release native execution accepted before 0.40, refused now with
/// its own explanation.
const PREVIOUS_AUDITED_VERSION: &str = "0.20.6";

/// Releases that are refused anyway, because one breaks a guarantee native
/// execution relies on. Empty: none is known.
pub const DENIED_OLLAMA_VERSIONS: &[&str] = &[];

/// Why native execution refuses an Ollama server that reports `version`, or
/// `None` when the version is accepted. An accepted version newer than the
/// audited minor line is still unaudited ([`ollama_version_unaudited`]); the
/// contract probes decide whether a model may run on it.
pub fn ollama_version_refusal(version: &str) -> Option<String> {
    refusal_with(version, DENIED_OLLAMA_VERSIONS)
}

/// Native execution accepts an Ollama server that reports `version`.
pub fn ollama_version_accepted(version: &str) -> bool {
    ollama_version_refusal(version).is_none()
}

/// `version` is accepted but newer than the audited minor line (0.41 or a
/// later 0.x release): native execution runs a model on it only after the
/// contract probes pass for that model, and says the version is unaudited.
pub fn ollama_version_unaudited(version: &str) -> bool {
    ollama_version_accepted(version)
        && release_numbers(version)
            .is_some_and(|[major, minor, _]| major == ACCEPTED_MAJOR && minor > AUDITED_MINOR)
}

/// What setup, `doctor` and a receipt say about an accepted version that is
/// newer than the audited one, or `None` for an audited or refused version.
pub fn ollama_version_note(version: &str) -> Option<String> {
    ollama_version_unaudited(version).then(|| {
        let shown: String = version.chars().take(64).collect();
        format!(
            "Ollama {shown} is newer than the audited {AUDITED_OLLAMA_VERSION}; native Sessions \
             and runs use a model on it only after the contract probes pass for that model"
        )
    })
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
             released Ollama {AUDITED_OLLAMA_VERSION} or a later 0.x release"
        ));
    }
    let Some([major, minor, patch]) = release_numbers(version) else {
        return Some(format!(
            "Ollama reported the version {shown:?}, which is not a release number; install Ollama \
             {AUDITED_OLLAMA_VERSION} or a later 0.x release"
        ));
    };
    if (major, minor, patch) == (0, 0, 0) {
        return Some(format!(
            "Ollama 0.0.0 is a development build that does not say which release it is; install \
             a released Ollama {AUDITED_OLLAMA_VERSION} or later"
        ));
    }
    if major > ACCEPTED_MAJOR {
        return Some(format!(
            "Ollama {shown} is a new major release, which this Axocoatl has not been checked \
             against; install Ollama {AUDITED_OLLAMA_VERSION} or a later 0.x release, or upgrade \
             Axocoatl"
        ));
    }
    if (major, minor, patch) < (ACCEPTED_MAJOR, AUDITED_MINOR, FIRST_ACCEPTED_PATCH) {
        return Some(format!(
            "Ollama {shown} is older than native Sessions support; upgrade Ollama to \
             {AUDITED_OLLAMA_VERSION} or later"
        ));
    }
    if denied.contains(&version) {
        return Some(format!(
            "Ollama {shown} is refused: this release is known to break a guarantee native \
             Sessions rely on; install {AUDITED_OLLAMA_VERSION} or another release that is not \
             refused"
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
    fn the_audited_release_and_later_patches_of_its_line_are_accepted_and_audited() {
        for version in [AUDITED_OLLAMA_VERSION, "0.40.6", "0.40.7", "0.40.12"] {
            assert_eq!(ollama_version_refusal(version), None, "{version}");
            assert!(ollama_version_accepted(version));
            assert!(!ollama_version_unaudited(version), "{version}");
            assert_eq!(ollama_version_note(version), None, "{version}");
        }
    }

    /// Ollama ships a minor release every few weeks and Homebrew and the
    /// app serve only the latest: a newer 0.x minor is accepted, unaudited,
    /// and the contract probes decide for each model.
    #[test]
    fn a_newer_minor_release_is_accepted_as_unaudited() {
        for version in ["0.41.0", "0.41.3", "0.52.1", "0.99.0"] {
            assert_eq!(ollama_version_refusal(version), None, "{version}");
            assert!(ollama_version_accepted(version));
            assert!(ollama_version_unaudited(version), "{version}");
            let note = ollama_version_note(version).unwrap();
            assert!(note.contains(&format!(
                "Ollama {version} is newer than the audited 0.40.6"
            )));
            assert!(note.contains("contract probes pass"), "{note}");
        }
    }

    #[test]
    fn older_major_pre_release_and_development_builds_are_refused_with_their_reason() {
        for (version, reason) in [
            (
                "0.40.5",
                "older than native Sessions support; upgrade Ollama to 0.40.6",
            ),
            ("0.39.9", "older than native Sessions support"),
            ("0.12.3", "older than native Sessions support"),
            ("1.0.0", "new major release"),
            ("2.3.4", "new major release"),
            ("0.40.7-rc0", "pre-release"),
            ("0.41.0-rc1", "pre-release"),
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
            assert!(!ollama_version_unaudited(version), "{version:?}");
            assert_eq!(ollama_version_note(version), None, "{version:?}");
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
        let denied = ["0.40.8", "0.41.2"];
        assert_eq!(refusal_with("0.40.7", &denied), None);
        assert_eq!(refusal_with("0.41.1", &denied), None);
        for version in denied {
            let refusal = refusal_with(version, &denied).unwrap();
            assert!(refusal.contains("known to break a guarantee"), "{refusal}");
        }
        // The denylist never turns a refused version into an accepted one.
        assert!(refusal_with("1.0.0", &denied).is_some());
        assert!(refusal_with("0.40.5", &denied).is_some());
    }

    #[test]
    fn a_long_or_hostile_report_is_shortened_in_the_refusal() {
        let refusal = ollama_version_refusal(&"9".repeat(500)).unwrap();
        assert!(refusal.len() < 400, "{refusal}");
    }
}
