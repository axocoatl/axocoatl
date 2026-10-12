//! Differential test of Axocoatl's YAML reader against serde_yaml 0.9.
//!
//! Until 1.3.1 Axocoatl read YAML with serde_yaml 0.9.34 (libyaml translated
//! to unsafe Rust). `tests/yaml_golden/corpus.json` records what serde_yaml
//! made of every YAML document in the repository when it was replaced:
//! 2,281 documents, being each `.yaml` and `.yml` file, each `yaml` code
//! block of the Markdown and MDX documentation (README files, `docs/`,
//! `sites/docs`), every document the workspace's tests handed to the parser
//! (configurations, loadouts, egress routes, configurations written and read
//! back) and a table of YAML 1.1/1.2 pitfalls read into the [`Probe`] type.
//! For each document it holds the result of reading it into
//! `serde_json::Value`, into the Rust types it is read as (every one of
//! them for the repository's files), and through the whole `parse_config`
//! and `parse_loadout` paths: the value as JSON (by its SHA-256 when long),
//! or the error message. For each valid document it also holds the error
//! serde_yaml gave when reading failed at each of its nodes in turn (see
//! [`node_messages`]), which names the node's path, line and column, as a
//! count and a SHA-256.
//!
//! This test reads every recorded document again and asserts the same
//! results: the same values, the same node positions, and for documents that
//! are valid YAML the same error messages. serde_yaml left the position out
//! of an error at line 1, column 1; such messages are compared without it. A
//! document that libyaml could not parse must still be refused, with a line
//! and column, except where `accepted_now` lists it, and a document libyaml
//! parsed must still parse, except where `refused_now` lists it.
//!
//! `tests/yaml_golden/review_cases.json` holds the cases the reader's review
//! tried, with what serde_yaml 0.9.34 made of each (recorded with serde_yaml
//! outside the workspace) and, where the reader differs, how and why:
//! [`yaml_reads_the_review_cases_as_recorded`] checks them.
//!
//! The corpus was recorded with serde_yaml while it was still a dependency
//! (the node positions by the same walk, run against serde_yaml outside the
//! workspace). It cannot be recorded again. When a configuration type
//! changes on purpose, the typed results of the documents it affects change
//! with it: check that the `value` results and positions still match, then
//! refresh the other results with `AXOCOATL_YAML_GOLDEN=retype`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use axocoatl_config::yaml;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value as Json};

const GOLDEN: &str = "tests/yaml_golden/corpus.json";

/// Documents libyaml refused that are valid YAML 1.2 and read now, each
/// with the reason.
fn accepted_now() -> BTreeMap<String, &'static str> {
    const TAG_BEFORE_FLOW_INDICATOR: &str = "A tag ends at a flow indicator (YAML 1.2 \
        ns-tag-char excludes `[]{},`), so this is a tagged empty value; libyaml read the \
        indicator into the tag and failed.";
    let mut documents = BTreeMap::new();
    documents.insert(
        "...\n".to_string(),
        "A stream holding only a document end marker has no document (YAML 1.2 \
         l-yaml-stream); libyaml reported a missing node.",
    );
    documents.insert(
        "map:\n  : v\n".to_string(),
        "A block mapping entry may have an empty key (YAML 1.2 example 8.18); libyaml \
         required one.",
    );
    for tag in ["!", "!alpha"] {
        for document in [
            format!("list: [{tag}]\n"),
            format!("map: {{k: {tag}}}\n"),
            format!("tagged: {{type: count, n: {tag}}}\n"),
            format!("tagged: {{type: schedule, every: {tag}}}\n"),
        ] {
            documents.insert(document, TAG_BEFORE_FLOW_INDICATOR);
        }
    }
    documents
}

/// Documents libyaml parsed that are refused now, each with the reason.
fn refused_now() -> BTreeMap<String, &'static str> {
    let mut documents = BTreeMap::new();
    documents.insert(
        format!("any: {}{}\n", "[".repeat(300), "]".repeat(300)),
        "granit-parser refuses nesting deeper than 255 collections before anything is \
         read. serde_yaml parsed it but read no deeper than 128 levels, so only such \
         nesting inside an ignored field was accepted before.",
    );
    documents
}

/// Whether `target` refuses `input` because the text it reads does not
/// parse: `parse_config` reads the text after `${...}` substitution.
fn target_syntax_error(target: &str, input: &str) -> bool {
    if target == "config_pipeline" {
        syntax_error(&interpolated(input)).is_some()
    } else {
        false
    }
}

