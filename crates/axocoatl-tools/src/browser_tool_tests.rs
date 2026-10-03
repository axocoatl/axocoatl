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

/// The call a refusal gives as its example, parsed.
fn example_of(error: &str) -> Value {
    let (_, example) = error
        .rsplit_once(" Example: ")
        .unwrap_or_else(|| panic!("no example in: {error}"));
    serde_json::from_str(example).unwrap_or_else(|failure| panic!("{failure}: {example}"))
}

#[test]
fn null_is_not_given_for_every_optional_browser_field() {
    let job = drive(json!({
        "url": "http://localhost:8765/", "steps": null, "snapshot": null, "viewport": null,
    }))
    .unwrap();
    assert_eq!(job.steps, Vec::<Value>::new());
    assert_eq!(job.snapshot, SnapshotKind::Aria);
    assert_eq!(job.viewport, None);
    let job =
        drive(json!({"url": "http://localhost/", "viewport": {"width": null, "height": 900}}))
            .unwrap();
    assert_eq!(job.viewport, Some((1280, 900)));
    let job =
        drive(json!({"url": "http://localhost/", "viewport": {"width": 390, "height": null}}))
            .unwrap();
    assert_eq!(job.viewport, Some((390, 720)));

    // A model that fills every field of every step: the nulls are dropped
    // before the driver sees the step.
    let job = drive(json!({"url": "http://localhost:8765/", "snapshot": null, "steps": [
        {"action": "click", "url": null, "value": null, "key": null, "text": null, "timeout_ms": null,
         "target": {"role": "button", "name": "Save", "label": null, "text": null, "testid": null,
                    "css": null, "nth": null}},
        {"action": "wait_for", "target": null, "url": null, "text": "Saved"},
        {"action": "press", "key": "Enter", "target": null},
    ]}))
    .unwrap();
    assert_eq!(
        job.steps,
        vec![
            json!({"action": "click", "target": {"role": "button", "name": "Save"}}),
            json!({"action": "wait_for", "text": "Saved"}),
            json!({"action": "press", "key": "Enter"}),
        ]
    );
    let payload = drive_payload(&job, &[], &BrowserSettings::default());
    assert!(!payload["steps"].to_string().contains("null"), "{payload}");
    assert_eq!(payload["snapshot"], "aria");

    // A required field given as null is missing, and says so.
    let error = drive(json!({"url": null})).unwrap_err();
    assert!(
        error.contains("url is required") && error.contains("got null"),
        "{error}"
    );
    let error = drive(json!({"url": "http://localhost/", "steps": [
        {"action": "fill", "target": {"label": "Email"}, "value": null}
    ]}))
    .unwrap_err();
    assert!(error.contains("steps[0]: fill needs value"), "{error}");
    let error =
        drive(json!({"url": "http://localhost/", "steps": [{"action": null}]})).unwrap_err();
    assert!(
        error.contains("steps[0].action is required") && error.contains("got null"),
        "{error}"
    );
    let error = drive(json!({"url": "http://localhost/", "steps": [
        {"action": "click", "target": {"role": null, "text": null}}
    ]}))
    .unwrap_err();
    assert!(error.contains("needs exactly one of role"), "{error}");

    // Null still counts as given for a field that does not exist.
    assert!(drive(json!({"url": "http://localhost/", "evaluate": null})).is_err());
}

#[test]
fn null_is_not_given_for_every_optional_browser_check_field() {
    let parse = |arguments: Value| parse_check_call(&arguments).map_err(|error| error.to_string());
    let job =
        parse(json!({"path": "qa/a.spec.ts", "script": null, "grep": null, "base_url": null}))
            .unwrap();
    assert_eq!(job.source, CheckSource::Path("qa/a.spec.ts".into()));
    assert_eq!((job.grep, job.base_url), (None, None));
    let job =
        parse(json!({"path": null, "script": "import { test } from '@playwright/test';"})).unwrap();
    assert!(matches!(job.source, CheckSource::Script(_)));
    let error = parse(json!({"path": null, "script": null})).unwrap_err();
    assert!(error.contains("neither was given"), "{error}");
}

