use super::*;
use axocoatl_core::ChatMessage;
use serde_json::json;

fn request() -> ChatRequest {
    let mut request = ChatRequest::simple("Fix the paging bug in src/paginate.js");
    for (name, properties, required) in [
        (
            "glob",
            json!({"pattern":{"type":"string"}}),
            json!(["pattern"]),
        ),
        (
            "read_file",
            json!({"path":{"type":"string"},"offset":{"type":"integer"},"limit":{"type":"integer"}}),
            json!(["path"]),
        ),
        (
            "bash",
            json!({"command":{"type":"string"}}),
            json!(["command"]),
        ),
    ] {
        request.tools.push(ToolDefinition {
            name: name.into(),
            description: String::new(),
            parameters: json!({"type":"object","properties":properties,"required":required}),
            concurrency: Default::default(),
        });
    }
    request
}

const GLOB: &str = "<function=glob>\n<parameter=pattern>\nsrc/*.js\n</parameter>\n</function>";

fn read(content: &str) -> TextCalls {
    read_text_calls(content, &request(), "ollama", true)
}

fn recovered(content: &str) -> (usize, Vec<ToolCall>) {
    match read(content) {
        TextCalls::Calls { answer_end, calls } => (answer_end, calls),
        other => panic!("{content:?}: {other:?}"),
    }
}

fn unreadable(content: &str) -> String {
    match read(content) {
        TextCalls::Unreadable(why) => why,
        other => panic!("{content:?}: {other:?}"),
    }
}

fn answer(content: &str) {
    assert!(
        matches!(read(content), TextCalls::Answer),
        "{content:?}: {:?}",
        read(content)
    );
}

/// The whole streamed text a [`TextHold`] shows for `pieces`, piece by
/// piece, before the response ends.
fn shown(pieces: &[&str]) -> Vec<String> {
    let mut hold = TextHold::default();
    let mut content = String::new();
    pieces
        .iter()
        .map(|piece| {
            content.push_str(piece);
            hold.release(&content).unwrap_or_default()
        })
        .collect()
}

#[test]
fn a_call_mentioned_inside_a_line_streams_without_waiting_for_the_end() {
    // A mid-answer mention used to hold all later text until the end.
    let shown = shown(&[
        "I'll use `<function=glob>` and <tool_call> later.\n",
        "First the plan.\n",
        "Then ",
    ]);
    assert_eq!(
        shown,
        [
            "I'll use `<function=glob>` and <tool_call> later.\n",
            "First the plan.\n",
            "Then "
        ]
    );
}

#[test]
fn a_line_that_starts_a_call_is_held_from_its_first_byte() {
    let text = format!("Looking.\n{GLOB}\n</tool_call>");
    for at in 1..text.len() {
        let shown: String = shown(&[&text[..at], &text[at..]]).concat();
        assert_eq!(shown, "Looking.\n", "split at {at}");
    }
    // A fenced or indented call line is text, shown as it comes.
    let fenced = format!("Example:\n```\n{GLOB}\n```\nDone.\n");
    assert_eq!(shown(&[&fenced]).concat(), fenced);
    let indented = "    <function=glob>\n";
    assert_eq!(shown(&[indented]).concat(), indented);
    // A line still short of telling is held until it does.
    assert_eq!(
        shown(&["Plan.\n<tool", "s are listed\n"]),
        ["Plan.\n", "<tools are listed\n"]
    );
}

#[test]
fn a_lone_cr_ends_a_line_as_markdown_ends_it() {
    // The fence behind a lone CR quotes the call.
    answer(&format!("Here is how a call looks:\r```\r\n{GLOB}"));
    let (end, calls) = recovered(&format!("Done.\r{GLOB}"));
    assert_eq!(end, "Done.\r".len());
    assert_eq!(calls.len(), 1);
    assert_eq!(shown(&["Done.\r", "<function="]).concat(), "Done.\r");
}

