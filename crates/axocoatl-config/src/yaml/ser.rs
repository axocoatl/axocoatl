//! Writing Rust values as YAML.
//!
//! The value becomes a tree of nodes first, then block-style text: mappings
//! as `key: value`, sequences as `- item`, a string plain when this module
//! reads it back as the same string and quoted otherwise, a string with line
//! breaks as a literal block. An enum variant with data is written with a
//! local tag (`!Variant`), as serde_yaml 0.9 wrote it. Whatever this writes,
//! [`super::from_str`] reads back to the same value.

use serde::ser::{self, Serialize};

use super::de::{digits_but_not_number, visit_untagged_scalar};
use super::error::{self, Error, ErrorImpl};

type Result<T, E = Error> = std::result::Result<T, E>;

/// Write `value` as one YAML document, ending with a line break.
pub fn to_string<T: ?Sized + Serialize>(value: &T) -> Result<String> {
    let node = value.serialize(NodeSerializer { tag: None })?;
    let mut out = String::new();
    render_document(&node, &mut out);
    Ok(out)
}

#[derive(Debug)]
enum Node {
    /// Text written as is: a number, a boolean, `null`.
    Raw(String),
    Str(String),
    Seq(Vec<Node>),
    Map(Vec<(Node, Node)>),
    Tagged(String, Box<Node>),
}

struct NodeSerializer {
    /// The variant tag that a nested enum would conflict with.
    tag: Option<()>,
}

fn tagged(variant: &str, node: Node) -> Node {
    Node::Tagged(format!("!{variant}"), Box::new(node))
}

fn float(v: f64) -> String {
    if v.is_nan() {
        ".nan".into()
    } else if v.is_infinite() {
        if v.is_sign_positive() {
            ".inf"
        } else {
            "-.inf"
        }
        .into()
    } else {
        // `{:?}` keeps a `.0` on whole numbers, so the text reads back as a
        // float, and prints the shortest text that reads back exactly.
        format!("{v:?}")
    }
}

impl ser::Serializer for NodeSerializer {
    type Ok = Node;
    type Error = Error;
    type SerializeSeq = SeqBuilder;
    type SerializeTuple = SeqBuilder;
    type SerializeTupleStruct = SeqBuilder;
    type SerializeTupleVariant = SeqBuilder;
    type SerializeMap = MapBuilder;
    type SerializeStruct = MapBuilder;
    type SerializeStructVariant = MapBuilder;

