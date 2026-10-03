use super::*;
use std::collections::HashMap;
use std::sync::Mutex;

fn drive(arguments: Value) -> Result<DriveJob, String> {
    parse_drive_call(&arguments).map_err(|error| error.to_string())
}

#[test]
fn browser_arguments_refuse_other_schemes_userinfo_and_oversized_input() {
    for url in [
        "javascript:alert(1)",
        "data:text/html,<h1>x</h1>",
        "file:///etc/passwd",
        "chrome://settings",
        "about:blank",
        "ftp://example.com/",
        "localhost:3000",
        "http://user:secret@localhost:3000/",
        "http://user@localhost:3000/",
    ] {
        assert!(drive(json!({"url": url})).is_err(), "{url}");
    }
    drive(json!({"url": "http://localhost:5173/"})).unwrap();
    drive(json!({"url": "https://example.com/a?b=c#d"})).unwrap();
    let long = format!("http://localhost/{}", "a".repeat(MAX_STRING_BYTES));
    assert!(drive(json!({"url": long})).unwrap_err().contains("4096"));

    let step = json!({"action": "reload"});
    let steps: Vec<Value> = std::iter::repeat_n(step, MAX_STEPS + 1).collect();
    assert!(drive(json!({"url": "http://localhost/", "steps": steps}))
        .unwrap_err()
        .contains("at most 40 steps"));
    let big = "x".repeat(MAX_STRING_BYTES + 1);
    let error = drive(json!({"url": "http://localhost/", "steps": [
        {"action": "fill", "target": {"label": "Name"}, "value": big}
    ]}))
    .unwrap_err();
    assert!(
        error.contains("steps[0].value exceeds 4096 bytes"),
        "{error}"
    );
    assert!(drive(json!({"url": "http://localhost/", "evaluate": "1"})).is_err());
    assert!(drive(json!({"url": "http://localhost/", "snapshot": "html"})).is_err());
    assert!(drive(json!({"url": "http://localhost/", "viewport": {"width": 100}})).is_err());
    let job = drive(json!({"url": "http://localhost/", "viewport": {"width": 400}})).unwrap();
    assert_eq!(job.viewport, Some((400, 720)));
}

#[test]
fn browser_steps_follow_the_driver_rules() {
    let bad = [
        (
            json!({"action": "evaluate", "text": "1"}),
            "action must be one of",
        ),
        (json!({"action": "goto"}), "goto needs url"),
        (
            json!({"action": "goto", "url": "javascript:x"}),
            "http or https",
        ),
        (json!({"action": "click"}), "click needs target"),
        (
            json!({"action": "click", "target": {"role": "button", "css": "b"}}),
            "exactly one of",
        ),
        (
            json!({"action": "click", "target": {"label": "x", "name": "y"}}),
            "only for role",
        ),
        (
            json!({"action": "click", "target": {"text": "x", "nth": -1}}),
            "whole number",
        ),
        (
            json!({"action": "click", "target": {"xpath": "//a"}}),
            "not a target field",
        ),
        (
            json!({"action": "fill", "target": {"label": "x"}}),
            "fill needs value",
        ),
        (json!({"action": "press"}), "press needs key"),
        (
            json!({"action": "reload", "url": "http://localhost/"}),
            "does not take url",
        ),
        (
            json!({"action": "wait_for", "text": "a", "url": "http://localhost/"}),
            "exactly one of target, url or text",
        ),
        (
            json!({"action": "wait_for"}),
            "exactly one of target, url or text",
        ),
        (json!({"action": "expect_text"}), "expect_text needs text"),
        (
            json!({"action": "click", "target": {"text": "x"}, "timeout_ms": 50}),
            "100-10000",
        ),
        (
            json!({"action": "click", "target": {"text": "x"}, "script": "x"}),
            "not a step field",
        ),
    ];
    for (step, expected) in bad {
        let error = check_step(&step, 3).unwrap_err();
        assert!(error.contains(expected), "{step}: {error}");
        assert!(error.starts_with("steps[3]"), "{error}");
    }
    for step in [
        json!({"action": "goto", "url": "http://localhost:8765/cart"}),
        json!({"action": "click", "target": {"role": "button", "name": "Place order"}}),
        json!({"action": "click", "target": {"text": "Add", "nth": 2}}),
        json!({"action": "fill", "target": {"label": "Email"}, "value": ""}),
        json!({"action": "select", "target": {"testid": "size"}, "value": "M"}),
        json!({"action": "check", "target": {"css": "#terms"}}),
        json!({"action": "press", "key": "Enter"}),
        json!({"action": "press", "key": "Enter", "target": {"label": "Search"}}),
        json!({"action": "wait_for", "url": "http://localhost:8765/done"}),
        json!({"action": "expect_text", "text": "$20.00", "timeout_ms": 500}),
        json!({"action": "back"}),
    ] {
        check_step(&step, 0).unwrap_or_else(|error| panic!("{step}: {error}"));
    }
}

