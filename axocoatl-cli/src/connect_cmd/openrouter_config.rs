//! The configuration lines `axocoatl connect openrouter` writes: under
//! `providers:`, `openrouter_billing: credits` and an `openrouter:` block
//! whose `api_key_secret` names the secret that holds the key. The key
//! itself is never in the file.
//!
//! The file is edited as text, so its comments, order and every other
//! line stay as they are. Only a plain block-style top-level `providers:`
//! mapping (or none, when the block is added at the end) is edited:
//! `openrouter_billing`'s line is replaced or added, and in `openrouter:`
//! any `api_key` (a key written into the file earlier) and `api_key_secret`
//! entries are replaced by the one `api_key_secret` line, keeping its other
//! entries. The result is read back and must equal the original document
//! apart from exactly those entries, and must still load as a
//! configuration when the original did; otherwise nothing is written and
//! the lines to add are shown instead ([`manual_lines`], which hold no
//! value). The file is replaced atomically with its own permissions.
//!
//! When there is no configuration yet at the user's configuration path,
//! [`create`] writes [`new_configuration`]: these entries and the server's
//! defaults, no Agents, in a new `0600` file it never writes over.

use std::io::Write as _;
use std::path::Path;

use axocoatl_config::yaml::Value;

use crate::user_paths::UserPaths;

/// OpenRouter's whole part of `providers:`, two spaces in, for the secret
/// `secret`: the `openrouter_billing:` line and the `openrouter:` block. A
/// configuration template puts it in place of both, not only of an
/// `api_key:` line.
pub(crate) fn provider_yaml(secret: &str) -> String {
    format!("  openrouter_billing: credits\n  openrouter:\n    api_key_secret: {secret}\n")
}

/// A configuration for a computer that has none: OpenRouter's entries and
/// the server's defaults. It has no Agents: `axocoatl run` takes its models
/// with `--model`, and `axocoatl onboard` writes a team for workbench
/// Sessions.
pub(crate) fn new_configuration(secret: &str) -> String {
    format!(
        "# Axocoatl — written by `axocoatl connect openrouter`.\n\
         # OpenRouter's key is in Axocoatl's secret store (secret {secret}), never in this file.\n\
         # Native Sessions use OpenRouter credits; BYOK is not supported.\n\
         # `axocoatl run` takes its models with --model; `axocoatl onboard` writes a default\n\
         # team of Agents for workbench Sessions.\n\
         {}\n\
         server:\n  port: 8080\n  host: \"127.0.0.1\"\n",
        manual_lines(secret)
    )
}

/// Write `text` as the configuration at `paths.config_path`: its directory
/// created if needed and kept private, the file new (never written over:
/// one that appeared since is an error) with `0600`.
pub(crate) fn create(paths: &UserPaths, text: &str) -> Result<(), String> {
    let name = paths
        .config_path
        .file_name()
        .ok_or_else(|| format!("{} names no file", paths.config_path.display()))?;
    let directory = axocoatl_core::SecureDir::open_or_create_all(&paths.config_dir)
        .and_then(|directory| crate::admit_private_directory(&directory).map(|()| directory))
        .map_err(|error| format!("{}: {error}", paths.config_dir.display()))?;
    let mut file = directory.create_new(name).map_err(|error| {
        if error.kind() == std::io::ErrorKind::AlreadyExists {
            format!(
                "{} appeared during the sign-in, so it was not written over",
                paths.config_path.display()
            )
        } else {
            format!("creating {}: {error}", paths.config_path.display())
        }
    })?;
    let written = file
        .write_all(text.as_bytes())
        .and_then(|()| file.sync_all());
    if let Err(error) = written {
        drop(file);
        let _ = directory.remove_file(name);
        return Err(format!("writing {}: {error}", paths.config_path.display()));
    }
    Ok(())
}