#[test]
fn an_open_html_block_quotes_the_calls_after_it() {
    for opener in [
        "<pre>",
        "<PRE class=\"x\">",
        "<script>",
        "<style>",
        "<textarea>",
        "<!-- an example:",
        "<?php",
        "<!DOCTYPE html",
        "<![CDATA[",
    ] {
        answer(&format!("Example:\n{opener}\n{GLOB}"));
        assert_eq!(
            shown(&[&format!("Example:\n{opener}\n{GLOB}\n")]).concat(),
            format!("Example:\n{opener}\n{GLOB}\n")
        );
    }
    // A block closed before the call does not quote it.
    for closed in ["<!-- note -->", "<pre>\nx\n</pre>", "<!--\nnote\n-->"] {
        let (_, calls) = recovered(&format!("Example:\n{closed}\n{GLOB}"));
        assert_eq!(calls[0].name, "glob", "{closed}");
    }
}

#[test]
fn a_tool_call_opener_ending_the_prose_is_not_left_in_the_answer() {
    let content = format!("Prose. <tool_call>\n{GLOB}\n</tool_call>");
    let (end, calls) = recovered(&content);
    assert_eq!(&content[..end], "Prose. ");
    assert_eq!(calls.len(), 1);
    let content = format!("I will use the <tool_call> format.\n\n{GLOB}");
    let (end, _) = recovered(&content);
    assert_eq!(&content[..end], "I will use the <tool_call> format.\n\n");
    // Streamed, the opener is held until the next line shows what it opens.
    assert_eq!(
        shown(&["Prose. <tool_call>\n", "\n", GLOB]),
        ["Prose. ", "", ""]
    );
    assert_eq!(
        shown(&["Prose. <tool_call>\n", "More", " text\n"]),
        ["Prose. ", "<tool_call>\nMore", " text\n"]
    );
}

#[test]
fn nothing_is_read_after_a_callers_stop_sequence_or_for_structured_output() {
    let content = format!("Plan.\n{GLOB}");
    let mut stop = request();
    stop.stop_sequences = vec!["</function>".into()];
    assert!(matches!(
        read_text_calls(&content, &stop, "ollama", true),
        TextCalls::Answer
    ));
    let mut json = request();
    json.response_format = Some(ResponseFormat::Json);
    assert!(matches!(
        read_text_calls(&content, &json, "ollama", true),
        TextCalls::Answer
    ));
    assert!(!reads_text_calls(&stop) && !reads_text_calls(&json));
    assert!(!reads_text_calls(&ChatRequest::simple("no tools")));
}

#[test]
fn recovered_ids_differ_between_responses_even_when_history_dropped_a_round() {
    let content = format!("Plan.\n{GLOB}");
    let (_, first) = recovered(&content);
    // The history the next request is sent with no longer holds the first
    // call (the actor dropped its round to fit), but the Session's does.
    let (_, second) = recovered(&content);
    assert_ne!(first[0].id, second[0].id);
    for id in [&first[0].id, &second[0].id] {
        let nonce = id
            .strip_prefix("call_text_")
            .and_then(|rest| rest.strip_suffix("_0"))
            .unwrap_or_default();
        assert!(
            nonce.len() == 16 && nonce.bytes().all(|b| b.is_ascii_hexdigit()),
            "{id}"
        );
    }
    // An id the history already holds is never reused.
    let taken = first[0].id.clone();
    let mut with_history = request();
    let mut assistant = ChatMessage::assistant("");
    assistant.tool_calls = first;
    with_history.messages.push(assistant);
    with_history
        .messages
        .push(ChatMessage::tool_result("[]", "glob", taken.as_str()));
    let two = format!("Plan.\n{GLOB}\n{GLOB}");
    let TextCalls::Calls { calls, .. } = read_text_calls(&two, &with_history, "ollama", true)
    else {
        panic!("not recovered");
    };
    assert!(calls.iter().all(|call| call.id != taken));
    assert_ne!(calls[0].id, calls[1].id);
}

