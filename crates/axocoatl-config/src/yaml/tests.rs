//! The YAML 1.1 versus 1.2 decisions, one by one, and the writer's round
//! trip. `tests/yaml_differential.rs` checks all of this, and every YAML
//! document in the repository, against serde_yaml 0.9's recorded results.

use std::collections::{BTreeMap, HashMap};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value as Json};

use super::{from_str, to_string, Number, Value};

fn json(text: &str) -> Json {
    from_str::<Json>(text).unwrap_or_else(|error| panic!("{text:?}: {error}"))
}

fn error<T: serde::de::DeserializeOwned + std::fmt::Debug>(text: &str) -> String {
    from_str::<T>(text)
        .map(|value| panic!("{text:?} read as {value:?}"))
        .unwrap_err()
        .to_string()
}

#[derive(Debug, Default, PartialEq, Deserialize)]
#[serde(default)]
struct Fields {
    b: Option<bool>,
    u: Option<u64>,
    i: Option<i64>,
    f: Option<f64>,
    s: Option<String>,
    text: String,
    list: Vec<String>,
    map: BTreeMap<String, String>,
}

fn fields(text: &str) -> Fields {
    from_str(text).unwrap_or_else(|error| panic!("{text:?}: {error}"))
}

#[test]
fn only_the_yaml_1_2_spellings_are_booleans() {
    for (text, value) in [
        ("true", true),
        ("True", true),
        ("TRUE", true),
        ("false", false),
        ("False", false),
        ("FALSE", false),
    ] {
        assert_eq!(json(&format!("a: {text}")), json!({ "a": value }));
        assert_eq!(fields(&format!("b: {text}")).b, Some(value));
    }
    // YAML 1.1's yes/no/on/off and odd capitalisations are strings: in a
    // boolean field they are an error, not a guess.
    for text in [
        "yes", "no", "on", "off", "y", "n", "Yes", "NO", "On", "tRue", "fAlse",
    ] {
        assert_eq!(json(&format!("a: {text}")), json!({ "a": text }));
        assert_eq!(fields(&format!("s: {text}")).s.as_deref(), Some(text));
        assert!(
            error::<Fields>(&format!("b: {text}")).contains("expected a boolean"),
            "{text}"
        );
    }
    // A quoted boolean is a string.
    assert!(error::<Fields>("b: 'true'").contains("expected a boolean"));
}

#[test]
fn integers_follow_yaml_1_2_without_legacy_octal_or_underscores() {
    for (text, value) in [
        ("12", json!(12)),
        ("+12", json!(12)),
        ("-12", json!(-12)),
        ("0", json!(0)),
        ("-0", json!(0)),
        ("0o17", json!(15)),
        ("0x1F", json!(31)),
        ("-0x1F", json!(-31)),
        ("0b101", json!(5)),
        ("18446744073709551615", json!(u64::MAX)),
    ] {
        assert_eq!(json(&format!("a: {text}")), json!({ "a": value }), "{text}");
    }
    assert_eq!(fields("u: 0o17").u, Some(15));
    assert_eq!(fields("i: -0x10").i, Some(-16));
    // A leading zero (YAML 1.1 octal), underscores and sexagesimal numbers
    // are strings, in YAML 1.2 and as serde_yaml read them.
    for text in ["017", "00", "08", "-017", "1_000", "0x_1F", "190:20:30"] {
        assert_eq!(json(&format!("a: {text}")), json!({ "a": text }), "{text}");
        assert!(
            error::<Fields>(&format!("u: {text}")).contains("expected u64"),
            "{text}"
        );
    }
    // A quoted number is a string, so not a number.
    assert!(error::<Fields>("u: '5'").contains("invalid type: string \"5\", expected u64"));
    assert_eq!(fields("s: 017").s.as_deref(), Some("017"));
}

#[test]
fn floats_and_their_special_values() {
    assert_eq!(json("a: 1.5"), json!({"a": 1.5}));
    assert_eq!(json("a: 1e3"), json!({"a": 1000.0}));
    assert_eq!(json("a: .5"), json!({"a": 0.5}));
    assert_eq!(fields("f: .inf").f, Some(f64::INFINITY));
    assert_eq!(fields("f: -.Inf").f, Some(f64::NEG_INFINITY));
    assert!(fields("f: .NaN").f.unwrap().is_nan());
    assert_eq!(fields("f: 5").f, Some(5.0));
    // Too large for a float: a string, as serde_yaml read it.
    assert_eq!(json("a: 1e400"), json!({"a": "1e400"}));
}

