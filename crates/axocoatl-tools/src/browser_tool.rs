//! The `browser` and `browser_check` tools.
//!
//! Both run in Axocoatl's browser container, never in the Session container
//! and never on the host. `browser` drives a fresh headless Chromium through a
//! list of steps and returns text: the page's accessibility snapshot, console
//! errors and failed requests. `browser_check` runs one Playwright test file
//! (a reproduction) against the Session's app and returns each test's status.
//! Screenshots never reach the model: the runner keeps them in the Session's
//! record and the result only names them.
//!
//! This module validates arguments, builds the driver input, and bounds what
//! comes back. Running the container is the [`BrowserRunner`]'s job.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use base64::Engine;
use serde_json::{json, Map, Value};

use crate::builtin::BuiltinTool;
use crate::error::ToolError;

pub const BROWSER_TOOL: &str = "browser";
pub const BROWSER_CHECK_TOOL: &str = "browser_check";
/// The Playwright release the image and both scripts are written for.
pub const PLAYWRIGHT_VERSION: &str = "1.60.0";
/// The driver for `browser`, passed to `node -e` on each call.
pub const DRIVER_SCRIPT: &str = include_str!("../assets/browser/driver.mjs");
/// The runner for `browser_check`, passed to `node -e` on each call.
pub const CHECK_SCRIPT: &str = include_str!("../assets/browser/check.mjs");
/// Build context for `axocoatl browser install`.
pub const BROWSER_CONTAINERFILE: &str = include_str!("../assets/browser/Containerfile");
pub const BROWSER_PACKAGE_JSON: &str = include_str!("../assets/browser/package.json");
pub const BROWSER_PACKAGE_LOCK: &str = include_str!("../assets/browser/package-lock.json");

pub const MAX_STEPS: usize = 40;
pub const MAX_STRING_BYTES: usize = 4096;
pub const MAX_CHECK_SCRIPT_BYTES: usize = 64 * 1024;
pub const MAX_CHECK_FILES: usize = 32;
pub const MAX_CHECK_FILE_BYTES: usize = 256 * 1024;
pub const MAX_CHECK_TOTAL_BYTES: usize = 1024 * 1024;
/// Largest model-facing result, before Axocoatl's own executor bound.
pub const OUTPUT_MAX_BYTES: usize = 64 * 1024;
/// Largest screenshot kept in the record.
pub const SCREENSHOT_MAX_BYTES: usize = 1024 * 1024;
/// Repository path the inline `script` of `browser_check` is written to.
pub const INLINE_CHECK_ENTRY: &str = "axocoatl-inline.spec.ts";
pub const BROWSER_PROXY_USER: &str = "axo";
/// Longest failure reason a call's `browser` event keeps.
pub const MAX_RECORDED_ERROR_CHARS: usize = 500;

const ACTIONS: [&str; 11] = [
    "goto",
    "click",
    "fill",
    "select",
    "check",
    "uncheck",
    "press",
    "wait_for",
    "expect_text",
    "reload",
    "back",
];
const TARGET_KINDS: [&str; 5] = ["role", "label", "text", "testid", "css"];
const STEP_FIELDS: [&str; 7] = [
    "action",
    "target",
    "url",
    "value",
    "key",
    "text",
    "timeout_ms",
];
const DRIVE_FIELDS: [&str; 4] = ["url", "steps", "snapshot", "viewport"];
const CHECK_FIELDS: [&str; 4] = ["path", "script", "grep", "base_url"];
const DRIVER_SCHEMA_IN: &str = "axocoatl.browser-input/1";
const DRIVER_SCHEMA_OUT: &str = "axocoatl.browser/1";
const CHECK_SCHEMA_IN: &str = "axocoatl.browser-check-input/1";
const CHECK_SCHEMA_OUT: &str = "axocoatl.browser-check/1";

const BROWSER_DESCRIPTION: &str = "Look at a web page in a fresh headless Chromium. Call it with just url to get the page's accessibility snapshot (roles, names, text and links). To act, call it again with url and steps (click, fill, press and so on) that target what the snapshot showed; the result has the snapshot after the steps. Each call starts a fresh browser and also returns console errors and failed requests. Steps are actions, not code: this tool runs no JavaScript. It reaches this Session's apps at http://localhost:<port> and the hosts the user declared. Results show the Playwright line for each step, to copy into a test.";
const CHECK_DESCRIPTION: &str = "Run one Playwright test file against this Session's app in the browser container (one worker, no retries) and return each test's status and first error. Give exactly one of path (a repository test file; files it imports by relative path are sent with it) or script (the file's source). Import test and expect from '@playwright/test'; page.goto('/') opens base_url. Use it to confirm a reproduction fails for the reason you report.";

/// Field names, or action names, under which a model sends code. A refusal
/// for one of them says that steps are not scripts.
const SCRIPT_WORDS: [&str; 10] = [
    "code",
    "script",
    "javascript",
    "js",
    "evaluate",
    "eval",
    "expression",
    "function",
    "playwright",
    "run",
];
/// The URL a refusal's example opens when the call has no usable one.
const EXAMPLE_URL: &str = "http://localhost:3000/";
const NO_SCRIPTS: &str = "Steps are actions, not code: browser runs no JavaScript or Playwright script. To read the page, call with just url and read the snapshot in the result.";
const STEP_GUIDE: &str = "A step is an object with an action: goto (url); click, check or uncheck (target); fill or select (target, value); press (key, optional target); wait_for (one of target, url or text); expect_text (text, optional target); reload; back. A target is one of {\"role\": \"button\", \"name\": \"Save\"}, {\"label\": \"Email\"}, {\"text\": \"Sign in\"}, {\"testid\": \"cart\"} or {\"css\": \"#total\"}, with an optional \"nth\".";
const TARGET_EXAMPLE: &str = "{\"role\": \"button\", \"name\": \"Save\"}";
const CHECK_PATH_EXAMPLE: &str = "{\"path\": \"qa/findings/B07.spec.ts\"}";
const CHECK_SCRIPT_EXAMPLE: &str = "{\"script\": \"import { test, expect } from '@playwright/test';\\ntest('home loads', async ({ page }) => { await page.goto('/'); await expect(page.locator('body')).toContainText('Orders'); });\"}";

fn invalid(tool: &str, reason: impl Into<String>) -> ToolError {
    ToolError::InvalidArgs {
        tool: tool.to_string(),
        reason: reason.into(),
    }
}

fn failed(tool: &str, reason: impl Into<String>) -> ToolError {
    ToolError::ExecutionFailed {
        tool: tool.to_string(),
        reason: reason.into(),
    }
}

/// A field's value, with JSON `null` read as "not given".
fn given<'a>(object: &'a Map<String, Value>, key: &str) -> Option<&'a Value> {
    object.get(key).filter(|value| !value.is_null())
}