#[test]
fn browser_refusals_name_the_field_show_what_was_received_and_give_a_valid_call() {
    let url = "http://localhost:8765/";
    let cases: Vec<(Value, Vec<&str>)> = vec![
        (json!("http://localhost:8765/"), vec!["arguments must be a JSON object", "got \"http://localhost:8765/\""]),
        (json!({}), vec!["url is required", "http://localhost:3000/", "it was not given"]),
        (json!({"url": 8765}), vec!["url must be a string; got 8765"]),
        (json!({"url": "javascript:alert(1)"}), vec!["url must use http or https, not javascript", "got \"javascript:alert(1)\"", "http://localhost:<port>"]),
        (json!({"url": url, "code": "await page.content();"}), vec!["unknown argument \"code\"", "got \"await page.content();\"", "browser takes url (required), steps, snapshot and viewport", "Steps are actions, not code"]),
        (json!({"url": url, "wait": 5}), vec!["unknown argument \"wait\"; got 5"]),
        (json!({"url": url, "steps": "await page.click('#buy')"}), vec!["steps must be a list of step objects", "got \"await page.click('#buy')\"", "not code", "A step is an object with an action"]),
        (json!({"url": url, "steps": [7]}), vec!["steps[0] must be an object; got 7", "A step is an object with an action"]),
        (json!({"url": url, "steps": [{"selector": "#buy"}]}), vec!["steps[0].selector is not a step field; got \"#buy\"", "Step fields are action, target, url, value, key, text, timeout_ms"]),
        (json!({"url": url, "steps": [{"target": {"text": "Buy"}}]}), vec!["steps[0].action is required and must be one of goto, click", "it was not given"]),
        (json!({"url": url, "steps": [{"action": "evaluate", "text": "document.title"}]}), vec!["steps[0].action must be one of goto, click", "got \"evaluate\"", "Steps are actions, not code"]),
        (json!({"url": url, "steps": [{"action": "hover", "target": {"text": "Buy"}}]}), vec!["got \"hover\""]),
        (json!({"url": url, "steps": [{"action": "click"}]}), vec!["steps[0]: click needs target such as {\"role\": \"button\", \"name\": \"Save\"}"]),
        (json!({"url": url, "steps": [{"action": "reload", "url": url}]}), vec!["steps[0]: reload does not take url; got url \"http://localhost:8765/\""]),
        (json!({"url": url, "steps": [{"action": "goto", "url": "/cart"}]}), vec!["steps[0].url is not an absolute URL; got \"/cart\""]),
        (json!({"url": url, "steps": [{"action": "click", "target": {"xpath": "//a"}}]}), vec!["steps[0].target.xpath is not a target field; got \"//a\"", "one of role (with an optional name), label, text, testid or css"]),
        (json!({"url": url, "steps": [{"action": "click", "target": "Buy"}]}), vec!["steps[0].target must be an object such as {\"role\": \"button\", \"name\": \"Save\"}; got \"Buy\""]),
        (json!({"url": url, "steps": [{"action": "click", "target": {"text": "Buy", "nth": "2"}}]}), vec!["steps[0].target.nth must be a whole number from 0; got \"2\""]),
        (json!({"url": url, "steps": [{"action": "click", "target": {"text": "Buy"}, "timeout_ms": 60000}]}), vec!["steps[0].timeout_ms must be 100-10000; got 60000"]),
        (json!({"url": url, "steps": [{"action": "fill", "target": {"label": "Qty"}, "value": 2}]}), vec!["steps[0].value must be a string; got 2"]),
        (json!({"url": url, "snapshot": "html"}), vec!["snapshot must be \"aria\", \"text\" or \"none\"; got \"html\"", "Leave it out for \"aria\", the default"]),
        (json!({"url": url, "snapshot": true}), vec!["got true"]),
        (json!({"url": url, "viewport": {"width": 100}}), vec!["viewport width must be 320-1920 and height 240-1200; got {\"width\":100}"]),
        (json!({"url": url, "viewport": {"depth": 2}}), vec!["viewport.depth is not a viewport field; got 2"]),
        (json!({"url": url, "viewport": "1280x800"}), vec!["viewport must be an object such as {\"width\": 390, \"height\": 844}; got \"1280x800\""]),
    ];
    for (arguments, expected) in cases {
        let error = drive(arguments.clone()).unwrap_err();
        assert!(
            error.starts_with("Invalid arguments for tool browser: "),
            "{error}"
        );
        for text in expected {
            assert!(
                error.contains(text),
                "{arguments}: missing {text:?} in {error}"
            );
        }
        // Every refusal ends with one call that is accepted, on the call's
        // own URL when it has a usable one.
        let example = example_of(&error);
        drive(example.clone()).unwrap_or_else(|failure| panic!("{example}: {failure}"));
        let own = arguments
            .get("url")
            .and_then(Value::as_str)
            .filter(|candidate| check_url(candidate, "url").is_ok());
        assert_eq!(
            example["url"],
            own.unwrap_or("http://localhost:3000/"),
            "{error}"
        );
        assert!(error.len() < 1500, "{} bytes: {error}", error.len());
    }
    // A long value is cut, not repeated whole.
    let error = drive(json!({"url": url, "steps": [{"code": "x".repeat(5000)}]})).unwrap_err();
    assert!(error.contains("xxx...\""), "{error}");
    assert!(error.len() < 1500, "{error}");
    let steps: Vec<Value> = std::iter::repeat_n(json!({"action": "back"}), MAX_STEPS + 1).collect();
    let error = drive(json!({"url": url, "steps": steps})).unwrap_err();
    assert!(
        error.contains("steps has 41 entries; at most 40 steps"),
        "{error}"
    );
}