fn interpolated(text: &str) -> String {
    regex::Regex::new(r"\$\{([^}]+)\}")
        .expect("regex")
        .replace_all(text, "")
        .to_string()
}

/// Typed results longer than this are recorded by their SHA-256.
const MAX_RECORDED_TYPED: usize = 400;

/// One recorded result: the value as compact JSON text, or the error
/// message.
fn outcome<T: Serialize>(result: Result<T, String>) -> Json {
    match result {
        Ok(value) => json!({ "ok": serde_json::to_string(&value).expect("serializable result") }),
        Err(message) => json!({ "err": message }),
    }
}

/// A typed result as recorded: long values by their digest.
fn recorded(target: &str, result: Json) -> Json {
    use sha2::{Digest, Sha256};
    match result.get("ok").and_then(Json::as_str) {
        Some(text) if target != "value" && text.len() > MAX_RECORDED_TYPED => {
            json!({ "ok": format!("sha256:{:x}", Sha256::digest(text.as_bytes())) })
        }
        _ => result,
    }
}

fn read_as<T: DeserializeOwned + Serialize>(text: &str) -> Json {
    outcome(yaml::from_str::<T>(text).map_err(|error| error.to_string()))
}

// ---------------------------------------------------------------------------
// The pitfalls: one probe type with a field of every kind.

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ProbeUnit {
    Alpha,
    BetaGamma,
}

#[derive(Debug, Serialize, Deserialize)]
enum ProbeData {
    Text(String),
    Point { x: u32, y: u32 },
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ProbeTagged {
    Schedule { every: String },
    Count { n: u32 },
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(untagged)]
enum ProbeUntagged {
    Param { param: String },
    Text(String),
    Number(u64),
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Probe {
    #[serde(default)]
    s: Option<String>,
    #[serde(default)]
    text: String,
    #[serde(default)]
    b: Option<bool>,
    #[serde(default)]
    u: Option<u64>,
    #[serde(default)]
    i: Option<i32>,
    #[serde(default)]
    p: Option<u16>,
    #[serde(default)]
    f: Option<f64>,
    #[serde(default)]
    list: Vec<String>,
    #[serde(default)]
    map: BTreeMap<String, String>,
    #[serde(default)]
    unit: Option<ProbeUnit>,
    #[serde(default)]
    data: Option<ProbeData>,
    #[serde(default)]
    tagged: Option<ProbeTagged>,
    #[serde(default)]
    untagged: Option<ProbeUntagged>,
    #[serde(default)]
    any: Option<Json>,
}

// ---------------------------------------------------------------------------
// The corpus.

#[derive(Debug, Serialize, Deserialize)]
struct Entry {
    /// Where the document came from.
    source: String,
    input: String,
    /// libyaml's error when the input is not valid YAML.
    syntax_error: Option<String>,
    /// Target name to recorded result.
    results: BTreeMap<String, Json>,
    /// Where each node of a valid document is, as serde_yaml reported it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    positions: Option<Positions>,
}

/// The positions of every node of a document: the error message serde_yaml
/// gave when reading failed at that node (see [`node_messages`]), joined by
/// line breaks and hashed.
#[derive(Debug, Serialize, Deserialize)]
struct Positions {
    nodes: usize,
    sha256: String,
}

/// `parse_config`, with every `${VAR}` replaced by the empty string as an
/// unset variable is, so the result does not depend on the environment.
fn config_pipeline(text: &str) -> Json {
    if let Err(error) = axocoatl_config::refuse_substitution(text) {
        return json!({ "err": error.to_string() });
    }
    outcome(
        axocoatl_config::parse_config(&interpolated(text), Path::new("corpus.yaml"))
            .map_err(|error| error.to_string()),
    )
}

fn loadout_pipeline(text: &str) -> Json {
    match axocoatl_config::loadout::parse_loadout(
        text,
        axocoatl_config::loadout::LoadoutSource::Builtin,
    ) {
        Ok(loadout) => outcome::<Json>(Ok(json!({
            "file": serde_json::to_value(&loadout.file).expect("loadout file"),
            "warnings": serde_json::to_value(&loadout.warnings).expect("warnings"),
            "digest": loadout.digest,
        }))),
        Err(error) => json!({ "err": error.to_string() }),
    }
}

fn substitution(text: &str) -> Json {
    match axocoatl_config::refuse_substitution(text) {
        Ok(()) => json!({ "ok": "null" }),
        Err(error) => json!({ "err": error.to_string() }),
    }
}

/// The result of reading `text` as `target`, as recorded.
fn read(target: &str, text: &str) -> Option<Json> {
    read_unrecorded(target, text).map(|result| recorded(target, result))
}

fn read_unrecorded(target: &str, text: &str) -> Option<Json> {
    use axocoatl_config::*;
    Some(match target {
        "value" => read_as::<Json>(text),
        "probe" => read_as::<Probe>(text),
        "config" => read_as::<AxocoatlConfig>(text),
        "config_pipeline" => config_pipeline(text),
        "loadout" => read_as::<loadout::LoadoutFile>(text),
        "loadout_pipeline" => loadout_pipeline(text),
        "substitution" => substitution(text),
        "egress_route" => read_as::<EgressRouteYaml>(text),
        "sandbox" => read_as::<SandboxConfigYaml>(text),
        "memory" => read_as::<MemoryConfigYaml>(text),
        "sampling" => read_as::<SamplingConfigYaml>(text),
        "egress_allow_list" => read_as::<Vec<EgressAllowYaml>>(text),
        "egress" => read_as::<EgressConfigYaml>(text),
        "overflow_policy" => read_as::<OverflowPolicyYaml>(text),
        "agent" => read_as::<AgentConfigYaml>(text),
        _ => return None,
    })
}

fn syntax_error(text: &str) -> Option<String> {
    yaml::from_str::<serde::de::IgnoredAny>(text)
        .err()
        .map(|error| error.to_string())
}

/// Reads a document, failing at its `target`-th node: the error names the
/// node's path, line and column.
struct FailAt<'a> {
    target: usize,
    count: &'a std::cell::Cell<usize>,
}

impl<'de> serde::de::DeserializeSeed<'de> for FailAt<'_> {
    type Value = ();