    fn serialize_bool(self, v: bool) -> Result<Node> {
        Ok(Node::Raw(v.to_string()))
    }
    fn serialize_i8(self, v: i8) -> Result<Node> {
        Ok(Node::Raw(v.to_string()))
    }
    fn serialize_i16(self, v: i16) -> Result<Node> {
        Ok(Node::Raw(v.to_string()))
    }
    fn serialize_i32(self, v: i32) -> Result<Node> {
        Ok(Node::Raw(v.to_string()))
    }
    fn serialize_i64(self, v: i64) -> Result<Node> {
        Ok(Node::Raw(v.to_string()))
    }
    fn serialize_i128(self, v: i128) -> Result<Node> {
        Ok(Node::Raw(v.to_string()))
    }
    fn serialize_u8(self, v: u8) -> Result<Node> {
        Ok(Node::Raw(v.to_string()))
    }
    fn serialize_u16(self, v: u16) -> Result<Node> {
        Ok(Node::Raw(v.to_string()))
    }
    fn serialize_u32(self, v: u32) -> Result<Node> {
        Ok(Node::Raw(v.to_string()))
    }
    fn serialize_u64(self, v: u64) -> Result<Node> {
        Ok(Node::Raw(v.to_string()))
    }
    fn serialize_u128(self, v: u128) -> Result<Node> {
        Ok(Node::Raw(v.to_string()))
    }
    fn serialize_f32(self, v: f32) -> Result<Node> {
        Ok(Node::Raw(if v.is_finite() {
            format!("{v:?}")
        } else {
            float(f64::from(v))
        }))
    }
    fn serialize_f64(self, v: f64) -> Result<Node> {
        Ok(Node::Raw(float(v)))
    }
    fn serialize_char(self, v: char) -> Result<Node> {
        Ok(Node::Str(v.to_string()))
    }
    fn serialize_str(self, v: &str) -> Result<Node> {
        Ok(Node::Str(v.to_owned()))
    }
    fn serialize_bytes(self, _v: &[u8]) -> Result<Node> {
        Err(error::new(ErrorImpl::BytesUnsupported))
    }
    fn serialize_none(self) -> Result<Node> {
        Ok(Node::Raw("null".into()))
    }
    fn serialize_some<T: ?Sized + Serialize>(self, value: &T) -> Result<Node> {
        value.serialize(self)
    }
    fn serialize_unit(self) -> Result<Node> {
        Ok(Node::Raw("null".into()))
    }
    fn serialize_unit_struct(self, _name: &'static str) -> Result<Node> {
        self.serialize_unit()
    }
    fn serialize_unit_variant(
        self,
        _name: &'static str,
        _index: u32,
        variant: &'static str,
    ) -> Result<Node> {
        Ok(Node::Str(variant.to_owned()))
    }
    fn serialize_newtype_struct<T: ?Sized + Serialize>(
        self,
        _name: &'static str,
        value: &T,
    ) -> Result<Node> {
        value.serialize(self)
    }
    fn serialize_newtype_variant<T: ?Sized + Serialize>(
        self,
        _name: &'static str,
        _index: u32,
        variant: &'static str,
        value: &T,
    ) -> Result<Node> {
        if self.tag.is_some() {
            return Err(error::new(ErrorImpl::SerializeNestedEnum));
        }
        let inner = value.serialize(NodeSerializer { tag: Some(()) })?;
        Ok(tagged(variant, inner))
    }
    fn serialize_seq(self, len: Option<usize>) -> Result<SeqBuilder> {
        Ok(SeqBuilder {
            items: Vec::with_capacity(len.unwrap_or(0)),
            variant: None,
        })
    }
    fn serialize_tuple(self, len: usize) -> Result<SeqBuilder> {
        self.serialize_seq(Some(len))
    }
    fn serialize_tuple_struct(self, _name: &'static str, len: usize) -> Result<SeqBuilder> {
        self.serialize_seq(Some(len))
    }
    fn serialize_tuple_variant(
        self,
        _name: &'static str,
        _index: u32,
        variant: &'static str,
        len: usize,
    ) -> Result<SeqBuilder> {
        if self.tag.is_some() {
            return Err(error::new(ErrorImpl::SerializeNestedEnum));
        }
        Ok(SeqBuilder {
            items: Vec::with_capacity(len),
            variant: Some(variant),
        })
    }
    fn serialize_map(self, len: Option<usize>) -> Result<MapBuilder> {
        Ok(MapBuilder {
            entries: Vec::with_capacity(len.unwrap_or(0)),
            key: None,
            variant: None,
        })
    }
    fn serialize_struct(self, _name: &'static str, len: usize) -> Result<MapBuilder> {
        self.serialize_map(Some(len))
    }
    fn serialize_struct_variant(
        self,
        _name: &'static str,
        _index: u32,
        variant: &'static str,
        len: usize,
    ) -> Result<MapBuilder> {
        if self.tag.is_some() {
            return Err(error::new(ErrorImpl::SerializeNestedEnum));
        }
        Ok(MapBuilder {
            entries: Vec::with_capacity(len),
            key: None,
            variant: Some(variant),
        })
    }
}

struct SeqBuilder {
    items: Vec<Node>,
    variant: Option<&'static str>,
}

impl SeqBuilder {
    fn push<T: ?Sized + Serialize>(&mut self, value: &T) -> Result<()> {
        self.items
            .push(value.serialize(NodeSerializer { tag: None })?);
        Ok(())
    }

    fn finish(self) -> Node {
        let node = Node::Seq(self.items);
        match self.variant {
            Some(variant) => tagged(variant, node),
            None => node,
        }
    }
}

impl ser::SerializeSeq for SeqBuilder {
    type Ok = Node;
    type Error = Error;
    fn serialize_element<T: ?Sized + Serialize>(&mut self, value: &T) -> Result<()> {
        self.push(value)
    }
    fn end(self) -> Result<Node> {
        Ok(self.finish())
    }
}