#[test]
fn browser_check_refusals_name_the_field_show_what_was_received_and_give_a_valid_call() {
    let parse = |arguments: Value| parse_check_call(&arguments).map_err(|error| error.to_string());
    let cases: Vec<(Value, Vec<&str>)> = vec![
        (
            json!(["qa/a.spec.ts"]),
            vec!["arguments must be a JSON object; got [\"qa/a.spec.ts\"]"],
        ),
        (
            json!({"path": "qa/a.spec.ts", "workers": 4}),
            vec![
                "unknown argument \"workers\"; got 4",
                "path or script (exactly one)",
            ],
        ),
        (
            json!({}),
            vec![
                "give exactly one of path",
                "qa/findings/B07.spec.ts",
                "neither was given",
            ],
        ),
        (
            json!({"path": "qa/a.spec.ts", "script": "x"}),
            vec!["not both"],
        ),
        (json!({"path": 7}), vec!["path must be a string; got 7"]),
        (
            json!({"path": "../outside.spec.ts"}),
            vec!["without '..'", "got \"../outside.spec.ts\""],
        ),
        (
            json!({"path": "qa/notes.md"}),
            vec!["path must name a .ts", "got \"qa/notes.md\""],
        ),
        (
            json!({"script": "  "}),
            vec!["script must be the source of a Playwright test file; got \"  \""],
        ),
        (
            json!({"script": "x".repeat(MAX_CHECK_SCRIPT_BYTES + 1)}),
            vec!["script must be at most 65536 bytes; got 65537 bytes"],
        ),
        (
            json!({"path": "qa/a.spec.ts", "grep": ""}),
            vec![
                "grep must be 1-512 bytes",
                "got 0 bytes",
                "Leave it out to run every test",
            ],
        ),
        (
            json!({"path": "qa/a.spec.ts", "base_url": "file:///tmp"}),
            vec![
                "base_url must use http or https, not file",
                "got \"file:///tmp\"",
            ],
        ),
    ];
    for (arguments, expected) in cases {
        let error = parse(arguments.clone()).unwrap_err();
        assert!(
            error.starts_with("Invalid arguments for tool browser_check: "),
            "{error}"
        );
        for text in expected {
            assert!(
                error.contains(text),
                "{arguments}: missing {text:?} in {error}"
            );
        }
        let example = example_of(&error);
        parse(example.clone()).unwrap_or_else(|failure| panic!("{example}: {failure}"));
    }
    let error = parse(json!({"script": ""})).unwrap_err();
    assert!(example_of(&error).get("script").is_some(), "{error}");
}