    fn deserialize<D: serde::Deserializer<'de>>(self, deserializer: D) -> Result<(), D::Error> {
        let index = self.count.get();
        self.count.set(index + 1);
        if index == self.target {
            return Err(serde::de::Error::custom("FAIL"));
        }
        deserializer.deserialize_any(self)
    }
}

impl<'de> serde::de::Visitor<'de> for FailAt<'_> {
    type Value = ();

    fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
        formatter.write_str("any value")
    }
    fn visit_bool<E>(self, _: bool) -> Result<(), E> {
        Ok(())
    }
    fn visit_i64<E>(self, _: i64) -> Result<(), E> {
        Ok(())
    }
    fn visit_u64<E>(self, _: u64) -> Result<(), E> {
        Ok(())
    }
    fn visit_i128<E>(self, _: i128) -> Result<(), E> {
        Ok(())
    }
    fn visit_u128<E>(self, _: u128) -> Result<(), E> {
        Ok(())
    }
    fn visit_f64<E>(self, _: f64) -> Result<(), E> {
        Ok(())
    }
    fn visit_str<E>(self, _: &str) -> Result<(), E> {
        Ok(())
    }
    fn visit_unit<E>(self) -> Result<(), E> {
        Ok(())
    }
    fn visit_none<E>(self) -> Result<(), E> {
        Ok(())
    }
    fn visit_some<D: serde::Deserializer<'de>>(self, deserializer: D) -> Result<(), D::Error> {
        serde::de::DeserializeSeed::deserialize(self, deserializer)
    }
    fn visit_seq<A: serde::de::SeqAccess<'de>>(self, mut seq: A) -> Result<(), A::Error> {
        let (target, count) = (self.target, self.count);
        while seq.next_element_seed(FailAt { target, count })?.is_some() {}
        Ok(())
    }
    fn visit_map<A: serde::de::MapAccess<'de>>(self, mut map: A) -> Result<(), A::Error> {
        let (target, count) = (self.target, self.count);
        while map.next_key_seed(FailAt { target, count })?.is_some() {
            map.next_value_seed(FailAt { target, count })?;
        }
        Ok(())
    }
    fn visit_enum<A: serde::de::EnumAccess<'de>>(self, data: A) -> Result<(), A::Error> {
        use serde::de::VariantAccess;
        let (target, count) = (self.target, self.count);
        let (_, variant) = data.variant::<serde::de::IgnoredAny>()?;
        variant.newtype_variant_seed(FailAt { target, count })
    }
}

