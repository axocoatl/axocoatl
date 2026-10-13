//! A JSON reader for request bodies the provider-fetch check reads
//! ([`super::provider_fetch`]): strict where two parsers could read one
//! body differently, lenient where JavaScript clients write what
//! `serde_json` refuses.
//!
//! - A key that appears twice in one object is refused. Parsers disagree
//!   on which copy wins, so `{"tools": [server tool], "tools": []}` could
//!   be checked as one and read by the provider as the other.
//! - Everything RFC 8259 does not allow is refused: a byte-order mark,
//!   comments, trailing commas, `NaN` or `Infinity`, single quotes, control
//!   characters inside strings, invalid UTF-8, text after the value, and
//!   nesting deeper than the caller allows.
//! - A `\u` escape of a lone UTF-16 surrogate is read as U+FFFD instead of
//!   refused. `JSON.stringify` writes one for a string cut inside a
//!   surrogate pair (a truncated tool output, for example), and providers
//!   accept it; `serde_json` refuses it when it reads a string.
//!
//! Numbers are checked against the grammar and not kept: nothing the check
//! decides depends on one.

/// A parsed value. Objects keep their keys in order, each once.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Value {
    Null,
    Bool(bool),
    Number,
    String(String),
    Array(Vec<Value>),
    Object(Vec<(String, Value)>),
}

impl Value {
    /// The value of `key`, when this is an object that has it.
    pub fn get(&self, key: &str) -> Option<&Value> {
        match self {
            Self::Object(entries) => entries
                .iter()
                .find(|(name, _)| name == key)
                .map(|(_, value)| value),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Self::String(text) => Some(text),
            _ => None,
        }
    }
}

/// Why a body is not JSON this reader accepts, and the byte offset where
/// it found out. Never holds any of the body.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{reason} at byte {offset}")]
pub struct ParseError {
    pub reason: &'static str,
    pub offset: usize,
}

/// Parse `bytes` as one JSON value nested at most `max_depth` arrays and
/// objects deep.
pub fn parse(bytes: &[u8], max_depth: usize) -> Result<Value, ParseError> {
    let mut parser = Parser {
        bytes,
        at: 0,
        max_depth,
    };
    parser.skip_whitespace();
    let value = parser.value(0)?;
    parser.skip_whitespace();
    if parser.at != bytes.len() {
        return Err(parser.error("text after the JSON value"));
    }
    Ok(value)
}

struct Parser<'a> {
    bytes: &'a [u8],
    at: usize,
    max_depth: usize,
}

