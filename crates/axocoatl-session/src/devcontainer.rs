//! Minimal `.devcontainer/devcontainer.json` reader.
//!
//! The spec at <https://containers.dev/implementors/json_reference/> is large.
//! We support only the subset that makes sense for an interactive agent
//! session today:
//!
//! | Field                | Used as                                          |
//! |----------------------|--------------------------------------------------|
//! | `image`              | base image for the session sandbox               |
//! | `postCreateCommand`  | one-shot command run after first container start |
//! | `forwardPorts`       | merged into the session's exposed_ports          |
//! | `containerEnv`       | env vars set inside the container                |
//!
//! Everything else (`build`, `features`, `mounts`, `runArgs`, ...) is
//! recognised but ignored. We log a hint when we see them so users know
//! we're not silently honoring richer config.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Subset of the devcontainer.json spec that Axocoatl honors today.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct DevContainer {
    /// OCI image to use as the session sandbox base.
    #[serde(default)]
    pub image: Option<String>,

    /// One or more shell commands to run after the container first boots.
    /// The spec lets this be a single string, a string array, or a map of
    /// named commands — we accept all three.
    #[serde(default)]
    pub post_create_command: Option<PostCreateCommand>,

    /// Ports the project author wants exposed. We merge these with the
    /// session's `exposed_ports` (deduplicating) before starting podman.
    #[serde(default)]
    pub forward_ports: Vec<ForwardPort>,

    /// Env vars set inside the container.
    #[serde(default)]
    pub container_env: std::collections::BTreeMap<String, String>,

    /// Anything else the spec defines but we don't act on — captured here so
    /// we can surface "X was ignored" warnings rather than silently dropping
    /// fields the user expected to matter.
    #[serde(flatten)]
    pub extras: serde_json::Map<String, serde_json::Value>,
}

/// `postCreateCommand` is polymorphic in the spec.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(untagged)]
pub enum PostCreateCommand {
    /// `"postCreateCommand": "pip install -r requirements.txt"`
    Shell(String),
    /// `"postCreateCommand": ["sh", "-c", "..."]`
    Argv(Vec<String>),
    /// `"postCreateCommand": { "deps": "...", "lint": "..." }` — runs in order
    /// by alphabetical key (the spec doesn't guarantee an order; we pick one).
    Named(std::collections::BTreeMap<String, String>),
}

/// `forwardPorts` can be a number or a `"host:guest"` string.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(untagged)]
pub enum ForwardPort {
    Number(u16),
    Mapping(String),
}

impl DevContainer {
    /// Load a `devcontainer.json` from the conventional locations under
    /// `working_dir`. Returns `Ok(None)` if no file exists (the common case
    /// in projects without a devcontainer). Returns `Err` only when a file
    /// exists but can't be parsed — the user wants to know about *that*.
    pub fn load(working_dir: &Path) -> Result<Option<(PathBuf, Self)>, DevContainerError> {
        for rel in [".devcontainer/devcontainer.json", ".devcontainer.json"] {
            let path = working_dir.join(rel);
            if !path.exists() {
                continue;
            }
            let text = std::fs::read_to_string(&path).map_err(DevContainerError::Io)?;
            // devcontainer.json is JSONC in the spec; strip line/block comments
            // and trailing commas before handing to serde_json.
            let stripped = strip_jsonc(&text);
            let parsed: DevContainer = serde_json::from_str(&stripped)
                .map_err(|e| DevContainerError::Parse(path.display().to_string(), e.to_string()))?;
            return Ok(Some((path, parsed)));
        }
        Ok(None)
    }

    /// Names of devcontainer fields we recognise but don't honor in v0.1 —
    /// for surfacing a "we saw this but ignored it" notice to the user.
    pub fn ignored_fields(&self) -> Vec<&str> {
        const KNOWN_BUT_IGNORED: &[&str] = &[
            "build",
            "dockerFile",
            "context",
            "features",
            "mounts",
            "runArgs",
            "customizations",
            "remoteUser",
            "containerUser",
            "workspaceFolder",
            "workspaceMount",
            "initializeCommand",
            "onCreateCommand",
            "updateContentCommand",
            "postStartCommand",
            "postAttachCommand",
        ];
        KNOWN_BUT_IGNORED
            .iter()
            .copied()
            .filter(|k| self.extras.contains_key(*k))
            .collect()
    }

    /// The `postCreateCommand` flattened to one or more shell-executable
    /// strings, run in order inside `sh -c`. Empty when unset.
    pub fn post_create_scripts(&self) -> Vec<String> {
        match &self.post_create_command {
            None => Vec::new(),
            Some(PostCreateCommand::Shell(s)) => vec![s.clone()],
            Some(PostCreateCommand::Argv(parts)) => {
                // Re-quote conservatively. Most ARGV forms in the wild are
                // `["sh", "-c", "..."]` — joining with a space recovers the
                // original intent for our `sh -c` execution path.
                vec![shell_quote(parts)]
            }
            Some(PostCreateCommand::Named(m)) => m.values().cloned().collect(),
        }
    }