#[test]
fn a_call_that_cannot_run_says_why() {
    for (content, says) in [
        (
            format!("{GLOB}\nAfter that I will read the file."),
            "text follows the call markup",
        ),
        (
            "<function=glob>\n<parameter=pattern>\nsrc\n</function>".to_owned(),
            "not well formed",
        ),
        (
            "<function=bash>\n<parameter=command>\nls\n</parameter>\n<parameter=command>\nx\n\
             </parameter>\n</function>"
                .to_owned(),
            "not well formed",
        ),
        (
            format!("Run.\n<function=delete_all>\n<parameter=path>\n/\n</parameter>\n</function>\n{GLOB}"),
            "it also calls delete_all, which this turn does not offer",
        ),
        (
            "<function=read_file>\n<parameter=path>\na.js\n</parameter>\n<parameter=limit>\nlots\n\
             </parameter>\n</function>"
                .to_owned(),
            "read_file gives limit a value that is not integer",
        ),
        (
            "<function=read_file>\n<parameter=limit>\n10\n</parameter>\n</function>".to_owned(),
            "read_file misses its required parameter path",
        ),
        (
            "<tool_call>\n{\"name\": \"glob\", \"arguments\": {\"pattern\": \"*\"}}\n</tool_call>"
                .to_owned(),
            "written as JSON",
        ),
        (
            r#"{"name": "bash", "parameters": {"command": "ls"}}"#.to_owned(),
            "call to bash as JSON",
        ),
        (
            "I'll list it:\n{\"bash\": {\"command\": \"ls\"}}\n".to_owned(),
            "call to bash as JSON",
        ),
        (GLOB.repeat(129), "more than 128 calls"),
    ] {
        let why = unreadable(&content);
        assert!(why.contains(says), "{content:?}: {why}");
    }
    let cut = match read_text_calls(&format!("Plan.\n{GLOB}"), &request(), "ollama", false) {
        TextCalls::Unreadable(why) => why,
        other => panic!("{other:?}"),
    };
    assert!(cut.contains("done_reason \"length\""), "{cut}");
}

#[test]
fn prose_quotes_and_unoffered_tools_stay_the_answer() {
    for content in [
        "Qwen3-Coder writes <function=glob> when it calls a tool as text.".to_owned(),
        format!("Example:\n```\n{GLOB}\n```\nThat is how it looks."),
        format!("Example:\n```\nnpm test\n```\u{a0}\n{GLOB}"),
        "<function=delete_all>\n<parameter=path>\n/\n</parameter>\n</function>".to_owned(),
        "<function=x>y</function>".repeat(1025),
        r#"Run {"name":"bash","arguments":{"command":"rm -rf build"}} to clean."#.to_owned(),
        "```json\n{\"name\":\"bash\",\"arguments\":{\"command\":\"ls\"}}\n```".to_owned(),
        r#"{"name": "unoffered", "arguments": {}}"#.to_owned(),
        r#"{"status": "ok", "files": 3}"#.to_owned(),
    ] {
        answer(&content);
    }
}

#[test]
fn text_values_are_typed_only_as_their_schema_declares() {
    let schema = json!({"type":"object","properties":{
        "limit":{"type":"integer"}, "ratio":{"type":"number"}, "all":{"type":"boolean"},
        "count":{"type":["integer","null"]}, "path":{"type":"string"}, "loose":{}}});
    let mut arguments = json!({"limit":"100", "ratio":"1.5", "all":"True", "count":" 7 ",
        "path":"100", "loose":"true", "other":"false"});
    coerce_text_arguments(&mut arguments, &schema);
    assert_eq!(
        arguments,
        json!({"limit":100, "ratio":1.5, "all":true, "count":7, "path":"100", "loose":"true",
            "other":"false"})
    );
    let schema_json = json!({"type":"object","properties":{
        "paths":{"type":"array"}, "env":{"type":"object"}, "count":{"type":["integer","null"]},
        "name":{"type":"string"}}});
    let mut json_values = json!({"paths":"[\"a\",\"b\"]", "env":"{\"K\":\"v\"}",
        "count":"null", "name":"null"});
    coerce_text_arguments(&mut json_values, &schema_json);
    assert_eq!(
        json_values,
        json!({"paths":["a","b"], "env":{"K":"v"}, "count":null, "name":"null"})
    );
    let mut unparsed = json!({"limit":"lots", "ratio":"NaN", "all":"yes"});
    coerce_text_arguments(&mut unparsed, &schema);
    assert_eq!(
        unparsed,
        json!({"limit":"lots", "ratio":"NaN", "all":"yes"})
    );
}

