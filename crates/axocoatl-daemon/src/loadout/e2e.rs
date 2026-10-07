//! The tester-army/e2e required check: expansion of a loadout `e2e` check
//! into an exact argv and options, the admission's model capability check,
//! and collection of its report after the turn. Owner: e2e.
//!
//! The check runs `e2e@0.18.0` (installed by the `e2e` recipe) inside the
//! Session container as one required check: `["sh", "-c", <wrapper>]`. The
//! wrapper
//!
//! - forces [`E2E_FORCED_ENV`] (telemetry off, CI defaults: one worker, one
//!   retry, a read-only replay cache);
//! - names e2e's model and its route-backed endpoint in [`E2E_MODEL_ENV`],
//!   [`E2E_BASE_URL_ENV`] and [`E2E_API_KEY_ENV`], plus the provider
//!   package's own key variable for a well-known route host. Every key is
//!   the placeholder `axocoatl-route:<host>`: the route adds the real
//!   credential from Axocoatl's secret store on the way out, so it never
//!   enters the container;
//! - runs `e2e <command> --reporter json <args>` with the report document
//!   on its stdout redirected to `/tmp/axocoatl-check-reports/<name>/report.json`
//!   (outside the Workspace, so the check changes no repository path) and
//!   e2e's diagnostics to a log beside it;
//! - prints at most [`E2E_STDOUT_TAIL_BYTES`] of that log, then
//!   `AXOCOATL-CHECK-REPORT sha256=<hex>` of the report as its last stdout
//!   line, and exits with e2e's status.
//!
//! The check's whole stdout stays far below the bytes a check's recorded
//! stdout retains (a prefix of 768 KiB), so the marker is always recorded;
//! [`collect_reports`] reads it from `CheckResult::stdout_tail`, which must
//! keep at least the last line (the marker is 94 bytes).

use std::fmt::Write as _;
use std::time::Duration;

use async_trait::async_trait;
use axocoatl_config::loadout::{E2eCheck, ParamOr, ResolvedCheck, ResolvedLoadout};
use axocoatl_config::types::EgressRouteYaml;
use axocoatl_session::check_options::{
    validate_check_options, CheckReportSpec, RequiredCheckOptions, CHECK_REPORT_DIR,
    MAX_CHECK_TIMEOUT_MS,
};
use axocoatl_session::check_report::{
    e2e_report_models, parse_report, report_digest, report_marker, MAX_REPORT_BYTES,
    REPORT_FORMAT_E2E,
};
use axocoatl_session::run_outcome::{CheckResult, CheckState, RunWarning};

use super::{RunContext, RunError, RunHost};

/// The npm package and version the check runs.
pub const E2E_PACKAGE: &str = "e2e@0.18.0";
/// The version `e2e --version` prints in the recipe image.
pub const E2E_VERSION: &str = "0.18.0";
/// Environment the check always sets: telemetry off (both of e2e's
/// switches), and e2e's CI defaults (one worker, one retry, a read-only
/// replay cache, focused tests refused).
pub const E2E_FORCED_ENV: &[(&str, &str)] = &[
    ("E2E_TELEMETRY_DISABLED", "1"),
    ("DO_NOT_TRACK", "1"),
    ("CI", "1"),
    ("NO_COLOR", "1"),
];
/// The model e2e's agent should use (the loadout check's `model`).
pub const E2E_MODEL_ENV: &str = "AXOCOATL_E2E_MODEL";
/// An OpenAI-compatible base URL on the check's model route.
pub const E2E_BASE_URL_ENV: &str = "AXOCOATL_E2E_BASE_URL";
/// The key placeholder the route replaces with the real credential.
pub const E2E_API_KEY_ENV: &str = "AXOCOATL_E2E_API_KEY";
/// Where the recipe image keeps Playwright's browsers.
pub const E2E_BROWSERS_PATH: &str = "/opt/axocoatl-e2e/ms-playwright";
/// The report file inside each check's report directory.
pub const E2E_REPORT_FILE: &str = "report.json";
/// Most bytes of e2e's own output the check prints before its marker.
pub const E2E_STDOUT_TAIL_BYTES: usize = 16 * 1024;
/// The e2e commands a check may run.
pub const E2E_COMMANDS: [&str; 2] = ["run", "explore"];
/// The route host whose models the admission verifies in a catalog.
pub const OPENROUTER_HOST: &str = "openrouter.ai";
/// OpenRouter's public API base, for its model catalog.
pub const OPENROUTER_API_BASE: &str = "https://openrouter.ai/api/v1";
/// Warning code: an e2e check's model could not be checked for tool calls
/// and image input.
pub const E2E_MODEL_UNVERIFIED: &str = "e2e_model_capability_unverified";

const MAX_E2E_ARGS: usize = 64;
const MAX_E2E_ARG_BYTES: usize = 1024;
const MAX_MODEL_BYTES: usize = 200;
const MAX_CATALOG_BYTES: usize = 32 * 1024 * 1024;
const CATALOG_TIMEOUT: Duration = Duration::from_secs(30);

/// Flags the wrapper owns, or that make no sense in a headless check.
const REFUSED_FLAGS: [&str; 6] = ["--reporter", "--help", "-h", "--version", "-v", "--headed"];

/// A model route host whose provider package Axocoatl knows: the path of
/// its OpenAI-compatible API and the key variable the package reads.
struct KnownRoute {
    host: &'static str,
    base_path: &'static str,
    key_env: &'static str,
}

const KNOWN_ROUTES: [KnownRoute; 7] = [
    KnownRoute {
        host: OPENROUTER_HOST,
        base_path: "/api/v1",
        key_env: "OPENROUTER_API_KEY",
    },
    KnownRoute {
        host: "api.openai.com",
        base_path: "/v1",
        key_env: "OPENAI_API_KEY",
    },
    KnownRoute {
        host: "api.anthropic.com",
        base_path: "/v1",
        key_env: "ANTHROPIC_API_KEY",
    },
    KnownRoute {
        host: "ai-gateway.vercel.sh",
        base_path: "/v1",
        key_env: "AI_GATEWAY_API_KEY",
    },
    KnownRoute {
        host: "generativelanguage.googleapis.com",
        base_path: "/v1beta/openai",
        key_env: "GOOGLE_GENERATIVE_AI_API_KEY",
    },
    KnownRoute {
        host: "api.mistral.ai",
        base_path: "/v1",
        key_env: "MISTRAL_API_KEY",
    },
    KnownRoute {
        host: "api.x.ai",
        base_path: "/v1",
        key_env: "XAI_API_KEY",
    },
];

fn usage(message: impl Into<String>) -> RunError {
    RunError::Usage(message.into())
}

/// The value a container sees in place of a route's credential (the same
/// placeholder routes' `env_placeholders` carry).
pub fn route_placeholder(host: &str) -> String {
    format!("axocoatl-route:{host}")
}

/// The report directory of check `name` inside the Session container.
pub fn report_dir(name: &str) -> String {
    format!("{CHECK_REPORT_DIR}{name}")
}

/// The report an e2e check named `name` writes.
pub fn report_spec(name: &str) -> CheckReportSpec {
    CheckReportSpec {
        format: REPORT_FORMAT_E2E.into(),
        path: format!("{}/{E2E_REPORT_FILE}", report_dir(name)),
    }
}