/// Recorded `browser` arguments, byte for byte, from two models calling the
/// tool through Ollama against an app on port 8765, with the SHA-256 the
/// Session record kept for each, and what Axocoatl 1.2.0 answered.
const RECORDED_GPT_OSS: [(&str, &str, &str); 7] = [
    (
        "9e959462a16c778cb0f8b5c81ce8346f709c192d23b23eff39991de3103591ae",
        r#"{"snapshot":null,"steps":[],"url":"http://localhost:8765","viewport":{"height":800,"width":1280}}"#,
        "snapshot must be aria, text or none",
    ),
    (
        "bcdc9971e1165d67617ba44b7befd6f52cf36be28c9ca848b7a6b39d41602978",
        r#"{"snapshot":"none","steps":[],"url":"http://localhost:8765","viewport":{"height":800,"width":1280}}"#,
        "",
    ),
    (
        "4bc3daa419752190f3714a258083c3cc4245707db76a20d5c0f128621f78d115",
        r#"{"snapshot":"none","steps":[{"code":"await page.content();"}],"url":"http://localhost:8765","viewport":{"height":800,"width":1280}}"#,
        "steps[0].code is not a step field",
    ),
    (
        "ebbb50c92019a74f572711a62e8785c23f28ef4be78c60e769f3cd5707903aa9",
        r#"{"snapshot":"none","steps":[{"code":"await page.content();","name":"content"}],"url":"http://localhost:8765","viewport":{"height":800,"width":1280}}"#,
        "steps[0].code is not a step field",
    ),
    (
        "9912da7ec3e3acc9763131c3928d84defbbcf64ca5ddd73474e27ce77dfdbc4c",
        r#"{"snapshot":"none","steps":["await page.content();"],"url":"http://localhost:8765","viewport":{"height":800,"width":1280}}"#,
        "steps[0] must be an object",
    ),
    (
        "065fd7a8d42fc6d0da8e116e16ff8910a3b583302ed5705b1b7d2730287d9793",
        r#"{"snapshot":"none","steps":[{"code":"await page.waitForSelector('h1'); const count = await page.$$eval('h1', els=>els.length); console.log('h1 count', count);"}],"url":"http://localhost:8765","viewport":{"height":800,"width":1280}}"#,
        "steps[0].code is not a step field",
    ),
    (
        "e2d7fb73d6fe7b8b7f3338335b49b5fab3d58b4931989bc328f0e3ccfdb3ad02",
        r#"{"snapshot":"none","steps":[{"code":"await page.goto('/'); await page.waitForSelector('.product-card'); const count = await page.locator('.product-card').count(); console.log('cards', count);"}],"url":"http://localhost:8765","viewport":{"height":800,"width":1280}}"#,
        "steps[0].code is not a step field",
    ),
];