#[test]
fn numbers_are_typed_only_from_exact_json_literals() {
    let integer = ["integer"];
    let number = ["number"];
    // The whole u64 range, exactly.
    assert_eq!(
        typed_text("9999999999999999999", &integer),
        Some(json!(9_999_999_999_999_999_999u64))
    );
    assert_eq!(
        typed_text("-9223372036854775808", &integer),
        Some(json!(i64::MIN))
    );
    // Beyond 64 bits an integer stays text rather than lose digits as 1e20.
    assert_eq!(typed_text("99999999999999999999", &integer), None);
    assert_eq!(typed_text("99999999999999999999", &number), None);
    // `0755` is not 755, and neither `+7` nor `0x10` is a JSON number.
    for text in [
        "007", "0755", "+7", "0x10", "1_000", "", "-", "1.", ".5", "1e", "NaN", "inf",
    ] {
        assert_eq!(typed_text(text, &number), None, "{text:?}");
    }
    assert_eq!(typed_text("0", &integer), Some(json!(0)));
    assert_eq!(typed_text("-0", &integer), Some(json!(0)));
    assert_eq!(typed_text("1.5", &integer), None);
    assert_eq!(typed_text("1.5", &number), Some(json!(1.5)));
    assert_eq!(typed_text("2.5e3", &number), Some(json!(2500.0)));
    assert_eq!(typed_text("1e400", &number), None);
}

#[test]
fn recovered_values_must_fit_the_tools_parameters() {
    let tool = |parameters: Value| ToolDefinition {
        name: "t".into(),
        description: String::new(),
        parameters,
        concurrency: Default::default(),
    };
    let strict = tool(
        json!({"type":"object","properties":{"mode":{"type":"string",
        "enum":["fast","slow"]}},"required":["mode"],"additionalProperties":false}),
    );
    assert!(fits_parameters(&json!({"mode":"fast"}), &strict).is_ok());
    assert_eq!(
        fits_parameters(&json!({"mode":"other"}), &strict).unwrap_err(),
        "gives mode a value its schema does not allow"
    );
    assert_eq!(
        fits_parameters(&json!({"mode":"fast","x":"1"}), &strict).unwrap_err(),
        "has a parameter x it does not declare"
    );
    assert_eq!(
        fits_parameters(&json!({}), &strict).unwrap_err(),
        "misses its required parameter mode"
    );
    let loose = tool(json!({"type":"object","properties":{"n":{"type":["integer","null"]}}}));
    assert!(fits_parameters(&json!({"n":null,"other":"kept"}), &loose).is_ok());
    assert_eq!(
        fits_parameters(&json!({"n":1.5}), &loose).unwrap_err(),
        "gives n a value that is not integer or null"
    );
}

/// A call block written after other text on its line is not a call that
/// begins a line, so it never runs; it is refused rather than left as an
/// answer that ran nothing.
#[test]
fn a_call_block_after_other_text_on_its_line_is_refused() {
    for content in [
        format!("Looking.{GLOB}"),
        format!("Looking. {GLOB}\n</tool_call>"),
        format!("Looking. <tool_call>{GLOB}\n</tool_call>"),
        format!("Looking.\n\n {GLOB}"),
        format!("Looking.\n  <tool_call>{GLOB}"),
        format!("Looking.{GLOB}\nThen I will read it."),
        "Looking.<function=glob>\n<parameter=pattern>\nsrc\n</function>".to_owned(),
        "Looking.<function=glob></function>".to_owned(),
        // A run that begins a line does not run without the call before it.
        format!(
            "Looking.{GLOB}\n<function=bash>\n<parameter=command>\nls\n</parameter>\n</function>"
        ),
    ] {
        let why = unreadable(&content);
        assert!(
            why.starts_with("the model wrote a call to glob as answer text")
                && why.contains("does not begin a line"),
            "{content:?}: {why}"
        );
    }
    let cut = match read_text_calls(
        "Looking.<function=glob>\n<parameter=pattern>\nsrc",
        &request(),
        "ollama",
        false,
    ) {
        TextCalls::Unreadable(why) => why,
        other => panic!("{other:?}"),
    };
    assert!(cut.contains("done_reason \"length\""), "{cut}");
}