/// The ids of the Agents in the configuration `text` whose provider is
/// OpenRouter.
pub(crate) fn openrouter_agents(text: &str) -> Vec<String> {
    agents(text)
        .into_iter()
        .filter(|(_, provider, _)| provider == "openrouter")
        .map(|(id, _, _)| id)
        .collect()
}

/// The model the configuration's Lead writes with, as `provider:model`
/// for `--model writer=…`, when it runs natively (Ollama or OpenRouter):
/// the Agent `lead`, or else the first Agent on one of those.
pub(crate) fn lead_writer(text: &str) -> Option<String> {
    let agents = agents(text);
    let native = |provider: &str| provider == "ollama" || provider == "openrouter";
    agents
        .iter()
        .find(|(id, _, _)| id == "lead")
        .filter(|(_, provider, _)| native(provider))
        .or_else(|| agents.iter().find(|(_, provider, _)| native(provider)))
        .map(|(_, provider, model)| format!("{provider}:{model}"))
}

/// Each Agent's (id, provider, model) in the configuration `text`.
fn agents(text: &str) -> Vec<(String, String, String)> {
    let Ok(document) = axocoatl_config::yaml::from_str::<Value>(text) else {
        return Vec::new();
    };
    let Some(Value::Sequence(agents)) = document.get("agents") else {
        return Vec::new();
    };
    let field = |agent: &Value, name: &str| match agent.get(name) {
        Some(Value::String(text)) => text.clone(),
        _ => String::new(),
    };
    agents
        .iter()
        .map(|agent| {
            (
                field(agent, "id"),
                field(agent, "provider"),
                field(agent, "model"),
            )
        })
        .filter(|(id, provider, model)| !id.is_empty() && !provider.is_empty() && !model.is_empty())
        .collect()
}

/// [`provider_yaml`] under its `providers:` line, for a message.
pub(crate) fn manual_lines(secret: &str) -> String {
    format!("providers:\n{}", provider_yaml(secret))
}

/// One line of the file, without its line break.
struct Line<'a> {
    text: &'a str,
    /// The text and its line break.
    whole: &'a str,
}

fn lines(text: &str) -> Vec<Line<'_>> {
    text.split_inclusive('\n')
        .map(|whole| Line {
            text: whole.trim_end_matches(['\n', '\r']),
            whole,
        })
        .collect()
}

/// Spaces before the text, or `None` when a tab is among them.
fn indent(text: &str) -> Option<usize> {
    let spaces = text.len() - text.trim_start_matches(' ').len();
    (!text[spaces..].starts_with('\t')).then_some(spaces)
}

/// Blank or only a comment.
fn is_filler(text: &str) -> bool {
    let trimmed = text.trim_start();
    trimmed.is_empty() || trimmed.starts_with('#')
}

/// `key:` at the start of `text` (after `indent` spaces), and whether only
/// a comment or nothing follows the colon.
fn key_at(text: &str, indent: usize) -> Option<(&str, bool)> {
    let rest = text.get(indent..)?;
    let end = rest
        .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == '-'))
        .unwrap_or(rest.len());
    let (key, after) = rest.split_at(end);
    if key.is_empty() {
        return None;
    }
    let after = after.trim_start_matches(' ').strip_prefix(':')?;
    let value = after.trim();
    Some((key, value.is_empty() || value.starts_with('#')))
}