/// The loadout's e2e checks, in declaration order.
pub fn e2e_checks(resolved: &ResolvedLoadout) -> impl Iterator<Item = (&ResolvedCheck, &E2eCheck)> {
    resolved
        .checks
        .iter()
        .filter_map(|check| check.e2e.as_ref().map(|e2e| (check, e2e)))
}

/// Whether the run's Session container mounts `<workspace>/.e2e/cache`
/// read-only (`SessionSandbox::start_in_with_mounts`): every loadout with an
/// e2e check.
pub fn workspace_mounts(resolved: &ResolvedLoadout) -> axocoatl_isolation::WorkspaceMounts {
    axocoatl_isolation::WorkspaceMounts {
        read_only_e2e_cache: e2e_checks(resolved).next().is_some(),
    }
}

/// Single-quoted for `sh`.
fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', r"'\''"))
}

fn valid_check_name(name: &str) -> bool {
    let mut bytes = name.bytes();
    matches!(bytes.next(), Some(b'a'..=b'z'))
        && name.len() <= 32
        && bytes.all(|byte| matches!(byte, b'a'..=b'z' | b'0'..=b'9' | b'-'))
}

fn valid_host(host: &str) -> bool {
    !host.is_empty()
        && host.len() <= 253
        && !host.starts_with(['.', '-'])
        && !host.ends_with(['.', '-'])
        && !host.contains("..")
        && host.contains('.')
        && host
            .bytes()
            .all(|byte| matches!(byte, b'a'..=b'z' | b'0'..=b'9' | b'.' | b'-'))
}

/// The model of `check`, its parameter resolved.
pub fn resolve_e2e_model(check: &E2eCheck, resolved: &ResolvedLoadout) -> Result<String, RunError> {
    let model = match &check.model {
        ParamOr::Value(model) => model.clone(),
        ParamOr::Param { param } => resolved.params.get(param).cloned().ok_or_else(|| {
            usage(format!(
                "the e2e check's model parameter {param} has no value: pass --param {param}=<model>"
            ))
        })?,
    };
    let model = model.trim().to_owned();
    if model.is_empty()
        || model.len() > MAX_MODEL_BYTES
        || !model
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"._:/@+-".contains(&byte))
    {
        return Err(usage(format!(
            "the e2e check's model {:?} is not a model id (letters, digits and ._:/@+- only, at \
             most {MAX_MODEL_BYTES} bytes)",
            model.chars().take(64).collect::<String>()
        )));
    }
    Ok(model)
}

/// The loadout route that carries `check`'s model calls. It must hold a
/// credential: the key reaches the provider only through the route.
fn model_route<'a>(
    name: &str,
    check: &E2eCheck,
    resolved: &'a ResolvedLoadout,
) -> Result<&'a EgressRouteYaml, RunError> {
    let route = resolved
        .loadout
        .file
        .routes
        .iter()
        .find(|route| route.host == check.model_route)
        .ok_or_else(|| {
            usage(format!(
                "check {name}: model_route {:?} names no host under the loadout's routes",
                check.model_route
            ))
        })?;
    if !valid_host(&route.host) {
        return Err(usage(format!(
            "check {name}: model_route {:?} is not an exact lowercase host name",
            route.host
        )));
    }
    if route.credential.is_none() || route.inject.is_none() {
        return Err(usage(format!(
            "check {name}: the route to {} carries no credential; give it a credential (a \
             secret set with `axocoatl secret set`) and inject, so e2e's model key never \
             enters the container",
            route.host
        )));
    }
    Ok(route)
}

/// `https://host[:port]` of `route`: port 443 unless the route allows only
/// others.
fn route_origin(route: &EgressRouteYaml) -> String {
    match route.ports.as_deref() {
        Some(ports) if !ports.is_empty() && !ports.contains(&443) => {
            format!("https://{}:{}", route.host, ports[0])
        }
        _ => format!("https://{}", route.host),
    }
}

fn check_args(name: &str, args: &[String]) -> Result<Vec<String>, RunError> {
    let args: Vec<String> = if args.is_empty() {
        vec!["run".into()]
    } else {
        args.to_vec()
    };
    if !E2E_COMMANDS.contains(&args[0].as_str()) {
        return Err(usage(format!(
            "check {name}: an e2e check runs `e2e run` or `e2e explore`, not `e2e {}`",
            args[0].chars().take(64).collect::<String>()
        )));
    }
    if args.len() > MAX_E2E_ARGS {
        return Err(usage(format!(
            "check {name}: at most {MAX_E2E_ARGS} e2e arguments"
        )));
    }
    for arg in &args {
        if arg.len() > MAX_E2E_ARG_BYTES || arg.chars().any(char::is_control) {
            return Err(usage(format!(
                "check {name}: e2e arguments are at most {MAX_E2E_ARG_BYTES} bytes, without \
                 control characters"
            )));
        }
        let flag = arg.split_once('=').map_or(arg.as_str(), |(flag, _)| flag);
        if REFUSED_FLAGS.contains(&flag) {
            return Err(usage(format!(
                "check {name}: {flag} is not an e2e check argument (the check writes its own \
                 JSON report and runs headless)"
            )));
        }
    }
    Ok(args)
}

/// What one e2e check's wrapper needs.
struct Wrapper<'a> {
    name: &'a str,
    report_dir: &'a str,
    model: &'a str,
    route: &'a EgressRouteYaml,
    args: &'a [String],
}

impl Wrapper<'_> {
    fn script(&self) -> String {
        let quote = |value: &str| shell_quote(value);
        let host = self.route.host.as_str();
        let known = KNOWN_ROUTES.iter().find(|known| known.host == host);
        let base_url = format!(
            "{}{}",
            route_origin(self.route),
            known.map_or("/v1", |known| known.base_path)
        );
        let placeholder = route_placeholder(host);
        let mut script = String::new();
        let _ = writeln!(
            script,
            "# Axocoatl e2e check {} ({E2E_PACKAGE}); its report is bound to this run by the \
             last stdout line.",
            self.name
        );
        script.push_str("set -u\numask 077\n");
        let _ = writeln!(script, "report_dir={}", quote(self.report_dir));
        let _ = writeln!(script, "report=\"$report_dir/{E2E_REPORT_FILE}\"");
        script.push_str("log=\"$report_dir/e2e.log\"\n");
        script.push_str(
            "rm -rf -- \"$report_dir\" && mkdir -p -- \"$report_dir\" || \
             { echo \"axocoatl: cannot prepare $report_dir\" >&2; exit 3; }\n",
        );
        for (name, value) in E2E_FORCED_ENV {
            let _ = writeln!(script, "export {name}={}", quote(value));
        }
        let _ = writeln!(script, "export {E2E_MODEL_ENV}={}", quote(self.model));
        let _ = writeln!(script, "export {E2E_BASE_URL_ENV}={}", quote(&base_url));
        let _ = writeln!(script, "export {E2E_API_KEY_ENV}={}", quote(&placeholder));
        if let Some(known) = known {
            let _ = writeln!(script, "export {}={}", known.key_env, quote(&placeholder));
        }
        let _ = writeln!(
            script,
            "export PLAYWRIGHT_BROWSERS_PATH=\"${{PLAYWRIGHT_BROWSERS_PATH:-{E2E_BROWSERS_PATH}}}\""
        );
        script.push_str(
            "command -v e2e >/dev/null 2>&1 || { echo \"axocoatl: e2e is not installed in this \
             Session's image; build it with environment.recipes: [e2e]\"; exit 127; }\n",
        );
        let mut command = vec![
            "e2e".to_owned(),
            quote(&self.args[0]),
            "--reporter".to_owned(),
            "json".to_owned(),
        ];
        command.extend(self.args[1..].iter().map(|arg| quote(arg)));
        let _ = writeln!(script, "{} >\"$report\" 2>\"$log\"", command.join(" "));
        script.push_str("status=$?\n");
        let _ = writeln!(script, "tail -c {E2E_STDOUT_TAIL_BYTES} -- \"$log\"");
        script.push_str(
            "digest=\n\
             if [ -s \"$report\" ]; then\n  \
               digest=$(sha256sum <\"$report\") && digest=${digest%% *}\n\
             fi\n\
             case $digest in *[!0-9a-f]*) digest= ;; esac\n\
             if [ \"${#digest}\" -eq 64 ]; then\n  \
               printf '\\n%s%s\\n' 'AXOCOATL-CHECK-REPORT sha256=' \"$digest\"\n\
             else\n  \
               printf '\\naxocoatl: e2e wrote no report (exit %s)\\n' \"$status\"\n\
             fi\n\
             exit \"$status\"\n",
        );
        script
    }
}