    /// `forwardPorts` flattened to a set of host ports. Mapping strings of
    /// the form `"host:guest"` contribute their host side.
    pub fn forwarded_ports(&self) -> Vec<u16> {
        self.forward_ports
            .iter()
            .filter_map(|p| match p {
                ForwardPort::Number(n) => Some(*n),
                ForwardPort::Mapping(s) => s.split(':').next().and_then(|h| h.parse::<u16>().ok()),
            })
            .collect()
    }
}

/// Conservative JSONC → JSON: strips `//` line comments, `/* … */` block
/// comments, and trailing commas. Doesn't touch strings; only operates
/// outside quotes.
fn strip_jsonc(src: &str) -> String {
    let bytes = src.as_bytes();
    let mut out = String::with_capacity(src.len());
    let mut i = 0;
    let mut in_str = false;
    let mut esc = false;
    while i < bytes.len() {
        let c = bytes[i];
        if in_str {
            out.push(c as char);
            if esc {
                esc = false;
            } else if c == b'\\' {
                esc = true;
            } else if c == b'"' {
                in_str = false;
            }
            i += 1;
            continue;
        }
        // Outside string: hunt for comments and trailing commas.
        if c == b'"' {
            in_str = true;
            out.push('"');
            i += 1;
            continue;
        }
        if c == b'/' && i + 1 < bytes.len() {
            if bytes[i + 1] == b'/' {
                // Line comment — skip to newline.
                i += 2;
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
                continue;
            }
            if bytes[i + 1] == b'*' {
                // Block comment — skip to closing.
                i += 2;
                while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                    i += 1;
                }
                i = (i + 2).min(bytes.len());
                continue;
            }
        }
        if c == b',' {
            // Trailing comma? Peek past whitespace for `}` or `]`.
            let mut j = i + 1;
            while j < bytes.len() && (bytes[j] as char).is_whitespace() {
                j += 1;
            }
            if j < bytes.len() && (bytes[j] == b'}' || bytes[j] == b']') {
                // Drop the comma.
                i += 1;
                continue;
            }
        }
        out.push(c as char);
        i += 1;
    }
    out
}

fn shell_quote(parts: &[String]) -> String {
    parts
        .iter()
        .map(|p| {
            if p.chars()
                .all(|c| c.is_alphanumeric() || "_-./=:".contains(c))
            {
                p.clone()
            } else {
                format!("'{}'", p.replace('\'', "'\\''"))
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

#[derive(Debug, thiserror::Error)]
pub enum DevContainerError {
    #[error("reading devcontainer file: {0}")]
    Io(#[from] std::io::Error),
    #[error("parsing {0}: {1}")]
    Parse(String, String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_minimal_image_only() {
        let json = r#"{ "image": "python:3.12-slim" }"#;
        let dc: DevContainer = serde_json::from_str(json).unwrap();
        assert_eq!(dc.image.as_deref(), Some("python:3.12-slim"));
        assert!(dc.post_create_scripts().is_empty());
        assert!(dc.forwarded_ports().is_empty());
    }

    #[test]
    fn parses_jsonc_with_comments_and_trailing_commas() {
        let json = r#"
            {
                // The base image
                "image": "node:20-slim",
                /* run our setup */
                "postCreateCommand": "npm ci",
                "forwardPorts": [3000, 5173,],
            }
        "#;
        let stripped = strip_jsonc(json);
        let dc: DevContainer = serde_json::from_str(&stripped).unwrap();
        assert_eq!(dc.image.as_deref(), Some("node:20-slim"));
        assert_eq!(dc.post_create_scripts(), vec!["npm ci".to_string()]);
        assert_eq!(dc.forwarded_ports(), vec![3000, 5173]);
    }

    #[test]
    fn forward_ports_accepts_mapping_strings() {
        let json = r#"{ "forwardPorts": [3000, "8080:80"] }"#;
        let dc: DevContainer = serde_json::from_str(json).unwrap();
        assert_eq!(dc.forwarded_ports(), vec![3000, 8080]);
    }

    #[test]
    fn flags_recognised_but_unhonored_fields() {
        let json = r#"{ "image": "x", "features": {"a":{}}, "mounts": [] }"#;
        let dc: DevContainer = serde_json::from_str(json).unwrap();
        let ignored = dc.ignored_fields();
        assert!(ignored.contains(&"features"));
        assert!(ignored.contains(&"mounts"));
    }
}