#[test]
fn the_payload_carries_the_credential_only_in_proxy_password() {
    let job = drive(json!({
        "url": "http://localhost:8765/",
        "steps": [{"action": "click", "target": {"text": "Add"}}],
    }))
    .unwrap();
    let settings = BrowserSettings {
        snapshot_max_bytes: 4096,
        timeout_secs: 60,
    };
    let payload = drive_payload(&job, &["http://localhost:8765".into()], &settings);
    assert_eq!(payload["proxy"], Value::Null);
    assert_eq!(payload["limits"]["total_timeout_ms"], 55_000);
    assert_eq!(payload["limits"]["snapshot_max_bytes"], 4096);
    assert_eq!(payload["aut_origins"], json!(["http://localhost:8765"]));

    let secret = "axe_c2VjcmV0LXRva2VuLW5ldmVyLWxvZ2dlZA";
    let bytes = stdin_with_proxy(
        &payload,
        Some(ProxyCredential {
            server: "http://127.0.0.1:3129",
            password: secret,
        }),
    )
    .unwrap();
    let text = String::from_utf8(bytes.clone()).unwrap();
    assert_eq!(text.matches(secret).count(), 1);
    let parsed: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(parsed["proxy"]["password"], secret);
    assert_eq!(parsed["proxy"]["username"], "axo");
    assert_eq!(parsed["proxy"]["server"], "http://127.0.0.1:3129");
    let mut without = parsed.clone();
    without["proxy"] = Value::Null;
    assert!(!without.to_string().contains(secret));
    // The job itself never holds the credential.
    assert!(!format!("{job:?}").contains(secret));
    let none: Value = serde_json::from_slice(&stdin_with_proxy(&payload, None).unwrap()).unwrap();
    assert_eq!(none["proxy"], Value::Null);
}

fn driver_output(document: Value) -> RunnerOutput {
    RunnerOutput {
        exit_code: Some(0),
        stdout: format!("{document}\n").into_bytes(),
        stderr: String::new(),
    }
}

fn jpeg() -> Vec<u8> {
    let mut bytes = vec![0xff, 0xd8, 0xff, 0xe0];
    bytes.extend(std::iter::repeat_n(7u8, 300));
    bytes
}

fn document(snapshot: &str) -> Value {
    json!({
        "schema": "axocoatl.browser/1", "ok": true, "url": "http://localhost:8765/",
        "final_url": "http://localhost:8765/cart", "title": "Shop", "status": 200, "ms": 812,
        "steps": [], "snapshot": {"kind": "aria", "text": snapshot, "bytes": snapshot.len(), "truncated": false},
        "console": [], "page_errors": [], "network": {"failed": [], "http_errors": [], "blocked": []},
        "dialogs": [], "truncated": {"console": false, "network": false, "output": false},
    })
}