/// A received value as the model wrote it, cut to a readable length.
fn shown(value: &Value) -> String {
    const MAX_CHARS: usize = 120;
    if let Value::String(text) = value {
        if text.chars().count() <= MAX_CHARS {
            return value.to_string();
        }
        let cut: String = text.chars().take(MAX_CHARS).collect();
        return Value::from(format!("{cut}...")).to_string();
    }
    let text = value.to_string();
    if text.chars().count() <= MAX_CHARS {
        return text;
    }
    let cut: String = text.chars().take(MAX_CHARS).collect();
    format!("{cut} (cut; {} bytes in all)", text.len())
}

/// What a refusal says was received for a field that may be absent.
fn got(value: Option<&Value>) -> String {
    match value {
        None => "it was not given".to_string(),
        Some(value) => format!("got {}", shown(value)),
    }
}

fn quoted(text: &str) -> String {
    Value::from(text).to_string()
}

fn is_script_word(word: &str) -> bool {
    SCRIPT_WORDS.contains(&word.to_ascii_lowercase().as_str())
}

/// A refusal that teaches: the reason, an optional guide to the valid shape,
/// and one minimal valid call.
fn teach(reason: impl Into<String>, guide: Option<&str>, example: &str) -> String {
    let mut text = reason.into();
    if !text.ends_with('.') {
        text.push('.');
    }
    if let Some(guide) = guide {
        text.push(' ');
        text.push_str(guide);
    }
    text.push_str(" Example: ");
    text.push_str(example);
    text
}

/// One minimal valid `browser` call of each shape a refusal may show, opening
/// the call's own URL when it has a usable one.
struct DriveExamples(String);

impl DriveExamples {
    fn new(object: Option<&Map<String, Value>>) -> Self {
        let url = object
            .and_then(|object| given(object, "url"))
            .and_then(Value::as_str)
            .filter(|url| check_url(url, "url").is_ok())
            .unwrap_or(EXAMPLE_URL);
        Self(quoted(url))
    }

    fn open(&self) -> String {
        format!("{{\"url\": {}}}", self.0)
    }

    fn steps(&self) -> String {
        format!(
            "{{\"url\": {}, \"steps\": [{{\"action\": \"click\", \"target\": {TARGET_EXAMPLE}}}]}}",
            self.0
        )
    }

    fn snapshot(&self) -> String {
        format!("{{\"url\": {}, \"snapshot\": \"text\"}}", self.0)
    }

    fn viewport(&self) -> String {
        format!(
            "{{\"url\": {}, \"viewport\": {{\"width\": 390, \"height\": 844}}}}",
            self.0
        )
    }
}

/// What the configuration lets one call use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BrowserSettings {
    pub snapshot_max_bytes: u32,
    pub timeout_secs: u64,
}

impl Default for BrowserSettings {
    fn default() -> Self {
        Self {
            snapshot_max_bytes: 16 * 1024,
            timeout_secs: 120,
        }
    }
}

impl BrowserSettings {
    /// The whole call, container exec included.
    pub fn run_timeout(&self) -> Duration {
        Duration::from_secs(self.timeout_secs)
    }

    /// The script's own budget: 5 s less than the call.
    pub fn script_budget_ms(&self) -> u64 {
        self.timeout_secs.saturating_sub(5).max(5) * 1000
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SnapshotKind {
    Aria,
    Text,
    None,
}

impl SnapshotKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Aria => "aria",
            Self::Text => "text",
            Self::None => "none",
        }
    }
}

/// One validated `browser` call.
#[derive(Debug, Clone, PartialEq)]
pub struct DriveJob {
    pub url: String,
    /// Validated steps as the model wrote them, without the fields it gave
    /// as `null` (in a step or in its target).
    pub steps: Vec<Value>,
    pub snapshot: SnapshotKind,
    pub viewport: Option<(u32, u32)>,
}

/// Where a `browser_check` test file comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CheckSource {
    /// A repository path; the runner reads it and its relative imports.
    Path(String),
    /// The test's source, written as [`INLINE_CHECK_ENTRY`].
    Script(String),
}

/// One validated `browser_check` call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckJob {
    pub source: CheckSource,
    pub grep: Option<String>,
    pub base_url: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum BrowserJob {
    Drive(DriveJob),
    Check(CheckJob),
}

impl BrowserJob {
    pub fn tool(&self) -> &'static str {
        match self {
            Self::Drive(_) => BROWSER_TOOL,
            Self::Check(_) => BROWSER_CHECK_TOOL,
        }
    }

    /// The URL the call opens first, when it names one.
    pub fn url(&self) -> Option<&str> {
        match self {
            Self::Drive(job) => Some(&job.url),
            Self::Check(job) => job.base_url.as_deref(),
        }
    }
}

/// Why `value` is not an http(s) URL the browser may open.
pub fn check_url(value: &str, field: &str) -> Result<(), String> {
    if value.len() > MAX_STRING_BYTES {
        return Err(format!("{field} exceeds {MAX_STRING_BYTES} bytes"));
    }
    let url = reqwest::Url::parse(value).map_err(|_| format!("{field} is not an absolute URL"))?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(format!(
            "{field} must use http or https, not {}",
            url.scheme()
        ));
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(format!("{field} must not contain a user name or password"));
    }
    Ok(())
}

fn check_string(value: &Value, field: &str) -> Result<(), String> {
    let text = value
        .as_str()
        .ok_or_else(|| format!("{field} must be a string; got {}", shown(value)))?;
    if text.len() > MAX_STRING_BYTES {
        return Err(format!(
            "{field} exceeds {MAX_STRING_BYTES} bytes (got {} bytes)",
            text.len()
        ));
    }
    Ok(())
}

/// Why `target` is not a usable element target. A field given as `null`
/// counts as not given.
fn check_target(target: &Value, field: &str) -> Result<(), String> {
    let target = target.as_object().ok_or_else(|| {
        format!(
            "{field} must be an object such as {TARGET_EXAMPLE}; got {}",
            shown(target)
        )
    })?;
    for (key, value) in target {
        if !TARGET_KINDS.contains(&key.as_str()) && key != "name" && key != "nth" {
            return Err(format!(
                "{field}.{key} is not a target field; got {}. A target has one of role (with an optional name), label, text, testid or css, and an optional nth",
                shown(value)
            ));
        }
    }
    let kinds: Vec<&str> = TARGET_KINDS
        .iter()
        .copied()
        .filter(|kind| given(target, kind).is_some())
        .collect();
    if kinds.len() != 1 {
        return Err(format!(
            "{field} needs exactly one of {}; got {}",
            TARGET_KINDS.join(", "),
            shown(&Value::Object(target.clone()))
        ));
    }
    for key in TARGET_KINDS.iter().copied().chain(["name"]) {
        if let Some(value) = given(target, key) {
            check_string(value, &format!("{field}.{key}"))?;
        }
    }
    if given(target, "name").is_some() && kinds[0] != "role" {
        return Err(format!(
            "{field}.name is only for role targets, such as {TARGET_EXAMPLE}; to match an element by its text use {{\"text\": \"...\"}}"
        ));
    }
    if let Some(nth) = given(target, "nth") {
        if nth.as_u64().is_none() {
            return Err(format!(
                "{field}.nth must be a whole number from 0; got {}",
                shown(nth)
            ));
        }
    }
    Ok(())
}