impl ser::SerializeTuple for SeqBuilder {
    type Ok = Node;
    type Error = Error;
    fn serialize_element<T: ?Sized + Serialize>(&mut self, value: &T) -> Result<()> {
        self.push(value)
    }
    fn end(self) -> Result<Node> {
        Ok(self.finish())
    }
}

impl ser::SerializeTupleStruct for SeqBuilder {
    type Ok = Node;
    type Error = Error;
    fn serialize_field<T: ?Sized + Serialize>(&mut self, value: &T) -> Result<()> {
        self.push(value)
    }
    fn end(self) -> Result<Node> {
        Ok(self.finish())
    }
}

impl ser::SerializeTupleVariant for SeqBuilder {
    type Ok = Node;
    type Error = Error;
    fn serialize_field<T: ?Sized + Serialize>(&mut self, value: &T) -> Result<()> {
        self.push(value)
    }
    fn end(self) -> Result<Node> {
        Ok(self.finish())
    }
}

struct MapBuilder {
    entries: Vec<(Node, Node)>,
    key: Option<Node>,
    variant: Option<&'static str>,
}

impl MapBuilder {
    fn finish(self) -> Node {
        let node = Node::Map(self.entries);
        match self.variant {
            Some(variant) => tagged(variant, node),
            None => node,
        }
    }

    fn field<T: ?Sized + Serialize>(&mut self, key: &'static str, value: &T) -> Result<()> {
        let value = value.serialize(NodeSerializer { tag: None })?;
        self.entries.push((Node::Str(key.to_owned()), value));
        Ok(())
    }
}

fn key_node<T: ?Sized + Serialize>(key: &T) -> Result<Node> {
    match key.serialize(NodeSerializer { tag: None })? {
        node @ (Node::Raw(_) | Node::Str(_)) => Ok(node),
        _ => Err(error::new(ErrorImpl::ComplexKey)),
    }
}

impl ser::SerializeMap for MapBuilder {
    type Ok = Node;
    type Error = Error;
    fn serialize_key<T: ?Sized + Serialize>(&mut self, key: &T) -> Result<()> {
        self.key = Some(key_node(key)?);
        Ok(())
    }
    fn serialize_value<T: ?Sized + Serialize>(&mut self, value: &T) -> Result<()> {
        let key = self
            .key
            .take()
            .ok_or_else(|| <Error as ser::Error>::custom("a map value came before its key"))?;
        let value = value.serialize(NodeSerializer { tag: None })?;
        self.entries.push((key, value));
        Ok(())
    }
    fn end(self) -> Result<Node> {
        Ok(self.finish())
    }
}

impl ser::SerializeStruct for MapBuilder {
    type Ok = Node;
    type Error = Error;
    fn serialize_field<T: ?Sized + Serialize>(
        &mut self,
        key: &'static str,
        value: &T,
    ) -> Result<()> {
        self.field(key, value)
    }
    fn end(self) -> Result<Node> {
        Ok(self.finish())
    }
}

impl ser::SerializeStructVariant for MapBuilder {
    type Ok = Node;
    type Error = Error;
    fn serialize_field<T: ?Sized + Serialize>(
        &mut self,
        key: &'static str,
        value: &T,
    ) -> Result<()> {
        self.field(key, value)
    }
    fn end(self) -> Result<Node> {
        Ok(self.finish())
    }
}

// ---------------------------------------------------------------------------
// Text

/// Whether a plain scalar with this text reads back as this string.
fn reads_as_string(text: &str) -> bool {
    struct IsString;
    impl serde::de::Visitor<'_> for IsString {
        type Value = bool;
        fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
            f.write_str("anything")
        }
        fn visit_bool<E>(self, _: bool) -> Result<bool, E> {
            Ok(false)
        }
        fn visit_i64<E>(self, _: i64) -> Result<bool, E> {
            Ok(false)
        }
        fn visit_i128<E>(self, _: i128) -> Result<bool, E> {
            Ok(false)
        }
        fn visit_u64<E>(self, _: u64) -> Result<bool, E> {
            Ok(false)
        }
        fn visit_u128<E>(self, _: u128) -> Result<bool, E> {
            Ok(false)
        }
        fn visit_f64<E>(self, _: f64) -> Result<bool, E> {
            Ok(false)
        }
        fn visit_unit<E>(self) -> Result<bool, E> {
            Ok(false)
        }
        fn visit_str<E>(self, _: &str) -> Result<bool, E> {
            Ok(true)
        }
    }
    !digits_but_not_number(text) && visit_untagged_scalar(IsString, text).unwrap_or(false)
}