#[test]
fn output_is_rebounded_and_the_screenshot_is_split_off() {
    let mut big = document(&"- listitem: ORD-2051\n".repeat(10_000));
    for index in 0..500 {
        big["console"]
            .as_array_mut()
            .unwrap()
            .push(json!({"type": "error", "text": format!("error {index} {}", "x".repeat(200))}));
    }
    big["screenshot"] = json!({
        "type": "jpeg",
        "base64": base64::engine::general_purpose::STANDARD.encode(jpeg()),
        "bytes": 304,
    });
    let report = parse_drive_output(&driver_output(big)).unwrap();
    assert!(serde_json::to_vec(&report.result).unwrap().len() <= OUTPUT_MAX_BYTES);
    assert_eq!(report.result["truncated"]["output"], true);
    assert_eq!(report.result["snapshot"]["truncated"], true);
    assert!(report.result.get("screenshot").is_none());
    assert_eq!(report.screenshot.as_ref().unwrap().media_type, "image/jpeg");
    assert_eq!(report.screenshot.as_ref().unwrap().bytes, jpeg());
    assert_eq!(
        report.final_url.as_deref(),
        Some("http://localhost:8765/cart")
    );
    assert_eq!(report.status, Some(200));
    assert!(report.ok);

    let small = parse_drive_output(&driver_output(document("- heading \"Orders\""))).unwrap();
    assert_eq!(small.result["truncated"]["output"], false);
    assert!(small.screenshot.is_none());

    let mut forged = document("x");
    forged["screenshot"] = json!({"type": "jpeg", "base64": base64::engine::general_purpose::STANDARD.encode(b"<svg/>")});
    let report = parse_drive_output(&driver_output(forged)).unwrap();
    assert!(report.screenshot.is_none());
    assert_eq!(report.screenshot_dropped.as_deref(), Some("invalid_image"));
    let mut dropped = document("x");
    dropped["screenshot"] = json!({"type": "jpeg", "dropped": "too_large", "bytes": 5_000_000});
    let report = parse_drive_output(&driver_output(dropped)).unwrap();
    assert_eq!(report.screenshot_dropped.as_deref(), Some("too_large"));
}

#[test]
fn driver_failures_become_tool_errors() {
    let error = parse_drive_output(&RunnerOutput {
        exit_code: Some(1),
        stdout: b"not json".to_vec(),
        stderr: "node: cannot find module".into(),
    })
    .unwrap_err()
    .to_string();
    assert!(
        error.contains("returned no result (exit 1)") && error.contains("cannot find module"),
        "{error}"
    );
    let error = parse_drive_output(&driver_output(json!({
        "schema": "axocoatl.browser/1", "ok": false,
        "error": "this image has no Playwright 1.60.0 at /opt/axocoatl/playwright/",
    })))
    .unwrap_err()
    .to_string();
    assert!(error.contains("no Playwright"), "{error}");
    let error = parse_drive_output(&driver_output(json!({"schema": "other/1"})))
        .unwrap_err()
        .to_string();
    assert!(error.contains("unknown format"), "{error}");
    let mut crashed = driver_output(document("x"));
    crashed.exit_code = None;
    assert!(parse_drive_output(&crashed).is_err());
}

#[derive(Default)]
struct FakeRunner {
    output: Mutex<Option<Result<RunnerOutput, String>>>,
    record: Mutex<Option<Result<Option<RecordedScreenshot>, String>>>,
    jobs: Mutex<Vec<BrowserJob>>,
    recorded: Mutex<Vec<(bool, Option<usize>)>>,
    errors: Mutex<Vec<Option<String>>>,
}

#[async_trait::async_trait]
impl BrowserRunner for FakeRunner {
    async fn run(&self, job: &BrowserJob) -> Result<RunnerOutput, String> {
        self.jobs.lock().unwrap().push(job.clone());
        self.output.lock().unwrap().take().expect("one run")
    }

    async fn record(
        &self,
        _job: &BrowserJob,
        report: &BrowserReport,
    ) -> Result<Option<RecordedScreenshot>, String> {
        self.recorded.lock().unwrap().push((
            report.ok,
            report.screenshot.as_ref().map(|shot| shot.bytes.len()),
        ));
        self.errors.lock().unwrap().push(report.error.clone());
        self.record.lock().unwrap().take().unwrap_or(Ok(None))
    }
}

#[tokio::test]
async fn the_tool_runs_records_and_names_the_screenshot() {
    let runner = Arc::new(FakeRunner::default());
    let mut answer = document("- button \"Add\"");
    answer["screenshot"] =
        json!({"type": "jpeg", "base64": base64::engine::general_purpose::STANDARD.encode(jpeg())});
    *runner.output.lock().unwrap() = Some(Ok(driver_output(answer)));
    *runner.record.lock().unwrap() = Some(Ok(Some(RecordedScreenshot {
        sha256: "ab".repeat(32),
        bytes: 304,
    })));
    let tool = BrowserTool::with_runner(runner.clone());
    let result = tool
        .execute(json!({"url": "http://localhost:8765/"}))
        .await
        .unwrap();
    assert_eq!(result["screenshot"]["recorded"], true);
    assert_eq!(result["screenshot"]["sha256"], "ab".repeat(32));
    assert!(result["screenshot"].get("base64").is_none());
    assert_eq!(*runner.recorded.lock().unwrap(), vec![(true, Some(304))]);
    assert!(
        matches!(&runner.jobs.lock().unwrap()[0], BrowserJob::Drive(job) if job.url == "http://localhost:8765/")
    );
    assert_eq!(
        tool.concurrency_policy(),
        axocoatl_llm::ConcurrencyPolicy::Exclusive
    );
}