/// Every `browser` call qwen3-coder made in the same task; 1.2.0 accepted
/// all of them.
const RECORDED_QWEN3_CODER: [(&str, &str); 11] = [
    (
        "1d0e3b3e97d549862c9605d9cf22532384d41e0a0455bcfa1b0a98bf0cd4a58c",
        r#"{"url":"http://localhost:8765"}"#,
    ),
    (
        "08a5cfdb1118f790c21e932bcfd4b72bd68804974d4db023eb8422a2737424aa",
        r#"{"url":"http://localhost:8765/product/NS-1101"}"#,
    ),
    (
        "87de5cc7e0d4f9e8dfffc72ff9357bf53609a44ae0e52d4905d4193bade368f0",
        r#"{"steps":[{"action":"click","target":{"name":"Add to cart","role":"button"}}],"url":"http://localhost:8765/product/NS-1101"}"#,
    ),
    (
        "d1df0e74f892f1d8d45bc9021ede7f50493026e32adb9c17c88bf6cc01c856af",
        r#"{"url":"http://localhost:8765/cart"}"#,
    ),
    (
        "d59ef2f7572ce0030fa2e93198130d145f046c42491ab235e15ef42a7121ff1f",
        r#"{"steps":[{"action":"goto","url":"http://localhost:8765/"},{"action":"click","target":{"name":"Arc Floor Lamp","role":"link"}},{"action":"click","target":{"name":"Add to cart","role":"button"}},{"action":"click","target":{"name":"Cart (1)","role":"link"}}],"url":"http://localhost:8765/"}"#,
    ),
    (
        "d1df0e74f892f1d8d45bc9021ede7f50493026e32adb9c17c88bf6cc01c856af",
        r#"{"url":"http://localhost:8765/cart"}"#,
    ),
    (
        "d59ef2f7572ce0030fa2e93198130d145f046c42491ab235e15ef42a7121ff1f",
        r#"{"steps":[{"action":"goto","url":"http://localhost:8765/"},{"action":"click","target":{"name":"Arc Floor Lamp","role":"link"}},{"action":"click","target":{"name":"Add to cart","role":"button"}},{"action":"click","target":{"name":"Cart (1)","role":"link"}}],"url":"http://localhost:8765/"}"#,
    ),
    (
        "fbd8e15f6ba68e904594f5cb289e8ba159bf634e5a82df5755ba9a3e6628e142",
        r#"{"steps":[{"action":"goto","url":"http://localhost:8765/"},{"action":"click","target":{"name":"Arc Floor Lamp","role":"link"}},{"action":"click","target":{"name":"Add to cart","role":"button"}},{"action":"goto","url":"http://localhost:8765/cart"}],"url":"http://localhost:8765/"}"#,
    ),
    (
        "2f1a734a84a4ffc8f486506c5d8de3a4e63988a891b7d204bcc926594f2549c7",
        r#"{"steps":[{"action":"goto","url":"http://localhost:8765/cart"},{"action":"click","target":{"name":"Checkout","role":"link"}}],"url":"http://localhost:8765/cart"}"#,
    ),
    (
        "1e86d72cb768751b0da910e2b07abe335b413f2b15cec52dce14984ff25baa8e",
        r#"{"steps":[{"action":"goto","url":"http://localhost:8765/"},{"action":"click","target":{"name":"Arc Floor Lamp","role":"link"}},{"action":"click","target":{"name":"Add to cart","role":"button"}},{"action":"click","target":{"name":"Cart (1)","role":"link"}},{"action":"click","target":{"name":"Checkout","role":"link"}}],"url":"http://localhost:8765/"}"#,
    ),
    (
        "15f37da28958cb8fae55d098653e1841da2d444d65a9135667c01f127510ab81",
        r#"{"steps":[{"action":"goto","url":"http://localhost:8765/orders"}],"url":"http://localhost:8765/orders"}"#,
    ),
];

fn recorded(sha256: &str, text: &str) -> Value {
    use sha2::{Digest, Sha256};
    assert_eq!(
        format!("{:x}", Sha256::digest(text.as_bytes())),
        sha256,
        "the replayed bytes are the recorded ones"
    );
    serde_json::from_str(text).unwrap()
}