thread_local! {
    static FAIL_TARGET: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static FAIL_COUNT: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// A document read through [`FailAt`], with the target in a thread local
/// because `yaml::from_str` reads a type, not a seed.
struct FailAtDocument;

impl<'de> Deserialize<'de> for FailAtDocument {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        FAIL_COUNT.with(|count| {
            let target = FAIL_TARGET.with(std::cell::Cell::get);
            serde::de::DeserializeSeed::deserialize(FailAt { target, count }, deserializer)
        })?;
        Ok(FailAtDocument)
    }
}

/// The message of reading `input` failing at each of its nodes in turn,
/// without the position serde_yaml left out at line 1, column 1.
fn node_messages(input: &str) -> Vec<String> {
    let mut messages = Vec::new();
    loop {
        let target = messages.len();
        FAIL_TARGET.with(|cell| cell.set(target));
        FAIL_COUNT.with(|cell| cell.set(0));
        let result = yaml::from_str::<FailAtDocument>(input);
        if FAIL_COUNT.with(std::cell::Cell::get) <= target {
            return messages;
        }
        match result {
            Err(error) => messages.push(
                error
                    .to_string()
                    .strip_suffix(" at line 1 column 1")
                    .map(str::to_string)
                    .unwrap_or_else(|| error.to_string()),
            ),
            Ok(_) => {
                messages.push(format!("no error at node {target}"));
                return messages;
            }
        }
    }
}

fn positions_digest(messages: &[String]) -> String {
    use sha2::{Digest, Sha256};
    format!("{:x}", Sha256::digest(messages.join("\n").as_bytes()))
}

fn golden_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join(GOLDEN)
}

fn load_golden() -> Vec<Entry> {
    let text = std::fs::read_to_string(golden_path()).expect("golden corpus");
    serde_json::from_str(&text).expect("golden corpus parses")
}

fn save_golden(entries: &[Entry]) {
    let path = golden_path();
    std::fs::create_dir_all(path.parent().expect("parent")).expect("create golden dir");
    // One document per line keeps the file small and its diffs readable.
    let lines = entries
        .iter()
        .map(|entry| serde_json::to_string(entry).expect("serialize corpus entry"))
        .collect::<Vec<_>>();
    let text = format!("[\n{}\n]\n", lines.join(",\n"));
    std::fs::write(path, text).expect("write golden corpus");
}

/// The message without the position serde_yaml left out at line 1, column 1.
fn normalized(message: &str) -> String {
    message.replace(" at line 1 column 1", "")
}

fn has_position(message: &str) -> bool {
    regex::Regex::new(r"line \d+ column \d+")
        .expect("regex")
        .is_match(message)
}

#[test]
fn yaml_reads_every_recorded_document_as_serde_yaml_did() {
    if std::env::var("AXOCOATL_YAML_GOLDEN").as_deref() == Ok("retype") {
        let mut entries = load_golden();
        for entry in &mut entries {
            for (target, result) in entry.results.iter_mut() {
                if target != "value" {
                    *result = read(target, &entry.input).expect("known target");
                }
            }
        }
        save_golden(&entries);
        return;
    }

    let entries = load_golden();
    let accepted_now: BTreeMap<String, &str> = accepted_now();
    let refused_now: BTreeMap<String, &str> = refused_now();
    let mut failures = Vec::new();
    let mut compared = 0usize;
    for entry in &entries {
        let input = entry.input.as_str();
        let syntax_now = syntax_error(input);
        match (&entry.syntax_error, &syntax_now) {
            (_, Some(new)) if !has_position(new) => failures.push(format!(
                "{}: syntax error without a line and column: {new}\ninput: {input:?}",
                entry.source
            )),
            (Some(old), None) if !accepted_now.contains_key(input) => failures.push(format!(
                "{}: libyaml refused this ({old}) and it is now accepted\ninput: {input:?}",
                entry.source
            )),
            (None, Some(new)) if !refused_now.contains_key(input) => failures.push(format!(
                "{}: libyaml accepted this and it is now refused ({new})\ninput: {input:?}",
                entry.source
            )),
            _ => {}
        }
        if let Some(positions) = &entry.positions {
            let messages = node_messages(input);
            if messages.len() != positions.nodes || positions_digest(&messages) != positions.sha256
            {
                failures.push(format!(
                    "{}: the positions of its {} nodes differ from serde_yaml's ({} now)\n{}\ninput: {input:?}",
                    entry.source,
                    positions.nodes,
                    messages.len(),
                    messages.join("\n")
                ));
            }
            compared += 1;
        }
        let syntax_either = entry.syntax_error.is_some() || syntax_now.is_some();
        for (target, expected) in &entry.results {
            compared += 1;
            let actual = read(target, input).expect("known target");
            let same = match (expected.get("ok"), actual.get("ok")) {
                (Some(old), Some(new)) => old == new,
                (None, None) => {
                    let old = expected["err"].as_str().unwrap_or_default();
                    let new = actual["err"].as_str().unwrap_or_default();
                    if syntax_either || target_syntax_error(target, input) {
                        // Both refuse YAML that does not parse; libyaml's and
                        // granit-parser's wording differ, but the position
                        // must be there.
                        has_position(new)
                    } else {
                        normalized(old) == normalized(new)
                    }
                }
                (None, Some(_)) => accepted_now.contains_key(input),
                (Some(_), None) => refused_now.contains_key(input),
            };
            if !same {
                let full = read_unrecorded(target, input).expect("known target");
                failures.push(format!(
                    "{} [{target}]\ninput: {input:?}\nserde_yaml: {expected}\nnow:        {full}",
                    entry.source
                ));
            }
        }
    }
    assert!(compared > 0, "the corpus is empty");
    assert!(
        failures.is_empty(),
        "{} of {compared} results differ from serde_yaml 0.9:\n\n{}",
        failures.len(),
        failures.join("\n\n")
    );
}