impl Parser<'_> {
    fn error(&self, reason: &'static str) -> ParseError {
        ParseError {
            reason,
            offset: self.at,
        }
    }

    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.at).copied()
    }

    fn skip_whitespace(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            self.at += 1;
        }
    }

    fn literal(&mut self, word: &'static [u8], value: Value) -> Result<Value, ParseError> {
        if self.bytes[self.at..].starts_with(word) {
            self.at += word.len();
            Ok(value)
        } else {
            Err(self.error("an unknown literal"))
        }
    }

    fn value(&mut self, depth: usize) -> Result<Value, ParseError> {
        match self.peek() {
            None => Err(self.error("the body ends before a value")),
            Some(b'{') => self.object(depth + 1),
            Some(b'[') => self.array(depth + 1),
            Some(b'"') => self.string().map(Value::String),
            Some(b't') => self.literal(b"true", Value::Bool(true)),
            Some(b'f') => self.literal(b"false", Value::Bool(false)),
            Some(b'n') => self.literal(b"null", Value::Null),
            Some(b'-' | b'0'..=b'9') => self.number(),
            Some(_) => Err(self.error("a character that starts no JSON value")),
        }
    }

    fn enter(&self, depth: usize) -> Result<(), ParseError> {
        if depth > self.max_depth {
            Err(self.error("nesting deeper than the check reads"))
        } else {
            Ok(())
        }
    }

    fn object(&mut self, depth: usize) -> Result<Value, ParseError> {
        self.enter(depth)?;
        self.at += 1;
        let mut entries: Vec<(String, Value)> = Vec::new();
        let mut seen = std::collections::HashSet::new();
        self.skip_whitespace();
        if self.peek() == Some(b'}') {
            self.at += 1;
            return Ok(Value::Object(entries));
        }
        loop {
            self.skip_whitespace();
            if self.peek() != Some(b'"') {
                return Err(self.error("an object key that is not a string"));
            }
            let key_at = self.at;
            let key = self.string()?;
            if !seen.insert(key.clone()) {
                return Err(ParseError {
                    reason: "a key that appears twice in one object",
                    offset: key_at,
                });
            }
            self.skip_whitespace();
            if self.peek() != Some(b':') {
                return Err(self.error("an object key without ':'"));
            }
            self.at += 1;
            self.skip_whitespace();
            let value = self.value(depth)?;
            entries.push((key, value));
            self.skip_whitespace();
            match self.peek() {
                Some(b',') => self.at += 1,
                Some(b'}') => {
                    self.at += 1;
                    return Ok(Value::Object(entries));
                }
                _ => return Err(self.error("an object that does not continue with ',' or '}'")),
            }
        }
    }

    fn array(&mut self, depth: usize) -> Result<Value, ParseError> {
        self.enter(depth)?;
        self.at += 1;
        let mut items = Vec::new();
        self.skip_whitespace();
        if self.peek() == Some(b']') {
            self.at += 1;
            return Ok(Value::Array(items));
        }
        loop {
            self.skip_whitespace();
            items.push(self.value(depth)?);
            self.skip_whitespace();
            match self.peek() {
                Some(b',') => self.at += 1,
                Some(b']') => {
                    self.at += 1;
                    return Ok(Value::Array(items));
                }
                _ => return Err(self.error("an array that does not continue with ',' or ']'")),
            }
        }
    }

    fn digits(&mut self) -> usize {
        let start = self.at;
        while matches!(self.peek(), Some(b'0'..=b'9')) {
            self.at += 1;
        }
        self.at - start
    }

    fn number(&mut self) -> Result<Value, ParseError> {
        if self.peek() == Some(b'-') {
            self.at += 1;
        }
        match self.peek() {
            Some(b'0') => {
                self.at += 1;
                if matches!(self.peek(), Some(b'0'..=b'9')) {
                    return Err(self.error("a number with a leading zero"));
                }
            }
            Some(b'1'..=b'9') => {
                self.digits();
            }
            _ => return Err(self.error("a number without digits")),
        }
        if self.peek() == Some(b'.') {
            self.at += 1;
            if self.digits() == 0 {
                return Err(self.error("a number without digits after '.'"));
            }
        }
        if matches!(self.peek(), Some(b'e' | b'E')) {
            self.at += 1;
            if matches!(self.peek(), Some(b'+' | b'-')) {
                self.at += 1;
            }
            if self.digits() == 0 {
                return Err(self.error("a number without digits in its exponent"));
            }
        }
        Ok(Value::Number)
    }

    fn hex4(&mut self) -> Result<u32, ParseError> {
        let digits = self
            .bytes
            .get(self.at..self.at + 4)
            .ok_or_else(|| self.error("a \\u escape with fewer than four digits"))?;
        let mut code = 0u32;
        for &digit in digits {
            let value = match digit {
                b'0'..=b'9' => digit - b'0',
                b'a'..=b'f' => digit - b'a' + 10,
                b'A'..=b'F' => digit - b'A' + 10,
                _ => return Err(self.error("a \\u escape with a character that is not hex")),
            };
            code = code * 16 + u32::from(value);
        }
        self.at += 4;
        Ok(code)
    }

    fn string(&mut self) -> Result<String, ParseError> {
        self.at += 1;
        let mut out = String::new();
        loop {
            // Copy the run of plain bytes up to the next quote, escape or
            // control byte, checking that it is UTF-8.
            let start = self.at;
            while let Some(byte) = self.peek() {
                if byte == b'"' || byte == b'\\' || byte < 0x20 {
                    break;
                }
                self.at += 1;
            }
            let run =
                std::str::from_utf8(&self.bytes[start..self.at]).map_err(|error| ParseError {
                    reason: "a string that is not UTF-8",
                    offset: start + error.valid_up_to(),
                })?;
            out.push_str(run);
            match self.peek() {
                None => return Err(self.error("a string that does not end")),
                Some(b'"') => {
                    self.at += 1;
                    return Ok(out);
                }
                Some(b'\\') => {
                    self.at += 1;
                    let Some(escape) = self.peek() else {
                        return Err(self.error("a string that ends inside an escape"));
                    };
                    self.at += 1;
                    match escape {
                        b'"' => out.push('"'),
                        b'\\' => out.push('\\'),
                        b'/' => out.push('/'),
                        b'b' => out.push('\u{8}'),
                        b'f' => out.push('\u{c}'),
                        b'n' => out.push('\n'),
                        b'r' => out.push('\r'),
                        b't' => out.push('\t'),
                        b'u' => {
                            let code = self.hex4()?;
                            out.push(self.code_point(code)?);
                        }
                        _ => return Err(self.error("an unknown escape in a string")),
                    }
                }
                Some(_) => return Err(self.error("a control character inside a string")),
            }
        }
    }

    /// The character a `\u` escape of `code` stands for, reading the low
    /// half of a surrogate pair when one follows. A lone surrogate is
    /// U+FFFD.
    fn code_point(&mut self, code: u32) -> Result<char, ParseError> {
        if (0xD800..0xDC00).contains(&code) && self.bytes[self.at..].starts_with(b"\\u") {
            let saved = self.at;
            self.at += 2;
            let low = self.hex4()?;
            if (0xDC00..0xE000).contains(&low) {
                let combined = 0x10000 + ((code - 0xD800) << 10) + (low - 0xDC00);
                return Ok(char::from_u32(combined).unwrap_or('\u{FFFD}'));
            }
            // Not a low half: this one is lone, and the next escape is read
            // on its own.
            self.at = saved;
        }
        Ok(char::from_u32(code).unwrap_or('\u{FFFD}'))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok(text: &str) -> Value {
        parse(text.as_bytes(), 64).unwrap_or_else(|error| panic!("{text}: {error}"))
    }

    fn refused(text: &[u8]) -> &'static str {
        parse(text, 64).unwrap_err().reason
    }

    #[test]
    fn reads_what_serde_json_reads_the_same_way() {
        let samples = [
            r#"{"a":[1,-2.5e+3,0,0.25,true,false,null,"x\u00e9\ud83d\ude00\n"],"b":{}}"#,
            "  [ ]  ",
            r#""\"\\\/\b\f\n\r\t""#,
            "-0",
            r#"{"tools":[{"type":"function","name":"x","parameters":{"type":"object"}}]}"#,
        ];
        for sample in samples {
            let ours = ok(sample);
            let theirs: serde_json::Value = serde_json::from_str(sample).unwrap();
            assert_eq!(shape(&ours), shape_serde(&theirs), "{sample}");
        }
    }

    fn shape(value: &Value) -> String {
        match value {
            Value::Null => "null".into(),
            Value::Bool(flag) => flag.to_string(),
            Value::Number => "n".into(),
            Value::String(text) => format!("{text:?}"),
            Value::Array(items) => format!(
                "[{}]",
                items.iter().map(shape).collect::<Vec<_>>().join(",")
            ),
            // serde_json's map is sorted; compare without the order.
            Value::Object(entries) => {
                let mut fields: Vec<String> = entries
                    .iter()
                    .map(|(key, value)| format!("{key:?}:{}", shape(value)))
                    .collect();
                fields.sort();
                format!("{{{}}}", fields.join(","))
            }
        }
    }

    fn shape_serde(value: &serde_json::Value) -> String {
        match value {
            serde_json::Value::Null => "null".into(),
            serde_json::Value::Bool(flag) => flag.to_string(),
            serde_json::Value::Number(_) => "n".into(),
            serde_json::Value::String(text) => format!("{text:?}"),
            serde_json::Value::Array(items) => format!(
                "[{}]",
                items.iter().map(shape_serde).collect::<Vec<_>>().join(",")
            ),
            serde_json::Value::Object(entries) => {
                let mut fields: Vec<String> = entries
                    .iter()
                    .map(|(key, value)| format!("{key:?}:{}", shape_serde(value)))
                    .collect();
                fields.sort();
                format!("{{{}}}", fields.join(","))
            }
        }
    }

    #[test]
    fn a_key_twice_in_one_object_is_refused() {
        // serde_json keeps the last copy; a provider may read the first.
        let error = parse(br#"{"tools":[{"type":"web_search"}],"tools":[]}"#, 64).unwrap_err();
        assert_eq!(error.reason, "a key that appears twice in one object");
        assert_eq!(error.offset, 33);
    }

    #[test]
    fn duplicates_are_compared_after_unescaping() {
        assert_eq!(
            refused(br#"{"tools":[],"t\u006fols":[{"type":"web_search"}]}"#),
            "a key that appears twice in one object"
        );
        // The same key in different objects is fine.
        ok(r#"[{"tools":[]},{"tools":[]}]"#);
    }

    #[test]
    fn lone_surrogates_read_as_replacement_characters() {
        // JSON.stringify("a\ud800") and a low half alone.
        assert_eq!(ok(r#""a\ud800""#), Value::String("a\u{FFFD}".into()));
        assert_eq!(ok(r#""\udc00b""#), Value::String("\u{FFFD}b".into()));
        // A high half followed by another escape that is not a low half.
        assert_eq!(ok(r#""\ud800\u0041""#), Value::String("\u{FFFD}A".into()));
        // serde_json refuses these when it reads a string.
        assert!(serde_json::from_str::<serde_json::Value>(r#""a\ud800""#).is_err());
    }

    #[test]
    fn what_rfc_8259_does_not_allow_is_refused() {
        for (text, reason) in [
            (
                &b"\xEF\xBB\xBF{}"[..],
                "a character that starts no JSON value",
            ),
            (b"{} {}", "text after the JSON value"),
            (b"[1,]", "a character that starts no JSON value"),
            (b"{\"a\":1,}", "an object key that is not a string"),
            (b"{'a':1}", "an object key that is not a string"),
            (b"[NaN]", "a character that starts no JSON value"),
            (b"[Infinity]", "a character that starts no JSON value"),
            (b"[01]", "a number with a leading zero"),
            (b"[1.]", "a number without digits after '.'"),
            (b"[1e]", "a number without digits in its exponent"),
            (b"\"a\tb\"", "a control character inside a string"),
            (b"\"\\x\"", "an unknown escape in a string"),
            (
                b"\"\\u12zz\"",
                "a \\u escape with a character that is not hex",
            ),
            (b"\"\\u12", "a \\u escape with fewer than four digits"),
            (b"\"\xff\"", "a string that is not UTF-8"),
            (b"// c\n{}", "a character that starts no JSON value"),
            (b"", "the body ends before a value"),
            (b"\"abc", "a string that does not end"),
            (b"[tru]", "an unknown literal"),
        ] {
            assert_eq!(refused(text), reason, "{}", String::from_utf8_lossy(text));
        }
    }

    #[test]
    fn nesting_is_bounded() {
        let deep = format!("{}{}", "[".repeat(65), "]".repeat(65));
        assert_eq!(
            refused(deep.as_bytes()),
            "nesting deeper than the check reads"
        );
        let fine = format!("{}{}", "[".repeat(64), "]".repeat(64));
        ok(&fine);
    }
}