/// Whether `text` can be written as a plain scalar in block context and read
/// back unchanged. Deliberately narrower than YAML allows.
fn plain_allowed(text: &str) -> bool {
    let Some(first) = text.chars().next() else {
        return false;
    };
    if matches!(
        first,
        '-' | '?'
            | ':'
            | ','
            | '['
            | ']'
            | '{'
            | '}'
            | '#'
            | '&'
            | '*'
            | '!'
            | '|'
            | '>'
            | '\''
            | '"'
            | '%'
            | '@'
            | '`'
    ) {
        return false;
    }
    // `...` and `---` at the start of a line end or start a document.
    if text.starts_with("...") || text.starts_with("---") {
        return false;
    }
    if text.starts_with(' ') || text.ends_with(' ') || text.ends_with(':') {
        return false;
    }
    if text.contains(": ") || text.contains(" #") {
        return false;
    }
    if text
        .chars()
        .any(|c| !is_printable(c) || c == '\t' || c == '\u{feff}')
    {
        return false;
    }
    if text.contains(['\n', '\r']) {
        return false;
    }
    reads_as_string(text)
}

/// The characters written unescaped: YAML 1.2 `c-printable` without the
/// line breaks, and without NEL, LINE SEPARATOR and PARAGRAPH SEPARATOR,
/// which YAML 1.1 readers such as libyaml take for line breaks.
fn is_printable(c: char) -> bool {
    matches!(c,
        '\u{20}'..='\u{7e}'
        | '\u{a0}'..='\u{2027}'
        | '\u{202a}'..='\u{d7ff}'
        | '\u{e000}'..='\u{fffd}'
        | '\u{10000}'..='\u{10ffff}')
}

fn single_quoted(text: &str) -> Option<String> {
    if text.chars().any(|c| !is_printable(c) || c == '\u{feff}') {
        return None;
    }
    // Leading or trailing spaces are kept inside quotes; a single-quoted
    // scalar on one line has no folding.
    Some(format!("'{}'", text.replace('\'', "''")))
}

fn double_quoted(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\r' => out.push_str("\\r"),
            c if is_printable(c) && c != '\u{feff}' => out.push(c),
            c if (c as u32) <= 0xff => out.push_str(&format!("\\x{:02x}", c as u32)),
            c if (c as u32) <= 0xffff => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push_str(&format!("\\U{:08x}", c as u32)),
        }
    }
    out.push('"');
    out
}

/// A string on one line: plain, single-quoted or double-quoted.
fn inline_string(text: &str) -> String {
    if plain_allowed(text) {
        return text.to_owned();
    }
    if !text.contains(['\n', '\r']) {
        if let Some(quoted) = single_quoted(text) {
            return quoted;
        }
    }
    double_quoted(text)
}

/// A literal block for a string with line breaks, when one can hold it.
fn literal_block(text: &str, indent: usize) -> Option<String> {
    if !text.contains('\n') || text.contains('\r') {
        return None;
    }
    if text
        .chars()
        .any(|c| c != '\n' && (!is_printable(c) && c != '\t' || c == '\u{feff}'))
    {
        return None;
    }
    // The block's indentation is taken from its first line with content, so
    // that line and the empty lines before it must not start with white
    // space; text that is only line breaks needs quotes too.
    if text.trim_matches('\n').is_empty() {
        return None;
    }
    for line in text.split('\n') {
        if line.starts_with([' ', '\t']) {
            return None;
        }
        if !line.is_empty() {
            break;
        }
    }
    let body = text.trim_end_matches('\n');
    let trailing = text.len() - body.len();
    let chomp = match trailing {
        0 => "-",
        1 => "",
        _ => "+",
    };
    let pad = " ".repeat(indent);
    let mut out = format!("|{chomp}");
    let lines: Vec<&str> = if trailing > 1 {
        // Keep every trailing line break: the empty lines are content.
        text[..text.len() - 1].split('\n').collect()
    } else {
        body.split('\n').collect()
    };
    for line in lines {
        out.push('\n');
        if !line.is_empty() {
            out.push_str(&pad);
            out.push_str(line);
        }
    }
    Some(out)
}