/// A call block quoted in code, a mention of the tag in prose and a block
/// that names only a tool the turn does not offer stay the answer.
#[test]
fn quoted_or_mentioned_call_blocks_after_other_text_stay_the_answer() {
    for content in [
        "The <function=glob> tag opens a call, and </function> closes it.".to_owned(),
        "Qwen writes `<function=glob></function>` for a call with no parameters.".to_owned(),
        // A code span may run over the lines of its paragraph.
        "Use ``<function=glob>\n</function>`` for that.".to_owned(),
        format!("Example:\n\n    Run: {GLOB}"),
        format!("Example:\n```\nRun: {GLOB}\n```"),
        format!("Example:\n<pre>\nRun: {GLOB}"),
        "Looking.<function=delete_all>\n<parameter=path>\n/\n</parameter>\n</function>".to_owned(),
        // A call block in a value of a run that begins a line is that value.
        "Plan.\n<function=bash>\n<parameter=command>\necho 'Run: <function=glob></function>'\n\
         </parameter>\n</function>"
            .to_owned(),
    ] {
        if content.starts_with("Plan.") {
            let (_, calls) = recovered(&content);
            assert_eq!(calls[0].name, "bash");
        } else {
            answer(&content);
        }
    }
}

/// JSON never runs. A JSON call to an offered tool that ends the answer (the
/// whole answer, its last lines or a `<tool_call>` block, arguments as an
/// object or as a string holding one) is refused; JSON in a fence or with
/// text after it stays the answer.
#[test]
fn a_json_call_ending_the_answer_is_refused_and_other_json_stays_text() {
    for content in [
        r#"{"name": "bash", "arguments": "{\"command\": \"ls\"}"}"#.to_owned(),
        "Listing it. <tool_call>{\"name\": \"bash\", \"arguments\": {\"command\": \"ls\"}}\
         </tool_call>"
            .to_owned(),
        "Listing it.\n  <tool_call>\n{\"name\": \"bash\", \"arguments\": {\"command\": \"ls\"}}\n"
            .to_owned(),
    ] {
        let why = unreadable(&content);
        assert!(why.contains("call to bash as JSON"), "{content:?}: {why}");
    }
    for content in [
        "I'll run:\n{\"name\": \"bash\", \"arguments\": {\"command\": \"ls\"}}\nThen check.",
        "Hermes: <tool_call>{\"name\": \"bash\", \"arguments\": {\"command\": \"ls\"}}</tool_call> \
         is the format.",
        "Listing it. <tool_call>{\"name\": \"other\", \"arguments\": {}}</tool_call>",
        r#"{"name": "bash", "arguments": "ls"}"#,
    ] {
        answer(content);
    }
}

/// The text a [`TextHold`] has shown after each of `pieces`, then the
/// answer's own text once the response ends: how finish ends a stream.
fn shown_then_finished(pieces: &[&str]) -> (String, String) {
    let mut hold = TextHold::default();
    let mut content = String::new();
    let mut shown = String::new();
    for piece in pieces {
        content.push_str(piece);
        shown.push_str(&hold.release(&content).unwrap_or_default());
    }
    let end = match read(&content) {
        TextCalls::Calls { answer_end, .. } => answer_end,
        _ => content.len(),
    };
    let before = shown.clone();
    shown.push_str(&hold.release_to(&content, end).unwrap_or_default());
    (before, shown)
}