#[tokio::test]
async fn runner_and_record_errors_become_tool_errors() {
    let runner = Arc::new(FakeRunner::default());
    *runner.output.lock().unwrap() = Some(Err(
        "the browser image is missing; run axocoatl browser install".into(),
    ));
    let error = BrowserTool::with_runner(runner.clone())
        .execute(json!({"url": "http://localhost:8765/"}))
        .await
        .unwrap_err();
    assert!(
        matches!(&error, ToolError::ExecutionFailed { tool, reason } if tool == "browser" && reason.contains("axocoatl browser install"))
    );
    // A failed call is still recorded, with its reason.
    assert_eq!(*runner.recorded.lock().unwrap(), vec![(false, None)]);
    assert!(runner.errors.lock().unwrap()[0]
        .as_deref()
        .is_some_and(|reason| reason.contains("axocoatl browser install")));

    *runner.output.lock().unwrap() = Some(Ok(driver_output(document("x"))));
    *runner.record.lock().unwrap() = Some(Err("record full".into()));
    let error = BrowserTool::with_runner(runner.clone())
        .execute(json!({"url": "http://localhost:8765/"}))
        .await
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("record is unavailable") && error.contains("record full"),
        "{error}"
    );

    // An invalid call never reaches the runner.
    let before = runner.jobs.lock().unwrap().len();
    assert!(BrowserTool::with_runner(runner.clone())
        .execute(json!({"url": "javascript:alert(1)"}))
        .await
        .is_err());
    assert_eq!(runner.jobs.lock().unwrap().len(), before);
    let error = BrowserTool::definition()
        .execute(json!({"url": "http://localhost/"}))
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("not bound"), "{error}");
}

#[tokio::test]
async fn every_failed_call_is_recorded_with_a_bounded_reason() {
    // The script reports an error document: recorded, then a tool error.
    let runner = Arc::new(FakeRunner::default());
    *runner.output.lock().unwrap() = Some(Ok(RunnerOutput {
        exit_code: Some(1),
        stdout: format!(
            "{}\n",
            json!({"schema": "axocoatl.browser-check/1", "ok": false, "status": "error",
                   "error": format!("the check runner failed: {}", "x".repeat(3000))})
        )
        .into_bytes(),
        stderr: String::new(),
    }));
    let error = BrowserCheckTool::with_runner(runner.clone())
        .execute(json!({"script": "import { test } from '@playwright/test';"}))
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("check runner failed"), "{error}");
    assert_eq!(*runner.recorded.lock().unwrap(), vec![(false, None)]);
    let reason = runner.errors.lock().unwrap()[0].clone().unwrap();
    assert!(reason.contains("check runner failed"), "{reason}");
    assert_eq!(reason.chars().count(), MAX_RECORDED_ERROR_CHARS);

    // A call that timed out in the container is recorded too.
    *runner.output.lock().unwrap() = Some(Err(
        "browser container: the call ran out of time after 120 s".into(),
    ));
    assert!(BrowserTool::with_runner(runner.clone())
        .execute(json!({"url": "http://localhost:8765/"}))
        .await
        .is_err());
    assert_eq!(runner.recorded.lock().unwrap().len(), 2);
    assert!(runner.errors.lock().unwrap()[1]
        .as_deref()
        .is_some_and(|reason| reason.contains("ran out of time")));

    // When even the failure cannot be recorded, the error says both.
    *runner.output.lock().unwrap() = Some(Err("the sidecar did not start".into()));
    *runner.record.lock().unwrap() = Some(Err("record full".into()));
    let error = BrowserTool::with_runner(runner.clone())
        .execute(json!({"url": "http://localhost:8765/"}))
        .await
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("record is unavailable")
            && error.contains("record full")
            && error.contains("sidecar did not start"),
        "{error}"
    );

    // An invalid call is refused before anything runs, and is not recorded.
    let before = runner.recorded.lock().unwrap().len();
    assert!(BrowserTool::with_runner(runner.clone())
        .execute(json!({"url": "file:///etc/passwd"}))
        .await
        .is_err());
    assert_eq!(runner.recorded.lock().unwrap().len(), before);
}

