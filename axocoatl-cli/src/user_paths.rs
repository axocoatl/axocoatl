//! User-level locations for Axocoatl's managed configuration and durable data.
//!
//! Explicit command-line configuration paths remain a caller concern. This
//! module resolves only the defaults used by the installed product.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

/// Platform convention used to resolve Axocoatl's user-level paths.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum UserPathPlatform {
    MacOs,
    Linux,
}

/// Axocoatl's managed user-level configuration and data locations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct UserPaths {
    pub(crate) config_dir: PathBuf,
    pub(crate) config_path: PathBuf,
    pub(crate) data_dir: PathBuf,
}

impl UserPaths {
    /// Resolve paths from the current process environment.
    pub(crate) fn discover() -> Result<Self, String> {
        let home = std::env::var_os("HOME");
        let xdg_config_home = std::env::var_os("XDG_CONFIG_HOME");
        let xdg_data_home = std::env::var_os("XDG_DATA_HOME");

        // `cfg!` keeps both constructors visible to the compiler, while still
        // selecting the platform convention at compile time.
        let platform = if cfg!(target_os = "macos") {
            UserPathPlatform::MacOs
        } else {
            UserPathPlatform::Linux
        };

        Self::from_environment(
            platform,
            home.as_deref(),
            xdg_config_home.as_deref(),
            xdg_data_home.as_deref(),
        )
    }

    /// Pure path resolution for callers and tests.
    ///
    /// Empty environment values are treated as unset. On Linux, `HOME` is
    /// required only for a path whose corresponding XDG override is absent.
    pub(crate) fn from_environment(
        platform: UserPathPlatform,
        home: Option<&OsStr>,
        xdg_config_home: Option<&OsStr>,
        xdg_data_home: Option<&OsStr>,
    ) -> Result<Self, String> {
        let home = nonempty(home);
        // The XDG Base Directory specification requires absolute values;
        // relative overrides are invalid and must be ignored.
        let xdg_config_home =
            nonempty(xdg_config_home).filter(|path| Path::new(path).is_absolute());
        let xdg_data_home = nonempty(xdg_data_home).filter(|path| Path::new(path).is_absolute());

        match platform {
            UserPathPlatform::MacOs => {
                let home = absolute_home(home, "macOS Application Support directory")?;
                let root = PathBuf::from(home)
                    .join("Library")
                    .join("Application Support")
                    .join("Axocoatl");
                Ok(Self {
                    config_dir: root.clone(),
                    config_path: root.join("config.yaml"),
                    data_dir: root.join("data"),
                })
            }
            UserPathPlatform::Linux => {
                let config_home = match xdg_config_home {
                    Some(path) => PathBuf::from(path),
                    None => PathBuf::from(absolute_home(home, "configuration directory")?)
                        .join(".config"),
                };
                let data_home = match xdg_data_home {
                    Some(path) => PathBuf::from(path),
                    None => PathBuf::from(absolute_home(home, "data directory")?)
                        .join(".local")
                        .join("share"),
                };
                let config_dir = config_home.join("axocoatl");
                Ok(Self {
                    config_path: config_dir.join("config.yaml"),
                    config_dir,
                    data_dir: data_home.join("axocoatl"),
                })
            }
        }
    }
}

fn absolute_home<'a>(home: Option<&'a OsStr>, purpose: &str) -> Result<&'a OsStr, String> {
    match home {
        Some(home) if Path::new(home).is_absolute() => Ok(home),
        Some(_) => Err(format!(
            "cannot determine Axocoatl's {purpose}: HOME must be an absolute path"
        )),
        None => Err(missing_home(purpose)),
    }
}

fn missing_home(purpose: &str) -> String {
    format!(
        "cannot determine Axocoatl's {purpose}: HOME is not set and no applicable XDG override is available"
    )
}