/// A `<tool_call>` opener that ends the prose arrives token by token, its
/// newline at the start of the next token as Ollama streams it: it is held
/// until its line ends, so it never shows while the call runs. Whatever the
/// split, the text shown is the answer's own text and never call markup.
#[test]
fn a_tool_call_opener_split_into_tokens_is_held_until_its_line_ends() {
    let tokens = [
        "Prose",
        ".",
        " <",
        "tool",
        "_call",
        ">",
        "\n\n<function",
        "=glob",
        ">\n",
        "<parameter=pattern>\n",
        "src/*.js\n",
        "</parameter>\n",
        "</function>\n",
        "</tool_call>",
    ];
    let content = tokens.concat();
    let (before, shown) = shown_then_finished(&tokens);
    assert_eq!((before.as_str(), shown.as_str()), ("Prose. ", "Prose. "));
    let answers = [
        content.clone(),
        format!("Prose <tool_call>  \n{GLOB}"),
        format!("I compare a < b.\nThen <tool_call>\n\n{GLOB}\n</tool_call>"),
        format!("Looking.\n{GLOB}"),
        "Ends with a <tool".to_owned(),
        "Ends with a <tool_call>".to_owned(),
        "x < y and <tool_call> is a tag\n".to_owned(),
    ];
    for content in &answers {
        let whole = match read(content) {
            TextCalls::Calls { answer_end, .. } => &content[..answer_end],
            _ => content.as_str(),
        };
        let mut splits: Vec<Vec<&str>> = (1..content.len())
            .filter(|at| content.is_char_boundary(*at))
            .map(|at| vec![&content[..at], &content[at..]])
            .collect();
        splits.push(
            content
                .char_indices()
                .map(|(at, c)| &content[at..at + c.len_utf8()])
                .collect(),
        );
        for pieces in splits {
            let (before, shown) = shown_then_finished(&pieces);
            assert!(whole.starts_with(&before), "{pieces:?}: {before:?}");
            assert_eq!(shown, whole, "{pieces:?}");
        }
    }
}

/// What qwen3-coder:30b ended a fix with on Ollama 0.40.8 (the 1.5.0
/// measurement's m4-rust writer): prose, then `<tool_call>` and nothing
/// after it. Ollama returns a call tag it is still collecting as content
/// when the response ends. That answer began a call and wrote none of it,
/// so it is refused instead of becoming a final answer that changed
/// nothing; a call tag alone on the last line, `<function=` with nothing
/// after it, and one cut at the output limit are too. A tag quoted or
/// mentioned before the end stays the answer.
#[test]
fn an_answer_that_ends_with_a_bare_call_tag_is_refused() {
    let measured = "This logic seems right, but let me create a test to verify what happens when \
                    we have a case that might be problematic.<tool_call>";
    assert_eq!(
        unreadable(measured),
        "the model wrote a call to a tool as answer text that Axocoatl cannot run: its answer \
         ends with <tool_call> and nothing after it, so the call was never written"
    );
    for content in [
        "Let me write the test.\n\n<tool_call>\n",
        "Let me write the test.\n<function=",
        "Let me write the test. <tool_call>  \n\n",
    ] {
        assert!(
            unreadable(content).ends_with("nothing after it, so the call was never written"),
            "{content:?}"
        );
    }
    match read_text_calls(measured, &request(), "ollama", false) {
        TextCalls::Unreadable(why) => assert!(why.ends_with(CUT_CALL), "{why}"),
        other => panic!("{other:?}"),
    }
    for content in [
        "Wrap each call in `<tool_call>`",
        "A call opens with <tool_call> and closes with </tool_call>.",
        "Example:\n```\n<tool_call>\n```",
    ] {
        answer(content);
    }
    // Without tools offered no text is read as a call.
    let mut final_answer = request();
    final_answer.tools.clear();
    assert!(matches!(
        read_text_calls(measured, &final_answer, "ollama", true),
        TextCalls::Answer
    ));
}