#[test]
fn check_arguments_need_one_source_and_a_safe_path() {
    let parse = |arguments: Value| parse_check_call(&arguments).map_err(|error| error.to_string());
    assert_eq!(
        parse(json!({"path": "./qa/findings/B07.spec.ts"}))
            .unwrap()
            .source,
        CheckSource::Path("qa/findings/B07.spec.ts".into())
    );
    assert!(matches!(
        parse(json!({"script": "import { test } from '@playwright/test';"}))
            .unwrap()
            .source,
        CheckSource::Script(_)
    ));
    for bad in [
        json!({}),
        json!({"path": "a.spec.ts", "script": "x"}),
        json!({"path": "../outside.spec.ts"}),
        json!({"path": "/etc/passwd.ts"}),
        json!({"path": "qa/../../x.spec.ts"}),
        json!({"path": "node_modules/x/a.spec.ts"}),
        json!({"path": "qa/.hidden.spec.ts"}),
        json!({"path": "qa/notes.md"}),
        json!({"script": ""}),
        json!({"script": "x".repeat(MAX_CHECK_SCRIPT_BYTES + 1)}),
        json!({"path": "a.spec.ts", "grep": ""}),
        json!({"path": "a.spec.ts", "base_url": "file:///tmp"}),
        json!({"path": "a.spec.ts", "workers": 4}),
    ] {
        assert!(parse(bad.clone()).is_err(), "{bad}");
    }
}

#[test]
fn imports_are_collected_inside_the_repository() {
    let files: HashMap<&str, &str> = HashMap::from([
        (
            "qa/findings/B07.spec.ts",
            "import { test, expect } from '../fixtures';\nimport data from \"./data.json\";\nimport { helper } from './helpers.js';\nimport { other } from '../../../outside';\nimport x from '@playwright/test';\nimport notes from './notes';\n",
        ),
        ("qa/fixtures.ts", "export * from '@playwright/test';\nimport './findings/B07.spec';\n"),
        ("qa/findings/data.json", "{}"),
        ("qa/findings/helpers.ts", "const y = require('./deep/index');\n"),
        ("qa/findings/deep/index.ts", "export {};"),
        ("qa/findings/notes", "not source; never sent"),
    ]);
    let mut reads = Vec::new();
    let mut read = |path: &str| -> Result<Option<String>, String> {
        reads.push(path.to_string());
        Ok(files.get(path).map(|content| content.to_string()))
    };
    let collected = collect_check_files("qa/findings/B07.spec.ts", &mut read).unwrap();
    let mut paths: Vec<&str> = collected.iter().map(|file| file.path.as_str()).collect();
    paths.sort_unstable();
    assert_eq!(
        paths,
        vec![
            "qa/findings/B07.spec.ts",
            "qa/findings/data.json",
            "qa/findings/deep/index.ts",
            "qa/findings/helpers.ts",
            "qa/fixtures.ts",
        ]
    );
    assert!(reads
        .iter()
        .all(|path| !path.contains("..") && !path.starts_with('/')));
    assert!(!reads.iter().any(|path| path.contains("playwright")));

    let mut missing = |_: &str| -> Result<Option<String>, String> { Ok(None) };
    assert!(collect_check_files("qa/none.spec.ts", &mut missing)
        .unwrap_err()
        .contains("does not exist"));
    assert_eq!(
        relative_imports("import a from './a';\nconst s = './not-an-import';\nexport { b } from \"../b\";\nawait import(`./c`);"),
        vec!["./a", "../b", "./c"]
    );
}

#[test]
fn both_scripts_fit_in_one_supervised_argv() {
    // The supervisor accepts at most 64 KiB of argv in total.
    for script in [DRIVER_SCRIPT, CHECK_SCRIPT] {
        assert!(script.len() + 64 < 64 * 1024, "{}", script.len());
        assert!(!script.contains('\0'));
    }
    assert!(BROWSER_CONTAINERFILE.contains("playwright-core/cli.js install --with-deps"));
    assert!(BROWSER_CONTAINERFILE.contains("@sha256:"));
    assert!(BROWSER_PACKAGE_LOCK.contains("\"@playwright/test\": \"1.60.0\""));
    assert!(DRIVER_SCRIPT.contains("PLAYWRIGHT_VERSION = '1.60.0'"));
    assert!(CHECK_SCRIPT.contains("PLAYWRIGHT_VERSION = '1.60.0'"));
}