#[test]
fn null_and_empty_values() {
    for text in ["~", "null", "Null", "NULL", ""] {
        assert_eq!(
            json(&format!("a: {text}")),
            json!({ "a": null }),
            "{text:?}"
        );
        assert_eq!(fields(&format!("s: {text}")).s, None, "{text:?}");
    }
    assert_eq!(json("a: nULL"), json!({"a": "nULL"}));
    // Read into a string a null is its text, and an empty value the empty
    // string: an unset `${VARIABLE}` leaves `key:` and reads as "".
    assert_eq!(fields("text: ~").text, "~");
    assert_eq!(fields("text: null").text, "null");
    assert_eq!(fields("text:").text, "");
    assert_eq!(fields("text:\n").text, "");
    assert_eq!(
        fields("list: [a, ~, 1, true]").list,
        ["a", "~", "1", "true"]
    );
    // An empty value is an empty list or map.
    assert!(fields("list:").list.is_empty());
    assert!(fields("map:").map.is_empty());
    assert!(error::<Fields>("list: ~").contains("expected a sequence"));
    // An empty document is null, or a struct whose fields all default.
    assert_eq!(json(""), Json::Null);
    assert_eq!(json("# only a comment\n"), Json::Null);
    assert_eq!(fields(""), Fields::default());
    assert_eq!(from_str::<Option<Fields>>("").unwrap(), None);
}

#[test]
fn tags_resolve_as_serde_yaml_did() {
    assert_eq!(json("a: !!str 017"), json!({"a": "017"}));
    assert_eq!(json("a: !!str true"), json!({"a": "true"}));
    assert_eq!(json("a: !!int '3'"), json!({"a": 3}));
    assert_eq!(json("a: !!float '1.5'"), json!({"a": 1.5}));
    assert_eq!(json("a: !!null ~"), json!({"a": null}));
    assert_eq!(
        fields("s: !!binary aGVsbG8=").s.as_deref(),
        Some("aGVsbG8=")
    );
    // A local tag names an enum variant, which a JSON value cannot hold.
    assert!(error::<Json>("a: !custom x").contains("invalid type: enum"));
    assert_eq!(
        from_str::<Value>("a: !custom x").unwrap().get("a"),
        Some(&Value::Tagged(
            "!custom".into(),
            Box::new(Value::String("x".into()))
        ))
    );
}

#[derive(Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Policy {
    Warn,
    HardStop,
}

#[derive(Debug, PartialEq, Serialize, Deserialize)]
enum Shape {
    Circle(u32),
    Rect { w: u32, h: u32 },
    Empty,
}

#[test]
fn enums_read_their_names_and_tags() {
    assert_eq!(from_str::<Policy>("warn").unwrap(), Policy::Warn);
    assert_eq!(from_str::<Policy>("hard_stop").unwrap(), Policy::HardStop);
    assert!(error::<Policy>("Warn").contains("unknown variant `Warn`"));
    assert_eq!(from_str::<Shape>("!Circle 3").unwrap(), Shape::Circle(3));
    assert_eq!(
        from_str::<Shape>("!Rect {w: 1, h: 2}").unwrap(),
        Shape::Rect { w: 1, h: 2 }
    );
    assert_eq!(from_str::<Shape>("Empty").unwrap(), Shape::Empty);
    // The map form is not a variant, as with serde_yaml 0.9.
    assert!(error::<Shape>("{Circle: 3}").contains("expected a YAML tag starting with '!'"));
    for shape in [Shape::Circle(3), Shape::Rect { w: 1, h: 2 }, Shape::Empty] {
        let text = to_string(&shape).unwrap();
        assert_eq!(from_str::<Shape>(&text).unwrap(), shape, "{text}");
    }
}

