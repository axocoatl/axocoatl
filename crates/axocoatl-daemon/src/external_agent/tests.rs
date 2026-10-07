use super::*;

const CLAUDE_STREAM: &str = include_str!("fixtures/claude-code-2.1.292-stream.jsonl");
const CLAUDE_AUTH_FAILURE: &str = include_str!("fixtures/claude-code-2.1.292-auth-failure.jsonl");
const CODEX_EXEC: &str = include_str!("fixtures/codex-0.160.1-exec.jsonl");
const CODEX_EXEC_AUTH_FAILURE: &str =
    include_str!("fixtures/codex-0.160.1-exec-auth-failure.jsonl");
const CODEX_APP_SERVER: &str = include_str!("fixtures/codex-0.153.4-app-server.json");
const CODEX_APP_SERVER_OFFICIAL: &str =
    include_str!("fixtures/codex-0.153.4-app-server-official.json");

#[test]
fn routes_add_the_stored_secret_for_the_paths_the_pinned_programs_call() {
    assert!(routes_for(AgentRuntime::Native).unwrap().is_empty());
    let claude = routes_for(AgentRuntime::ClaudeCode).unwrap();
    assert_eq!(claude.len(), 1);
    let route = &claude[0];
    assert_eq!(route.host, "api.anthropic.com");
    assert_eq!(route.credential.as_deref(), Some("claude-code-oauth"));
    let inject = route.inject.as_ref().unwrap();
    assert_eq!(inject.header.as_deref(), Some("Authorization"));
    assert_eq!(inject.format.as_deref(), Some("Bearer {}"));
    assert_eq!(route.env_placeholders, ["CLAUDE_CODE_OAUTH_TOKEN"]);
    assert_eq!(route.rules.len(), 1);
    assert_eq!(route.rules[0].methods, ["POST"]);
    assert_eq!(route.rules[0].path, "/v1/messages");
    assert_eq!(
        route.bindings.as_deref(),
        Some(&[axocoatl_config::RouteForYaml::Agent][..])
    );
    let codex = routes_for(AgentRuntime::Codex).unwrap();
    assert_eq!(codex[0].host, "api.openai.com");
    assert_eq!(codex[0].credential.as_deref(), Some("codex-openai"));
    assert_eq!(codex[0].env_placeholders, ["OPENAI_API_KEY"]);
    assert_eq!(codex[0].rules[0].path, "/v1/responses");
    // The routes pass the config's own route validation when the credential
    // exists.
    for (runtime, credential) in [
        (AgentRuntime::ClaudeCode, "claude-code-oauth"),
        (AgentRuntime::Codex, "codex-openai"),
    ] {
        let mut config = axocoatl_config::AxocoatlConfig::default();
        config.sandbox.network = "egress".into();
        config.credentials.insert(
            credential.into(),
            axocoatl_config::CredentialSourceYaml {
                env: None,
                file: Some("/var/empty/secret".into()),
            },
        );
        config.sandbox.egress = Some(axocoatl_config::EgressConfigYaml {
            routes: routes_for(runtime).unwrap(),
            ..Default::default()
        });
        axocoatl_config::validate_egress_routes(&config).unwrap();
        // And the broker compiles them with a file credential.
        let credentials = config.credentials.clone();
        crate::egress_broker::RouteTable::compile(
            &config.sandbox.egress.as_ref().unwrap().routes,
            &credentials,
            &[],
        )
        .unwrap();
    }
}

#[test]
fn the_argv_runs_the_pinned_programs_headless_with_their_own_sandbox_off() {
    let claude = command_argv(AgentRuntime::ClaudeCode, "claude-sonnet-4-5").unwrap();
    assert_eq!(&claude[..2], ["sh", "-c"]);
    assert_eq!(claude[4], MAX_OUTPUT_FILE_BYTES.to_string());
    assert_eq!(claude[5], MAX_READ_BACK_BYTES.to_string());
    let program = &claude[6..];
    assert_eq!(program[0], "env");
    for variable in [
        "DISABLE_TELEMETRY=1",
        "DISABLE_ERROR_REPORTING=1",
        "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC=1",
        "DISABLE_AUTOUPDATER=1",
    ] {
        assert!(program.contains(&variable.to_string()), "{variable}");
    }
    let start = program.iter().position(|arg| arg == "claude").unwrap();
    assert_eq!(
        &program[start..],
        [
            "claude",
            "-p",
            "--output-format",
            "stream-json",
            "--verbose",
            "--model",
            "claude-sonnet-4-5",
            "--dangerously-skip-permissions",
            "--no-session-persistence"
        ]
    );
    // No token, placeholder or key is ever an argument.
    assert!(!claude.iter().any(|arg| arg.contains("OAUTH_TOKEN=")));

    let codex = program_argv(AgentRuntime::Codex, "gpt-5.5").unwrap();
    let start = codex.iter().position(|arg| arg == "codex").unwrap();
    assert_eq!(
        &codex[start..start + 5],
        ["codex", "exec", "--json", "--model", "gpt-5.5"]
    );
    assert!(codex.contains(&"--dangerously-bypass-approvals-and-sandbox".to_string()));
    assert!(codex.contains(&"model_providers.axocoatl.supports_websockets=false".to_string()));
    assert!(codex
        .contains(&"model_providers.axocoatl.base_url=\"https://api.openai.com/v1\"".to_string()));
    assert!(codex.contains(&"model_providers.axocoatl.env_key=\"OPENAI_API_KEY\"".to_string()));
    assert_eq!(codex.last().map(String::as_str), Some("-"));
    assert!(!codex.iter().any(|arg| arg.starts_with("OPENAI_API_KEY=")));

    for model in ["", "--help", "a b", "x\ny"] {
        assert!(
            command_argv(AgentRuntime::ClaudeCode, model).is_err(),
            "{model:?}"
        );
    }
    assert!(command_argv(AgentRuntime::Native, "m").is_err());
}