/// What a step needs `key` to be, for a refusal that names it as missing.
fn needed(key: &str) -> String {
    match key {
        "target" => format!(" such as {TARGET_EXAMPLE}"),
        "url" => ", an http(s) URL".into(),
        "value" => ", the text to enter or the option to pick".into(),
        "key" => " such as \"Enter\"".into(),
        _ => ", the text to look for".into(),
    }
}

/// Why step `index` is invalid. Mirrors the driver's own check; a field
/// given as `null` counts as not given, as it does once
/// [`parse_drive_call`] has left such fields out.
pub fn check_step(step: &Value, index: usize) -> Result<(), String> {
    let field = format!("steps[{index}]");
    let object = match step {
        Value::Object(object) => object,
        Value::String(_) => {
            return Err(format!(
                "{field} must be an object, not a string; got {}. {NO_SCRIPTS}",
                shown(step)
            ))
        }
        other => return Err(format!("{field} must be an object; got {}", shown(other))),
    };
    for (key, value) in object {
        if STEP_FIELDS.contains(&key.as_str()) {
            continue;
        }
        return Err(if is_script_word(key) {
            format!(
                "{field}.{key} is not a step field; got {}. {NO_SCRIPTS}",
                shown(value)
            )
        } else {
            format!(
                "{field}.{key} is not a step field; got {}. Step fields are {}",
                shown(value),
                STEP_FIELDS.join(", ")
            )
        });
    }
    let action = match given(object, "action") {
        None => {
            return Err(format!(
                "{field}.action is required and must be one of {}; {}",
                ACTIONS.join(", "),
                got(object.get("action"))
            ))
        }
        Some(value) => match value.as_str().filter(|action| ACTIONS.contains(action)) {
            Some(action) => action,
            None => {
                let mut reason = format!(
                    "{field}.action must be one of {}; got {}",
                    ACTIONS.join(", "),
                    shown(value)
                );
                if value.as_str().is_some_and(is_script_word) {
                    reason.push_str(". ");
                    reason.push_str(NO_SCRIPTS);
                }
                return Err(reason);
            }
        },
    };
    for key in ["value", "key", "text"] {
        if let Some(value) = given(object, key) {
            check_string(value, &format!("{field}.{key}"))?;
        }
    }
    if let Some(url) = given(object, "url") {
        let text = url
            .as_str()
            .ok_or_else(|| format!("{field}.url must be a string; got {}", shown(url)))?;
        check_url(text, &format!("{field}.url"))
            .map_err(|reason| format!("{reason}; got {}", shown(url)))?;
    }
    if let Some(target) = given(object, "target") {
        check_target(target, &format!("{field}.target"))?;
    }
    if let Some(timeout) = given(object, "timeout_ms") {
        if !timeout
            .as_u64()
            .is_some_and(|timeout| (100..=10_000).contains(&timeout))
        {
            return Err(format!(
                "{field}.timeout_ms must be 100-10000; got {}. Leave it out for 10000",
                shown(timeout)
            ));
        }
    }
    let has = |key: &str| given(object, key).is_some();
    let need = |required: &[&str], optional: &[&str]| -> Result<(), String> {
        if let Some(missing) = required.iter().find(|key| !has(key)) {
            return Err(format!(
                "{field}: {action} needs {missing}{}",
                needed(missing)
            ));
        }
        for key in ["target", "url", "value", "key", "text"] {
            if has(key) && !required.contains(&key) && !optional.contains(&key) {
                return Err(format!(
                    "{field}: {action} does not take {key}; got {key} {}",
                    shown(&object[key])
                ));
            }
        }
        Ok(())
    };
    match action {
        "goto" => need(&["url"], &[]),
        "click" | "check" | "uncheck" => need(&["target"], &[]),
        "fill" | "select" => need(&["target", "value"], &[]),
        "press" => need(&["key"], &["target"]),
        "expect_text" => need(&["text"], &["target"]),
        "reload" | "back" => need(&[], &[]),
        "wait_for" => {
            let waits: Vec<&str> = ["target", "url", "text"]
                .into_iter()
                .filter(|key| has(key))
                .collect();
            if waits.len() != 1 {
                return Err(format!(
                    "{field}: wait_for needs exactly one of target, url or text; got {}",
                    if waits.is_empty() {
                        "none".to_string()
                    } else {
                        waits.join(" and ")
                    }
                ));
            }
            need(&[], &["target", "url", "text"])
        }
        _ => Err(format!("{field}.action is unknown")),
    }
}

/// A step as the driver gets it: the fields given as `null`, in the step and
/// in its target, are left out.
fn without_nulls(step: &Value) -> Value {
    let Value::Object(object) = step else {
        return step.clone();
    };
    let kept = object
        .iter()
        .filter(|(_, value)| !value.is_null())
        .map(|(key, value)| {
            let value = match value {
                Value::Object(target) if key == "target" => Value::Object(
                    target
                        .iter()
                        .filter(|(_, value)| !value.is_null())
                        .map(|(key, value)| (key.clone(), value.clone()))
                        .collect(),
                ),
                other => other.clone(),
            };
            (key.clone(), value)
        })
        .collect();
    Value::Object(kept)
}