/// The argv and options of one `e2e` check.
pub fn expand_e2e_check(
    name: &str,
    check: &E2eCheck,
    timeout_secs: u64,
    resolved: &ResolvedLoadout,
) -> Result<(Vec<String>, RequiredCheckOptions), RunError> {
    if !valid_check_name(name) {
        return Err(usage(format!(
            "e2e check name {name:?}: 1-32 lowercase letters, digits or '-', starting with a letter"
        )));
    }
    let timeout_ms = timeout_secs
        .checked_mul(1000)
        .filter(|ms| (1_000..=MAX_CHECK_TIMEOUT_MS).contains(ms))
        .ok_or_else(|| {
            usage(format!(
                "check {name}: a timeout of 1 second to {} minutes",
                MAX_CHECK_TIMEOUT_MS / 60_000
            ))
        })?;
    let model = resolve_e2e_model(check, resolved)?;
    let route = model_route(name, check, resolved)?;
    let args = check_args(name, &check.args)?;
    let report = report_spec(name);
    let dir = report_dir(name);
    let script = Wrapper {
        name,
        report_dir: &dir,
        model: &model,
        route,
        args: &args,
    }
    .script();
    let argv = vec!["sh".to_owned(), "-c".to_owned(), script];
    let options = RequiredCheckOptions {
        name: Some(name.to_owned()),
        timeout_ms: Some(timeout_ms),
        report: Some(report),
    };
    validate_check_options(std::slice::from_ref(&argv), std::slice::from_ref(&options))
        .map_err(|reason| usage(format!("check {name}: {reason}")))?;
    Ok((argv, options))
}

/// Why a check's report was not attached, in words.
fn missing_marker_reason(result: &CheckResult) -> String {
    match result.state {
        CheckState::NotRun => {
            "the check did not run on the final candidate, so it has no report".into()
        }
        CheckState::Unavailable => {
            "the check's record could not be read, so its report cannot be bound".into()
        }
        CheckState::TimedOut => "the check timed out before it bound a report to its run".into(),
        CheckState::Passed | CheckState::Failed => {
            "the check's recorded stdout names no report (no AXOCOATL-CHECK-REPORT line): e2e \
             wrote no report.json for this run"
                .into()
        }
    }
}

/// The report of `result` bound by its stdout marker, or why there is none.
async fn bound_report(
    host: &dyn RunHost,
    session_id: &str,
    spec: &CheckReportSpec,
    result: &CheckResult,
) -> Result<Result<(axocoatl_session::run_outcome::CheckReport, Vec<u8>), String>, RunError> {
    let Some(marker) = report_marker(&result.stdout_tail) else {
        return Ok(Err(missing_marker_reason(result)));
    };
    let bytes = match host
        .read_sandbox_file(session_id, &spec.path, MAX_REPORT_BYTES + 1)
        .await
    {
        Ok(Some(bytes)) => bytes,
        Ok(None) => {
            return Ok(Err(format!(
                "the check named report sha256 {marker}, but {} is not in the Session container",
                spec.path
            )))
        }
        Err(RunError::NotImplemented(what)) => return Err(RunError::NotImplemented(what)),
        Err(error) => {
            return Ok(Err(format!(
                "the report {} could not be read: {error}",
                spec.path
            )))
        }
    };
    if bytes.len() > MAX_REPORT_BYTES {
        return Ok(Err(format!(
            "the report {} is larger than {MAX_REPORT_BYTES} bytes",
            spec.path
        )));
    }
    let digest = report_digest(&bytes);
    if digest != marker {
        return Ok(Err(format!(
            "the report {} (sha256 {digest}) is not the one the check run named (sha256 \
             {marker}); it changed after the run",
            spec.path
        )));
    }
    match parse_report(&spec.format, &bytes) {
        Ok(report) => Ok(Ok((report, bytes))),
        Err(error) => Ok(Err(format!("the bound report is unreadable: {error}"))),
    }
}

fn add_reason(result: &mut CheckResult, reason: String) {
    result.reason = Some(match result.reason.take() {
        Some(existing) if !existing.is_empty() => format!("{existing}; {reason}"),
        _ => reason,
    });
}