fn nonempty(value: Option<&OsStr>) -> Option<&OsStr> {
    value.filter(|value| !value.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;
    use std::path::Path;

    #[test]
    fn macos_uses_application_support_and_ignores_xdg_overrides() {
        let paths = UserPaths::from_environment(
            UserPathPlatform::MacOs,
            Some(OsStr::new("/Users/alex")),
            Some(OsStr::new("/tmp/config")),
            Some(OsStr::new("/tmp/data")),
        )
        .unwrap();

        assert_eq!(
            paths.config_dir,
            Path::new("/Users/alex/Library/Application Support/Axocoatl")
        );
        assert_eq!(
            paths.config_path,
            Path::new("/Users/alex/Library/Application Support/Axocoatl/config.yaml")
        );
        assert_eq!(
            paths.data_dir,
            Path::new("/Users/alex/Library/Application Support/Axocoatl/data")
        );
    }

    #[test]
    fn linux_uses_independent_xdg_overrides() {
        let paths = UserPaths::from_environment(
            UserPathPlatform::Linux,
            None,
            Some(OsStr::new("/srv/config")),
            Some(OsStr::new("/srv/data")),
        )
        .unwrap();

        assert_eq!(paths.config_dir, Path::new("/srv/config/axocoatl"));
        assert_eq!(
            paths.config_path,
            Path::new("/srv/config/axocoatl/config.yaml")
        );
        assert_eq!(paths.data_dir, Path::new("/srv/data/axocoatl"));
    }

    #[test]
    fn linux_falls_back_to_home_when_xdg_values_are_absent() {
        let paths = UserPaths::from_environment(
            UserPathPlatform::Linux,
            Some(OsStr::new("/home/alex")),
            None,
            None,
        )
        .unwrap();

        assert_eq!(
            paths.config_path,
            Path::new("/home/alex/.config/axocoatl/config.yaml")
        );
        assert_eq!(
            paths.data_dir,
            Path::new("/home/alex/.local/share/axocoatl")
        );
    }

    #[test]
    fn empty_xdg_values_use_the_home_fallback() {
        let paths = UserPaths::from_environment(
            UserPathPlatform::Linux,
            Some(OsStr::new("/home/alex")),
            Some(OsStr::new("")),
            Some(OsStr::new("")),
        )
        .unwrap();

        assert_eq!(
            paths.config_path,
            Path::new("/home/alex/.config/axocoatl/config.yaml")
        );
        assert_eq!(
            paths.data_dir,
            Path::new("/home/alex/.local/share/axocoatl")
        );
    }

    #[test]
    fn relative_xdg_values_are_ignored_in_favor_of_absolute_home() {
        let paths = UserPaths::from_environment(
            UserPathPlatform::Linux,
            Some(OsStr::new("/home/alex")),
            Some(OsStr::new("relative-config")),
            Some(OsStr::new("relative-data")),
        )
        .unwrap();

        assert_eq!(
            paths.config_path,
            Path::new("/home/alex/.config/axocoatl/config.yaml")
        );
        assert_eq!(
            paths.data_dir,
            Path::new("/home/alex/.local/share/axocoatl")
        );
    }

    #[test]
    fn relative_home_is_rejected_when_a_fallback_is_needed() {
        let error = UserPaths::from_environment(
            UserPathPlatform::Linux,
            Some(OsStr::new("relative-home")),
            None,
            None,
        )
        .unwrap_err();

        assert!(error.contains("HOME must be an absolute path"));
    }

    #[test]
    fn macos_reports_why_home_is_required() {
        let error = UserPaths::from_environment(UserPathPlatform::MacOs, None, None, None)
            .unwrap_err()
            .to_string();

        assert!(error.contains("macOS Application Support directory"));
        assert!(error.contains("HOME is not set"));
    }

    #[test]
    fn linux_reports_which_fallback_cannot_be_resolved() {
        let error = UserPaths::from_environment(
            UserPathPlatform::Linux,
            None,
            Some(OsStr::new("/srv/config")),
            None,
        )
        .unwrap_err()
        .to_string();

        assert!(error.contains("data directory"));
        assert!(error.contains("HOME is not set"));
    }

    #[test]
    fn os_strings_are_preserved_without_lossy_conversion() {
        let home = OsString::from("/home/alex");
        let paths = UserPaths::from_environment(
            UserPathPlatform::Linux,
            Some(home.as_os_str()),
            None,
            None,
        )
        .unwrap();

        assert_eq!(
            paths.config_path,
            Path::new("/home/alex/.config/axocoatl/config.yaml")
        );
    }
}