fn round_trips<T>(input: &str) -> Option<Result<(), String>>
where
    T: DeserializeOwned + Serialize,
{
    let value = yaml::from_str::<T>(input).ok()?;
    let expected = serde_json::to_value(&value).expect("serializable");
    let written = match yaml::to_string(&value) {
        Ok(written) => written,
        Err(error) => return Some(Err(format!("cannot write: {error}"))),
    };
    let read = match yaml::from_str::<T>(&written) {
        Ok(read) => read,
        Err(error) => return Some(Err(format!("cannot read back: {error}\n{written}"))),
    };
    let actual = serde_json::to_value(&read).expect("serializable");
    Some(if actual == expected {
        Ok(())
    } else {
        Err(format!("reads back differently:\n{written}"))
    })
}

/// Every configuration, loadout and route of the corpus that reads, written
/// back as YAML, reads back to the same value.
#[test]
fn written_yaml_reads_back_for_every_recorded_document() {
    use axocoatl_config::*;
    let mut checked = 0usize;
    let mut failures = Vec::new();
    for entry in load_golden() {
        let input = entry.input.as_str();
        let results = [
            ("value", round_trips::<Json>(input)),
            ("config", round_trips::<AxocoatlConfig>(input)),
            ("loadout", round_trips::<loadout::LoadoutFile>(input)),
            ("egress_route", round_trips::<EgressRouteYaml>(input)),
            ("sandbox", round_trips::<SandboxConfigYaml>(input)),
            ("agent", round_trips::<AgentConfigYaml>(input)),
        ];
        for (target, result) in results {
            match result {
                None => {}
                Some(Ok(())) => checked += 1,
                Some(Err(message)) => {
                    failures.push(format!("{} [{target}]: {message}", entry.source))
                }
            }
        }
    }
    assert!(checked > 1000, "only {checked} documents were written back");
    assert!(failures.is_empty(), "{}", failures.join("\n\n"));
}

/// What `tests/yaml_golden/review_cases.json` says of one document now, next
/// to what serde_yaml 0.9.34 made of it.
#[derive(Deserialize)]
#[serde(untagged)]
enum ReviewNow {
    /// The same value, or an error with a line and column where serde_yaml
    /// gave an error too.
    Same(String),
    /// serde_yaml read it; it is refused now, with its position, rather than
    /// read differently.
    Refused { refused: String },
    /// libyaml refused it; it is valid YAML 1.2 and reads now.
    Accepted { accepted: String, value: Json },
    /// serde_yaml read it wrongly; it reads as YAML says now.
    Corrected { corrected: String, value: Json },
}

#[derive(Deserialize)]
struct ReviewCase {
    input: String,
    serde_yaml: Json,
    now: ReviewNow,
}