/// Read each check's bound report from the Session container and attach it
/// to its result.
///
/// For every e2e check of the run's loadout, the report is accepted only
/// when its SHA-256 equals the marker in the result's recorded stdout; it is
/// parsed and attached as `report`. Otherwise `report` stays `None` and
/// `reason` says why. Pass or fail is never changed: it is the check's exit
/// status. When the report shows e2e's agent calling a model other than the
/// loadout's, `reason` says so (the project's config chose the model).
pub async fn collect_reports(
    host: &dyn RunHost,
    run: &RunContext,
    checks: &mut [CheckResult],
) -> Result<(), RunError> {
    for (check, e2e) in e2e_checks(&run.resolved) {
        let Some(result) = checks.iter_mut().find(|result| result.name == check.name) else {
            continue;
        };
        let spec = report_spec(&check.name);
        match bound_report(host, &run.session_id, &spec, result).await? {
            Ok((report, bytes)) => {
                result.report = Some(report);
                let declared = resolve_e2e_model(e2e, &run.resolved).ok();
                let used = e2e_report_models(&bytes).unwrap_or_default();
                if let Some(declared) = declared
                    .filter(|declared| !used.is_empty() && !used.contains(declared.as_str()))
                {
                    let used: Vec<&str> = used.iter().map(String::as_str).collect();
                    add_reason(
                        result,
                        format!(
                            "e2e's agent called {}, not the loadout's model {declared}: the \
                             project's e2e config must select its model from {E2E_MODEL_ENV}",
                            used.join(", ")
                        ),
                    );
                }
            }
            Err(reason) => {
                result.report = None;
                add_reason(result, reason);
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Model capability (admission)
// ---------------------------------------------------------------------------

/// A source of OpenRouter's public model catalog (`GET /models`).
#[async_trait]
pub trait ModelCatalog: Send + Sync {
    async fn openrouter_models(&self) -> Result<serde_json::Value, String>;
}

/// OpenRouter's catalog over HTTPS from the host. The catalog is public:
/// the request carries no credential.
#[derive(Debug, Clone)]
pub struct OpenRouterCatalog {
    base_url: String,
}

impl OpenRouterCatalog {
    pub fn new(base_url: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into().trim_end_matches('/').to_owned(),
        }
    }
}

impl Default for OpenRouterCatalog {
    fn default() -> Self {
        Self::new(OPENROUTER_API_BASE)
    }
}

#[async_trait]
impl ModelCatalog for OpenRouterCatalog {
    async fn openrouter_models(&self) -> Result<serde_json::Value, String> {
        let client = reqwest::Client::builder()
            .timeout(CATALOG_TIMEOUT)
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|error| error.to_string())?;
        let mut response = client
            .get(format!("{}/models", self.base_url))
            .header(reqwest::header::ACCEPT, "application/json")
            .send()
            .await
            .map_err(|error| error.to_string())?;
        if !response.status().is_success() {
            return Err(format!("the catalog answered {}", response.status()));
        }
        let mut body = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|error| error.to_string())? {
            if body.len() + chunk.len() > MAX_CATALOG_BYTES {
                return Err(format!(
                    "the catalog is larger than {MAX_CATALOG_BYTES} bytes"
                ));
            }
            body.extend_from_slice(&chunk);
        }
        serde_json::from_slice(&body).map_err(|error| format!("the catalog is not JSON: {error}"))
    }
}

fn strings(value: &serde_json::Value) -> Vec<&str> {
    value
        .as_array()
        .map(|items| items.iter().filter_map(serde_json::Value::as_str).collect())
        .unwrap_or_default()
}

/// Whether OpenRouter's `catalog` lists `model` with tool calls and image
/// input, the two capabilities e2e's agent needs. A routing variant
/// (`vendor/model:nitro`) is checked as its base model when the catalog has
/// no row of its own for it.
pub fn openrouter_model_capable(model: &str, catalog: &serde_json::Value) -> Result<(), String> {
    let rows = catalog
        .get("data")
        .and_then(serde_json::Value::as_array)
        .ok_or("OpenRouter's model catalog has no data list")?;
    let find = |id: &str| {
        rows.iter()
            .find(|row| row.get("id").and_then(serde_json::Value::as_str) == Some(id))
    };
    let row = find(model)
        .or_else(|| model.split_once(':').and_then(|(base, _)| find(base)))
        .ok_or_else(|| format!("{model} is not in OpenRouter's model catalog"))?;
    let parameters = strings(&row["supported_parameters"]);
    let inputs = strings(&row["architecture"]["input_modalities"]);
    let mut missing = Vec::new();
    if !parameters.contains(&"tools") {
        missing.push("tool calls");
    }
    if !inputs.contains(&"image") {
        missing.push("image input");
    }
    if missing.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "{model} does not support {} on OpenRouter; e2e's agent needs tool calls and image \
             input",
            missing.join(" or ")
        ))
    }
}