/// Validate a `browser` call before anything starts. A field given as
/// `null` counts as not given. A refusal names the field, shows what was
/// received and what is accepted, and gives one valid call.
pub fn parse_drive_call(arguments: &Value) -> Result<DriveJob, ToolError> {
    let tool = BROWSER_TOOL;
    let refuse = |reason: String, guide: Option<&str>, example: String| {
        invalid(tool, teach(reason, guide, &example))
    };
    let Some(object) = arguments.as_object() else {
        return Err(refuse(
            format!("arguments must be a JSON object; got {}", shown(arguments)),
            None,
            DriveExamples::new(None).open(),
        ));
    };
    let examples = DriveExamples::new(Some(object));
    if let Some((key, value)) = object
        .iter()
        .find(|(key, _)| !DRIVE_FIELDS.contains(&key.as_str()))
    {
        return Err(refuse(
            format!(
                "unknown argument {}; got {}. browser takes url (required), steps, snapshot and viewport",
                quoted(key),
                shown(value)
            ),
            is_script_word(key).then_some(NO_SCRIPTS),
            examples.open(),
        ));
    }
    let url = match given(object, "url") {
        Some(Value::String(url)) => url,
        Some(other) => {
            return Err(refuse(
                format!("url must be a string; got {}", shown(other)),
                None,
                examples.open(),
            ))
        }
        None => {
            return Err(refuse(
                format!(
                    "url is required: the http(s) page to open, such as http://localhost:3000/ for an app on port 3000 of this Session; {}",
                    got(object.get("url"))
                ),
                None,
                examples.open(),
            ))
        }
    };
    check_url(url, "url").map_err(|reason| {
        refuse(
            format!(
                "{reason}; got {}. This Session's apps are at http://localhost:<port>",
                shown(&object["url"])
            ),
            None,
            examples.open(),
        )
    })?;
    let steps = match given(object, "steps") {
        None => Vec::new(),
        Some(Value::Array(steps)) => steps.clone(),
        Some(other) => {
            let mut reason = format!("steps must be a list of step objects; got {}", shown(other));
            if other.is_string() {
                reason.push_str(". ");
                reason.push_str(NO_SCRIPTS);
            }
            return Err(refuse(reason, Some(STEP_GUIDE), examples.steps()));
        }
    };
    if steps.len() > MAX_STEPS {
        return Err(refuse(
            format!(
                "steps has {} entries; at most {MAX_STEPS} steps per call. Each call starts a fresh browser, so split a longer check into calls that each start from url with the steps they need",
                steps.len()
            ),
            None,
            examples.steps(),
        ));
    }
    for (index, step) in steps.iter().enumerate() {
        check_step(step, index)
            .map_err(|reason| refuse(reason, Some(STEP_GUIDE), examples.steps()))?;
    }
    let steps = steps.iter().map(without_nulls).collect();
    let snapshot = match given(object, "snapshot") {
        None => SnapshotKind::Aria,
        Some(value) => match value.as_str() {
            Some("aria") => SnapshotKind::Aria,
            Some("text") => SnapshotKind::Text,
            Some("none") => SnapshotKind::None,
            _ => {
                return Err(refuse(
                    format!(
                        "snapshot must be \"aria\", \"text\" or \"none\"; got {}. Leave it out for \"aria\", the default: the page's accessibility tree, with the roles and names that step targets use. \"text\" is the page's visible text; \"none\" returns no snapshot",
                        shown(value)
                    ),
                    None,
                    examples.snapshot(),
                ))
            }
        },
    };
    let viewport = match given(object, "viewport") {
        None => None,
        Some(Value::Object(viewport)) => {
            if let Some((key, value)) = viewport
                .iter()
                .find(|(key, _)| !matches!(key.as_str(), "width" | "height"))
            {
                return Err(refuse(
                    format!(
                        "viewport.{key} is not a viewport field; got {}. viewport takes width (320-1920) and height (240-1200)",
                        shown(value)
                    ),
                    None,
                    examples.viewport(),
                ));
            }
            let width = given(viewport, "width").map_or(Some(1280), Value::as_u64);
            let height = given(viewport, "height").map_or(Some(720), Value::as_u64);
            match (width, height) {
                (Some(width @ 320..=1920), Some(height @ 240..=1200)) => {
                    Some((width as u32, height as u32))
                }
                _ => {
                    return Err(refuse(
                        format!(
                            "viewport width must be 320-1920 and height 240-1200; got {}. Leave either out for 1280 wide and 720 high",
                            shown(&object["viewport"])
                        ),
                        None,
                        examples.viewport(),
                    ))
                }
            }
        }
        Some(other) => {
            return Err(refuse(
                format!(
                    "viewport must be an object such as {{\"width\": 390, \"height\": 844}}; got {}. Leave it out for 1280x720",
                    shown(other)
                ),
                None,
                examples.viewport(),
            ))
        }
    };
    Ok(DriveJob {
        url: url.to_string(),
        steps,
        snapshot,
        viewport,
    })
}

/// Validate a `browser_check` call before anything starts. A field given as
/// `null` counts as not given. A refusal names the field, shows what was
/// received and what is accepted, and gives one valid call.
pub fn parse_check_call(arguments: &Value) -> Result<CheckJob, ToolError> {
    let tool = BROWSER_CHECK_TOOL;
    let refuse = |reason: String, example: &str| invalid(tool, teach(reason, None, example));
    let Some(object) = arguments.as_object() else {
        return Err(refuse(
            format!("arguments must be a JSON object; got {}", shown(arguments)),
            CHECK_PATH_EXAMPLE,
        ));
    };
    if let Some((key, value)) = object
        .iter()
        .find(|(key, _)| !CHECK_FIELDS.contains(&key.as_str()))
    {
        return Err(refuse(
            format!(
                "unknown argument {}; got {}. browser_check takes path or script (exactly one), and optional grep and base_url",
                quoted(key),
                shown(value)
            ),
            CHECK_PATH_EXAMPLE,
        ));
    }
    let text = |key: &str| -> Result<Option<&str>, ToolError> {
        match given(object, key) {
            None => Ok(None),
            Some(Value::String(value)) => Ok(Some(value.as_str())),
            Some(other) => Err(refuse(
                format!("{key} must be a string; got {}", shown(other)),
                if key == "script" {
                    CHECK_SCRIPT_EXAMPLE
                } else {
                    CHECK_PATH_EXAMPLE
                },
            )),
        }
    };
    let source = match (text("path")?, text("script")?) {
        (Some(path), None) => CheckSource::Path(normalize_repo_path(path).map_err(|reason| {
            refuse(
                format!("{reason}; got {}", shown(&object["path"])),
                CHECK_PATH_EXAMPLE,
            )
        })?),
        (None, Some(script)) => {
            if script.trim().is_empty() {
                return Err(refuse(
                    format!(
                        "script must be the source of a Playwright test file; got {}",
                        shown(&object["script"])
                    ),
                    CHECK_SCRIPT_EXAMPLE,
                ));
            }
            if script.len() > MAX_CHECK_SCRIPT_BYTES {
                return Err(refuse(
                    format!(
                        "script must be at most {MAX_CHECK_SCRIPT_BYTES} bytes; got {} bytes. Write the test to a file and give its path instead",
                        script.len()
                    ),
                    CHECK_PATH_EXAMPLE,
                ));
            }
            CheckSource::Script(script.to_string())
        }
        (Some(_), Some(_)) => {
            return Err(refuse(
                "give exactly one of path or script, not both".to_string(),
                CHECK_PATH_EXAMPLE,
            ))
        }
        (None, None) => {
            return Err(refuse(
                "give exactly one of path (a repository test file, such as qa/findings/B07.spec.ts) or script (a test file's source); neither was given".to_string(),
                CHECK_PATH_EXAMPLE,
            ))
        }
    };
    if let CheckSource::Path(path) = &source {
        if !is_test_source(path) {
            return Err(refuse(
                format!(
                    "path must name a .ts, .js, .mjs, .cjs, .mts, .cts, .tsx or .jsx file; got {}",
                    shown(&object["path"])
                ),
                CHECK_PATH_EXAMPLE,
            ));
        }
    }
    let grep = text("grep")?.map(str::to_string);
    if let Some(pattern) = grep
        .as_ref()
        .filter(|grep| grep.is_empty() || grep.len() > 512)
    {
        return Err(refuse(
            format!(
                "grep must be 1-512 bytes: a regular expression for the titles of the tests to run; got {} bytes. Leave it out to run every test",
                pattern.len()
            ),
            CHECK_PATH_EXAMPLE,
        ));
    }
    let base_url = text("base_url")?.map(str::to_string);
    if let Some(url) = &base_url {
        check_url(url, "base_url").map_err(|reason| {
            refuse(
                format!(
                    "{reason}; got {}. Leave it out for the app on the Session's first exposed port",
                    shown(&object["base_url"])
                ),
                CHECK_PATH_EXAMPLE,
            )
        })?;
    }
    Ok(CheckJob {
        source,
        grep,
        base_url,
    })
}