fn scalar_text(node: &Node) -> Option<String> {
    match node {
        Node::Raw(text) => Some(text.clone()),
        Node::Str(text) => Some(inline_string(text)),
        _ => None,
    }
}

fn render_document(node: &Node, out: &mut String) {
    match node {
        Node::Seq(items) if !items.is_empty() => render_seq(items, 0, out),
        Node::Map(entries) if !entries.is_empty() => render_map(entries, 0, out),
        _ => {
            render_inline_value(node, 0, out);
            out.push('\n');
        }
    }
}

/// A value after `key:` or `- `: scalars, empty collections and literal
/// blocks stay on the line; other collections start on the next line.
fn render_inline_value(node: &Node, indent: usize, out: &mut String) {
    match node {
        Node::Str(text) => match literal_block(text, indent + 2) {
            Some(block) => out.push_str(&block),
            None => out.push_str(&inline_string(text)),
        },
        Node::Raw(text) => out.push_str(text),
        Node::Seq(items) if items.is_empty() => out.push_str("[]"),
        Node::Map(entries) if entries.is_empty() => out.push_str("{}"),
        Node::Tagged(tag, inner) => {
            out.push_str(tag);
            match inner.as_ref() {
                Node::Seq(items) if !items.is_empty() => {
                    out.push('\n');
                    render_seq(items, indent + 2, out);
                    // render_seq ends with a line break; the caller adds one.
                    out.pop();
                }
                Node::Map(entries) if !entries.is_empty() => {
                    out.push('\n');
                    render_map(entries, indent + 2, out);
                    out.pop();
                }
                other => {
                    out.push(' ');
                    render_inline_value(other, indent, out);
                }
            }
        }
        Node::Seq(_) | Node::Map(_) => unreachable_collection(out),
    }
}

fn unreachable_collection(out: &mut String) {
    // Non-empty collections are rendered by render_seq / render_map; reaching
    // here would be a bug in this module, so write an empty flow collection
    // rather than panicking.
    out.push_str("{}");
}

fn render_seq(items: &[Node], indent: usize, out: &mut String) {
    let pad = " ".repeat(indent);
    for item in items {
        out.push_str(&pad);
        out.push('-');
        match item {
            Node::Seq(inner) if !inner.is_empty() => {
                out.push(' ');
                let mut nested = String::new();
                render_seq(inner, indent + 2, &mut nested);
                out.push_str(&nested[indent + 2..]);
            }
            Node::Map(entries) if !entries.is_empty() => {
                out.push(' ');
                let mut nested = String::new();
                render_map(entries, indent + 2, &mut nested);
                out.push_str(&nested[indent + 2..]);
            }
            other => {
                out.push(' ');
                render_inline_value(other, indent, out);
                out.push('\n');
            }
        }
    }
}

/// YAML limits an implicit key to 1024 characters.
const MAX_SIMPLE_KEY_CHARS: usize = 1000;

fn render_map(entries: &[(Node, Node)], indent: usize, out: &mut String) {
    let pad = " ".repeat(indent);
    for (key, value) in entries {
        out.push_str(&pad);
        let key_text = scalar_text(key).unwrap_or_else(|| "null".into());
        if key_text.chars().count() > MAX_SIMPLE_KEY_CHARS {
            // A key this long cannot be an implicit key; write it explicitly.
            out.push_str("? ");
            out.push_str(&key_text);
            out.push('\n');
            out.push_str(&pad);
        } else {
            out.push_str(&key_text);
        }
        out.push(':');
        match value {
            Node::Seq(items) if !items.is_empty() => {
                out.push('\n');
                render_seq(items, indent + 2, out);
            }
            Node::Map(inner) if !inner.is_empty() => {
                out.push('\n');
                render_map(inner, indent + 2, out);
            }
            other => {
                out.push(' ');
                render_inline_value(other, indent, out);
                out.push('\n');
            }
        }
    }
}