#[test]
fn recorded_gpt_oss_calls_are_accepted_or_refused_with_a_lesson() {
    let outcomes: Vec<Result<DriveJob, String>> = RECORDED_GPT_OSS
        .iter()
        .map(|(sha256, text, before)| {
            let outcome = drive(recorded(sha256, text));
            if let Err(error) = &outcome {
                // The old one-line reason is still the start of the new one.
                assert!(error.contains(before), "{error}");
            }
            outcome
        })
        .collect();

    // 1. `snapshot: null` was refused; it is now the default aria snapshot.
    let first = outcomes[0].as_ref().unwrap();
    assert_eq!(first.snapshot, SnapshotKind::Aria);
    assert_eq!(first.url, "http://localhost:8765");
    assert_eq!(first.steps, Vec::<Value>::new());
    assert_eq!(first.viewport, Some((1280, 800)));
    // 2. An explicit "none" is still honored.
    assert_eq!(outcomes[1].as_ref().unwrap().snapshot, SnapshotKind::None);
    // 3-7. Script in steps is still refused, now with what to do instead.
    for (index, outcome) in outcomes.iter().enumerate().skip(2) {
        let error = outcome.as_ref().unwrap_err();
        assert!(
            error.contains(
                "Steps are actions, not code: browser runs no JavaScript or Playwright script"
            ) && error.contains("call with just url and read the snapshot")
                && error.contains("A step is an object with an action: goto (url); click")
                && error.contains(
                    r#"Example: {"url": "http://localhost:8765", "steps": [{"action": "click""#
                ),
            "call {}: {error}",
            index + 1
        );
    }
    let third = outcomes[2].as_ref().unwrap_err();
    assert!(
        third.contains(r#"steps[0].code is not a step field; got "await page.content();""#),
        "{third}"
    );
    let fifth = outcomes[4].as_ref().unwrap_err();
    assert!(
        fifth.contains(r#"steps[0] must be an object, not a string; got "await page.content();""#),
        "{fifth}"
    );
}

#[test]
fn recorded_qwen3_coder_calls_are_accepted_unchanged() {
    for (sha256, text) in RECORDED_QWEN3_CODER {
        let arguments = recorded(sha256, text);
        let job = drive(arguments.clone()).unwrap_or_else(|error| panic!("{text}: {error}"));
        assert_eq!(job.snapshot, SnapshotKind::Aria);
        assert_eq!(
            Value::Array(job.steps),
            arguments.get("steps").cloned().unwrap_or(json!([]))
        );
    }
}

#[test]
fn the_schemas_state_defaults_and_match_the_parser() {
    let schema = BrowserTool::schema();
    let properties = schema["properties"].as_object().unwrap();
    for (name, property) in properties {
        assert!(property["type"].is_string(), "{name} has no type");
        let description = property["description"].as_str().unwrap_or_default();
        assert!(!description.is_empty(), "{name} has no description");
    }
    assert_eq!(schema["required"], json!(["url"]));
    let mut names: Vec<&str> = properties.keys().map(String::as_str).collect();
    names.sort_unstable();
    let mut fields = DRIVE_FIELDS.to_vec();
    fields.sort_unstable();
    assert_eq!(names, fields);
    // Defaults the schema states are the parser's.
    let defaults = drive(json!({"url": "http://localhost/", "viewport": {}})).unwrap();
    assert_eq!(properties["snapshot"]["default"], "aria");
    assert_eq!(defaults.snapshot, SnapshotKind::Aria);
    assert_eq!(
        properties["viewport"]["properties"]["width"]["default"],
        defaults.viewport.unwrap().0
    );
    assert_eq!(
        properties["viewport"]["properties"]["height"]["default"],
        defaults.viewport.unwrap().1
    );
    for kind in properties["snapshot"]["enum"].as_array().unwrap() {
        drive(json!({"url": "http://localhost/", "snapshot": kind})).unwrap();
    }
    let step = &properties["steps"]["items"];
    assert_eq!(step["properties"]["action"]["type"], "string");
    assert_eq!(step["properties"]["action"]["enum"], json!(ACTIONS));
    let mut fields: Vec<&str> = step["properties"]
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    fields.sort_unstable();
    let mut expected = STEP_FIELDS.to_vec();
    expected.sort_unstable();
    assert_eq!(fields, expected);
    // The example step in the steps description is a valid step.
    let description = properties["steps"]["description"].as_str().unwrap();
    let start = description.find("{\"action\"").unwrap();
    let end = description[start..].find("}}").unwrap() + start + 2;
    check_step(&serde_json::from_str(&description[start..end]).unwrap(), 0).unwrap();
    for action in ACTIONS {
        assert!(description.contains(action), "{action}");
    }
    // The description makes the common path plain.
    assert!(BROWSER_DESCRIPTION.contains("Call it with just url"));
    assert!(BROWSER_DESCRIPTION.contains("Steps are actions, not code"));

    let check = BrowserCheckTool::schema();
    let mut names: Vec<&str> = check["properties"]
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    names.sort_unstable();
    let mut fields = CHECK_FIELDS.to_vec();
    fields.sort_unstable();
    assert_eq!(names, fields);
    assert!(CHECK_DESCRIPTION.contains("exactly one of path"));
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