fn is_test_source(path: &str) -> bool {
    [".ts", ".js", ".mjs", ".cjs", ".mts", ".cts", ".tsx", ".jsx"]
        .iter()
        .any(|extension| path.ends_with(extension))
}

fn valid_segment(segment: &str) -> bool {
    let mut bytes = segment.bytes();
    matches!(bytes.next(), Some(first) if first.is_ascii_alphanumeric() || matches!(first, b'_' | b'@' | b'+'))
        && bytes.all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-' | b'@' | b'+')
        })
}

/// A repository-relative path in the form the check runner accepts: `/`
/// separated, no `.` or `..`, no hidden or unusual names, not under
/// `node_modules`. A leading `./` is dropped.
pub fn normalize_repo_path(path: &str) -> Result<String, String> {
    let trimmed = path.strip_prefix("./").unwrap_or(path);
    if trimmed.is_empty() || trimmed.len() > 512 {
        return Err("path must be 1-512 bytes".into());
    }
    let segments: Vec<&str> = trimmed.split('/').collect();
    if segments.iter().any(|segment| !valid_segment(segment)) {
        return Err(format!(
            "path {path:?} must be a repository path of letters, digits, '.', '_', '-', '@' and '+', without '..'"
        ));
    }
    if segments[0] == "node_modules" {
        return Err("path must not be under node_modules".into());
    }
    Ok(trimmed.to_string())
}

/// Relative module specifiers (`./x`, `../y`) a source file imports.
pub fn relative_imports(source: &str) -> Vec<String> {
    let bytes = source.as_bytes();
    let mut found = Vec::new();
    let mut index = 0;
    while index < bytes.len() {
        let quote = bytes[index];
        if quote != b'\'' && quote != b'"' && quote != b'`' {
            index += 1;
            continue;
        }
        let start = index + 1;
        let mut end = start;
        while end < bytes.len() && bytes[end] != quote && bytes[end] != b'\n' {
            if bytes[end] == b'\\' {
                end += 1;
            }
            end += 1;
        }
        if end >= bytes.len() || bytes[end] != quote {
            index = end.max(start);
            continue;
        }
        let literal = &source[start..end];
        let before = source[..index].trim_end();
        let introduced = before.ends_with("from")
            || before.ends_with("import")
            || before.ends_with("require(")
            || before.ends_with("import(");
        if introduced
            && (literal.starts_with("./") || literal.starts_with("../"))
            && !literal.contains('\\')
            && !found.iter().any(|seen| seen == literal)
        {
            found.push(literal.to_string());
        }
        index = end + 1;
    }
    found
}

fn join_relative(from: &str, specifier: &str) -> Option<String> {
    let mut parts: Vec<&str> = from.split('/').collect();
    parts.pop();
    for part in specifier.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop()?;
            }
            other => parts.push(other),
        }
    }
    (!parts.is_empty()).then(|| parts.join("/"))
}

/// Extensions a test's imports may resolve to. Nothing else of the
/// repository is sent to the browser container.
const IMPORT_EXTENSIONS: [&str; 9] = [
    ".ts", ".tsx", ".mts", ".cts", ".js", ".mjs", ".cjs", ".jsx", ".json",
];

fn import_candidates(base: &str) -> Vec<String> {
    let mut candidates = Vec::new();
    if IMPORT_EXTENSIONS
        .iter()
        .any(|extension| base.ends_with(extension))
    {
        candidates.push(base.to_string());
    }
    for extension in IMPORT_EXTENSIONS {
        candidates.push(format!("{base}{extension}"));
    }
    // TypeScript ESM code imports `./x.js` for `./x.ts`.
    for (js, ts) in [(".js", ".ts"), (".mjs", ".mts"), (".cjs", ".cts")] {
        if let Some(stem) = base.strip_suffix(js) {
            candidates.push(format!("{stem}{ts}"));
            candidates.push(format!("{stem}.tsx"));
        }
    }
    candidates.push(format!("{base}/index.ts"));
    candidates.push(format!("{base}/index.js"));
    candidates
}

/// One file sent to the check runner.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckFile {
    pub path: String,
    pub content: String,
}

/// Read `entry` and, transitively, every repository file it imports by a
/// relative path. `read` returns `Ok(None)` for a missing file. Imports
/// outside the repository or that do not exist are left to the runner to
/// report. Bounded by [`MAX_CHECK_FILES`] and the byte limits.
pub fn collect_check_files(
    entry: &str,
    read: &mut dyn FnMut(&str) -> Result<Option<String>, String>,
) -> Result<Vec<CheckFile>, String> {
    let entry = normalize_repo_path(entry)?;
    let content = read(&entry)?.ok_or_else(|| format!("{entry} does not exist"))?;
    let mut files = Vec::new();
    let mut seen = BTreeSet::from([entry.clone()]);
    let mut queue = vec![(entry, content)];
    let mut total = 0usize;
    while let Some((path, content)) = queue.pop() {
        if content.len() > MAX_CHECK_FILE_BYTES {
            return Err(format!("{path} exceeds {MAX_CHECK_FILE_BYTES} bytes"));
        }
        total += content.len();
        if total > MAX_CHECK_TOTAL_BYTES {
            return Err(format!(
                "the test and its imports exceed {MAX_CHECK_TOTAL_BYTES} bytes"
            ));
        }
        for specifier in relative_imports(&content) {
            let Some(base) = join_relative(&path, &specifier) else {
                continue;
            };
            for candidate in import_candidates(&base) {
                let Ok(candidate) = normalize_repo_path(&candidate) else {
                    continue;
                };
                if seen.contains(&candidate) {
                    break;
                }
                if let Some(found) = read(&candidate)? {
                    if seen.len() >= MAX_CHECK_FILES {
                        return Err(format!(
                            "the test imports more than {MAX_CHECK_FILES} files"
                        ));
                    }
                    seen.insert(candidate.clone());
                    queue.push((candidate, found));
                    break;
                }
            }
        }
        files.push(CheckFile { path, content });
    }
    Ok(files)
}