/// The admission's check of every e2e check's model: an OpenRouter model
/// must list tool calls and image input in OpenRouter's catalog (else a
/// usage error, exit 3; an unreadable catalog is an infrastructure error,
/// exit 5, never a silent pass). A model on any other route gets the
/// warning [`E2E_MODEL_UNVERIFIED`].
pub async fn verify_e2e_models(
    resolved: &ResolvedLoadout,
    catalog: &dyn ModelCatalog,
) -> Result<Vec<RunWarning>, RunError> {
    let mut warnings = Vec::new();
    let mut openrouter: Option<serde_json::Value> = None;
    for (check, e2e) in e2e_checks(resolved) {
        let model = resolve_e2e_model(e2e, resolved)?;
        let route = model_route(&check.name, e2e, resolved)?;
        if route.host == OPENROUTER_HOST {
            if openrouter.is_none() {
                openrouter = Some(catalog.openrouter_models().await.map_err(|error| {
                    RunError::Infrastructure(format!(
                        "check {}: OpenRouter's model catalog could not be read to verify that \
                         {model} supports tool calls and image input: {error}",
                        check.name
                    ))
                })?);
            }
            if let Some(catalog) = &openrouter {
                openrouter_model_capable(&model, catalog)
                    .map_err(|reason| usage(format!("check {}: {reason}", check.name)))?;
            }
        } else {
            warnings.push(RunWarning {
                code: E2E_MODEL_UNVERIFIED.into(),
                message: format!(
                    "Check {}: Axocoatl verifies tool calls and image input only for OpenRouter \
                     models; {model} through {} is not verified. e2e's agent needs both.",
                    check.name, route.host
                ),
            });
        }
    }
    Ok(warnings)
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;
    use std::time::Instant;

    use axocoatl_config::loadout::{
        builtin_loadouts, parse_loadout, resolve_loadout, LoadoutSource, ParamValues,
    };
    use axocoatl_session::check_report::{REPORT_MARKER_PREFIX, STATUS_FAILED};
    use axocoatl_session::run_outcome::{ReproRun, TurnObservation};
    use axocoatl_session::run_record::RunEvent;
    use serde_json::json;

    use super::*;
    use crate::loadout::host::ReproRequest;
    use crate::loadout::{KeepMode, RunOptions};
    use crate::SessionTeamEdit;

    const LOADOUT: &str = r#"
schema: axocoatl.loadout/1
id: web-e2e
version: 1
name: Web e2e
kind: custom
params:
  e2e_model: { kind: text, default: anthropic/claude-sonnet-4.5 }
agents:
  - id: writer
    role: writer
    model: { provider: openrouter, model: anthropic/claude-sonnet-4.5 }
    tools: [read_file, list_dir, bash]
checks:
  - name: e2e
    run: { e2e: { args: [run, --tag, smoke], model_route: openrouter.ai, model: { param: e2e_model } } }
    timeout: 20m
  - name: local
    run: { e2e: { model_route: llm.example.test, model: vision-7b } }
  - name: unit
    run: { argv: [npm, test] }
routes:
  - host: openrouter.ai
    credential: openrouter-e2e
    inject: { header: Authorization, format: "Bearer {}" }
    rules: [{ methods: [POST], path: "/api/v1/**" }]
  - host: llm.example.test
    ports: [8443]
    credential: local-llm
    inject: { header: Authorization, format: "Bearer {}" }
  - host: plain.example.com
budgets:
  agent: { activations: 1, invocations: 10, tokens: 1000, cost_usd: 1 }
  wall_clock: 30m
prompt: "{task}"
environment: { recipes: [e2e] }
"#;

    fn resolved_with(params: &[(&str, &str)]) -> ResolvedLoadout {
        let loadout = parse_loadout(LOADOUT, LoadoutSource::Builtin).expect("loadout parses");
        let values: ParamValues = params
            .iter()
            .map(|(name, value)| (name.to_string(), value.to_string()))
            .collect();
        resolve_loadout(&loadout, &values, "check the checkout", "/repo").expect("resolves")
    }

    fn resolved() -> ResolvedLoadout {
        resolved_with(&[])
    }

    fn e2e(resolved: &ResolvedLoadout, name: &str) -> E2eCheck {
        e2e_checks(resolved)
            .find(|(check, _)| check.name == name)
            .map(|(_, e2e)| e2e.clone())
            .unwrap()
    }

    fn expand(name: &str, check: &E2eCheck, timeout: u64) -> Result<String, RunError> {
        expand_e2e_check(name, check, timeout, &resolved()).map(|(argv, _)| argv[2].clone())
    }

    #[test]
    fn the_check_forces_telemetry_off_and_binds_its_report() {
        let resolved = resolved();
        let check = e2e(&resolved, "e2e");
        let timeout = resolved.checks[0].timeout_secs;
        let (argv, options) = expand_e2e_check("e2e", &check, timeout, &resolved).unwrap();
        assert_eq!(argv.len(), 3);
        assert_eq!(&argv[..2], ["sh", "-c"]);
        let script = &argv[2];
        for line in [
            "export E2E_TELEMETRY_DISABLED='1'",
            "export DO_NOT_TRACK='1'",
            "export CI='1'",
            "report_dir='/tmp/axocoatl-check-reports/e2e'",
            "export AXOCOATL_E2E_MODEL='anthropic/claude-sonnet-4.5'",
            "export AXOCOATL_E2E_BASE_URL='https://openrouter.ai/api/v1'",
            "export AXOCOATL_E2E_API_KEY='axocoatl-route:openrouter.ai'",
            "export OPENROUTER_API_KEY='axocoatl-route:openrouter.ai'",
            "e2e 'run' --reporter json '--tag' 'smoke' >\"$report\" 2>\"$log\"",
            "tail -c 16384 -- \"$log\"",
        ] {
            assert!(
                script.lines().any(|candidate| candidate == line),
                "{line}\n{script}"
            );
        }
        // Telemetry is forced before e2e runs; the marker is the last thing
        // printed, then e2e's status is the check's.
        let forced = script.find("export E2E_TELEMETRY_DISABLED").unwrap();
        assert!(forced < script.find("e2e 'run'").unwrap());
        assert!(script.contains("'AXOCOATL-CHECK-REPORT sha256='"));
        assert!(script.trim_end().ends_with("exit \"$status\""));
        assert!(!script.contains("Bearer") && !script.contains("openrouter-e2e"));
        assert_eq!(options.name.as_deref(), Some("e2e"));
        assert_eq!(options.timeout_ms, Some(20 * 60 * 1000));
        let report = options.report.unwrap();
        assert_eq!(report.format, "e2e_report_json");
        assert_eq!(report.path, "/tmp/axocoatl-check-reports/e2e/report.json");
        assert_eq!(report, report_spec("e2e"));
        assert!(report.path.starts_with(CHECK_REPORT_DIR));
    }

    #[test]
    fn the_model_parameter_and_other_routes_reach_the_wrapper() {
        let with_param = resolved_with(&[("e2e_model", "google/gemini-3-flash:nitro")]);
        let (argv, _) = expand_e2e_check("e2e", &e2e(&with_param, "e2e"), 60, &with_param).unwrap();
        assert!(argv[2].contains("export AXOCOATL_E2E_MODEL='google/gemini-3-flash:nitro'\n"));
        // A route Axocoatl has no provider entry for: an OpenAI-compatible
        // base on the route's port, and no provider key variable.
        let plain = resolved();
        let (argv, options) =
            expand_e2e_check("local", &e2e(&plain, "local"), 180, &plain).unwrap();
        let script = &argv[2];
        assert!(
            script.contains("export AXOCOATL_E2E_BASE_URL='https://llm.example.test:8443/v1'\n")
        );
        assert!(script.contains("export AXOCOATL_E2E_API_KEY='axocoatl-route:llm.example.test'\n"));
        assert!(!script.contains("OPENROUTER_API_KEY"));
        // No args: `e2e run`.
        assert!(script.contains("\ne2e 'run' --reporter json >\"$report\" 2>\"$log\"\n"));
        assert_eq!(options.timeout_ms, Some(180_000));
    }

    #[test]
    fn arguments_are_quoted_never_interpreted() {
        let mut check = e2e(&resolved(), "e2e");
        check.args = vec![
            "explore".into(),
            "it's $(rm -rf /) `x` \"y\"".into(),
            "--max-steps=4".into(),
        ];
        let script = expand("e2e", &check, 60).unwrap();
        assert!(script.contains(
            "e2e 'explore' --reporter json 'it'\\''s $(rm -rf /) `x` \"y\"' '--max-steps=4'"
        ));
    }

    #[test]
    fn checks_that_cannot_run_safely_are_usage_errors() {
        let resolved = resolved();
        let good = e2e(&resolved, "e2e");
        let usage_error = |name: &str, check: &E2eCheck, timeout: u64| {
            let error = expand_e2e_check(name, check, timeout, &resolved).unwrap_err();
            assert!(matches!(error, RunError::Usage(_)), "{error}");
            error.to_string()
        };
        for args in [
            vec!["init"],
            vec!["feedback", "-m", "x"],
            vec!["cache", "clear"],
            vec!["run", "--reporter", "list"],
            vec!["run", "--reporter=list"],
            vec!["run", "--help"],
            vec!["run", "-h"],
            vec!["run", "--version"],
            vec!["run", "--headed"],
            vec!["run", "two\nlines"],
        ] {
            let mut check = good.clone();
            check.args = args.iter().map(|arg| arg.to_string()).collect();
            usage_error("e2e", &check, 60);
        }
        let mut many = good.clone();
        many.args = std::iter::once("run".to_string())
            .chain((0..MAX_E2E_ARGS).map(|index| format!("t{index}")))
            .collect();
        usage_error("e2e", &many, 60);
        // The key must travel through a credentialed route the loadout names.
        let mut plain = good.clone();
        plain.model_route = "plain.example.com".into();
        assert!(usage_error("e2e", &plain, 60).contains("carries no credential"));
        let mut unknown = good.clone();
        unknown.model_route = "api.example.org".into();
        assert!(usage_error("e2e", &unknown, 60).contains("names no host"));
        // Bounds.
        usage_error("e2e", &good, 0);
        usage_error("e2e", &good, 30 * 60 + 1);
        assert!(expand_e2e_check("e2e", &good, 30 * 60, &resolved).is_ok());
        for name in ["", "E2E", "../x", "a b", &"x".repeat(33)] {
            usage_error(name, &good, 60);
        }
        for model in ["has space", "semi;colon", "$(id)", "", &"m".repeat(201)] {
            let mut bad = good.clone();
            bad.model = ParamOr::Value(model.to_string());
            usage_error("e2e", &bad, 60);
        }
        let mut missing = good.clone();
        missing.model = ParamOr::Param {
            param: "absent".into(),
        };
        assert!(usage_error("e2e", &missing, 60).contains("--param absent="));
    }

    #[test]
    fn workspace_mounts_follow_the_loadouts_checks() {
        assert!(workspace_mounts(&resolved()).read_only_e2e_cache);
        let fix = builtin_loadouts()
            .into_iter()
            .map(Result::unwrap)
            .find(|loadout| loadout.file.id == "fix")
            .unwrap();
        let values: ParamValues = [
            ("writer_model".to_string(), "openrouter:a/b".to_string()),
            ("reviewer_model".to_string(), "openrouter:c/d".to_string()),
        ]
        .into_iter()
        .collect();
        let fix = resolve_loadout(&fix, &values, "task", "/repo").unwrap();
        assert!(!workspace_mounts(&fix).read_only_e2e_cache);
    }

    /// The wrapper as the Session runs it, against a fake `e2e` on PATH.
    #[cfg(unix)]
    mod wrapper {
        use std::os::unix::fs::PermissionsExt;
        use std::process::{Command, Output};

        use super::*;

        fn script(report_dir: &str, args: &[&str]) -> String {
            let resolved = resolved();
            let route = resolved
                .loadout
                .file
                .routes
                .iter()
                .find(|route| route.host == OPENROUTER_HOST)
                .unwrap();
            let args: Vec<String> = args.iter().map(|arg| arg.to_string()).collect();
            Wrapper {
                name: "e2e",
                report_dir,
                model: "vendor/vision-model",
                route,
                args: &args,
            }
            .script()
        }

        fn run(fake_e2e: Option<&str>, args: &[&str]) -> (Output, PathBuf, tempfile::TempDir) {
            let scratch = tempfile::tempdir().unwrap();
            let bin = scratch.path().join("bin");
            std::fs::create_dir(&bin).unwrap();
            let path = match fake_e2e {
                Some(body) => {
                    let program = bin.join("e2e");
                    std::fs::write(&program, format!("#!/bin/sh\n{body}")).unwrap();
                    std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755))
                        .unwrap();
                    format!("{}:/usr/bin:/bin:/usr/sbin:/sbin", bin.display())
                }
                None => "/usr/bin:/bin:/usr/sbin:/sbin".to_string(),
            };
            let report_dir = scratch.path().join("reports/e2e");
            // A stale report from an earlier run never survives.
            std::fs::create_dir_all(&report_dir).unwrap();
            std::fs::write(report_dir.join("report.json"), "stale").unwrap();
            let output = Command::new("sh")
                .arg("-c")
                .arg(script(&report_dir.to_string_lossy(), args))
                .env_clear()
                .env("PATH", path)
                .env("E2E_TELEMETRY_DISABLED", "0")
                .current_dir(scratch.path())
                .output()
                .unwrap();
            (output, report_dir, scratch)
        }

        const FAKE: &str = r#"