#[test]
fn anchors_aliases_and_merge_keys() {
    assert_eq!(
        json("base: &b {x: 1}\ncopy: *b\n"),
        json!({"base": {"x": 1}, "copy": {"x": 1}})
    );
    assert_eq!(fields("s: &a x\ntext: *a").text, "x");
    // `<<` is an ordinary key: merge keys are not expanded.
    assert_eq!(
        json("base: &b {x: 1}\nc:\n  <<: *b\n  y: 2\n"),
        json!({"base": {"x": 1}, "c": {"<<": {"x": 1}, "y": 2}})
    );
    let mut map = from_str::<HashMap<String, Json>>("<<: {x: 1}\ny: 2").unwrap();
    assert_eq!(map.remove("<<"), Some(json!({"x": 1})));
    // An alias to an anchor that is not defined yet is an error.
    assert!(error::<Json>("a: *missing").contains("line 1 column 4"));
    // Aliases cannot expand without bound: each level here multiplies the
    // document by ten.
    let mut bomb = String::from("a: &a [x, x, x, x, x, x, x, x, x, x]\n");
    for level in 1..10 {
        let previous = (b'a' + level - 1) as char;
        let name = (b'a' + level) as char;
        bomb.push_str(&format!(
            "{name}: &{name} [{}]\n",
            vec![format!("*{previous}"); 10].join(", ")
        ));
    }
    assert!(error::<Json>(&bomb).contains("repetition limit exceeded"));
    // A recursive alias stops at the recursion limit.
    assert!(error::<Json>("a: &r [*r]").contains("recursion limit exceeded"));
}

#[test]
fn duplicate_keys() {
    // A struct refuses a field given twice, with its position.
    assert_eq!(
        error::<Fields>("s: a\ns: b\n"),
        "duplicate field `s` at line 1 column 1"
    );
    // A JSON value and a map type keep the last, as they did.
    assert_eq!(json("a: 1\na: 2\n"), json!({"a": 2}));
    assert_eq!(fields("map: {k: a, k: b}").map["k"], "b");
    // The YAML value tree refuses it.
    assert!(error::<Value>("a: 1\na: 2\n").contains("duplicate entry with key \"a\""));
}

#[test]
fn one_document_per_input() {
    assert_eq!(fields("---\ns: a\n...\n").s.as_deref(), Some("a"));
    assert_eq!(
        error::<Fields>("s: a\n---\ns: b\n"),
        "deserializing from YAML containing more than one document is not supported at line 3 column 1"
    );
}

#[test]
fn errors_name_the_path_line_and_column() {
    assert_eq!(
        error::<BTreeMap<String, Vec<Fields>>>("x:\n  - u: 1\n  - u: many\n"),
        "x[1].u: invalid type: string \"many\", expected u64 at line 3 column 8"
    );
    #[derive(Debug, Deserialize)]
    #[serde(deny_unknown_fields)]
    #[allow(dead_code)]
    struct Strict {
        known: Option<u32>,
    }
    assert_eq!(
        error::<Strict>(
            "known: 1
zzz: 1"
        ),
        "unknown field `zzz`, expected `known` at line 2 column 1"
    );
    let syntax = error::<Json>("a: [1, 2\nb: c\n");
    assert!(
        regex::Regex::new(r"at line \d+ column \d+$")
            .unwrap()
            .is_match(&syntax),
        "{syntax}"
    );
    let error = from_str::<Json>("a:\n  b: c\n d: e\n").unwrap_err();
    let location = error.location().expect("a syntax error has a position");
    assert_eq!(location.line(), 3);
}