fn limits(settings: &BrowserSettings) -> Value {
    json!({
        "snapshot_max_bytes": settings.snapshot_max_bytes,
        "step_timeout_ms": 10_000,
        "total_timeout_ms": settings.script_budget_ms(),
        "test_timeout_ms": 30_000,
        "console_max": 50,
        "network_max": 50,
        "output_max_bytes": OUTPUT_MAX_BYTES,
        "log_max_bytes": 8192,
        "screenshot_max_bytes": SCREENSHOT_MAX_BYTES,
    })
}

/// The driver input for a `browser` call, without a proxy credential.
pub fn drive_payload(job: &DriveJob, aut_origins: &[String], settings: &BrowserSettings) -> Value {
    let mut payload = json!({
        "schema": DRIVER_SCHEMA_IN,
        "url": job.url,
        "steps": job.steps,
        "snapshot": job.snapshot.as_str(),
        "proxy": Value::Null,
        "aut_origins": aut_origins,
        "screenshot": true,
        "limits": limits(settings),
    });
    if let Some((width, height)) = job.viewport {
        payload["viewport"] = json!({ "width": width, "height": height });
    }
    payload
}

/// The runner input for a `browser_check` call, without a proxy credential.
pub fn check_payload(
    entry: &str,
    files: &[CheckFile],
    job: &CheckJob,
    base_url: &str,
    settings: &BrowserSettings,
) -> Value {
    json!({
        "schema": CHECK_SCHEMA_IN,
        "entry": entry,
        "files": files.iter().map(|file| json!({"path": file.path, "content": file.content})).collect::<Vec<_>>(),
        "grep": job.grep,
        "base_url": base_url,
        "proxy": Value::Null,
        "limits": limits(settings),
    })
}

/// The egress proxy as Chromium sees it inside the browser container.
#[derive(Clone, Copy)]
pub struct ProxyCredential<'a> {
    /// `http://127.0.0.1:<port>`, the browser container's proxy listener.
    pub server: &'a str,
    pub password: &'a str,
}

impl std::fmt::Debug for ProxyCredential<'_> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ProxyCredential")
            .field("server", &self.server)
            .finish_non_exhaustive()
    }
}

/// Serialize a payload for the script's stdin, adding the proxy and its
/// credential when the call uses declared hosts. Without them the script
/// configures no proxy at all, and only loopback is reachable. This is the
/// only place the credential is written.
pub fn stdin_with_proxy(
    payload: &Value,
    proxy: Option<ProxyCredential<'_>>,
) -> Result<Vec<u8>, String> {
    let mut payload = payload.clone();
    payload["proxy"] = match proxy {
        Some(proxy) => json!({
            "server": proxy.server,
            "username": BROWSER_PROXY_USER,
            "password": proxy.password,
        }),
        None => Value::Null,
    };
    serde_json::to_vec(&payload).map_err(|error| error.to_string())
}

/// The bytes one script run produced.
#[derive(Debug, Clone, Default)]
pub struct RunnerOutput {
    /// `None` when the process did not exit normally.
    pub exit_code: Option<i32>,
    pub stdout: Vec<u8>,
    pub stderr: String,
}

/// A screenshot taken during a call, for the record only.
#[derive(Clone, PartialEq, Eq)]
pub struct Screenshot {
    pub media_type: &'static str,
    pub bytes: Vec<u8>,
}

impl std::fmt::Debug for Screenshot {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Screenshot")
            .field("media_type", &self.media_type)
            .field("bytes", &self.bytes.len())
            .finish()
    }
}

/// A finished call: the model-facing result and what only the record keeps.
#[derive(Debug, Clone)]
pub struct BrowserReport {
    pub tool: &'static str,
    pub ok: bool,
    /// `browser`: the page's final URL. `browser_check`: the test status.
    pub final_url: Option<String>,
    pub status: Option<u16>,
    pub check_status: Option<String>,
    pub ms: u64,
    pub result: Value,
    pub screenshot: Option<Screenshot>,
    /// Why a screenshot was not kept, when the script said so.
    pub screenshot_dropped: Option<String>,
    /// Why the call failed before it produced a result, for a call that
    /// returns a tool error. Such a call is still recorded.
    pub error: Option<String>,
}

impl BrowserReport {
    /// A call that failed before it produced a result: the runner, the
    /// container or the script failed, or the call was cancelled or ran out
    /// of time. It may still have changed the app's state.
    pub fn failure(job: &BrowserJob, reason: &str, ms: u64) -> Self {
        Self {
            tool: job.tool(),
            ok: false,
            final_url: None,
            status: None,
            check_status: None,
            ms,
            result: Value::Null,
            screenshot: None,
            screenshot_dropped: None,
            error: Some(reason.chars().take(MAX_RECORDED_ERROR_CHARS).collect()),
        }
    }
}

/// A screenshot the runner kept.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordedScreenshot {
    pub sha256: String,
    pub bytes: usize,
}

/// Runs the scripts in the browser container and keeps the record.
#[async_trait::async_trait]
pub trait BrowserRunner: Send + Sync {
    /// Run the job's script and return its output.
    async fn run(&self, job: &BrowserJob) -> Result<RunnerOutput, String>;
    /// Keep a call in the Session's record: every finished call, and every
    /// call that failed after its arguments were accepted (with
    /// [`BrowserReport::error`] set). An error fails the call, so no browser
    /// call goes unrecorded.
    async fn record(
        &self,
        _job: &BrowserJob,
        _report: &BrowserReport,
    ) -> Result<Option<RecordedScreenshot>, String> {
        Ok(None)
    }
}

fn stderr_tail(stderr: &str) -> String {
    let tail: String = stderr
        .chars()
        .rev()
        .take(2000)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    tail.trim().to_string()
}