#[test]
fn claude_code_stream_json_parses_into_items_answer_and_usage() {
    let result = claude_code::parse_output(CLAUDE_STREAM.as_bytes()).unwrap();
    assert_eq!(
        result.final_answer.as_deref(),
        Some("Created hello.txt with the text hello.")
    );
    assert!(result.usage_complete);
    // input 2003 + cache creation 400 + cache read 600; cost $0.008574.
    assert_eq!(result.usage(), Some((3003, 59, Some(8574))));
    assert_eq!(
        result.items[..4],
        [
            ExternalItem::AssistantText {
                text: "I will create the file.".into()
            },
            ExternalItem::ToolCall {
                name: "Bash".into(),
                arguments: r#"{"command":"printf 'hello\\n' > hello.txt && cat hello.txt","description":"Create hello.txt"}"#.into(),
            },
            ExternalItem::ToolResult {
                name: "Bash".into(),
                output: "hello".into(),
                is_error: false
            },
            ExternalItem::AssistantText {
                text: "Created hello.txt with the text hello.".into()
            },
        ]
    );
    assert!(result.last_error().is_none());
}

#[test]
fn claude_code_errors_leave_no_answer_and_say_why() {
    let result = claude_code::parse_output(CLAUDE_AUTH_FAILURE.as_bytes()).unwrap();
    assert_eq!(result.final_answer, None);
    let error = result.last_error().unwrap();
    assert!(error.contains("api_error, HTTP 401"), "{error}");
    assert!(result
        .items
        .iter()
        .any(|item| matches!(item, ExternalItem::Error { message } if message.contains("retried (attempt 1 of 10)"))));
    // The program reported zero usage for the failed run.
    assert!(result.usage_complete);
    assert_eq!(result.usage(), Some((0, 0, Some(0))));

    // Cut short: no result line, usage unknown.
    let cut: String = CLAUDE_STREAM.lines().take(3).collect::<Vec<_>>().join("\n");
    let result = claude_code::parse_output(cut.as_bytes()).unwrap();
    assert_eq!(result.final_answer, None);
    assert!(!result.usage_complete);
    assert_eq!(result.usage(), None);
    assert!(result.last_error().unwrap().contains("no result"));
}

#[test]
fn codex_exec_jsonl_parses_into_items_answer_and_usage() {
    let result = codex::parse_output(CODEX_EXEC.as_bytes()).unwrap();
    assert_eq!(
        result.final_answer.as_deref(),
        Some("Created hello.txt with the text hello.")
    );
    assert!(result.usage_complete);
    assert_eq!(result.usage(), Some((4107, 60, None)));
    assert!(
        matches!(&result.items[0], ExternalItem::ToolCall { name, arguments }
        if name == "command" && arguments.contains("printf 'hello"))
    );
    assert!(
        matches!(&result.items[1], ExternalItem::ToolResult { output, is_error: false, .. }
        if output == "exit 0\nhello\n")
    );

    let failed = codex::parse_output(CODEX_EXEC_AUTH_FAILURE.as_bytes()).unwrap();
    assert_eq!(failed.final_answer, None);
    assert!(!failed.usage_complete);
    assert!(failed.last_error().unwrap().contains("401 Unauthorized"));
}