#[test]
fn block_scalars_and_quoted_continuations() {
    assert_eq!(fields("s: |\n  a\n  b\n").s.as_deref(), Some("a\nb\n"));
    assert_eq!(fields("s: |-\n  a\n  b\n").s.as_deref(), Some("a\nb"));
    assert_eq!(fields("s: >\n  a\n  b\n").s.as_deref(), Some("a b\n"));
    // A block that ends the input without a line break has none.
    assert_eq!(fields("s: |\n  a\n  b").s.as_deref(), Some("a\nb"));
    assert_eq!(fields("s: >\n  a\n  b").s.as_deref(), Some("a b"));
    // A continuation line of a quoted string indented less than YAML 1.2
    // asks reads as libyaml read it.
    assert_eq!(fields("s: \"one\ntwo\"\n").s.as_deref(), Some("one two"));
    assert_eq!(fields("s: 'one\n\ntwo'\n").s.as_deref(), Some("one\ntwo"));
    assert_eq!(
        from_str::<BTreeMap<String, Vec<String>>>("list:\n  - \"one\n  two\n three\"\n").unwrap()
            ["list"],
        ["one two three"]
    );
    // Quoted escapes.
    assert_eq!(
        fields(r#"s: "\t\u00e9\U0001F600\x41""#).s.as_deref(),
        Some("\té😀A")
    );
    assert_eq!(fields("s: 'it''s'").s.as_deref(), Some("it's"));
}

#[test]
fn deep_nesting_is_refused_with_a_position() {
    let nested = |depth: usize| format!("a: {}{}\n", "[".repeat(depth), "]".repeat(depth));
    assert!(from_str::<Json>(&nested(127)).is_ok());
    assert_eq!(
        error::<Json>(&nested(129)),
        "recursion limit exceeded at line 1 column 131"
    );
    // Deeper than the parser's limit: refused before reading.
    let refused = error::<serde::de::IgnoredAny>(&nested(300));
    assert!(refused.contains("line 1 column"), "{refused}");
}

#[test]
fn the_value_tree_and_its_lookup() {
    let value = from_str::<Value>("a: {b: [1, -2, 1.5, true, ~, x]}\nc: !t {d: e}\n").unwrap();
    assert_eq!(
        value.get("a").and_then(|a| a.get("b")),
        Some(&Value::Sequence(vec![
            Value::Number(Number::PosInt(1)),
            Value::Number(Number::NegInt(-2)),
            Value::Number(Number::Float(1.5)),
            Value::Bool(true),
            Value::Null,
            Value::String("x".into()),
        ]))
    );
    // Lookup looks through a tag.
    assert_eq!(
        value.get("c").and_then(|c| c.get("d")),
        Some(&Value::String("e".into()))
    );
    assert_eq!(value.get("missing"), None);
}

// ---------------------------------------------------------------------------
// Writing

/// Strings a writer must quote or block to read back unchanged.
const AWKWARD: &[&str] = &[
    "",
    " ",
    "  lead",
    "trail ",
    "yes",
    "no",
    "on",
    "off",
    "true",
    "True",
    "FALSE",
    "null",
    "Null",
    "~",
    "017",
    "00",
    "0o17",
    "0x1F",
    "0b1",
    "1_000",
    "12",
    "-12",
    "+12",
    "1.5",
    "1e3",
    ".inf",
    "-.inf",
    ".nan",
    "12:30",
    "2001-12-14",
    "- dash",
    "-",
    "--",
    "---",
    "...",
    "? q",
    "?",
    ":",
    "a: b",
    "a:",
    "# hash",
    "a #b",
    "a#b",
    "'single'",
    "\"double\"",
    "it's",
    "!tag",
    "&anchor",
    "*alias",
    "%pct",
    "@at",
    "`tick",
    "|",
    ">",
    "{brace}",
    "[bracket]",
    "a, b",
    ",",
    "<<",
    "=",
    "${VAR}",
    "tab\there",
    "\ttab",
    "multi\nline",
    "multi\nline\n",
    "multi\nline\n\n",
    "\nlead break",
    "trail break\n",
    "\n",
    "\n\n",
    "crlf\r\nline",
    "\r",
    "é ü ö",
    "😀",
    "\u{7}",
    "\u{0}",
    "\u{85}",
    "\u{2028}",
    "\u{feff}bom",
    "\u{a0}nbsp",
    "back\\slash",
    "  indented\n  block\n",
    "line\n  indented\n",
    "key: value\n- item\n",
    "a\u{7f}b",
    "\u{fffe}",
];

#[derive(Debug, PartialEq, Serialize, Deserialize)]
struct Holder {
    text: String,
    list: Vec<String>,
    map: BTreeMap<String, String>,
    nested: Vec<BTreeMap<String, Vec<String>>>,
}

#[test]
fn every_awkward_string_reads_back_unchanged() {
    for text in AWKWARD {
        let written = to_string(text).unwrap();
        assert_eq!(
            &from_str::<String>(&written).unwrap_or_else(|error| panic!("{written:?}: {error}")),
            text,
            "{written:?}"
        );
        let holder = Holder {
            text: text.to_string(),
            list: vec![text.to_string(), "plain".into(), text.to_string()],
            map: BTreeMap::from([(text.to_string(), text.to_string())]),
            nested: vec![BTreeMap::from([(text.to_string(), vec![text.to_string()])])],
        };
        let written = to_string(&holder).unwrap();
        assert_eq!(
            from_str::<Holder>(&written).unwrap_or_else(|error| panic!("{written}\n{error}")),
            holder,
            "{written}"
        );
    }
}

#[test]
fn generated_strings_read_back_unchanged() {
    // A fixed pseudo-random sequence over the characters that matter.
    const ALPHABET: &[char] = &[
        'a', 'b', 'Z', '0', '1', '7', ' ', ' ', '\n', '\t', '\r', ':', '-', '?', '#', '\'', '"',
        '!', '&', '*', '|', '>', '%', '@', '`', '[', ']', '{', '}', ',', '.', '~', '\\', '_', '+',
        'é', '😀', '\u{85}', '\u{feff}', '\u{7}',
    ];
    let mut state = 0x9E37_79B9_7F4A_7C15_u64;
    let mut next = || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };
    for _ in 0..5000 {
        let len = (next() % 12) as usize;
        let text: String = (0..len)
            .map(|_| ALPHABET[(next() % ALPHABET.len() as u64) as usize])
            .collect();
        let value = json!({ "key": text, "list": [text, {"inner": text}], text.clone(): 1 });
        let written = to_string(&value).unwrap();
        let read: Json = from_str(&written).unwrap_or_else(|error| panic!("{written:?}\n{error}"));
        assert_eq!(read, value, "{written:?}");
    }
}