/// The end (exclusive) of the entry whose key line is `start`: the first
/// later content line indented `limit` or less.
fn entry_end(lines: &[Line<'_>], start: usize, limit: usize) -> Result<usize, String> {
    for (index, line) in lines.iter().enumerate().skip(start + 1) {
        if is_filler(line.text) {
            continue;
        }
        let spaces = indent(line.text).ok_or("a tab indents a line")?;
        if spaces <= limit {
            return Ok(index);
        }
    }
    Ok(lines.len())
}

/// The last content line before `end`, from `start`, plus one: where to
/// insert so trailing blank lines and comments stay after the insertion.
fn after_content(lines: &[Line<'_>], start: usize, end: usize) -> usize {
    (start..end)
        .rev()
        .find(|&index| !is_filler(lines[index].text))
        .map_or(start, |index| index + 1)
}

/// The document `text` with the entries this edit changes taken out:
/// `providers.openrouter_billing`, and `providers.openrouter.api_key` and
/// `api_key_secret` (the `openrouter` block itself when only those were in
/// it, and `providers` when it is then empty).
fn without_edited(text: &str) -> Result<Value, String> {
    let mut document: Value =
        axocoatl_config::yaml::from_str(text).map_err(|error| match error.location() {
            Some(at) => format!(
                "it is not valid YAML (line {} column {})",
                at.line(),
                at.column()
            ),
            None => "it is not valid YAML".to_string(),
        })?;
    if matches!(document, Value::Null) {
        document = Value::Mapping(Vec::new());
    }
    let Value::Mapping(root) = &mut document else {
        return Err("its top level is not a mapping".into());
    };
    let is = |key: &Value, name: &str| matches!(key, Value::String(text) if text == name);
    if let Some(index) = root.iter().position(|(key, _)| is(key, "providers")) {
        let providers = &mut root[index].1;
        if matches!(providers, Value::Null) {
            *providers = Value::Mapping(Vec::new());
        }
        let Value::Mapping(entries) = providers else {
            return Err("its providers: is not a mapping".into());
        };
        entries.retain(|(key, _)| !is(key, "openrouter_billing"));
        if let Some(openrouter) = entries.iter().position(|(key, _)| is(key, "openrouter")) {
            match &mut entries[openrouter].1 {
                Value::Mapping(inner) => {
                    inner.retain(|(key, _)| !is(key, "api_key") && !is(key, "api_key_secret"));
                    if inner.is_empty() {
                        entries.remove(openrouter);
                    }
                }
                Value::Null => {
                    entries.remove(openrouter);
                }
                _ => return Err("its providers.openrouter is not a mapping".into()),
            }
        }
        if entries.is_empty() {
            root.remove(index);
        }
    }
    Ok(document)
}

/// What [`plan_connect`] changes.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Planned {
    /// The new text.
    pub(crate) text: String,
    /// The file held a key in `providers.openrouter.api_key`, now removed.
    pub(crate) removed_key: bool,
    /// The file already said exactly this.
    pub(crate) unchanged: bool,
}

/// The text of `text` with `providers.openrouter` naming the secret
/// `secret` and `providers.openrouter_billing: credits`. The reason when
/// the file cannot be edited this way.
pub(crate) fn plan_connect(text: &str, secret: &str) -> Result<Planned, String> {
    let before = without_edited(text)?;
    let all = lines(text);
    let mut removed_key = false;
    let providers = all
        .iter()
        .position(|line| key_at(line.text, 0).is_some_and(|(key, _)| key == "providers"));
    let new_text = match providers {
        None => {
            let mut out = text.to_string();
            if !out.is_empty() && !out.ends_with('\n') {
                out.push('\n');
            }
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(&manual_lines(secret));
            out
        }
        Some(start) => {
            if key_at(all[start].text, 0) != Some(("providers", true)) {
                return Err(
                    "its providers: is not written as a plain block of entries, one per line"
                        .into(),
                );
            }
            let end = entry_end(&all, start, 0)?;
            // The entries' indentation: the first content line's.
            let child = match (start + 1..end).find(|&index| !is_filler(all[index].text)) {
                Some(index) => indent(all[index].text).ok_or("a tab indents a line")?,
                None => 2,
            };
            if child == 0 {
                return Err("its providers: entries are not indented".into());
            }
            let pad = " ".repeat(child);
            // Each entry of `providers` at the child indentation.
            let mut billing = None;
            let mut openrouter = None;
            let mut index = start + 1;
            while index < end {
                let text = all[index].text;
                if is_filler(text) {
                    index += 1;
                    continue;
                }
                let spaces = indent(text).ok_or("a tab indents a line")?;
                if spaces != child {
                    return Err("its providers: entries are not all indented the same way".into());
                }
                let (key, block) = key_at(text, child)
                    .ok_or("an entry under providers: is not a plain key: value line")?;
                let entry_end = entry_end(&all, index, child)?;
                match key {
                    "openrouter_billing" => billing = Some((index, entry_end, block)),
                    "openrouter" => openrouter = Some((index, entry_end, block)),
                    _ => {}
                }
                index = entry_end;
            }
            let billing_line = format!("{pad}openrouter_billing: credits\n");
            let mut out: Vec<String> = Vec::new();
            let mut emitted_billing = false;
            let mut index = 0;
            // Where a new entry goes: after providers' last content line.
            let insert_at = after_content(&all, start + 1, end);
            while index < all.len() {
                if let Some((billing_start, billing_end, block)) = billing {
                    if index == billing_start {
                        if block || after_content(&all, billing_start, billing_end) != index + 1 {
                            return Err("its providers.openrouter_billing is not one line".into());
                        }
                        out.push(billing_line.clone());
                        emitted_billing = true;
                        index += 1;
                        continue;
                    }
                }
                if let Some((open_start, open_end, block)) = openrouter {
                    if index == open_start {
                        if !block {
                            return Err("its providers.openrouter is not written as a plain \
                                        block of entries, one per line"
                                .into());
                        }
                        if !emitted_billing && billing.is_none() {
                            out.push(billing_line.clone());
                            emitted_billing = true;
                        }
                        out.push(all[index].whole.to_string());
                        let inner = match (open_start + 1..open_end)
                            .find(|&inner| !is_filler(all[inner].text))
                        {
                            Some(inner) => indent(all[inner].text).ok_or("a tab indents a line")?,
                            None => child * 2,
                        };
                        if inner <= child {
                            return Err("its providers.openrouter entries are not indented".into());
                        }
                        out.push(format!("{}api_key_secret: {secret}\n", " ".repeat(inner)));
                        let mut inner_index = open_start + 1;
                        while inner_index < open_end {
                            let text = all[inner_index].text;
                            if is_filler(text) {
                                out.push(all[inner_index].whole.to_string());
                                inner_index += 1;
                                continue;
                            }
                            if indent(text) != Some(inner) {
                                return Err("its providers.openrouter entries are not all \
                                            indented the same way"
                                    .into());
                            }
                            let key = key_at(text, inner).map(|(key, _)| key);
                            // The entry's own lines; blank lines and
                            // comments after it stay.
                            let next = after_content(
                                &all,
                                inner_index,
                                entry_end(&all, inner_index, inner)?,
                            );
                            if matches!(key, Some("api_key" | "api_key_secret")) {
                                removed_key |= key == Some("api_key");
                            } else {
                                for line in &all[inner_index..next] {
                                    out.push(line.whole.to_string());
                                }
                            }
                            inner_index = next;
                        }
                        index = open_end;
                        continue;
                    }
                }
                if index == insert_at && openrouter.is_none() {
                    if !emitted_billing {
                        out.push(billing_line.clone());
                        emitted_billing = true;
                    }
                    out.push(format!(
                        "{pad}openrouter:\n{pad}{pad}api_key_secret: {secret}\n"
                    ));
                }
                let mut whole = all[index].whole.to_string();
                if index + 1 == insert_at && !whole.ends_with('\n') {
                    whole.push('\n');
                }
                out.push(whole);
                index += 1;
            }
            if insert_at == all.len() && openrouter.is_none() {
                if let Some(last) = out.last_mut() {
                    if !last.ends_with('\n') {
                        last.push('\n');
                    }
                }
                if !emitted_billing {
                    out.push(billing_line.clone());
                }
                out.push(format!(
                    "{pad}openrouter:\n{pad}{pad}api_key_secret: {secret}\n"
                ));
            }
            out.concat()
        }
    };
    check_edit(text, &new_text, &before, secret)?;
    Ok(Planned {
        unchanged: new_text == text,
        text: new_text,
        removed_key,
    })
}

/// The edit changed nothing but its entries, which now say what they must,
/// and the file still loads as a configuration if it did.
fn check_edit(old: &str, new: &str, before: &Value, secret: &str) -> Result<(), String> {
    let changed_more = || {
        "editing it would change more than the OpenRouter entries (this command edits only a \
         plain providers: block)"
            .to_string()
    };
    let after = without_edited(new).map_err(|_| changed_more())?;
    if &after != before {
        return Err(changed_more());
    }
    let document: Value = axocoatl_config::yaml::from_str(new).map_err(|_| changed_more())?;
    let providers = document.get("providers");
    let named = providers
        .and_then(|providers| providers.get("openrouter"))
        .and_then(|openrouter| openrouter.get("api_key_secret"));
    let billing = providers.and_then(|providers| providers.get("openrouter_billing"));
    if named != Some(&Value::String(secret.into()))
        || billing != Some(&Value::String("credits".into()))
    {
        return Err(changed_more());
    }
    let loads = |text: &str| {
        axocoatl_config::refuse_substitution(text).is_ok()
            && axocoatl_config::yaml::from_str::<axocoatl_config::AxocoatlConfig>(text).is_ok()
    };
    if loads(old) && !loads(new) {
        return Err(changed_more());
    }
    Ok(())
}

/// The text of `text` without `providers.openrouter`'s reference to the
/// secret `secret` (the whole `openrouter:` entry when that was all of it).
/// `None` when the file does not name that secret there.
pub(crate) fn plan_remove(text: &str, secret: &str) -> Result<Option<String>, String> {
    let document: Value =
        axocoatl_config::yaml::from_str(text).map_err(|_| "it is not valid YAML".to_string())?;
    let named = document
        .get("providers")
        .and_then(|providers| providers.get("openrouter"))
        .and_then(|openrouter| openrouter.get("api_key_secret"));
    if named != Some(&Value::String(secret.into())) {
        return Ok(None);
    }
    let all = lines(text);
    let cannot = || {
        format!(
            "its providers.openrouter is not a plain block this command edits; remove the line \
             api_key_secret: {secret} from it yourself"
        )
    };
    let start = all
        .iter()
        .position(|line| key_at(line.text, 0) == Some(("providers", true)))
        .ok_or_else(cannot)?;
    let end = entry_end(&all, start, 0)?;
    let child = (start + 1..end)
        .find(|&index| !is_filler(all[index].text))
        .and_then(|index| indent(all[index].text))
        .ok_or_else(cannot)?;
    let open = (start + 1..end)
        .find(|&index| {
            indent(all[index].text) == Some(child)
                && key_at(all[index].text, child) == Some(("openrouter", true))
        })
        .ok_or_else(cannot)?;
    let open_end = entry_end(&all, open, child)?;
    let content: Vec<usize> = (open + 1..open_end)
        .filter(|&index| !is_filler(all[index].text))
        .collect();
    let inner = content
        .first()
        .and_then(|&index| indent(all[index].text))
        .ok_or_else(cannot)?;
    let mut keep = vec![true; all.len()];
    let mut others = false;
    let mut index = open + 1;
    while index < open_end {
        if is_filler(all[index].text) {
            index += 1;
            continue;
        }
        let next = after_content(&all, index, entry_end(&all, index, inner)?);
        if key_at(all[index].text, inner).map(|(key, _)| key) == Some("api_key_secret") {
            keep[index..next].iter_mut().for_each(|kept| *kept = false);
        } else {
            others = true;
        }
        index = next;
    }
    if !others {
        let content_end = after_content(&all, open, open_end);
        keep[open..content_end]
            .iter_mut()
            .for_each(|kept| *kept = false);
    }
    let new: String = all
        .iter()
        .zip(&keep)
        .filter(|(_, kept)| **kept)
        .map(|(line, _)| line.whole)
        .collect();
    let check: Value = axocoatl_config::yaml::from_str(&new).map_err(|_| cannot())?;
    let still = check
        .get("providers")
        .and_then(|providers| providers.get("openrouter"))
        .and_then(|openrouter| openrouter.get("api_key_secret"));
    if still.is_some() {
        return Err(cannot());
    }
    Ok(Some(new))
}

/// Replace `path` with `text`, atomically, keeping its permission bits
/// (`0600` for a new file).
pub(crate) fn write(path: &Path, text: &str) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    let mode = match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_file() => metadata.permissions().mode() & 0o777,
        Ok(_) => return Err(format!("{} is not a regular file", path.display())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => 0o600,
        Err(error) => return Err(format!("{}: {error}", path.display())),
    };
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let name = path
        .file_name()
        .ok_or_else(|| format!("{} names no file", path.display()))?;
    axocoatl_core::SecureDir::open_existing_all(parent)
        .and_then(|directory| directory.atomic_write_with_mode(name, text.as_bytes(), mode))
        .map_err(|error| format!("writing {}: {error}", path.display()))
}