/// The app-server captures kept from the adapter removed in b922287.
#[test]
fn codex_app_server_notifications_from_b922287_still_parse() {
    for fixture in [CODEX_APP_SERVER, CODEX_APP_SERVER_OFFICIAL] {
        let capture: serde_json::Value = serde_json::from_str(fixture).unwrap();
        let normal = capture["normal_turn_id"].as_str().unwrap();
        let turn_of = |event: &serde_json::Value| {
            event["params"]["turnId"]
                .as_str()
                .or_else(|| event["params"]["turn"]["id"].as_str())
                .map(str::to_string)
        };
        let lines = |wanted: &dyn Fn(Option<String>) -> bool| {
            capture["events"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|event| wanted(turn_of(event)))
                .map(|event| event.to_string())
                .collect::<Vec<_>>()
                .join("\n")
        };
        let result =
            codex::parse_output(lines(&|turn| turn.as_deref() == Some(normal)).as_bytes()).unwrap();
        assert!(result
            .final_answer
            .as_deref()
            .is_some_and(|answer| answer.starts_with("AXOCOATL_")));
        assert!(result.usage_complete);
        let (input, output, cost) = result.usage().unwrap();
        assert!(input > 0 && output > 0);
        assert_eq!(cost, None);
        // The whole capture includes an interrupted turn: no answer stands.
        let whole = codex::parse_output(lines(&|_| true).as_bytes()).unwrap();
        assert_eq!(whole.final_answer, None);
        assert!(whole.last_error().unwrap().contains("interrupted"));
    }
}

#[test]
fn unreadable_lines_and_the_truncation_marker_are_recorded_not_fatal() {
    let mut stdout = String::from("not json\n");
    stdout.push_str(CLAUDE_STREAM.lines().next().unwrap());
    stdout.push_str("\n{\"type\":\"axocoatl_truncated\",\"bytes\":20000000,\"kept\":8388608}\n");
    stdout.push_str(CLAUDE_STREAM.lines().last().unwrap());
    let result = claude_code::parse_output(stdout.as_bytes()).unwrap();
    assert!(result.final_answer.is_some());
    let errors: Vec<&str> = result
        .items
        .iter()
        .filter_map(|item| match item {
            ExternalItem::Error { message } => Some(message.as_str()),
            _ => None,
        })
        .collect();
    assert!(
        errors[0].starts_with("output line 1 is not JSON"),
        "{errors:?}"
    );
    assert!(errors[1].contains("20000000 bytes"), "{errors:?}");
}

#[test]
fn items_and_previews_are_bounded() {
    let long = "x".repeat(MAX_PREVIEW_BYTES * 3);
    let mut stdout = String::new();
    for index in 0..(MAX_ITEMS + 10) {
        stdout.push_str(
            &serde_json::json!({"type":"assistant","message":{"content":[
            {"type":"tool_use","id":format!("t{index}"),"name":"Bash","input":{"command":long}}]}})
            .to_string(),
        );
        stdout.push('\n');
    }
    let result = claude_code::parse_output(stdout.as_bytes()).unwrap();
    assert!(result.items.len() <= MAX_ITEMS);
    assert!(result.items.iter().all(|item| match item {
        ExternalItem::ToolCall { arguments, .. } => arguments.len() < MAX_PREVIEW_BYTES + 64,
        _ => true,
    }));
    assert!(result.items.iter().any(
        |item| matches!(item, ExternalItem::Error { message } if message.contains("left out"))
    ));
    let lines = work_log(AgentRuntime::ClaudeCode, "m", &result, 3, None);
    let total: usize = lines.iter().map(String::len).sum();
    assert!(total <= MAX_WORK_LOG_BYTES + 4096, "{total}");
    assert!(lines.last().unwrap().contains("left out of the record"));
}

#[test]
fn the_work_log_leads_with_the_run_and_leaves_the_answer_to_the_text() {
    let result = claude_code::parse_output(CLAUDE_STREAM.as_bytes()).unwrap();
    let lines = work_log(
        AgentRuntime::ClaudeCode,
        "claude-sonnet-4-5",
        &result,
        2,
        None,
    );
    assert!(lines[0].starts_with(
        "[external agent] @anthropic-ai/claude-code 2.1.292, model claude-sonnet-4-5: exit unknown, 2 model request(s)"
    ));
    assert!(lines
        .iter()
        .any(|line| line.starts_with("[tool call] Bash: ")));
    assert!(lines
        .iter()
        .any(|line| line.starts_with("[tool result] Bash: hello")));
    assert!(lines
        .iter()
        .any(|line| line.contains("[usage] 3003 input and 59 output tokens, $0.008574")));
    assert!(!lines
        .iter()
        .any(|line| line.contains("Created hello.txt with the text hello.")));
    let codex = codex::parse_output(CODEX_EXEC.as_bytes()).unwrap();
    let lines = work_log(
        AgentRuntime::Codex,
        "gpt-5.5",
        &codex,
        2,
        Some("time limit"),
    );
    assert!(lines[0].contains("stopped: time limit"));
}