printf '{"schemaVersion":"report-1","run":{"results":[{"titlePath":["t"],"file":"a.e2e.ts","targetId":"web","status":"failed","attempts":[{"status":"failed","error":{"category":"test","code":"X","message":"m"}}]}],"errors":[]},"argv":"%s","telemetry":"%s","dnt":"%s","ci":"%s","model":"%s","base":"%s","key":"%s","provider_key":"%s"}' "$*" "$E2E_TELEMETRY_DISABLED" "$DO_NOT_TRACK" "$CI" "$AXOCOATL_E2E_MODEL" "$AXOCOATL_E2E_BASE_URL" "$AXOCOATL_E2E_API_KEY" "$OPENROUTER_API_KEY"
echo "e2e diagnostics on stderr" >&2
exit 1
"#;

        #[test]
        fn the_report_digest_is_the_last_stdout_line_and_e2e_status_is_kept() {
            let (output, report_dir, _scratch) = run(Some(FAKE), &["run", "--tag", "smoke"]);
            assert_eq!(output.status.code(), Some(1), "{output:?}");
            let stdout = String::from_utf8(output.stdout).unwrap();
            let last = stdout.lines().last().unwrap();
            assert!(last.starts_with(REPORT_MARKER_PREFIX), "{stdout}");
            assert!(stdout.contains("e2e diagnostics on stderr"));
            let bytes = std::fs::read(report_dir.join("report.json")).unwrap();
            assert_eq!(report_marker(&stdout), Some(report_digest(&bytes)));
            let seen: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(seen["argv"], "run --reporter json --tag smoke");
            assert_eq!(seen["telemetry"], "1", "forced over the caller's 0");
            assert_eq!(seen["dnt"], "1");
            assert_eq!(seen["ci"], "1");
            assert_eq!(seen["model"], "vendor/vision-model");
            assert_eq!(seen["base"], "https://openrouter.ai/api/v1");
            assert_eq!(seen["key"], "axocoatl-route:openrouter.ai");
            assert_eq!(seen["provider_key"], "axocoatl-route:openrouter.ai");
            let report = parse_report(REPORT_FORMAT_E2E, &bytes).unwrap();
            assert_eq!(report.failed, 1);
            assert_eq!(report.tests[0].status, STATUS_FAILED);
            let log = std::fs::read_to_string(report_dir.join("e2e.log")).unwrap();
            assert_eq!(log, "e2e diagnostics on stderr\n");
        }

        #[test]
        fn no_report_means_no_marker() {
            let (output, report_dir, _scratch) =
                run(Some("echo 'CONFIG_NOT_FOUND' >&2\nexit 2\n"), &["run"]);
            assert_eq!(output.status.code(), Some(2));
            let stdout = String::from_utf8(output.stdout).unwrap();
            assert_eq!(report_marker(&stdout), None);
            assert!(
                stdout.ends_with("axocoatl: e2e wrote no report (exit 2)\n"),
                "{stdout}"
            );
            // The stale report of an earlier run was removed first.
            assert_eq!(std::fs::read(report_dir.join("report.json")).unwrap(), b"");
        }

        #[test]
        fn e2e_output_on_stdout_stays_bounded() {
            let noisy = "i=0\nwhile [ $i -lt 3000 ]; do echo \"diagnostic line $i padded to be long enough\" >&2; i=$((i+1)); done\nprintf '{}'\nexit 0\n";
            let (output, _dir, _scratch) = run(Some(noisy), &["run"]);
            assert_eq!(output.status.code(), Some(0));
            assert!(output.stdout.len() <= E2E_STDOUT_TAIL_BYTES + 128);
            let stdout = String::from_utf8_lossy(&output.stdout);
            assert!(stdout.contains("diagnostic line 2999"));
            assert!(report_marker(&stdout).is_some());
        }

        #[test]
        fn a_missing_e2e_says_which_recipe_installs_it() {
            let (output, _dir, _scratch) = run(None, &["run"]);
            assert_eq!(output.status.code(), Some(127));
            let stdout = String::from_utf8(output.stdout).unwrap();
            assert!(stdout.contains("environment.recipes: [e2e]"), "{stdout}");
            assert_eq!(report_marker(&stdout), None);
        }
    }

    /// The registry's e2e recipe fragment on a Node base: it builds, and the
    /// image runs the pinned CLI and Chromium for a non-root user with no
    /// network. Needs Podman (`CONTAINER_CONNECTION`) and network access to
    /// npm, Debian and Playwright's CDN for the build;
    /// `AXO_E2E_RECIPE_BASE` names another base image.
    #[tokio::test]
    #[ignore = "requires podman and network for the image build"]
    async fn the_e2e_recipe_builds_with_the_pinned_cli_node_and_chromium() {
        use tokio::process::Command;

        let fragment = axocoatl_isolation::recipes::recipe("e2e")
            .expect("the e2e recipe is registered")
            .fragment;
        let base = std::env::var("AXO_E2E_RECIPE_BASE")
            .unwrap_or_else(|_| "docker.io/library/node:22-bookworm-slim".into());
        let context = tempfile::tempdir().unwrap();
        std::fs::write(
            context.path().join("Containerfile"),
            format!("FROM {base}\n{fragment}"),
        )
        .unwrap();
        let tag = format!(
            "localhost/axocoatl-e2e-recipe-test:{}",
            uuid::Uuid::new_v4().simple()
        );
        let built = Command::new("podman")
            .args(["build", "--layers=false", "-t", &tag, "-f"])
            .arg(context.path().join("Containerfile"))
            .arg(context.path())
            .output()
            .await
            .unwrap();
        assert!(
            built.status.success(),
            "{}",
            String::from_utf8_lossy(&built.stderr)
        );
        let ran = Command::new("podman")
            .args([
                "run",
                "--rm",
                "--network",
                "none",
                "--user",
                "1000:1000",
                &tag,
                "sh",
                "-c",
                "e2e --version && node --version && ls \"$PLAYWRIGHT_BROWSERS_PATH\" && \
                 printf '%s|%s\\n' \"$E2E_TELEMETRY_DISABLED\" \"$DO_NOT_TRACK\"",
            ])
            .output()
            .await
            .unwrap();
        let _ = Command::new("podman")
            .args(["rmi", "--force", &tag])
            .output()
            .await;
        assert!(
            ran.status.success(),
            "{}",
            String::from_utf8_lossy(&ran.stderr)
        );
        let stdout = String::from_utf8(ran.stdout).unwrap();
        let mut lines = stdout.lines();
        assert_eq!(lines.next(), Some(E2E_VERSION));
        let node = lines.next().unwrap().trim_start_matches('v');
        let parts: Vec<u32> = node.split('.').map(|part| part.parse().unwrap()).collect();
        assert!(
            (parts[0] == 22 && (parts[1], parts[2]) >= (22, 3))
                || (parts[0] == 24 && parts[1] >= 8)
                || parts[0] > 24,
            "Node {node} is below e2e's floor (^22.22.3 or >=24.8.0)"
        );
        assert!(
            stdout.contains("chromium_headless_shell-"),
            "no Chromium in the image: {stdout}"
        );
        assert!(stdout.lines().any(|line| line == "1|1"), "{stdout}");
    }

    /// A Session container holding files, for `read_sandbox_file`.
    #[derive(Default)]
    struct FakeHost {
        files: HashMap<String, Vec<u8>>,
        failure: Mutex<Option<RunError>>,
        reads: AtomicUsize,
    }

    #[async_trait]
    impl RunHost for FakeHost {
        async fn apply_team(&self, _: &str, _: SessionTeamEdit) -> Result<(), RunError> {
            Err(RunError::NotImplemented("fake"))
        }
        async fn send_turn(&self, _: &str, _: &str) -> Result<String, RunError> {
            Err(RunError::NotImplemented("fake"))
        }
        async fn wait_turn(
            &self,
            _: &str,
            _: &str,
            _: Instant,
        ) -> Result<TurnObservation, RunError> {
            Err(RunError::NotImplemented("fake"))
        }
        async fn stop_turn(&self, _: &str, _: &str) -> Result<(), RunError> {
            Err(RunError::NotImplemented("fake"))
        }
        async fn run_repro(&self, _: &str, _: &ReproRequest) -> Result<ReproRun, RunError> {
            Err(RunError::NotImplemented("fake"))
        }
        async fn read_sandbox_file(
            &self,
            session_id: &str,
            path: &str,
            max_bytes: usize,
        ) -> Result<Option<Vec<u8>>, RunError> {
            assert_eq!(session_id, "ses-1");
            assert!(max_bytes > MAX_REPORT_BYTES);
            self.reads.fetch_add(1, Ordering::SeqCst);
            if let Some(error) = self.failure.lock().unwrap().take() {
                return Err(error);
            }
            Ok(self
                .files
                .get(path)
                .map(|bytes| bytes[..bytes.len().min(max_bytes)].to_vec()))
        }
        async fn record(&self, _: &str, _: RunEvent) -> Result<(), RunError> {
            Ok(())
        }
    }

    fn run_context() -> RunContext {
        RunContext {
            run_id: "run-1".into(),
            session_id: "ses-1".into(),
            workspace_id: "wsp-1".into(),
            resolved: resolved(),
            options: RunOptions {
                task: "check the checkout".into(),
                repo: PathBuf::from("/repo"),
                params: ParamValues::new(),
                keep: KeepMode::None,
                check_command: None,
                setup_command: None,
            },
            deadline: Instant::now(),
        }
    }

    fn report_bytes(model: Option<&str>) -> Vec<u8> {
        let steps = match model {
            Some(model) => json!([{"model": {"provider": "openrouter.chat", "model": model}}]),
            None => json!([]),
        };
        serde_json::to_vec(&json!({"schemaVersion": "report-1", "run": {"results": [
            {"titlePath": ["checkout", "pays"], "file": "tests/checkout.e2e.ts", "targetId": "web",
             "status": "passed", "attempts": [{"status": "passed", "durationMs": 12, "steps": steps}]},
            {"titlePath": ["checkout", "refunds"], "file": "tests/checkout.e2e.ts", "targetId": "web",
             "status": "failed", "attempts": [{"status": "failed", "durationMs": 40,
              "error": {"category": "test", "code": "ASSERTION_FAILED", "message": "no refund"}}]}
        ], "errors": []}}))
        .unwrap()
    }

    fn result(name: &str, state: CheckState, stdout_tail: String) -> CheckResult {
        CheckResult {
            name: name.into(),
            argv: vec!["sh".into(), "-c".into(), "…".into()],
            state,
            timeout_ms: 180_000,
            exit_code: Some(1),
            stdout_tail,
            stderr_tail: String::new(),
            candidate_sha256: None,
            report: None,
            reason: None,
        }
    }

    fn marked(bytes: &[u8]) -> String {
        format!(
            "e2e output\n{REPORT_MARKER_PREFIX}{}\n",
            report_digest(bytes)
        )
    }

    #[tokio::test]
    async fn a_report_bound_by_its_digest_is_attached() {
        let bytes = report_bytes(Some("anthropic/claude-sonnet-4.5"));
        let host = FakeHost {
            files: HashMap::from([(report_spec("e2e").path, bytes.clone())]),
            ..FakeHost::default()
        };
        let mut checks = vec![
            result("e2e", CheckState::Failed, marked(&bytes)),
            result("unit", CheckState::Passed, marked(&bytes)),
        ];
        collect_reports(&host, &run_context(), &mut checks)
            .await
            .unwrap();
        let report = checks[0].report.as_ref().expect("attached");
        assert_eq!(report.sha256, report_digest(&bytes));
        assert_eq!((report.passed, report.failed), (1, 1));
        assert_eq!(
            report.tests[1].message.as_deref(),
            Some("ASSERTION_FAILED: no refund")
        );
        assert_eq!(checks[0].reason, None);
        // The verdict stays the exit status's.
        assert_eq!(checks[0].state, CheckState::Failed);
        // A check that is not an e2e check is never touched; the e2e check
        // without a result reads nothing.
        assert!(checks[1].report.is_none() && checks[1].reason.is_none());
        assert_eq!(host.reads.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn a_report_that_does_not_match_its_marker_is_refused() {
        let bytes = report_bytes(None);
        let mut tampered = bytes.clone();
        tampered.extend_from_slice(b" ");
        let host = FakeHost {
            files: HashMap::from([(report_spec("e2e").path, tampered)]),
            ..FakeHost::default()
        };
        let mut checks = vec![result("e2e", CheckState::Passed, marked(&bytes))];
        checks[0].reason = Some("earlier".into());
        collect_reports(&host, &run_context(), &mut checks)
            .await
            .unwrap();
        assert!(checks[0].report.is_none());
        let reason = checks[0].reason.as_deref().unwrap();
        assert!(reason.starts_with("earlier; "), "{reason}");
        assert!(
            reason.contains("is not the one the check run named"),
            "{reason}"
        );
        assert_eq!(checks[0].state, CheckState::Passed);
    }

    #[tokio::test]
    async fn a_missing_marker_or_file_leaves_a_reason() {
        let bytes = report_bytes(None);
        // The marker names a report the container does not have.
        let host = FakeHost::default();
        let mut checks = vec![result("e2e", CheckState::Passed, marked(&bytes))];
        collect_reports(&host, &run_context(), &mut checks)
            .await
            .unwrap();
        assert!(checks[0].report.is_none());
        assert!(checks[0]
            .reason
            .as_deref()
            .unwrap()
            .contains("is not in the Session container"));

        // No marker at all: the reason follows the check's state, and the
        // file (even a matching one) is never read.
        let host = FakeHost {
            files: HashMap::from([(report_spec("e2e").path, bytes.clone())]),
            ..FakeHost::default()
        };
        for (state, expected) in [
            (CheckState::TimedOut, "timed out"),
            (CheckState::NotRun, "did not run"),
            (CheckState::Unavailable, "could not be read"),
            (CheckState::Failed, "e2e wrote no report.json"),
        ] {
            let mut checks = vec![result("e2e", state, "no marker\n".into())];
            collect_reports(&host, &run_context(), &mut checks)
                .await
                .unwrap();
            assert!(checks[0].report.is_none());
            assert!(
                checks[0].reason.as_deref().unwrap().contains(expected),
                "{state:?}: {:?}",
                checks[0].reason
            );
        }
        assert_eq!(host.reads.load(Ordering::SeqCst), 0);

        // Too large, unreadable, or a read error.
        let large = vec![b'x'; MAX_REPORT_BYTES + 10];
        let host = FakeHost {
            files: HashMap::from([(report_spec("e2e").path, large.clone())]),
            ..FakeHost::default()
        };
        let mut checks = vec![result("e2e", CheckState::Passed, marked(&large))];
        collect_reports(&host, &run_context(), &mut checks)
            .await
            .unwrap();
        assert!(checks[0].reason.as_deref().unwrap().contains("larger than"));
        let junk = b"not json".to_vec();
        let host = FakeHost {
            files: HashMap::from([(report_spec("e2e").path, junk.clone())]),
            ..FakeHost::default()
        };
        let mut checks = vec![result("e2e", CheckState::Passed, marked(&junk))];
        collect_reports(&host, &run_context(), &mut checks)
            .await
            .unwrap();
        assert!(checks[0].reason.as_deref().unwrap().contains("unreadable"));
        let host = FakeHost::default();
        *host.failure.lock().unwrap() = Some(RunError::Infrastructure("container gone".into()));
        let mut checks = vec![result("e2e", CheckState::Passed, marked(&bytes))];
        collect_reports(&host, &run_context(), &mut checks)
            .await
            .unwrap();
        assert!(checks[0]
            .reason
            .as_deref()
            .unwrap()
            .contains("container gone"));
    }

    #[tokio::test]
    async fn an_unfilled_host_hook_is_not_hidden() {
        let bytes = report_bytes(None);
        let host = FakeHost::default();
        *host.failure.lock().unwrap() = Some(RunError::NotImplemented("read_sandbox_file"));
        let mut checks = vec![result("e2e", CheckState::Passed, marked(&bytes))];
        let error = collect_reports(&host, &run_context(), &mut checks)
            .await
            .unwrap_err();
        assert!(matches!(error, RunError::NotImplemented(_)));
    }

    #[tokio::test]
    async fn a_report_that_used_another_model_says_so() {
        let bytes = report_bytes(Some("openai/gpt-something"));
        let host = FakeHost {
            files: HashMap::from([(report_spec("e2e").path, bytes.clone())]),
            ..FakeHost::default()
        };
        let mut checks = vec![result("e2e", CheckState::Passed, marked(&bytes))];
        collect_reports(&host, &run_context(), &mut checks)
            .await
            .unwrap();
        assert!(checks[0].report.is_some());
        let reason = checks[0].reason.as_deref().unwrap();
        assert!(reason.contains("openai/gpt-something"), "{reason}");
        assert!(reason.contains("anthropic/claude-sonnet-4.5"), "{reason}");
        assert!(reason.contains(E2E_MODEL_ENV), "{reason}");
    }

    struct FakeCatalog {
        catalog: Result<serde_json::Value, String>,
        calls: AtomicUsize,
    }

    #[async_trait]
    impl ModelCatalog for FakeCatalog {
        async fn openrouter_models(&self) -> Result<serde_json::Value, String> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.catalog.clone()
        }
    }

    fn catalog(rows: serde_json::Value) -> FakeCatalog {
        FakeCatalog {
            catalog: Ok(json!({ "data": rows })),
            calls: AtomicUsize::new(0),
        }
    }

    fn row(id: &str, parameters: &[&str], inputs: &[&str]) -> serde_json::Value {
        json!({"id": id, "supported_parameters": parameters,
               "architecture": {"input_modalities": inputs, "output_modalities": ["text"]}})
    }

    #[tokio::test]
    async fn openrouter_models_need_tools_and_image_input() {
        let capable = catalog(json!([
            row(
                "anthropic/claude-sonnet-4.5",
                &["tools", "max_tokens"],
                &["text", "image"]
            ),
            row(
                "google/gemini-3-flash",
                &["tools"],
                &["text", "image", "audio"]
            ),
            row("text/only", &["tools"], &["text"]),
            row("no/tools", &["max_tokens"], &["text", "image"]),
        ]));
        let warnings = verify_e2e_models(&resolved(), &capable).await.unwrap();
        // The OpenRouter check is verified; the other route only warns.
        assert_eq!(warnings.len(), 1);
        assert_eq!(warnings[0].code, E2E_MODEL_UNVERIFIED);
        assert!(warnings[0].message.contains("vision-7b"));
        assert!(warnings[0].message.contains("llm.example.test"));
        assert_eq!(capable.calls.load(Ordering::SeqCst), 1);
        // A routing variant is checked as its base model.
        let variant = resolved_with(&[("e2e_model", "google/gemini-3-flash:nitro")]);
        assert!(verify_e2e_models(&variant, &capable).await.is_ok());

        for (model, expected) in [
            ("text/only", "image input"),
            ("no/tools", "tool calls"),
            ("missing/model", "not in OpenRouter's model catalog"),
        ] {
            let resolved = resolved_with(&[("e2e_model", model)]);
            let error = verify_e2e_models(&resolved, &capable).await.unwrap_err();
            assert!(matches!(error, RunError::Usage(_)), "{error}");
            assert!(error.to_string().contains(expected), "{error}");
        }
        // An unreadable catalog never passes silently.
        let down = FakeCatalog {
            catalog: Err("connection refused".into()),
            calls: AtomicUsize::new(0),
        };
        let error = verify_e2e_models(&resolved(), &down).await.unwrap_err();
        assert!(matches!(error, RunError::Infrastructure(_)), "{error}");
        assert!(error.to_string().contains("connection refused"));
        assert!(openrouter_model_capable("a/b", &json!({})).is_err());
    }

    #[tokio::test]
    async fn the_openrouter_catalog_is_read_without_a_credential() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/models"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": [
                row("anthropic/claude-sonnet-4.5", &["tools"], &["text", "image"])
            ]})))
            .expect(1)
            .mount(&server)
            .await;
        let catalog = OpenRouterCatalog::new(format!("{}/api/v1/", server.uri()));
        let models = catalog.openrouter_models().await.unwrap();
        assert!(openrouter_model_capable("anthropic/claude-sonnet-4.5", &models).is_ok());
        let requests = server.received_requests().await.unwrap();
        assert!(requests
            .iter()
            .all(|request| !request.headers.contains_key("authorization")));

        let failing = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(503))
            .mount(&failing)
            .await;
        let error = OpenRouterCatalog::new(failing.uri())
            .openrouter_models()
            .await
            .unwrap_err();
        assert!(error.contains("503"), "{error}");
    }
}