fn parse_document(tool: &str, output: &RunnerOutput, schema: &str) -> Result<Value, ToolError> {
    let line = output
        .stdout
        .split(|byte| *byte == b'\n')
        .find(|line| !line.iter().all(u8::is_ascii_whitespace));
    let document: Option<Value> = line.and_then(|line| serde_json::from_slice(line).ok());
    let Some(document) = document.filter(Value::is_object) else {
        return Err(failed(
            tool,
            format!(
                "the browser container returned no result (exit {}): {}",
                output
                    .exit_code
                    .map_or("unknown".to_string(), |code| code.to_string()),
                stderr_tail(&output.stderr)
            ),
        ));
    };
    if document.get("schema").and_then(Value::as_str) != Some(schema) {
        return Err(failed(
            tool,
            "the browser container answered in an unknown format",
        ));
    }
    if let Some(error) = document.get("error").and_then(Value::as_str) {
        return Err(failed(tool, error.chars().take(2000).collect::<String>()));
    }
    if output.exit_code != Some(0) {
        return Err(failed(
            tool,
            format!(
                "the browser container's script exited with {}: {}",
                output
                    .exit_code
                    .map_or("a signal".to_string(), |code| code.to_string()),
                stderr_tail(&output.stderr)
            ),
        ));
    }
    Ok(document)
}

fn take_screenshot(document: &mut Value) -> (Option<Screenshot>, Option<String>) {
    let Some(shot) = document
        .as_object_mut()
        .and_then(|object| object.remove("screenshot"))
    else {
        return (None, None);
    };
    if let Some(dropped) = shot.get("dropped").and_then(Value::as_str) {
        return (None, Some(dropped.chars().take(200).collect()));
    }
    let media_type = match shot.get("type").and_then(Value::as_str) {
        Some("jpeg") => "image/jpeg",
        Some("png") => "image/png",
        _ => return (None, Some("unknown_type".into())),
    };
    let Some(encoded) = shot.get("base64").and_then(Value::as_str) else {
        return (None, Some("missing".into()));
    };
    if encoded.len() > SCREENSHOT_MAX_BYTES * 4 / 3 + 8 {
        return (None, Some("too_large".into()));
    }
    let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(encoded) else {
        return (None, Some("undecodable".into()));
    };
    let magic_ok = match media_type {
        "image/jpeg" => bytes.starts_with(&[0xff, 0xd8, 0xff]),
        _ => bytes.starts_with(b"\x89PNG\r\n\x1a\n"),
    };
    if !magic_ok || bytes.len() > SCREENSHOT_MAX_BYTES {
        return (None, Some("invalid_image".into()));
    }
    (Some(Screenshot { media_type, bytes }), None)
}

fn json_size(value: &Value) -> usize {
    serde_json::to_vec(value).map_or(usize::MAX, |bytes| bytes.len())
}

fn truncate_utf8(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_string();
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_string()
}

/// Keep a result within [`OUTPUT_MAX_BYTES`]: shrink the snapshot text, then
/// the longest lists. Sets `truncated.output` when anything was cut.
pub fn rebound(result: &mut Value, lists: &[&[&str]]) {
    if json_size(result) <= OUTPUT_MAX_BYTES {
        return;
    }
    if !result["truncated"].is_object() {
        result["truncated"] = json!({});
    }
    result["truncated"]["output"] = Value::Bool(true);
    for key in ["snapshot", "stdout", "stderr"] {
        let size = json_size(result);
        if size <= OUTPUT_MAX_BYTES {
            return;
        }
        let excess = size - OUTPUT_MAX_BYTES + 64;
        let slot = if key == "snapshot" {
            result.pointer_mut("/snapshot/text")
        } else {
            result.get_mut(key)
        };
        if let Some(Value::String(text)) = slot {
            let keep = text.len().saturating_sub(excess);
            *text = truncate_utf8(text, keep);
            if key == "snapshot" {
                result["snapshot"]["truncated"] = Value::Bool(true);
            }
        }
    }
    for path in lists {
        loop {
            if json_size(result) <= OUTPUT_MAX_BYTES {
                return;
            }
            let mut slot = Some(&mut *result);
            for key in *path {
                slot = slot.and_then(|value| value.get_mut(*key));
            }
            match slot {
                Some(Value::Array(items)) if !items.is_empty() => {
                    items.pop();
                }
                _ => break,
            }
        }
    }
}

/// Bound and split the driver's answer for a `browser` call.
pub fn parse_drive_output(output: &RunnerOutput) -> Result<BrowserReport, ToolError> {
    let mut document = parse_document(BROWSER_TOOL, output, DRIVER_SCHEMA_OUT)?;
    let (screenshot, screenshot_dropped) = take_screenshot(&mut document);
    rebound(
        &mut document,
        &[
            &["console"],
            &["page_errors"],
            &["network", "failed"],
            &["network", "http_errors"],
            &["network", "blocked"],
            &["dialogs"],
            &["steps"],
        ],
    );
    Ok(BrowserReport {
        tool: BROWSER_TOOL,
        ok: document["ok"].as_bool().unwrap_or(false),
        final_url: document["final_url"].as_str().map(str::to_string),
        status: document["status"]
            .as_u64()
            .and_then(|status| u16::try_from(status).ok()),
        check_status: None,
        ms: document["ms"].as_u64().unwrap_or(0),
        result: document,
        screenshot,
        screenshot_dropped,
        error: None,
    })
}

/// Bound and split the runner's answer for a `browser_check` call.
pub fn parse_check_output(output: &RunnerOutput) -> Result<BrowserReport, ToolError> {
    let mut document = parse_document(BROWSER_CHECK_TOOL, output, CHECK_SCHEMA_OUT)?;
    let (screenshot, screenshot_dropped) = take_screenshot(&mut document);
    rebound(&mut document, &[&["tests"], &["errors"]]);
    Ok(BrowserReport {
        tool: BROWSER_CHECK_TOOL,
        ok: document["ok"].as_bool().unwrap_or(false),
        final_url: None,
        status: None,
        check_status: document["status"].as_str().map(str::to_string),
        ms: document["ms"].as_u64().unwrap_or(0),
        result: document,
        screenshot,
        screenshot_dropped,
        error: None,
    })
}

fn screenshot_note(report: &BrowserReport, recorded: Option<RecordedScreenshot>) -> Option<Value> {
    match (recorded, &report.screenshot, &report.screenshot_dropped) {
        (Some(recorded), _, _) => Some(json!({
            "recorded": true,
            "sha256": recorded.sha256,
            "bytes": recorded.bytes,
            "note": "Kept in the Session's record for people to see; screenshots are not shown to you.",
        })),
        (None, Some(_), _) => Some(json!({"recorded": false})),
        (None, None, Some(reason)) => Some(json!({"recorded": false, "reason": reason})),
        (None, None, None) => None,
    }
}

fn record_unavailable(tool: &str, reason: String) -> ToolError {
    failed(
        tool,
        format!("the Session's record is unavailable, so the result is withheld: {reason}"),
    )
}