/// Whether `path` can be replaced by [`write`]: a regular file (not a
/// link) in a directory that opens.
pub(crate) fn check_writable(path: &Path) -> Result<(), String> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_file() => {}
        Ok(_) => {
            return Err(format!(
                "{} is a link or not a regular file, so it is not edited",
                path.display()
            ))
        }
        Err(error) => return Err(format!("{}: {error}", path.display())),
    }
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    axocoatl_core::SecureDir::open_existing_all(parent)
        .map(drop)
        .map_err(|error| format!("{}: {error}", parent.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    const ONBOARDED_OLLAMA: &str = r#"# Axocoatl — local Ollama setup
agents:
  - id: lead
    name: "Lead"
    provider: ollama
    model: "qwen3-coder:30b"

providers:
  ollama:
    base_url: "http://127.0.0.1:11434"

server:
  port: 8080
  host: "127.0.0.1"
"#;

    #[test]
    fn an_ollama_configuration_gains_the_openrouter_entries_and_keeps_every_other_line() {
        let planned = plan_connect(ONBOARDED_OLLAMA, "openrouter").unwrap();
        assert!(!planned.removed_key && !planned.unchanged);
        assert_eq!(
            planned.text,
            ONBOARDED_OLLAMA.replace(
                "    base_url: \"http://127.0.0.1:11434\"\n",
                "    base_url: \"http://127.0.0.1:11434\"\n  openrouter_billing: credits\n  \
                 openrouter:\n    api_key_secret: openrouter\n"
            )
        );
        let config: axocoatl_config::AxocoatlConfig =
            axocoatl_config::parse_config(&planned.text, Path::new("c.yaml")).unwrap();
        let openrouter = config.providers.openrouter.unwrap();
        assert_eq!(openrouter.api_key_secret.as_deref(), Some("openrouter"));
        assert!(openrouter.api_key.is_empty());
        assert_eq!(
            config.providers.openrouter_billing,
            Some(axocoatl_config::OpenRouterBilling::Credits)
        );
        // Running it again changes nothing.
        let again = plan_connect(&planned.text, "openrouter").unwrap();
        assert!(again.unchanged);
        assert_eq!(again.text, planned.text);
    }

    #[test]
    fn a_key_written_into_the_file_is_replaced_by_the_secrets_name() {
        let onboarded = "agents: []\n\nproviders:\n  openrouter_billing: byok\n  openrouter:\n    \
                         api_key: \"sk-or-v1-WRITTEN-IN-FILE\"\n    # a comment kept\n\nserver:\n  \
                         port: 8080\n";
        let planned = plan_connect(onboarded, "openrouter").unwrap();
        assert!(planned.removed_key);
        assert!(!planned.text.contains("WRITTEN-IN-FILE"));
        assert_eq!(
            planned.text,
            "agents: []\n\nproviders:\n  openrouter_billing: credits\n  openrouter:\n    \
             api_key_secret: openrouter\n    # a comment kept\n\nserver:\n  port: 8080\n"
        );
    }

    #[test]
    fn other_openrouter_entries_and_indentation_are_kept() {
        let text =
            "providers:\n    openrouter:\n        api_key: \"${OPENROUTER_API_KEY}\"\n        \
                    fallback: \"ollama:qwen3\"\n    ollama:\n        base_url: x\n";
        let planned = plan_connect(text, "work").unwrap();
        assert_eq!(
            planned.text,
            "providers:\n    openrouter_billing: credits\n    openrouter:\n        \
             api_key_secret: work\n        fallback: \"ollama:qwen3\"\n    ollama:\n        \
             base_url: x\n"
        );
    }

    #[test]
    fn a_file_without_providers_gets_the_block_at_its_end() {
        for (text, expected) in [
            ("", manual_lines("openrouter")),
            (
                "server:\n  port: 8081",
                format!("server:\n  port: 8081\n\n{}", manual_lines("openrouter")),
            ),
            (
                "providers:\n# nothing yet\nserver:\n  port: 1\n",
                "providers:\n  openrouter_billing: credits\n  openrouter:\n    api_key_secret: \
                 openrouter\n# nothing yet\nserver:\n  port: 1\n"
                    .to_string(),
            ),
            (
                "providers:\n  ollama:\n    base_url: x",
                "providers:\n  ollama:\n    base_url: x\n  openrouter_billing: credits\n  \
                 openrouter:\n    api_key_secret: openrouter\n"
                    .to_string(),
            ),
        ] {
            let planned = plan_connect(text, "openrouter").unwrap();
            assert_eq!(planned.text, expected, "{text:?}");
        }
    }

    #[test]
    fn a_form_this_edit_does_not_handle_is_refused_with_no_value_in_the_reason() {
        for text in [
            "providers: {openrouter: {api_key: sk-or-FLOWKEY}}\n",
            "providers:\n  openrouter: {api_key: sk-or-FLOWKEY}\n",
            "providers:\n\topenrouter:\n\t\tapi_key: sk-or-FLOWKEY\n",
            "[1, 2]\n",
            "providers: [a]\n",
            "providers:\n  openrouter_billing:\n    nested: x\n",
            "not: [valid\n",
        ] {
            let refused = plan_connect(text, "openrouter").unwrap_err();
            assert!(!refused.contains("FLOWKEY"), "{text:?}: {refused}");
        }
    }

    #[test]
    fn a_key_over_several_lines_is_removed_whole() {
        for text in [
            "providers:\n  openrouter:\n    api_key:\n      sk-or-NEXTLINE\n",
            "providers:\n  openrouter:\n    api_key: |\n      sk-or-NEXTLINE\n",
        ] {
            let planned = plan_connect(text, "openrouter").unwrap();
            assert!(planned.removed_key);
            assert_eq!(
                planned.text,
                "providers:\n  openrouter_billing: credits\n  openrouter:\n    \
                 api_key_secret: openrouter\n"
            );
        }
    }

    #[test]
    fn remove_takes_out_the_secrets_name_and_an_entry_left_empty() {
        let planned = plan_connect(ONBOARDED_OLLAMA, "openrouter").unwrap().text;
        let removed = plan_remove(&planned, "openrouter").unwrap().unwrap();
        assert_eq!(
            removed,
            ONBOARDED_OLLAMA.replace(
                "    base_url: \"http://127.0.0.1:11434\"\n",
                "    base_url: \"http://127.0.0.1:11434\"\n  openrouter_billing: credits\n"
            )
        );
        let with_other = "providers:\n  openrouter:\n    api_key_secret: openrouter\n    \
                          fallback: x\n";
        assert_eq!(
            plan_remove(with_other, "openrouter").unwrap().unwrap(),
            "providers:\n  openrouter:\n    fallback: x\n"
        );
        // Another secret, or none: nothing to remove.
        assert_eq!(plan_remove(&planned, "work").unwrap(), None);
        assert_eq!(plan_remove(ONBOARDED_OLLAMA, "openrouter").unwrap(), None);
    }

    /// Onboarding's OpenRouter configuration with the key left blank (its
    /// `${OPENROUTER_API_KEY}` placeholder, which the always-on service
    /// cannot read) holds no placeholder once connected.
    #[test]
    fn an_onboarded_placeholder_is_replaced_so_the_service_needs_no_variable() {
        let onboarded = "# Axocoatl — OpenRouter setup\n# OpenRouter uses your normal API key and \
                         OpenRouter credits.\nagents:\n  - id: lead\n    name: \"Lead\"\n    \
                         provider: openrouter\n    model: \"qwen/qwen3-coder\"\n\nproviders:\n  \
                         openrouter_billing: credits\n  openrouter:\n    api_key: \
                         \"${OPENROUTER_API_KEY}\"\n\nserver:\n  port: 8080\n  host: \
                         \"127.0.0.1\"\n";
        let planned = plan_connect(onboarded, "openrouter").unwrap();
        assert!(planned.removed_key);
        assert!(!planned.text.contains("${"), "{}", planned.text);
        assert_eq!(
            planned.text,
            onboarded.replace(
                "    api_key: \"${OPENROUTER_API_KEY}\"\n",
                "    api_key_secret: openrouter\n"
            )
        );
        axocoatl_config::parse_config(&planned.text, Path::new("c.yaml")).unwrap();
    }

    #[test]
    fn a_new_configuration_holds_only_the_openrouter_entries_and_the_servers_defaults() {
        let text = new_configuration("openrouter");
        let config = axocoatl_config::parse_config(&text, Path::new("c.yaml")).unwrap();
        assert!(config.agents.is_empty());
        assert_eq!(config.server.port, 8080);
        assert_eq!(
            config
                .providers
                .openrouter
                .unwrap()
                .api_key_secret
                .as_deref(),
            Some("openrouter")
        );
        assert!(!text.contains("${"));
        // It is the file `connect` edits: connecting again changes nothing.
        assert!(plan_connect(&text, "openrouter").unwrap().unchanged);
    }

    #[test]
    fn the_lead_and_the_agents_on_openrouter_are_read_from_the_file() {
        let text = "agents:\n  - id: scout\n    name: S\n    provider: ollama\n    model: \
                    \"qwen3:8b\"\n  - id: lead\n    name: L\n    provider: anthropic\n    \
                    model: claude\n  - id: reviewer\n    name: R\n    provider: openrouter\n    \
                    model: openai/gpt-oss-120b\n";
        // The Lead is not native: the first native Agent's model.
        assert_eq!(lead_writer(text).as_deref(), Some("ollama:qwen3:8b"));
        assert_eq!(openrouter_agents(text), vec!["reviewer".to_string()]);
        assert_eq!(lead_writer("agents: []\n"), None);
        assert_eq!(lead_writer("not: [valid\n"), None);
        assert!(openrouter_agents("providers: {}\n").is_empty());
    }

    #[test]
    fn writing_keeps_the_files_permissions_and_refuses_a_link() {
        use std::os::unix::fs::PermissionsExt;
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.yaml");
        std::fs::write(&path, "a: 1\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640)).unwrap();
        check_writable(&path).unwrap();
        write(&path, "a: 2\n").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "a: 2\n");
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o640
        );
        let link = directory.path().join("link.yaml");
        std::os::unix::fs::symlink(&path, &link).unwrap();
        assert!(check_writable(&link).is_err());
        assert!(write(&link, "a: 3\n").is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "a: 2\n");
    }
}