#[test]
fn numbers_and_nulls_read_back_unchanged() {
    let value = json!({
        "u": u64::MAX, "i": i64::MIN, "zero": 0, "f": 1.5, "whole": 2.0, "tiny": 1e-300,
        "big": 1e300, "neg": -0.25, "t": true, "f2": false, "n": null,
        "empty_list": [], "empty_map": {}, "nested": [[1, [2, []]], {"a": {}}],
    });
    let written = to_string(&value).unwrap();
    assert_eq!(from_str::<Json>(&written).unwrap(), value, "{written}");
    for float in [f64::INFINITY, f64::NEG_INFINITY] {
        assert_eq!(from_str::<f64>(&to_string(&float).unwrap()).unwrap(), float);
    }
    assert!(from_str::<f64>(&to_string(&f64::NAN).unwrap())
        .unwrap()
        .is_nan());
    assert_eq!(
        from_str::<Option<u8>>(&to_string(&None::<u8>).unwrap()).unwrap(),
        None
    );
    assert_eq!(from_str::<()>(&to_string(&()).unwrap()).unwrap(), ());
    let keys = BTreeMap::from([(1u32, "one"), (20, "twenty")]);
    let written = to_string(&keys).unwrap();
    assert_eq!(
        from_str::<BTreeMap<u32, String>>(&written).unwrap()[&20],
        "twenty"
    );
}

#[test]
fn the_writer_lays_out_block_yaml() {
    #[derive(Serialize)]
    struct Agent {
        id: &'static str,
        tools: Vec<&'static str>,
        prompt: &'static str,
        budget: BTreeMap<&'static str, u32>,
        empty: Vec<u32>,
    }
    let written = to_string(&vec![Agent {
        id: "lead",
        tools: vec!["read_file", "grep"],
        prompt: "You review.\nBe brief.\n",
        budget: BTreeMap::from([("per_call", 8192)]),
        empty: vec![],
    }])
    .unwrap();
    assert_eq!(
        written,
        "- id: lead\n  tools:\n    - read_file\n    - grep\n  prompt: |\n    You review.\n    Be brief.\n  budget:\n    per_call: 8192\n  empty: []\n"
    );
    // A key longer than YAML's limit for an implicit key is written
    // explicitly.
    let long = "k".repeat(2000);
    let written = to_string(&BTreeMap::from([(long.clone(), 1)])).unwrap();
    assert_eq!(
        from_str::<BTreeMap<String, u32>>(&written).unwrap()[&long],
        1
    );
    // A sequence or mapping cannot be a key.
    assert!(to_string(&HashMap::from([(vec![1], 1)])).is_err());
}
