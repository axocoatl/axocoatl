//! The provider-fetch check on the request bodies the pinned programs send
//! and on each construct it refuses.
//!
//! The fixtures under `fixtures/` are request bodies captured from the
//! pinned programs in the recipe images, running through the Session's
//! route against the fake model API of
//! `bootstrap_external_turn_tests.rs` (Podman): every key, type and
//! structure is as the program sent it; strings longer than 120 characters
//! (the programs' prompts, tool descriptions and the task) are replaced by
//! `<elided by Axocoatl: N characters>`, and Claude Code's device id by
//! zeros. No body held a token or the route's placeholder.
//!
//! - `claude-code-2.1.292-messages.json`: Claude Code's second model call
//!   of a run (custom tools only, a `tool_use` and its `tool_result`).
//! - `claude-code-2.1.292-messages-tool-reference.json`: a later call of a
//!   run in which the model loaded the deferred WebSearch tool through
//!   ToolSearch (`tool_reference` blocks, WebSearch as a custom tool with
//!   `defer_loading`).
//! - `claude-code-2.1.292-web-search.json`: the request Claude Code's
//!   WebSearch tool then sends itself, offering Anthropic's server-side
//!   `web_search_20250305`; before this check, the route Axocoatl adds for
//!   Claude Code forwarded it.
//! - `codex-0.160.1-responses.json`: Codex's second model call, with
//!   `web_search="disabled"` and `--disable image_generation` as Axocoatl
//!   now runs it (the ChatGPT backend gets the same body).
//! - `codex-0.160.1-responses-web-search-live.json`: the same call as Codex
//!   0.160.1 sent it before, offering OpenAI's hosted `web_search` with
//!   `external_web_access: true`.
//! - `codex-0.160.1-responses-code-mode.json`: Codex's first model call for
//!   `gpt-6.1-sol`, a model its catalog marks `code_mode_only`, run as
//!   Axocoatl runs it in the codex recipe image against a local capture
//!   server: no top-level tools, and its tools (the custom `exec` and its
//!   functions) in namespaces in an `additional_tools` input item.
//!   `gpt-6-luna` sends the same tools.
//!
//! The `e2e-0.18.0-*` fixtures are the first request of e2e 0.18.0's own
//! model adapter (`createModelAdapter`, with its provider hints: a
//! judgment with a schema and a 1×1 PNG) and of an AI SDK 7.0.139 call with
//! one function tool, run with Node in the e2e recipe image with the
//! environment Axocoatl's e2e check exports for a `model_route` of
//! `ai-gateway.vercel.sh`, the prompts the capture's own:
//!
//! - `e2e-0.18.0-ai-gateway-default.json`: the config `e2e init --yes`
//!   writes, `gateway('openai/gpt-6-luna-fast')` from `ai`, which sends it
//!   to `/v4/ai/language-model` (`@ai-sdk/gateway` 4.0.112), with e2e's
//!   `providerOptions.openai`.
//! - `e2e-0.18.0-ai-gateway-openai-compatible.json` and
//!   `e2e-0.18.0-ai-gateway-openai-compatible-tools.json`: the config the
//!   e2e check's docs give instead, `createOpenAICompatible` of
//!   `@ai-sdk/openai-compatible` 3.0.68 with `AXOCOATL_E2E_BASE_URL`, which
//!   sends them to `/v1/chat/completions`.

use super::*;

const CLAUDE_MESSAGES: &str = include_str!("fixtures/claude-code-2.1.292-messages.json");
const CLAUDE_TOOL_REFERENCE: &str =
    include_str!("fixtures/claude-code-2.1.292-messages-tool-reference.json");
const CLAUDE_WEB_SEARCH: &str = include_str!("fixtures/claude-code-2.1.292-web-search.json");
const CODEX_RESPONSES: &str = include_str!("fixtures/codex-0.160.1-responses.json");
const CODEX_WEB_SEARCH_LIVE: &str =
    include_str!("fixtures/codex-0.160.1-responses-web-search-live.json");
const CODEX_CODE_MODE: &str = include_str!("fixtures/codex-0.160.1-responses-code-mode.json");

const E2E_GATEWAY_DEFAULT: &str = include_str!("fixtures/e2e-0.18.0-ai-gateway-default.json");
const E2E_GATEWAY_COMPATIBLE: &str =
    include_str!("fixtures/e2e-0.18.0-ai-gateway-openai-compatible.json");
const E2E_GATEWAY_COMPATIBLE_TOOLS: &str =
    include_str!("fixtures/e2e-0.18.0-ai-gateway-openai-compatible-tools.json");

const MESSAGES: &str = "/v1/messages";
const RESPONSES: &str = "/v1/responses";
const CHATGPT: &str = "/backend-api/codex/responses";
const CHAT: &str = "/api/v1/chat/completions";
const GEMINI: &str = "/v1beta/models/gemini-3-pro:generateContent";

fn refused(path: &str, body: &str) -> Refused {
    check(path, body.as_bytes()).expect_err(&format!("{path} should refuse {body}"))
}

fn passes(path: &str, body: &str) {
    if let Err(refused) = check(path, body.as_bytes()) {
        panic!("{path} refused {body}: {refused:?}");
    }
}

