//! Named host lists for `sandbox.egress.allow` and `browser.allow`.
//!
//! The host lists are UNVERIFIED until the gated live egress test
//! (`AXOCOATL_LIVE_EGRESS=1`) passes against the real registries. Correct them
//! from that test's refusals, never by guessing.

/// One preset: the hosts and ports it allows, and what the user should know.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Preset {
    pub name: &'static str,
    pub hosts: &'static [(&'static str, &'static [u16])],
    /// The hosts sit behind a shared CDN, so a tunnel to them can reach other
    /// sites on that CDN.
    pub cdn_fronted: bool,
    /// The service accepts uploads (publish, push), so data can leave through it.
    pub write_capable: bool,
}

const HTTPS: &[u16] = &[443];
const HTTP_AND_HTTPS: &[u16] = &[80, 443];

pub const PRESETS: [Preset; 9] = [
    Preset {
        name: "npm",
        hosts: &[("registry.npmjs.org", HTTPS)],
        cdn_fronted: true,
        write_capable: true,
    },
    Preset {
        name: "yarn",
        hosts: &[("registry.yarnpkg.com", HTTPS), ("repo.yarnpkg.com", HTTPS)],
        cdn_fronted: true,
        write_capable: false,
    },
    Preset {
        name: "pypi",
        hosts: &[("pypi.org", HTTPS), ("files.pythonhosted.org", HTTPS)],
        cdn_fronted: true,
        write_capable: true,
    },
    Preset {
        name: "crates",
        hosts: &[
            ("index.crates.io", HTTPS),
            ("static.crates.io", HTTPS),
            ("crates.io", HTTPS),
        ],
        cdn_fronted: true,
        write_capable: true,
    },
    Preset {
        name: "go",
        hosts: &[("proxy.golang.org", HTTPS), ("sum.golang.org", HTTPS)],
        cdn_fronted: true,
        write_capable: false,
    },
    Preset {
        name: "github",
        hosts: &[
            ("github.com", HTTPS),
            ("api.github.com", HTTPS),
            ("codeload.github.com", HTTPS),
            ("objects.githubusercontent.com", HTTPS),
            ("raw.githubusercontent.com", HTTPS),
        ],
        cdn_fronted: true,
        write_capable: true,
    },
    Preset {
        name: "alpine",
        hosts: &[("dl-cdn.alpinelinux.org", HTTP_AND_HTTPS)],
        cdn_fronted: true,
        write_capable: false,
    },
    Preset {
        name: "debian",
        hosts: &[
            ("deb.debian.org", HTTP_AND_HTTPS),
            ("security.debian.org", HTTP_AND_HTTPS),
        ],
        cdn_fronted: true,
        write_capable: false,
    },
    Preset {
        name: "ubuntu",
        hosts: &[
            ("archive.ubuntu.com", HTTP_AND_HTTPS),
            ("security.ubuntu.com", HTTP_AND_HTTPS),
            ("ports.ubuntu.com", HTTP_AND_HTTPS),
        ],
        cdn_fronted: false,
        write_capable: false,
    },
];

/// The presets readiness provisioning may use to install missing commands.
pub const DISTRO_PRESETS: [&str; 3] = ["alpine", "debian", "ubuntu"];

/// The preset with exactly this name.
pub fn preset(name: &str) -> Option<&'static Preset> {
    PRESETS.iter().find(|preset| preset.name == name)
}

/// Every preset name, for messages.
pub fn preset_names() -> Vec<&'static str> {
    PRESETS.iter().map(|preset| preset.name).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_preset_host_is_a_valid_name_with_ports() {
        for preset in &PRESETS {
            assert!(!preset.hosts.is_empty(), "{}", preset.name);
            for (host, ports) in preset.hosts {
                assert_eq!(
                    axocoatl_core::netaddr::normalize_host_name(host).as_deref(),
                    Ok(*host),
                    "{}",
                    preset.name
                );
                assert!(!ports.is_empty() && !ports.contains(&0), "{host}");
            }
        }
    }

    #[test]
    fn names_are_unique_and_distro_presets_exist() {
        let mut names = preset_names();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), PRESETS.len());
        for name in DISTRO_PRESETS {
            assert!(preset(name).is_some(), "{name}");
        }
        assert!(preset("NPM").is_none());
        assert!(preset("unknown").is_none());
    }
}