#[test]
fn the_prompt_carries_the_session_note_instructions_history_and_task() {
    let messages = vec![
        ChatMessage::system("Fix the bug. Keep changes small."),
        ChatMessage::user("First request"),
        ChatMessage::assistant("First answer"),
        ChatMessage::user("Review findings: F1 off by one"),
    ];
    let prompt = prompt_text(&messages);
    let note = prompt.find("# Axocoatl").unwrap();
    let instructions = prompt.find("# Instructions\nFix the bug.").unwrap();
    let earlier = prompt.find("# Earlier request\nFirst request").unwrap();
    let answer = prompt.find("# Your earlier answer\nFirst answer").unwrap();
    let task = prompt
        .find("# Task\nReview findings: F1 off by one")
        .unwrap();
    assert!(note < instructions && instructions < earlier && earlier < answer && answer < task);
    let single = prompt_text(&[ChatMessage::user("Only this")]);
    assert!(single.ends_with("# Task\nOnly this"));
    assert!(!single.contains("# Instructions"));
}

#[test]
fn an_external_definition_is_the_autonomous_writer_with_bash() {
    let config = AgentConfig {
        id: axocoatl_core::AgentId::new("writer"),
        name: "writer".into(),
        tools: vec!["read_file".into(), "write_file".into()],
        system_prompt: Some("Own the change.".into()),
        writes: Some(vec!["src/".into()]),
        token_budget: Some(axocoatl_core::TokenBudget {
            per_call: 10,
            per_execution: 10,
            overflow_policy: Default::default(),
        }),
        ..Default::default()
    };
    let external =
        external_agent_config(config.clone(), AgentRuntime::ClaudeCode, "claude-opus-4-5").unwrap();
    assert_eq!(external.provider, "claude-code");
    assert_eq!(external.model, "claude-opus-4-5");
    assert_eq!(external.tools, ["bash"]);
    assert_eq!(external.role, AgentRole::Autonomous);
    assert!(external.token_budget.is_none());
    assert_eq!(external.writes, config.writes);
    assert_eq!(external.system_prompt, config.system_prompt);
    assert_eq!(
        validate_external_config(&external).unwrap(),
        AgentRuntime::ClaudeCode
    );
    assert_eq!(runtime_for_provider("codex"), Some(AgentRuntime::Codex));
    assert_eq!(runtime_for_provider("ollama"), None);
    assert!(external_agent_config(config.clone(), AgentRuntime::Native, "m").is_err());
    let mut changed = external.clone();
    changed.tools.push("write_file".into());
    assert!(validate_external_config(&changed).is_err());
    let mut worker = external;
    worker.role = AgentRole::Worker;
    assert!(validate_external_config(&worker).is_err());
    let profile = ExternalRuntimeProfile::for_runtime(AgentRuntime::Codex, "gpt-5.5").unwrap();
    assert_eq!(
        (
            profile.package.as_str(),
            profile.version.as_str(),
            profile.recipe.as_str()
        ),
        ("@openai/codex", "0.160.1", "codex")
    );
}

/// The bounded output wrapper, run by this computer's `sh` with small
/// limits: whole output when it fits; first and last halves around a marker
/// when it does not; the program stopped at the file limit; its exit status
/// kept; its stdin passed through.
#[test]
fn the_output_wrapper_bounds_the_file_and_keeps_the_status() {
    let run = |limit: usize, keep: usize, script: &str, stdin: &str| {
        use std::io::Write;
        let mut child = std::process::Command::new("sh")
            .args(["-c", OUTPUT_WRAPPER, "sh"])
            .arg(limit.to_string())
            .arg(keep.to_string())
            .args(["sh", "-c", script])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(stdin.as_bytes())
            .unwrap();
        let output = child.wait_with_output().unwrap();
        (
            String::from_utf8_lossy(&output.stdout).into_owned(),
            output.status.code(),
        )
    };
    let (out, code) = run(1000, 500, "cat; echo; echo done; exit 3", "prompt");
    assert_eq!(out, "prompt\ndone\n");
    assert_eq!(code, Some(3));
    let lines = "i=0; while [ $i -lt 60 ]; do echo line-$i-xxxxxxxx; i=$((i+1)); done";
    let (out, code) = run(10_000, 200, lines, "");
    assert_eq!(code, Some(0));
    assert!(out.starts_with("line-0-"));
    assert!(out.contains("{\"type\":\"axocoatl_truncated\""));
    assert!(out.trim_end().ends_with("line-59-xxxxxxxx"));
    assert!(out.len() < 400);
    let endless = "while :; do echo yyyyyyyyyyyyyyyyyyyy; done";
    let (out, code) = run(5000, 100_000, endless, "");
    assert_ne!(code, Some(0));
    assert!(out.contains("\"limit\":5000"), "{out}");
}