/// The message fragment each kind of refusal names.
fn refusal_message(kind: &str) -> &'static str {
    match kind {
        // libyaml ended a name at a character other than a letter, digit,
        // `-` or `_` (so `&a: v` was an anchor `a` on an empty key);
        // granit-parser reads `a:` as the name.
        "anchor-name" => "an anchor or alias name may hold only letters, digits, `-` and `_`",
        // libyaml took `?` or `:` directly followed by text inside `[...]`
        // or `{...}` as a key or value indicator; YAML 1.2 reads text.
        "flow-indicator" => "inside `[...]` or `{...}`: write",
        // libyaml kept the separator in the value as a line break of its
        // own; YAML 1.2 does not break the line there.
        "separator-in-value" => {
            "a LINE SEPARATOR (U+2028) or PARAGRAPH SEPARATOR (U+2029) inside a value"
        }
        // libyaml read a second byte order mark into the first key.
        "second-bom" => "a second byte order mark (U+FEFF) at the start of the input",
        // libyaml skipped a byte order mark at the start of a line, or read
        // one at the start of a value into the text.
        "bom-inside" => "a BOM must not appear inside a document",
        // libyaml took a `#` directly after `[`, `]`, `,`, `|` or another
        // token as the start of a comment; YAML 1.2 needs a blank before
        // it.
        "comment-unseparated" => "comments must be separated from other tokens by whitespace",
        // libyaml took `- ?` as an entry holding an empty key.
        "indicator-unseparated" => "expected whitespace",
        other => panic!("unknown refusal kind {other}"),
    }
}

/// Whether `kind` is a known kind of document libyaml refused that YAML 1.2
/// reads.
fn known_acceptance(kind: &str) -> bool {
    matches!(
        kind,
        // A tab as separation, on a blank or comment line, or ending the
        // input after a block.
        "tab"
            // A byte order mark before `---` or a directive, or after `...`.
            | "bom"
            // `%YAML 1.3` and unknown directives, which are ignored.
            | "directive"
            // A non-ASCII implicit key of up to 1,024 characters (libyaml
            // counted its limit in bytes).
            | "long-key"
            // An empty key or value: `: v`, `- :`, `[:]`, `[a:]`.
            | "empty-key-or-value"
            // A stream holding only `...`.
            | "document-end"
    )
}

/// Every case the review of the YAML reader tried, with what serde_yaml
/// 0.9.34 made of each (recorded with serde_yaml outside the workspace):
/// edge cases of block scalars, tags, anchors, byte order marks, line
/// breaks, tabs, flow collections, documents and nesting, and a sample of
/// generated documents. Each reads as serde_yaml read it, or differs in the
/// way recorded with it.
#[test]
fn yaml_reads_the_review_cases_as_recorded() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/yaml_golden/review_cases.json");
    let text = std::fs::read_to_string(path).expect("review cases");
    let cases: Vec<ReviewCase> = serde_json::from_str(&text).expect("review cases parse");
    let mut failures = Vec::new();
    let mut same = 0usize;
    for case in &cases {
        let input = case.input.as_str();
        let now = yaml::from_str::<Json>(input).map_err(|error| error.to_string());
        let ok = match (&case.now, &now) {
            (ReviewNow::Same(word), _) if word != "same" => false,
            (ReviewNow::Same(_), Ok(value)) => {
                same += 1;
                case.serde_yaml.get("ok") == Some(value)
            }
            (ReviewNow::Same(_), Err(message)) => {
                same += 1;
                case.serde_yaml.get("err").is_some() && has_position(message)
            }
            (ReviewNow::Refused { refused }, Err(message)) => {
                case.serde_yaml.get("ok").is_some()
                    && message.contains(refusal_message(refused))
                    && has_position(message)
            }
            (ReviewNow::Accepted { accepted, value }, Ok(now)) => {
                known_acceptance(accepted) && case.serde_yaml.get("err").is_some() && value == now
            }
            // serde_yaml numbered anchors by how many names it had seen, so
            // an alias after a redefined anchor and a new one named the new
            // one.
            (ReviewNow::Corrected { corrected, value }, Ok(now)) => {
                corrected == "redefined-anchor"
                    && case.serde_yaml.get("ok").is_some_and(|old| old != value)
                    && value == now
            }
            _ => false,
        };
        if !ok {
            failures.push(format!(
                "input: {input:?}\nserde_yaml: {}\nnow: {now:?}",
                case.serde_yaml
            ));
        }
    }
    assert!(same > 5000, "only {same} cases read as serde_yaml did");
    assert!(
        failures.is_empty(),
        "{} of {} review cases differ from the record:\n\n{}",
        failures.len(),
        cases.len(),
        failures.join("\n\n")
    );
}