#[test]
fn what_the_pinned_programs_send_passes() {
    passes(MESSAGES, CLAUDE_MESSAGES);
    passes(MESSAGES, CLAUDE_TOOL_REFERENCE);
    passes(RESPONSES, CODEX_RESPONSES);
    passes(CHATGPT, CODEX_RESPONSES);
    // The fixtures hold what the check decides on.
    assert!(CLAUDE_TOOL_REFERENCE.contains(r#"{"type":"tool_reference","tool_name":"WebSearch"}"#));
    assert!(CODEX_RESPONSES.contains(r#"{"type":"tool_search","execution":"client""#));
    assert!(!CODEX_RESPONSES.contains(r#""type":"web_search""#));
}

#[test]
fn e2e_through_the_gateways_openai_compatible_api_passes_and_its_own_protocol_is_known() {
    // The compatible client e2e's docs give: Chat Completions, a data: image,
    // a JSON schema and a function tool.
    passes("/v1/chat/completions", E2E_GATEWAY_COMPATIBLE);
    passes("/v1/chat/completions", E2E_GATEWAY_COMPATIBLE_TOOLS);
    assert!(E2E_GATEWAY_COMPATIBLE.contains(r#""image_url":{"url":"data:image/png;base64,"#));
    assert!(E2E_GATEWAY_COMPATIBLE_TOOLS.contains(r#""tools":[{"type":"function""#));

    // e2e init's default client speaks the AI SDK's own gateway protocol,
    // which the broker refuses by its path before this check reads it, and
    // which this check, given it, does not know either.
    let path = "/v4/ai/language-model";
    assert!(ai_sdk_gateway(VERCEL_AI_GATEWAY_HOST, path));
    assert!(refused(path, E2E_GATEWAY_DEFAULT)
        .construct
        .contains("endpoint whose request format the check does not know"));
    assert!(E2E_GATEWAY_DEFAULT.contains(r#""providerOptions":{"openai":"#));

    for (host, path, expected) in [
        (VERCEL_AI_GATEWAY_HOST, "/v4/ai/language-model", true),
        (VERCEL_AI_GATEWAY_HOST, "/v3/ai/language-model", true),
        (VERCEL_AI_GATEWAY_HOST, "/v1/ai/embedding-model", true),
        (VERCEL_AI_GATEWAY_HOST, "/v12/ai/config", true),
        (VERCEL_AI_GATEWAY_HOST, "/v1/chat/completions", false),
        (VERCEL_AI_GATEWAY_HOST, "/v1/responses", false),
        (VERCEL_AI_GATEWAY_HOST, "/v1/messages", false),
        (VERCEL_AI_GATEWAY_HOST, "/v/ai/language-model", false),
        (VERCEL_AI_GATEWAY_HOST, "/vx/ai/language-model", false),
        (VERCEL_AI_GATEWAY_HOST, "/ai/language-model", false),
        ("gateway.example.test", "/v4/ai/language-model", false),
        ("api.openai.com", "/v4/ai/language-model", false),
    ] {
        assert_eq!(ai_sdk_gateway(host, path), expected, "{host}{path}");
    }
    assert!(blank(b" \r\n\t"));
    assert!(blank(b""));
    assert!(!blank(b"{}"));
}

#[test]
fn claude_codes_web_search_and_codexs_live_web_search_are_refused() {
    let refusal = refused(MESSAGES, CLAUDE_WEB_SEARCH);
    assert_eq!(refusal.at, "$.tools[0]");
    assert_eq!(
        refusal.construct,
        "tool type web_search_20250305, which the provider runs itself"
    );
    for path in [RESPONSES, CHATGPT] {
        let refusal = refused(path, CODEX_WEB_SEARCH_LIVE);
        assert_eq!(refusal.at, "$.tools[9]");
        assert_eq!(
            refusal.construct,
            "tool type web_search, which the provider runs itself"
        );
    }
}

#[test]
fn the_endpoint_decides_the_format() {
    for (path, dialect) in [
        ("/v1/messages", Some(Dialect::Messages)),
        ("/v1/messages/count_tokens", Some(Dialect::Messages)),
        ("/api/v1/messages", Some(Dialect::Messages)),
        ("/v1/responses", Some(Dialect::Responses)),
        ("/v1/responses/compact", Some(Dialect::Responses)),
        ("/v1/responses/input_tokens", Some(Dialect::Responses)),
        ("/backend-api/codex/responses", Some(Dialect::Responses)),
        ("/v1/chat/completions", Some(Dialect::ChatCompletions)),
        ("/api/v1/chat/completions", Some(Dialect::ChatCompletions)),
        (
            "/v1beta/openai/chat/completions",
            Some(Dialect::ChatCompletions),
        ),
        ("/v1/completions", Some(Dialect::ChatCompletions)),
        (
            "/v1beta/models/gemini-3-pro:generateContent",
            Some(Dialect::GenerateContent),
        ),
        (
            "/v1/models/gemini-3-pro:streamGenerateContent",
            Some(Dialect::GenerateContent),
        ),
        (
            "/v1beta/models/gemini-3-pro:countTokens",
            Some(Dialect::GenerateContent),
        ),
        ("/v1beta/models/:generateContent", None),
        ("/v1beta/models/gemini-3-pro%3AgenerateContent", None),
        ("/v1/embeddings", None),
        ("/v1/files", None),
        ("/v1/messages/batches", None),
        ("/v1/batches", None),
        ("/v1beta/files:register", None),
        ("/backend-api/conversation", None),
    ] {
        assert_eq!(Dialect::for_path(path), dialect, "{path}");
    }
}

#[test]
fn a_body_to_an_unknown_endpoint_is_refused_and_an_empty_one_passes() {
    let refusal = refused("/v1/messages/batches", r#"{"requests":[]}"#);
    assert_eq!(refusal.at, "$");
    assert!(
        refusal.construct.contains("endpoint whose request format"),
        "{refusal:?}"
    );
    refused(
        "/v1/files",
        "--boundary\r\nContent-Disposition: form-data\r\n",
    );
    for body in ["", " \r\n\t"] {
        passes("/v1/files", body);
        passes(MESSAGES, body);
    }
}

#[test]
fn a_body_the_check_cannot_read_is_refused() {
    for body in [
        "not json",
        r#"{"model":"m","tools":[{"type":"web_search_20250305"}],"tools":[]}"#,
        r#"{"model":"m","tools":[]"#,
        "\u{feff}{}",
    ] {
        let refusal = refused(MESSAGES, body);
        assert_eq!(refusal.at, "$", "{body}");
        assert!(
            refusal.construct.contains("cannot read as one JSON value"),
            "{refusal:?}"
        );
    }
    let deep = format!(
        r#"{{"messages":{}{}}}"#,
        "[".repeat(MAX_CHECKED_DEPTH + 1),
        "]".repeat(MAX_CHECKED_DEPTH + 1)
    );
    assert!(refused(MESSAGES, &deep)
        .construct
        .contains("nesting deeper"));
}

#[test]
fn anthropic_tools_pass_only_when_the_client_runs_them() {
    for tool in [
        r#"{"name":"Bash","input_schema":{"type":"object"}}"#,
        r#"{"type":"custom","name":"x","input_schema":{"type":"object"}}"#,
        r#"{"name":"D","input_schema":{"type":"object"},"defer_loading":true}"#,
        r#"{"type":"bash_20250124","name":"bash"}"#,
        r#"{"type":"text_editor_20250728","name":"str_replace_based_edit_tool"}"#,
        r#"{"type":"computer_20251124","name":"computer","display_width_px":1024}"#,
        r#"{"type":"memory_20250818","name":"memory"}"#,
    ] {
        passes(MESSAGES, &format!(r#"{{"model":"m","tools":[{tool}]}}"#));
    }
    for (tool, kind) in [
        (
            r#"{"type":"web_search_20250305","name":"web_search"}"#,
            "web_search_20250305",
        ),
        (
            r#"{"type":"web_search_20260209","name":"web_search"}"#,
            "web_search_20260209",
        ),
        (
            r#"{"type":"web_fetch_20250910","name":"web_fetch"}"#,
            "web_fetch_20250910",
        ),
        (
            r#"{"type":"code_execution_20250825","name":"code_execution"}"#,
            "code_execution_20250825",
        ),
        (
            r#"{"type":"mcp_toolset","mcp_server_name":"s"}"#,
            "mcp_toolset",
        ),
        (
            r#"{"type":"tool_search_tool_regex_20251119","name":"t"}"#,
            "tool_search_tool_regex_20251119",
        ),
        (r#"{"type":"bash_2025","name":"bash"}"#, "bash_2025"),
        (r#"{"type":"Bash_20250124","name":"bash"}"#, "Bash_20250124"),
        (r#"{"type":"something_new","name":"x"}"#, "something_new"),
    ] {
        let refusal = refused(
            MESSAGES,
            &format!(r#"{{"model":"m","tools":[{{"name":"ok","input_schema":{{}}}},{tool}]}}"#),
        );
        assert_eq!(refusal.at, "$.tools[1]", "{tool}");
        assert_eq!(
            refusal.construct,
            format!("tool type {kind}, which the provider runs itself")
        );
    }
    let refusal = refused(MESSAGES, r#"{"tools":[{"type":7}]}"#);
    assert_eq!(refusal.construct, "a tool whose type is not a string");
    let refusal = refused(MESSAGES, r#"{"tools":{"type":"custom"}}"#);
    assert_eq!(refusal.construct, "tools in a form the check does not read");
    passes(MESSAGES, r#"{"tools":null}"#);
}

#[test]
fn anthropic_mcp_servers_and_url_sources_are_refused() {
    let refusal = refused(
        MESSAGES,
        r#"{"model":"m","mcp_servers":[{"type":"url","url":"https://mcp.example","name":"x"}]}"#,
    );
    assert_eq!(refusal.at, "$.mcp_servers");
    assert!(refusal.construct.contains("MCP servers"));
    passes(MESSAGES, r#"{"model":"m","mcp_servers":[]}"#);
    passes(MESSAGES, r#"{"model":"m","mcp_servers":null}"#);

    let image =
        r#"{"type":"image","source":{"type":"url","url":"https://evil.example/x.png?d=secret"}}"#;
    let refusal = refused(
        MESSAGES,
        &format!(
            r#"{{"messages":[{{"role":"user","content":[{{"type":"text","text":"hi"}},{image}]}}]}}"#
        ),
    );
    assert_eq!(refusal.at, "$.messages[0].content[1].source.type");
    assert!(
        refusal.construct.contains("fetches from a URL"),
        "{refusal:?}"
    );
    // Never the URL, nor anything else of the body.
    assert!(!format!("{refusal:?}").contains("evil.example"));
    assert!(!format!("{refusal:?}").contains("secret"));

    // Anywhere: in a tool result, in a document's content, in the system
    // prompt.
    let document =
        r#"{"type":"document","source":{"type":"url","url":"https://evil.example/a.pdf"}}"#;
    let refusal = refused(
        MESSAGES,
        &format!(
            r#"{{"messages":[{{"role":"user","content":[{{"type":"tool_result","tool_use_id":"t","content":[{document}]}}]}}]}}"#
        ),
    );
    assert_eq!(
        refusal.at,
        "$.messages[0].content[0].content[0].source.type"
    );
    let nested = format!(
        r#"{{"messages":[{{"role":"user","content":[{{"type":"document","source":{{"type":"content","content":[{image}]}}}}]}}]}}"#
    );
    assert_eq!(
        refused(MESSAGES, &nested).at,
        "$.messages[0].content[0].source.content[0].source.type"
    );
    assert_eq!(
        refused(MESSAGES, &format!(r#"{{"system":[{image}]}}"#)).at,
        "$.system[0].source.type"
    );
    // A source type the check does not know is refused too.
    let refusal = refused(
        MESSAGES,
        r#"{"messages":[{"role":"user","content":[{"type":"image","source":{"type":"uri","uri":"x"}}]}]}"#,
    );
    assert_eq!(
        refusal.construct,
        "a content source of type uri, which the check does not know"
    );

    // Inline data, files uploaded to the provider and text pass, and so
    // does a search result's source, which names where text came from.
    for block in [
        r#"{"type":"image","source":{"type":"base64","media_type":"image/png","data":"iVBORw0KGgo="}}"#,
        r#"{"type":"document","source":{"type":"file","file_id":"file_1"}}"#,
        r#"{"type":"document","source":{"type":"text","media_type":"text/plain","data":"x"}}"#,
        r#"{"type":"search_result","source":"https://example.com/a","title":"a","content":[{"type":"text","text":"x"}]}"#,
        r#"{"type":"tool_result","tool_use_id":"t","content":[{"type":"tool_reference","tool_name":"WebSearch"}]}"#,
    ] {
        passes(
            MESSAGES,
            &format!(r#"{{"messages":[{{"role":"user","content":[{block}]}}]}}"#),
        );
    }
}

#[test]
fn what_the_provider_does_not_act_on_is_not_read() {
    // A tool call's arguments, as the model wrote them, a schema that names
    // a URL field, and metadata.
    passes(
        MESSAGES,
        r#"{"messages":[{"role":"assistant","content":[{"type":"tool_use","id":"t","name":"x","input":{"tools":[{"type":"web_search_20250305"}],"source":{"type":"url"},"file_url":"https://e.example"}}]}],
            "tools":[{"name":"x","input_schema":{"type":"object","properties":{"file_url":{"type":"string"},"image_url":{"type":"string","default":"https://e.example"}}}}],
            "metadata":{"user_id":"u","file_url":"https://e.example"}}"#,
    );
    passes(
        RESPONSES,
        r#"{"input":[{"type":"local_shell_call","call_id":"c","action":{"type":"exec","command":["true"],"env":{"image_url":"https://e.example"}}}],
            "text":{"format":{"type":"json_schema","name":"n","schema":{"properties":{"file_url":{"type":"string"}}}}},
            "client_metadata":{"file_url":"x"}}"#,
    );
    // At the top of the body, `input` is the request's content, whatever
    // `type` says.
    let refusal = refused(
        RESPONSES,
        r#"{"type":"tool_use","input":[{"role":"user","content":[{"type":"input_image","image_url":"https://e.example/x.png"}]}]}"#,
    );
    assert_eq!(refusal.at, "$.input[0].content[0].image_url");
}

#[test]
fn openai_tools_pass_only_when_the_client_runs_them() {
    for tool in [
        r#"{"type":"function","name":"exec_command","parameters":{"type":"object"}}"#,
        r#"{"type":"custom","name":"apply_patch","format":{"type":"grammar"}}"#,
        r#"{"type":"local_shell"}"#,
        r#"{"type":"apply_patch"}"#,
        r#"{"type":"shell"}"#,
        r#"{"type":"shell","environment":{"type":"local"}}"#,
        r#"{"type":"tool_search","execution":"client","parameters":{"type":"object"}}"#,
    ] {
        passes(
            RESPONSES,
            &format!(r#"{{"model":"gpt-5.5","tools":[{tool}]}}"#),
        );
    }
    for (tool, construct) in [
        (r#"{"type":"web_search"}"#, "tool type web_search, which the provider runs itself"),
        (r#"{"type":"web_search","external_web_access":false}"#, "tool type web_search, which the provider runs itself"),
        (r#"{"type":"web_search_preview"}"#, "tool type web_search_preview, which the provider runs itself"),
        (r#"{"type":"web_search_2025_08_26"}"#, "tool type web_search_2025_08_26, which the provider runs itself"),
        (r#"{"type":"file_search","vector_store_ids":["vs"]}"#, "tool type file_search, which the provider runs itself"),
        (r#"{"type":"code_interpreter","container":{"type":"auto"}}"#, "tool type code_interpreter, which the provider runs itself"),
        (r#"{"type":"image_generation"}"#, "tool type image_generation, which the provider runs itself"),
        (r#"{"type":"computer_use_preview","display_width":1,"display_height":1,"environment":"browser"}"#, "tool type computer_use_preview, which the provider runs itself"),
        (r#"{"type":"mcp","server_label":"x","server_url":"https://mcp.example"}"#, "tool type mcp, which the provider runs itself"),
        (r#"{"type":"mcp","server_label":"x","connector_id":"connector_gmail"}"#, "tool type mcp, which the provider runs itself"),
        (r#"{"type":"tool_search"}"#, "tool type tool_search without execution: client, which the provider runs itself"),
        (r#"{"type":"tool_search","execution":"server"}"#, "tool type tool_search without execution: client, which the provider runs itself"),
        (r#"{"type":"shell","environment":{"type":"container_auto"}}"#, "tool type shell with an environment the provider may host, which the check does not allow"),
        (r#"{"name":"x","parameters":{}}"#, "a tool without a type, which this request format does not run on the client"),
    ] {
        let refusal = refused(RESPONSES, &format!(r#"{{"tools":[{{"type":"function","name":"f"}},{tool}]}}"#));
        assert_eq!(refusal.at, "$.tools[1]", "{tool}");
        assert_eq!(refusal.construct, construct, "{tool}");
    }
    // Tools a tool search loaded, in the history, are checked as tools.
    let refusal = refused(
        RESPONSES,
        r#"{"input":[{"type":"tool_search_output","call_id":"c","tools":[{"type":"mcp","server_label":"x"}]}]}"#,
    );
    assert_eq!(refusal.at, "$.input[0].tools[0]");
}

#[test]
fn a_namespace_passes_when_every_tool_in_it_does() {
    // Codex 0.160.1's request for a code_mode_only model: its tools in
    // namespaces, in an additional_tools input item.
    passes(RESPONSES, CODEX_CODE_MODE);
    passes(CHATGPT, CODEX_CODE_MODE);
    assert!(CODEX_CODE_MODE.contains(r#""type":"additional_tools""#));
    assert!(CODEX_CODE_MODE.contains(r#""type":"namespace","name":"functions""#));
    for body in [
        r#"{"tools":[{"type":"namespace","name":"crm","description":"d","tools":[{"type":"function","name":"get","defer_loading":true,"parameters":{"type":"object"}}]},{"type":"tool_search","execution":"client"}]}"#,
        r#"{"tools":[{"type":"namespace","name":"empty","tools":[]}]}"#,
        r#"{"input":[{"type":"additional_tools","role":"developer","tools":[{"type":"namespace","name":"outer","tools":[{"type":"namespace","name":"inner","tools":[{"type":"custom","name":"exec"}]}]}]}]}"#,
    ] {
        passes(RESPONSES, body);
    }
    for (body, at, construct) in [
        (
            r#"{"tools":[{"type":"namespace","name":"n","tools":[{"type":"function","name":"f"},{"type":"web_search"}]}]}"#,
            "$.tools[0].tools[1]",
            "tool type web_search, which the provider runs itself",
        ),
        (
            r#"{"input":[{"type":"additional_tools","role":"developer","tools":[{"type":"namespace","name":"n","tools":[{"type":"namespace","name":"m","tools":[{"type":"mcp","server_label":"x"}]}]}]}]}"#,
            "$.input[0].tools[0].tools[0].tools[0]",
            "tool type mcp, which the provider runs itself",
        ),
        (
            r#"{"tools":[{"type":"namespace","name":"n"}]}"#,
            "$.tools[0]",
            "a namespace without a list of tools, which the check does not read",
        ),
        (
            r#"{"tools":[{"type":"namespace","name":"n","tools":{"type":"function"}}]}"#,
            "$.tools[0]",
            "a namespace without a list of tools, which the check does not read",
        ),
    ] {
        let refusal = refused(RESPONSES, body);
        assert_eq!(refusal.at, at, "{body}");
        assert_eq!(refusal.construct, construct, "{body}");
    }
    // Only the Responses format has namespaces.
    let refusal = refused(
        "/v1/chat/completions",
        r#"{"tools":[{"type":"namespace","name":"n","tools":[]}]}"#,
    );
    assert_eq!(
        refusal.construct,
        "tool type namespace, which the provider runs itself"
    );
}

#[test]
fn openai_inputs_the_provider_fetches_are_refused() {
    for (item, at) in [
        (
            r#"{"role":"user","content":[{"type":"input_image","image_url":"https://e.example/x.png"}]}"#,
            "$.input[0].content[0].image_url",
        ),
        (
            r#"{"role":"user","content":[{"type":"input_image","image_url":"  HTTP://e.example/x.png"}]}"#,
            "$.input[0].content[0].image_url",
        ),
        (
            r#"{"role":"user","content":[{"type":"input_file","file_url":"https://e.example/a.pdf"}]}"#,
            "$.input[0].content[0].file_url",
        ),
        (
            r#"{"role":"user","content":[{"type":"input_file","filename":"a.pdf","file_data":"https://e.example/a.pdf"}]}"#,
            "$.input[0].content[0].file_data",
        ),
        (
            r#"{"type":"function_call_output","call_id":"c","output":[{"type":"input_image","image_url":"https://e.example/x.png"}]}"#,
            "$.input[0].output[0].image_url",
        ),
        (
            r#"{"type":"computer_call_output","call_id":"c","output":{"type":"computer_screenshot","image_url":"https://e.example/s.png"}}"#,
            "$.input[0].output.image_url",
        ),
    ] {
        let refusal = refused(RESPONSES, &format!(r#"{{"input":[{item}]}}"#));
        assert_eq!(refusal.at, at, "{item}");
        assert!(
            refusal.construct.contains("fetches from a URL"),
            "{refusal:?}"
        );
        assert!(!refusal.construct.contains("e.example"));
    }
    for item in [
        r#"{"role":"user","content":[{"type":"input_image","image_url":"data:image/png;base64,iVBORw0KGgo="}]}"#,
        r#"{"role":"user","content":[{"type":"input_image","image_url":"DATA:image/png;base64,iVBORw0KGgo="}]}"#,
        r#"{"role":"user","content":[{"type":"input_image","file_id":"file_1"}]}"#,
        r#"{"role":"user","content":[{"type":"input_file","filename":"a.pdf","file_data":"data:application/pdf;base64,JVBERi0="}]}"#,
        r#"{"role":"user","content":[{"type":"input_file","filename":"a.pdf","file_data":"JVBERi0xLjQK+/="}]}"#,
        r#"{"role":"user","content":[{"type":"input_file","file_url":null}]}"#,
        r#"{"type":"web_search_call","id":"ws","status":"completed","action":{"type":"open_page","url":"https://e.example"}}"#,
    ] {
        passes(RESPONSES, &format!(r#"{{"input":[{item}]}}"#));
    }
    let refusal = refused(
        RESPONSES,
        r#"{"prompt":{"id":"pmpt_1","version":"2"},"input":"hi"}"#,
    );
    assert_eq!(refusal.at, "$.prompt");
    assert!(refusal.construct.contains("stored prompt"));
    passes("/v1/completions", r#"{"model":"m","prompt":"hi"}"#);
    let refusal = refused(RESPONSES, r#"{"model":"o3-deep-research","input":"hi"}"#);
    assert_eq!(
        refusal.construct,
        "model o3-deep-research: a deep research model, which searches the web"
    );
}

#[test]
fn chat_completions_plugins_web_search_and_online_models_are_refused() {
    // Axocoatl's own OpenRouter request turns the web plugin off explicitly.
    passes(
        CHAT,
        r#"{"model":"qwen/qwen3-coder","messages":[{"role":"user","content":"hi"}],
            "plugins":[{"id":"web","enabled":false},{"id":"context-compression","enabled":false}],
            "tools":[{"type":"function","function":{"name":"f","parameters":{"type":"object"}}}]}"#,
    );
    for (body, at, fragment) in [
        (
            r#"{"plugins":[{"id":"web"}]}"#,
            "$.plugins[0]",
            "plugin web",
        ),
        (
            r#"{"plugins":[{"id":"web","enabled":false},{"id":"file-parser","enabled":true}]}"#,
            "$.plugins[1]",
            "plugin file-parser",
        ),
        (
            r#"{"plugins":{"id":"web"}}"#,
            "$.plugins",
            "plugins in a form",
        ),
        (
            r#"{"web_search_options":{}}"#,
            "$.web_search_options",
            "web search",
        ),
        (
            r#"{"model":"openai/gpt-5.5:online"}"#,
            "$.model",
            "model openai/gpt-5.5:online: an :online model",
        ),
        (
            r#"{"model":"x","models":["y","z:Online"]}"#,
            "$.models[1]",
            "model z:Online",
        ),
        (
            r#"{"model":"gpt-4o-search-preview"}"#,
            "$.model",
            "a search model",
        ),
        (
            r#"{"model":"gpt-5-search-api"}"#,
            "$.model",
            "a search model",
        ),
        (
            r#"{"model":"perplexity/sonar-pro"}"#,
            "$.model",
            "a Perplexity model",
        ),
        (
            r#"{"tools":[{"type":"web_search"}]}"#,
            "$.tools[0]",
            "tool type web_search",
        ),
        (
            r#"{"tools":[{"function":{"name":"f"}}]}"#,
            "$.tools[0]",
            "a tool without a type",
        ),
        (
            r#"{"tools":[{"type":"local_shell"}]}"#,
            "$.tools[0]",
            "tool type local_shell",
        ),
        (
            r#"{"messages":[{"role":"user","content":[{"type":"image_url","image_url":{"url":"https://e.example/x.png"}}]}]}"#,
            "$.messages[0].content[0].image_url",
            "image_url that is not a data: URL",
        ),
        (
            r#"{"messages":[{"role":"user","content":[{"type":"video_url","video_url":{"url":"https://e.example/v.mp4"}}]}]}"#,
            "$.messages[0].content[0].video_url",
            "video_url",
        ),
        (
            r#"{"messages":[{"role":"user","content":[{"type":"file","file":{"filename":"a.pdf","file_data":"https://e.example/a.pdf"}}]}]}"#,
            "$.messages[0].content[0].file.file_data",
            "file_data that is a URL",
        ),
    ] {
        let refusal = refused(CHAT, body);
        assert_eq!(refusal.at, at, "{body}");
        assert!(refusal.construct.contains(fragment), "{body}: {refusal:?}");
    }
    passes(
        CHAT,
        r#"{"messages":[{"role":"user","content":[{"type":"image_url","image_url":{"url":"data:image/png;base64,iVBORw0KGgo=","detail":"auto"}}]}]}"#,
    );
}

#[test]
fn gemini_tools_and_file_uris_are_refused() {
    passes(
        GEMINI,
        r#"{"contents":[{"role":"user","parts":[{"text":"hi"},{"inlineData":{"mimeType":"image/png","data":"iVBORw0KGgo="}},
            {"fileData":{"mimeType":"application/pdf","fileUri":"https://generativelanguage.googleapis.com/v1beta/files/abc"}}]}],
            "tools":[{"functionDeclarations":[{"name":"f","parameters":{"type":"object"}}]}],
            "generationConfig":{"responseSchema":{"properties":{"file_url":{"type":"string"}}}}}"#,
    );
    for (tool, name) in [
        (r#"{"googleSearch":{}}"#, "googleSearch"),
        (r#"{"google_search":{}}"#, "google_search"),
        (r#"{"googleSearchRetrieval":{}}"#, "googleSearchRetrieval"),
        (r#"{"urlContext":{}}"#, "urlContext"),
        (r#"{"codeExecution":{}}"#, "codeExecution"),
        (
            r#"{"functionDeclarations":[],"urlContext":{}}"#,
            "urlContext",
        ),
    ] {
        let refusal = refused(GEMINI, &format!(r#"{{"tools":[{tool}]}}"#));
        assert_eq!(refusal.at, "$.tools[0]", "{tool}");
        assert_eq!(
            refusal.construct,
            format!("tool {name}, which the provider runs itself")
        );
    }
    // A typed tool is no Gemini tool.
    assert_eq!(
        refused(GEMINI, r#"{"tools":[{"type":"function"}]}"#).construct,
        "tool type function, which the provider runs itself"
    );
    for (part, at) in [
        (
            r#"{"fileData":{"fileUri":"https://www.youtube.com/watch?v=x"}}"#,
            "$.contents[0].parts[0].fileData.fileUri",
        ),
        (
            r#"{"file_data":{"file_uri":"gs://bucket/object"}}"#,
            "$.contents[0].parts[0].file_data.file_uri",
        ),
        (
            r#"{"fileData":{"fileUri":"https://generativelanguage.googleapis.com.evil.example/x"}}"#,
            "$.contents[0].parts[0].fileData.fileUri",
        ),
    ] {
        let refusal = refused(
            GEMINI,
            &format!(r#"{{"contents":[{{"role":"user","parts":[{part}]}}]}}"#),
        );
        assert_eq!(refusal.at, at, "{part}");
    }
}

#[test]
fn nothing_hides_under_a_key_in_a_shape_the_provider_would_not_expect() {
    for (body, at) in [
        (
            r#"{"model":{"tools":[{"type":"web_search"}]}}"#,
            "$.model.tools[0]",
        ),
        (
            r#"{"input":[{"image_url":[{"file_url":"https://e.example"}]}]}"#,
            "$.input[0].image_url[0].file_url",
        ),
        (
            r#"{"models":{"mcp_servers":[{"url":"https://e.example"}]}}"#,
            "$.models.mcp_servers",
        ),
    ] {
        assert_eq!(refused(RESPONSES, body).at, at, "{body}");
    }
}

#[test]
fn refusals_show_only_plain_names() {
    assert_eq!(shown("web_search_20250305"), "web_search_20250305");
    assert_eq!(shown("openai/gpt-5.5:online"), "openai/gpt-5.5:online");
    for odd in [
        "",
        "a b",
        "x\"y",
        "https://e.example/?q=secret value",
        &"a".repeat(81),
    ] {
        assert_eq!(shown(odd), "(not shown)", "{odd}");
    }
    let refusal = refused(MESSAGES, r#"{"tools":[{"type":"exfil secret value"}]}"#);
    assert_eq!(
        refusal.construct,
        "tool type (not shown), which the provider runs itself"
    );
    // A key that is not a plain name is not shown in the path either.
    let refusal = refused(
        RESPONSES,
        r#"{"input":[{"secret key":{"image_url":"https://e.example"}}]}"#,
    );
    assert_eq!(refusal.at, "$.input[0][?].image_url");
}

#[test]
fn ollama_cloud_model_names_are_the_ones_ollama_0_20_6_sends_to_ollama_com() {
    // Seen by running the Ollama 0.20.6 binary with its proxy pointed at a
    // recorder: these went to ollama.com, the others stayed local.
    for name in [
        "kimi-k2.5:cloud",
        "gpt-oss:120b-cloud",
        "foo:CLOUD",
        "foo:Cloud",
        "foo:x-cloud",
        "foo:x-Cloud",
        "ns/foo:cloud",
        "evil.example/ns/foo:cloud",
        "registry.ollama.ai/library/foo:cloud",
        "FOO:cloud",
        " foo:cloud",
        "foo:cloud ",
    ] {
        assert!(ollama_cloud_model(name), "{name:?}");
    }
    for name in [
        "foo-cloud",
        "foo:cloudy",
        "foo:cloud-x",
        "qwen3:8b",
        "evil.example/ns/model:tag",
        "host:8080/ns/model",
        "",
    ] {
        assert!(!ollama_cloud_model(name), "{name:?}");
    }
}

#[test]
fn ollama_bodies_are_checked_by_ollamas_endpoints() {
    passes_ollama(
        "/api/chat",
        r#"{"model":"qwen3:8b","messages":[{"role":"user","content":"hi","images":["iVBORw0KGgo="]}],
            "format":{"properties":{"file_url":{"type":"string"}}},
            "tools":[{"type":"function","function":{"name":"f","parameters":{"type":"object"}}}]}"#,
    );
    passes_ollama("/api/tags", "");
    let refusal = check_ollama(
        "/api/chat",
        br#"{"model":"gpt-oss:120b-cloud","messages":[]}"#,
    )
    .unwrap_err();
    assert_eq!(refusal.at, "$.model");
    assert_eq!(
        refusal.construct,
        "model gpt-oss:120b-cloud: a cloud model, which Ollama runs on ollama.com"
    );
    // Ollama's native tools are function tools only.
    let refusal = check_ollama(
        "/api/chat",
        br#"{"model":"qwen3:8b","tools":[{"type":"web_search"}]}"#,
    )
    .unwrap_err();
    assert_eq!(refusal.at, "$.tools[0]");
    // The compatible endpoints get their formats' checks.
    assert!(check_ollama(
        "/v1/messages",
        br#"{"model":"qwen3:8b","tools":[{"type":"web_search_20250305","name":"w"}]}"#
    )
    .is_err());
    // An endpoint the route does not allow has no format here either.
    assert!(check_ollama("/api/pull", br#"{"model":"x"}"#)
        .unwrap_err()
        .construct
        .contains("Ollama endpoint the check does not know"));
    // A web-searching model name means nothing to Ollama; only cloud ones
    // are refused there.
    passes_ollama("/api/chat", r#"{"model":"perplexity/sonar:online"}"#);
}

fn passes_ollama(path: &str, body: &str) {
    if let Err(refused) = check_ollama(path, body.as_bytes()) {
        panic!("{path} refused {body}: {refused:?}");
    }
}

#[test]
fn xai_live_search_and_server_tools_are_refused() {
    const XAI_CHAT: &str = "/v1/chat/completions";
    for body in [
        r#"{"model":"grok-4","messages":[],"search_parameters":{}}"#,
        r#"{"model":"grok-4","messages":[],"search_parameters":{"mode":"on"}}"#,
        r#"{"model":"grok-4","messages":[],"search_parameters":{"mode":"auto","sources":[{"type":"web"}]}}"#,
        r#"{"model":"grok-4","messages":[],"search_parameters":{"mode":"OFF"}}"#,
        r#"{"model":"grok-4","messages":[],"search_parameters":true}"#,
    ] {
        let refusal = refused(XAI_CHAT, body);
        assert_eq!(refusal.at, "$.search_parameters", "{body}");
        assert!(refusal.construct.contains("live search"), "{refusal:?}");
        // The Responses endpoint takes it too.
        assert_eq!(refused(RESPONSES, body).at, "$.search_parameters");
    }
    passes(
        XAI_CHAT,
        r#"{"model":"grok-4","messages":[],"search_parameters":{"mode":"off"}}"#,
    );
    passes(
        XAI_CHAT,
        r#"{"model":"grok-4","messages":[],"search_parameters":null}"#,
    );
    for tool in [
        "web_search",
        "x_search",
        "code_interpreter",
        "code_execution",
        "collections_search",
    ] {
        let refusal = refused(RESPONSES, &format!(r#"{{"tools":[{{"type":"{tool}"}}]}}"#));
        assert_eq!(refusal.at, "$.tools[0]", "{tool}");
    }
    let refusal = refused(
        XAI_CHAT,
        r#"{"messages":[{"role":"user","content":[{"type":"image_url","image_url":{"url":"https://e.example/x.jpg"}}]}]}"#,
    );
    assert_eq!(refusal.at, "$.messages[0].content[0].image_url");
}

#[test]
fn mistral_urls_built_in_tools_and_agents_are_refused() {
    const MISTRAL_CHAT: &str = "/v1/chat/completions";
    for (part, at) in [
        (
            r#"{"type":"document_url","document_url":"https://arxiv.org/pdf/1805.04770"}"#,
            "$.messages[0].content[0].document_url",
        ),
        (
            r#"{"type":"image_url","image_url":"https://e.example/eiffel.jpg"}"#,
            "$.messages[0].content[0].image_url",
        ),
        (
            r#"{"type":"input_audio","input_audio":{"data":"https://e.example/a.mp3","format":"mp3"}}"#,
            "$.messages[0].content[0].input_audio",
        ),
        (
            r#"{"type":"input_audio","input_audio":"https://e.example/a.mp3"}"#,
            "$.messages[0].content[0].input_audio",
        ),
    ] {
        let refusal = refused(
            MISTRAL_CHAT,
            &format!(
                r#"{{"model":"mistral-small-latest","messages":[{{"role":"user","content":[{part}]}}]}}"#
            ),
        );
        assert_eq!(refusal.at, at, "{part}");
        assert!(
            refusal.construct.contains("fetches from a URL"),
            "{refusal:?}"
        );
    }
    for part in [
        r#"{"type":"input_audio","input_audio":"UklGRiQAAABXQVZF"}"#,
        r#"{"type":"input_audio","input_audio":{"data":"UklGRiQAAABXQVZF","format":"wav"}}"#,
        r#"{"type":"image_url","image_url":"data:image/jpeg;base64,/9j/4AAQ"}"#,
        r#"{"type":"file","file_id":"f-1"}"#,
    ] {
        passes(
            MISTRAL_CHAT,
            &format!(r#"{{"model":"m","messages":[{{"role":"user","content":[{part}]}}]}}"#),
        );
    }
    for (tool, kind) in [
        (r#"{"type":"image_generation"}"#, "image_generation"),
        (
            r#"{"type":"document_library","library_ids":["l"]}"#,
            "document_library",
        ),
        (
            r#"{"type":"connector","connector_id":"my_deepwiki"}"#,
            "connector",
        ),
        (r#"{"type":"web_search_premium"}"#, "web_search_premium"),
    ] {
        let refusal = refused(MISTRAL_CHAT, &format!(r#"{{"tools":[{tool}]}}"#));
        assert_eq!(
            refusal.construct,
            format!("tool type {kind}, which the provider runs itself")
        );
    }
    // Agents and conversations run tools an agent stored with Mistral
    // carries, which the body names only by its id.
    for path in [
        "/v1/agents/completions",
        "/v1/agents",
        "/v1/conversations",
        "/v1/conversations/c-1",
        "/v1/conversations/c-1/restart",
    ] {
        let refusal = refused(path, r#"{"agent_id":"ag-1","inputs":"hi"}"#);
        assert_eq!(refusal.at, "$", "{path}");
        assert!(
            refusal
                .construct
                .contains("agents or conversations endpoint"),
            "{refusal:?}"
        );
    }
    // Fill-in-the-middle is a completion like any other.
    passes(
        "/v1/fim/completions",
        r#"{"model":"codestral-latest","prompt":"def f(","suffix":"return 1"}"#,
    );
}

#[test]
fn vercel_ai_gateway_server_tools_and_provider_options_are_refused() {
    const GATEWAY_CHAT: &str = "/v1/chat/completions";
    for kind in [
        "vercel:perplexity_search",
        "vercel:exa_search",
        "vercel:parallel_search",
        "vercel:tako_search",
        "vercel:browserbase_search",
        "vercel:browserbase_fetch",
    ] {
        let refusal = refused(
            GATEWAY_CHAT,
            &format!(
                r#"{{"model":"openai/gpt-6-astra","messages":[],"tools":[{{"type":"{kind}","config":{{"query":"q"}}}}]}}"#
            ),
        );
        assert_eq!(refusal.at, "$.tools[0]", "{kind}");
        assert_eq!(
            refusal.construct,
            format!("tool type {kind}, which the provider runs itself")
        );
    }
    // The gateway's own routing options pass; anything passed through to
    // the provider does not.
    passes(
        GATEWAY_CHAT,
        r#"{"model":"anthropic/claude-opus-5","messages":[],"models":["openai/gpt-6-astra"],
            "provider":{"sort":"tps"},
            "providerOptions":{"gateway":{"order":["vertex","anthropic"],"models":["google/gemini-3.6-flash"],"caching":"auto"}}}"#,
    );
    let refusal = refused(
        GATEWAY_CHAT,
        r#"{"model":"anthropic/claude-sonnet-5","messages":[],"providerOptions":{"gateway":{"order":["anthropic"]},"anthropic":{"thinking":{"type":"adaptive"}}}}"#,
    );
    assert_eq!(refusal.at, "$.providerOptions.anthropic");
    assert!(
        refusal.construct.contains("passed through to anthropic"),
        "{refusal:?}"
    );
    assert_eq!(
        refused(
            MESSAGES,
            r#"{"providerOptions":{"google":{"useSearchGrounding":true}}}"#
        )
        .at,
        "$.providerOptions.google"
    );
    // Fallback models that search are refused where they are named.
    assert_eq!(
        refused(
            GATEWAY_CHAT,
            r#"{"model":"m","providerOptions":{"gateway":{"models":["perplexity/sonar-pro"]}}}"#
        )
        .at,
        "$.providerOptions.gateway.models[0]"
    );
    for model in [
        "exa/exa-research",
        "parallel/search",
        "browserbase/fetch",
        "tako/x",
    ] {
        let refusal = refused(GATEWAY_CHAT, &format!(r#"{{"model":"{model}"}}"#));
        assert!(
            refusal.construct.contains("search or fetch provider"),
            "{model}"
        );
    }
    // The AI SDK's own gateway protocol is not a format the check knows.
    assert!(refused("/v1/ai/language-model", r#"{"prompt":[]}"#)
        .construct
        .contains("endpoint whose request format"));
}