async fn run_job(runner: &Arc<dyn BrowserRunner>, job: BrowserJob) -> Result<Value, ToolError> {
    let tool = job.tool();
    let started = std::time::Instant::now();
    let parsed = match runner.run(&job).await {
        Ok(output) => match &job {
            BrowserJob::Drive(_) => parse_drive_output(&output),
            BrowserJob::Check(_) => parse_check_output(&output),
        },
        Err(reason) => Err(failed(tool, reason)),
    };
    let mut report = match parsed {
        Ok(report) => report,
        Err(error) => {
            // A failed call may still have changed the app's state, so it is
            // recorded too, with the reason, before the error is returned.
            let reason = match &error {
                ToolError::ExecutionFailed { reason, .. } => reason.clone(),
                other => other.to_string(),
            };
            let ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
            runner
                .record(&job, &BrowserReport::failure(&job, &reason, ms))
                .await
                .map_err(|record| {
                    record_unavailable(tool, format!("{record}; the call failed: {reason}"))
                })?;
            return Err(error);
        }
    };
    let recorded = runner
        .record(&job, &report)
        .await
        .map_err(|reason| record_unavailable(tool, reason))?;
    if let Some(note) = screenshot_note(&report, recorded) {
        report.result["screenshot"] = note;
    }
    Ok(report.result)
}

/// The `browser` tool. Without a runner it only describes itself.
#[derive(Clone)]
pub struct BrowserTool {
    runner: Option<Arc<dyn BrowserRunner>>,
}

impl BrowserTool {
    /// The definition offered to models; calling it fails.
    pub fn definition() -> Self {
        Self { runner: None }
    }

    pub fn with_runner(runner: Arc<dyn BrowserRunner>) -> Self {
        Self {
            runner: Some(runner),
        }
    }

    /// The argument schema. Some providers' chat templates show a model
    /// only each top-level property's type and description, so every
    /// description states, on its own, the default and the accepted values
    /// or shape.
    pub fn schema() -> Value {
        let target = json!({
            "type": "object",
            "additionalProperties": false,
            "description": "Exactly one of role (with optional name), label, text, testid or css; nth picks one of several matches.",
            "properties": {
                "role": {"type": "string"}, "name": {"type": "string"}, "label": {"type": "string"},
                "text": {"type": "string"}, "testid": {"type": "string"}, "css": {"type": "string"},
                "nth": {"type": "integer", "minimum": 0}
            }
        });
        json!({
            "type": "object",
            "required": ["url"],
            "additionalProperties": false,
            "properties": {
                "url": {"type": "string", "maxLength": MAX_STRING_BYTES, "description": "Required. The http(s) page to open first, such as http://localhost:3000/. This Session's apps are at http://localhost:<port> for its exposed ports."},
                "steps": {"type": "array", "maxItems": MAX_STEPS, "description": "Optional; leave out to just read the page. Up to 40 actions run in order after url loads. Each is an object such as {\"action\": \"click\", \"target\": {\"role\": \"button\", \"name\": \"Save\"}}. Actions: goto (url); click, check, uncheck (target); fill, select (target, value); press (key, optional target); wait_for (one of target, url or text); expect_text (text, optional target); reload; back. A target is one of role (with optional name), label, text, testid or css, with optional nth. No JavaScript or Playwright code.", "items": {
                    "type": "object", "required": ["action"], "additionalProperties": false,
                    "properties": {
                        "action": {"type": "string", "enum": ACTIONS},
                        "target": target,
                        "url": {"type": "string"}, "value": {"type": "string"}, "key": {"type": "string"},
                        "text": {"type": "string"},
                        "timeout_ms": {"type": "integer", "minimum": 100, "maximum": 10000, "default": 10000}
                    }
                }},
                "snapshot": {"type": "string", "enum": ["aria", "text", "none"], "default": "aria", "description": "Optional. What the result shows of the page after the steps: \"aria\" (the default; the accessibility tree, with the roles and names targets use), \"text\" (the visible text) or \"none\"."},
                "viewport": {"type": "object", "additionalProperties": false, "description": "Optional; default {\"width\": 1280, \"height\": 720}. width 320-1920, height 240-1200.", "properties": {
                    "width": {"type": "integer", "minimum": 320, "maximum": 1920, "default": 1280},
                    "height": {"type": "integer", "minimum": 240, "maximum": 1200, "default": 720}
                }}
            }
        })
    }
}

#[async_trait::async_trait]
impl BuiltinTool for BrowserTool {
    fn description(&self) -> &str {
        BROWSER_DESCRIPTION
    }

    fn parameters_schema(&self) -> Value {
        Self::schema()
    }

    fn concurrency_policy(&self) -> axocoatl_llm::ConcurrencyPolicy {
        axocoatl_llm::ConcurrencyPolicy::Exclusive
    }

    async fn execute(&self, arguments: Value) -> Result<Value, ToolError> {
        let job = BrowserJob::Drive(parse_drive_call(&arguments)?);
        let runner = self
            .runner
            .as_ref()
            .ok_or_else(|| failed(BROWSER_TOOL, "the browser tool is not bound to a Session"))?;
        run_job(runner, job).await
    }
}

/// The `browser_check` tool. Without a runner it only describes itself.
#[derive(Clone)]
pub struct BrowserCheckTool {
    runner: Option<Arc<dyn BrowserRunner>>,
}

impl BrowserCheckTool {
    pub fn definition() -> Self {
        Self { runner: None }
    }

    pub fn with_runner(runner: Arc<dyn BrowserRunner>) -> Self {
        Self {
            runner: Some(runner),
        }
    }

    pub fn schema() -> Value {
        json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "path": {"type": "string", "maxLength": 512, "description": "Repository path of a Playwright test file, such as qa/findings/B07.spec.ts. Give path or script, not both."},
                "script": {"type": "string", "maxLength": MAX_CHECK_SCRIPT_BYTES, "description": "A Playwright test file's source, instead of path."},
                "grep": {"type": "string", "maxLength": 512, "description": "Optional; leave out to run every test. Runs only tests whose title matches this regular expression."},
                "base_url": {"type": "string", "maxLength": MAX_STRING_BYTES, "description": "Optional. baseURL for page.goto('/'); defaults to the app on the Session's first exposed port."}
            }
        })
    }
}

#[async_trait::async_trait]
impl BuiltinTool for BrowserCheckTool {
    fn description(&self) -> &str {
        CHECK_DESCRIPTION
    }

    fn parameters_schema(&self) -> Value {
        Self::schema()
    }

    fn concurrency_policy(&self) -> axocoatl_llm::ConcurrencyPolicy {
        axocoatl_llm::ConcurrencyPolicy::Exclusive
    }

    async fn execute(&self, arguments: Value) -> Result<Value, ToolError> {
        let job = BrowserJob::Check(parse_check_call(&arguments)?);
        let runner = self.runner.as_ref().ok_or_else(|| {
            failed(
                BROWSER_CHECK_TOOL,
                "the browser_check tool is not bound to a Session",
            )
        })?;
        run_job(runner, job).await
    }
}

#[cfg(test)]
#[path = "browser_tool_tests.rs"]
mod tests;
