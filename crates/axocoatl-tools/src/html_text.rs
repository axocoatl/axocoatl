//! Readable text from a fetched page, as numbered paragraphs, for `web_fetch`.
//!
//! This is a small in-house extractor, not an HTML5 parser. It reads the
//! input once, left to right, so its time is linear in the input: raw-text
//! elements (`script`, `style`, `title`) are skipped by a forward search for
//! their closing tag, never by backtracking. It drops markup that is not page
//! text (`script`, `style`, `noscript`, `template`, `svg`, `iframe`,
//! `object`, and `head` apart from its `title`), starts a new paragraph at
//! block elements, decodes character references and collapses whitespace
//! outside `pre`. Broken markup degrades to more or fewer paragraph breaks;
//! it never fails.

/// Most input bytes one extraction reads. Longer input is cut at a character
/// boundary first.
pub const MAX_EXTRACT_INPUT_BYTES: usize = 4 * 1024 * 1024;

/// Most bytes kept for a page title.
const TITLE_MAX_BYTES: usize = 1024;

/// The text of one page.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ExtractedPage {
    /// The first `<title>`, decoded and with whitespace collapsed.
    pub title: Option<String>,
    /// Non-empty paragraphs in document order. Paragraph `n` is
    /// `paragraphs[n - 1]`.
    pub paragraphs: Vec<String>,
    /// The input was longer than [`MAX_EXTRACT_INPUT_BYTES`] and only its
    /// prefix was read.
    pub input_truncated: bool,
}

/// Elements whose start or end begins a new paragraph.
const BLOCK_ELEMENTS: &[&str] = &[
    "p",
    "div",
    "br",
    "li",
    "dt",
    "dd",
    "h1",
    "h2",
    "h3",
    "h4",
    "h5",
    "h6",
    "tr",
    "section",
    "article",
    "header",
    "footer",
    "nav",
    "pre",
    "blockquote",
    "hr",
    "table",
    "form",
    "ul",
    "ol",
    "dl",
    "main",
    "aside",
    "figure",
    "figcaption",
    "details",
    "summary",
    "caption",
    "address",
    "fieldset",
    "legend",
    "body",
    "html",
];

/// Elements whose whole content is dropped. Their content is markup, so the
/// tokenizer still reads it, but no text inside reaches the output.
const DROPPED_CONTAINERS: &[&str] = &[
    "noscript", "template", "svg", "iframe", "object", "head", "math", "noframes",
];

/// Start tags that may appear inside `head` (the HTML "in head" insertion
/// mode). Any other start tag, or text that is not whitespace, ends a `head`
/// whose `</head>` and `<body>` were left out, as both may be.
const HEAD_CONTENT: &[&str] = &[
    "base", "basefont", "bgsound", "link", "meta", "title", "noscript", "noframes", "style",
    "script", "template", "head",
];

/// Elements whose content is raw text up to the matching end tag.
const RAW_TEXT_ELEMENTS: &[&str] = &["script", "style", "textarea", "xmp"];

/// Most nested dropped containers tracked. Deeper nesting stays dropped.
const MAX_DROP_DEPTH: usize = 256;

struct Builder {
    paragraphs: Vec<String>,
    current: String,
    pending_space: bool,
    pre_depth: u32,
}

impl Builder {
    fn new() -> Self {
        Self {
            paragraphs: Vec::new(),
            current: String::new(),
            pending_space: false,
            pre_depth: 0,
        }
    }

    fn push_text(&mut self, text: &str) {
        if self.pre_depth > 0 {
            self.current.push_str(text);
            return;
        }
        let bytes = text.as_bytes();
        let mut index = 0;
        while index < bytes.len() {
            if is_space(bytes[index]) {
                self.pending_space = true;
                index += 1;
                continue;
            }
            let start = index;
            while index < bytes.len() && !is_space(bytes[index]) {
                index += 1;
            }
            if self.pending_space && !self.current.is_empty() {
                self.current.push(' ');
            }
            self.pending_space = false;
            // ASCII whitespace bytes are always character boundaries.
            self.current.push_str(&text[start..index]);
        }
    }

    /// Separate cells and similar inline neighbours with one space.
    fn soft_break(&mut self) {
        if self.pre_depth == 0 {
            self.pending_space = true;
        } else {
            self.current.push(' ');
        }
    }

    fn break_paragraph(&mut self) {
        let text = std::mem::take(&mut self.current);
        self.pending_space = false;
        let kept = if self.pre_depth > 0 || text.contains('\n') {
            trim_block(&text)
        } else {
            text.trim()
        };
        if !kept.is_empty() {
            self.paragraphs.push(kept.to_string());
        }
    }

    fn finish(mut self) -> Vec<String> {
        self.break_paragraph();
        self.paragraphs
    }
}

/// Trim a preformatted block: drop leading and trailing blank lines and
/// trailing spaces, keep interior layout.
fn trim_block(text: &str) -> &str {
    let text = text.trim_end();
    let start = text
        .char_indices()
        .find(|(_, c)| !c.is_whitespace())
        .map(|(index, _)| text[..index].rfind('\n').map_or(0, |line| line + 1))
        .unwrap_or(text.len());
    &text[start..]
}

fn is_space(byte: u8) -> bool {
    matches!(byte, b' ' | b'\t' | b'\n' | b'\r' | 0x0c)
}

/// Extract the title and paragraphs of an HTML document.
pub fn html_to_paragraphs(input: &str) -> ExtractedPage {
    let (input, input_truncated) = bounded(input);
    let bytes = input.as_bytes();
    let mut builder = Builder::new();
    let mut title: Option<String> = None;
    // Names of open dropped containers, innermost last.
    let mut dropped: Vec<&'static str> = Vec::new();
    let mut index = 0;

    while index < bytes.len() {
        let next = memchr::memchr(b'<', &bytes[index..]).map_or(bytes.len(), |at| index + at);
        if next > index {
            let text = &input[index..next];
            if in_head(&dropped) && !text.bytes().all(is_space) {
                // Text ends a head whose end was left out.
                dropped.pop();
            }
            if dropped.is_empty() {
                push_decoded(&mut builder, text);
            }
            index = next;
            continue;
        }
        // `bytes[index]` is '<'.
        let rest = &bytes[index..];
        if rest.starts_with(b"<!--") {
            index = memchr::memmem::find(&rest[4..], b"-->")
                .map_or(bytes.len(), |end| index + 4 + end + 3);
            continue;
        }
        if rest.starts_with(b"<!") || rest.starts_with(b"<?") {
            index = memchr::memchr(b'>', rest).map_or(bytes.len(), |end| index + end + 1);
            continue;
        }
        let closing = rest.get(1) == Some(&b'/');
        let name_start = index + if closing { 2 } else { 1 };
        let name_end = tag_name_end(bytes, name_start);
        if name_end == name_start {
            // Not a tag: a literal '<'.
            if dropped.is_empty() {
                builder.push_text("<");
            }
            index += 1;
            continue;
        }
        let name = ascii_lowercase(&input[name_start..name_end]);
        let (after, self_closing) = tag_end(bytes, name_end);
        index = after;

        if closing {
            close_tag(&name, &mut builder, &mut dropped);
            continue;
        }

        if in_head(&dropped) && !HEAD_CONTENT.contains(&name.as_str()) {
            // A start tag that cannot be in head ends it; the tag is then
            // read as body content.
            dropped.pop();
        }

        if RAW_TEXT_ELEMENTS.contains(&name.as_str()) || name == "title" {
            if self_closing {
                continue;
            }
            let (content_end, resume) = raw_text_end(bytes, index, &name);
            let content = &input[index..content_end];
            index = resume;
            if name == "title" {
                let only_head = dropped.iter().all(|open| *open == "head");
                if title.is_none() && only_head {
                    let mut text = Builder::new();
                    push_decoded(&mut text, content);
                    let text = text.finish().join(" ");
                    if !text.is_empty() {
                        title = Some(crate::limits::limit_text(text, TITLE_MAX_BYTES).text);
                    }
                }
            } else if dropped.is_empty() && matches!(name.as_str(), "textarea" | "xmp") {
                builder.break_paragraph();
                push_decoded(&mut builder, content);
                builder.break_paragraph();
            }
            continue;
        }

        if name == "body" {
            // A body start ends an unclosed head.
            if let Some(head) = dropped.iter().position(|open| *open == "head") {
                dropped.truncate(head);
            }
        }
        if let Some(container) = DROPPED_CONTAINERS.iter().find(|item| **item == name) {
            if !self_closing && dropped.len() < MAX_DROP_DEPTH {
                dropped.push(container);
            }
            continue;
        }
        if !dropped.is_empty() {
            continue;
        }
        open_tag(&name, self_closing, &mut builder);
    }

    ExtractedPage {
        title,
        paragraphs: builder.finish(),
        input_truncated,
    }
}

/// Whether the innermost open dropped container is `head`, so its content
/// is read in the "in head" mode.
fn in_head(dropped: &[&'static str]) -> bool {
    dropped.last() == Some(&"head")
}

fn open_tag(name: &str, self_closing: bool, builder: &mut Builder) {
    if BLOCK_ELEMENTS.contains(&name) {
        builder.break_paragraph();
        if name == "pre" && !self_closing {
            builder.pre_depth += 1;
        }
    } else if matches!(name, "td" | "th") {
        builder.soft_break();
    }
}

fn close_tag(name: &str, builder: &mut Builder, dropped: &mut Vec<&'static str>) {
    if !dropped.is_empty() {
        // Close the innermost matching container; an unmatched end tag inside
        // a dropped region changes nothing.
        if let Some(open) = dropped.iter().rposition(|open| *open == name) {
            dropped.truncate(open);
        }
        return;
    }
    if BLOCK_ELEMENTS.contains(&name) {
        if name == "pre" {
            builder.break_paragraph();
            builder.pre_depth = builder.pre_depth.saturating_sub(1);
        } else {
            builder.break_paragraph();
        }
    } else if matches!(name, "td" | "th") {
        builder.soft_break();
    }
}

/// End of an ASCII tag name starting at `start`, or `start` when none.
fn tag_name_end(bytes: &[u8], start: usize) -> usize {
    match bytes.get(start) {
        Some(first) if first.is_ascii_alphabetic() => {}
        _ => return start,
    }
    let mut end = start + 1;
    while end < bytes.len()
        && (bytes[end].is_ascii_alphanumeric() || matches!(bytes[end], b'-' | b'_' | b':'))
    {
        end += 1;
    }
    end
}

/// Skip a tag's attributes, starting after its name. Returns the index after
/// `>` and whether the tag ended with `/>`. Quoted attribute values may
/// contain `>`.
fn tag_end(bytes: &[u8], start: usize) -> (usize, bool) {
    let mut index = start;
    loop {
        let Some(offset) = memchr::memchr3(b'>', b'"', b'\'', &bytes[index.min(bytes.len())..])
        else {
            return (bytes.len(), false);
        };
        let at = index + offset;
        match bytes[at] {
            b'>' => {
                let self_closing = at > 0 && bytes[at - 1] == b'/';
                return (at + 1, self_closing);
            }
            quote if starts_attribute_value(bytes, start, at) => {
                match memchr::memchr(quote, &bytes[at + 1..]) {
                    Some(close) => index = at + 1 + close + 1,
                    None => return (bytes.len(), false),
                }
            }
            // A quote anywhere else, such as the apostrophe in an unquoted
            // `title=Bob's`, is an ordinary character, as browsers read it.
            _ => index = at + 1,
        }
    }
}

/// Whether the quote at `at` opens an attribute value: the last byte before
/// it, past any whitespace and not before `start`, is `=`. Each call steps
/// back only over the whitespace directly before one quote, so a tag is
/// still read in linear time.
fn starts_attribute_value(bytes: &[u8], start: usize, at: usize) -> bool {
    let mut before = at;
    while before > start && is_space(bytes[before - 1]) {
        before -= 1;
    }
    before > start && bytes[before - 1] == b'='
}

/// For a raw-text element starting at `start`, the end of its content and
/// the index after its end tag. Searches forward only.
fn raw_text_end(bytes: &[u8], start: usize, name: &str) -> (usize, usize) {
    let mut from = start;
    let finder = memchr::memmem::Finder::new(b"</");
    while let Some(offset) = finder.find(&bytes[from..]) {
        let at = from + offset;
        let candidate = at + 2;
        let end = candidate + name.len();
        if end <= bytes.len()
            && bytes[candidate..end].eq_ignore_ascii_case(name.as_bytes())
            && bytes
                .get(end)
                .is_none_or(|byte| is_space(*byte) || matches!(byte, b'>' | b'/'))
        {
            let resume = memchr::memchr(b'>', &bytes[end..]).map_or(bytes.len(), |gt| end + gt + 1);
            return (at, resume);
        }
        from = at + 2;
    }
    (bytes.len(), bytes.len())
}

fn ascii_lowercase(name: &str) -> String {
    name.to_ascii_lowercase()
}

fn push_decoded(builder: &mut Builder, text: &str) {
    if memchr::memchr(b'&', text.as_bytes()).is_none() {
        builder.push_text(text);
    } else {
        builder.push_text(&decode_entities(text));
    }
}

/// Cut `input` to [`MAX_EXTRACT_INPUT_BYTES`] at a character boundary.
fn bounded(input: &str) -> (&str, bool) {
    if input.len() <= MAX_EXTRACT_INPUT_BYTES {
        return (input, false);
    }
    let mut end = MAX_EXTRACT_INPUT_BYTES;
    while !input.is_char_boundary(end) {
        end -= 1;
    }
    (&input[..end], true)
}

/// Paragraphs of plain text or Markdown: blocks separated by blank lines.
/// Line breaks inside a block are kept.
pub fn plain_to_paragraphs(input: &str) -> ExtractedPage {
    let (input, input_truncated) = bounded(input);
    let mut paragraphs = Vec::new();
    let mut current = String::new();
    for line in input.lines() {
        let line = line.trim_end();
        if line.trim().is_empty() {
            if !current.is_empty() {
                paragraphs.push(std::mem::take(&mut current));
            }
            continue;
        }
        if !current.is_empty() {
            current.push('\n');
        }
        current.push_str(line);
    }
    if !current.is_empty() {
        paragraphs.push(current);
    }
    ExtractedPage {
        title: None,
        paragraphs,
        input_truncated,
    }
}

/// A JSON or XML body passed through as one block.
pub fn single_block(input: &str) -> ExtractedPage {
    let (input, input_truncated) = bounded(input);
    let text = input.trim();
    ExtractedPage {
        title: None,
        paragraphs: if text.is_empty() {
            Vec::new()
        } else {
            vec![text.to_string()]
        },
        input_truncated,
    }
}

/// Decode numeric character references and the common named ones. Unknown
/// or malformed references are kept as written.
pub fn decode_entities(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut index = 0;
    while let Some(offset) = memchr::memchr(b'&', &bytes[index..]) {
        let at = index + offset;
        out.push_str(&text[index..at]);
        match decode_reference(&bytes[at + 1..]) {
            Some((decoded, consumed)) => {
                match decoded {
                    Decoded::Char(c) => out.push(c),
                    Decoded::Static(text) => out.push_str(text),
                }
                index = at + 1 + consumed;
            }
            None => {
                out.push('&');
                index = at + 1;
            }
        }
    }
    out.push_str(&text[index..]);
    out
}

enum Decoded {
    Char(char),
    Static(&'static str),
}

/// Decode one reference after its `&`. Returns the text and the bytes used,
/// including the `;`.
fn decode_reference(rest: &[u8]) -> Option<(Decoded, usize)> {
    const MAX_REFERENCE: usize = 32;
    let window = &rest[..rest.len().min(MAX_REFERENCE)];
    let semicolon = memchr::memchr(b';', window)?;
    let body = &window[..semicolon];
    if body.is_empty() {
        return None;
    }
    if body[0] == b'#' {
        let digits = &body[1..];
        let value = if let Some(hex) = digits
            .strip_prefix(b"x")
            .or_else(|| digits.strip_prefix(b"X"))
        {
            if hex.is_empty() || !hex.iter().all(u8::is_ascii_hexdigit) {
                return None;
            }
            u32::from_str_radix(std::str::from_utf8(hex).ok()?, 16).ok()
        } else {
            if digits.is_empty() || !digits.iter().all(u8::is_ascii_digit) {
                return None;
            }
            std::str::from_utf8(digits).ok()?.parse::<u32>().ok()
        };
        let decoded = match value {
            Some(0) | None => '\u{FFFD}',
            Some(value) => char::from_u32(value).unwrap_or('\u{FFFD}'),
        };
        return Some((Decoded::Char(decoded), semicolon + 1));
    }
    let name = std::str::from_utf8(body).ok()?;
    named_entity(name).map(|text| (Decoded::Static(text), semicolon + 1))
}

fn named_entity(name: &str) -> Option<&'static str> {
    Some(match name {
        "amp" => "&",
        "lt" => "<",
        "gt" => ">",
        "quot" => "\"",
        "apos" => "'",
        // A non-breaking space reads as a space in extracted text.
        "nbsp" | "ensp" | "emsp" | "thinsp" => " ",
        "shy" | "zwj" | "zwnj" | "lrm" | "rlm" => "",
        "copy" => "©",
        "reg" => "®",
        "trade" => "™",
        "hellip" => "…",
        "mdash" => "—",
        "ndash" => "–",
        "lsquo" => "‘",
        "rsquo" => "’",
        "sbquo" => "‚",
        "ldquo" => "“",
        "rdquo" => "”",
        "bdquo" => "„",
        "laquo" => "«",
        "raquo" => "»",
        "lsaquo" => "‹",
        "rsaquo" => "›",
        "middot" => "·",
        "bull" => "•",
        "deg" => "°",
        "times" => "×",
        "divide" => "÷",
        "plusmn" => "±",
        "minus" => "−",
        "le" => "≤",
        "ge" => "≥",
        "ne" => "≠",
        "asymp" => "≈",
        "infin" => "∞",
        "larr" => "←",
        "rarr" => "→",
        "uarr" => "↑",
        "darr" => "↓",
        "harr" => "↔",
        "lArr" => "⇐",
        "rArr" => "⇒",
        "hArr" => "⇔",
        "euro" => "€",
        "pound" => "£",
        "yen" => "¥",
        "cent" => "¢",
        "curren" => "¤",
        "sect" => "§",
        "para" => "¶",
        "dagger" => "†",
        "Dagger" => "‡",
        "permil" => "‰",
        "prime" => "′",
        "Prime" => "″",
        "iexcl" => "¡",
        "iquest" => "¿",
        "frac12" => "½",
        "frac14" => "¼",
        "frac34" => "¾",
        "sup1" => "¹",
        "sup2" => "²",
        "sup3" => "³",
        "micro" => "µ",
        "ordf" => "ª",
        "ordm" => "º",
        "not" => "¬",
        "macr" => "¯",
        "acute" => "´",
        "cedil" => "¸",
        "uml" => "¨",
        "brvbar" => "¦",
        "Agrave" => "À",
        "Aacute" => "Á",
        "Acirc" => "Â",
        "Atilde" => "Ã",
        "Auml" => "Ä",
        "Aring" => "Å",
        "AElig" => "Æ",
        "Ccedil" => "Ç",
        "Egrave" => "È",
        "Eacute" => "É",
        "Ecirc" => "Ê",
        "Euml" => "Ë",
        "Igrave" => "Ì",
        "Iacute" => "Í",
        "Icirc" => "Î",
        "Iuml" => "Ï",
        "ETH" => "Ð",
        "Ntilde" => "Ñ",
        "Ograve" => "Ò",
        "Oacute" => "Ó",
        "Ocirc" => "Ô",
        "Otilde" => "Õ",
        "Ouml" => "Ö",
        "Oslash" => "Ø",
        "Ugrave" => "Ù",
        "Uacute" => "Ú",
        "Ucirc" => "Û",
        "Uuml" => "Ü",
        "Yacute" => "Ý",
        "THORN" => "Þ",
        "szlig" => "ß",
        "agrave" => "à",
        "aacute" => "á",
        "acirc" => "â",
        "atilde" => "ã",
        "auml" => "ä",
        "aring" => "å",
        "aelig" => "æ",
        "ccedil" => "ç",
        "egrave" => "è",
        "eacute" => "é",
        "ecirc" => "ê",
        "euml" => "ë",
        "igrave" => "ì",
        "iacute" => "í",
        "icirc" => "î",
        "iuml" => "ï",
        "eth" => "ð",
        "ntilde" => "ñ",
        "ograve" => "ò",
        "oacute" => "ó",
        "ocirc" => "ô",
        "otilde" => "õ",
        "ouml" => "ö",
        "oslash" => "ø",
        "ugrave" => "ù",
        "uacute" => "ú",
        "ucirc" => "û",
        "uuml" => "ü",
        "yacute" => "ý",
        "thorn" => "þ",
        "yuml" => "ÿ",
        "OElig" => "Œ",
        "oelig" => "œ",
        "Scaron" => "Š",
        "scaron" => "š",
        "Yuml" => "Ÿ",
        "fnof" => "ƒ",
        "circ" => "ˆ",
        "tilde" => "˜",
        "alpha" => "α",
        "beta" => "β",
        "gamma" => "γ",
        "delta" => "δ",
        "pi" => "π",
        "sigma" => "σ",
        "mu" => "μ",
        "lambda" => "λ",
        "Omega" => "Ω",
        "omega" => "ω",
        "check" => "✓",
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paragraphs(html: &str) -> Vec<String> {
        html_to_paragraphs(html).paragraphs
    }

    #[test]
    fn script_style_and_hidden_containers_are_dropped() {
        let page = html_to_paragraphs(
            "<html><head><title>T</title><style>p{color:red}</style>\
             <script>var a = '<p>no</p>';</script></head><body>\
             <p>Kept</p><script type=\"text/javascript\">if (a < b) { document.write('</p>') }</script>\
             <noscript><p>enable js</p></noscript><template><p>tpl</p></template>\
             <svg><title>icon</title><text>svg text</text></svg>\
             <iframe src=\"x\">frame</iframe><object>obj</object><p>After</p></body></html>",
        );
        assert_eq!(page.title.as_deref(), Some("T"));
        assert_eq!(page.paragraphs, ["Kept", "After"]);
    }

    #[test]
    fn named_and_numeric_entities_decode() {
        assert_eq!(
            paragraphs("<p>Fish &amp; chips &lt;3 &quot;q&quot; &#39;s&#x27; &copy; caf&eacute; &mdash;&nbsp;x</p>"),
            ["Fish & chips <3 \"q\" 's' © café — x"]
        );
        assert_eq!(
            paragraphs("<p>&#0; &#xD800; &#1114112;</p>"),
            ["\u{FFFD} \u{FFFD} \u{FFFD}"]
        );
        assert_eq!(
            paragraphs("<p>AT&T &unknown; &amp &#xZZ; &</p>"),
            ["AT&T &unknown; &amp &#xZZ; &"]
        );
    }

    #[test]
    fn pre_keeps_layout_and_other_text_collapses() {
        assert_eq!(
            paragraphs(
                "<p>  a \n\t b   c </p><pre>\nfn main() {\n    let x  = 1;\n}\n</pre><p>d</p>"
            ),
            ["a b c", "fn main() {\n    let x  = 1;\n}", "d"]
        );
    }

    #[test]
    fn lists_and_definition_lists_are_separate_paragraphs() {
        assert_eq!(
            paragraphs(
                "<ul><li>one</li><li>two <b>bold</b></li></ul><dl><dt>term</dt><dd>def</dd></dl>"
            ),
            ["one", "two bold", "term", "def"]
        );
    }

    #[test]
    fn nested_blocks_and_inline_markup() {
        assert_eq!(
            paragraphs(
                "<article><header><h1>Title <em>here</em></h1></header><section><div><p>A <a href=\"/x?a=1&amp;b=2\">link</a>.</p>\
                 <div>Inner<span>joined</span></div></div></section><footer>foot</footer></article>"
            ),
            ["Title here", "A link.", "Innerjoined", "foot"]
        );
    }

    #[test]
    fn paragraphs_are_non_empty_and_numbered_in_order() {
        let page = html_to_paragraphs(
            "<p></p><p>  </p><p>first</p><br><br><hr><p>second</p><div>\n</div>third",
        );
        assert_eq!(page.paragraphs, ["first", "second", "third"]);
    }

    #[test]
    fn title_is_first_head_title_only() {
        let page = html_to_paragraphs(
            "<head><title>  Page\n  Title &amp; More </title></head><body><title>body title</title><p>x</p></body>",
        );
        assert_eq!(page.title.as_deref(), Some("Page Title & More"));
        assert_eq!(page.paragraphs, ["x"]);
        assert_eq!(html_to_paragraphs("<p>no title</p>").title, None);
    }

    #[test]
    fn broken_markup_degrades_without_failing() {
        assert_eq!(
            paragraphs("<p>open <b>bold <i>x</p><p>a < b and c > d</p><div class=\"unterminated"),
            ["open bold x", "a < b and c > d"]
        );
        // An unclosed head ends at body.
        assert_eq!(
            paragraphs("<head><title>t</title><body><p>visible</p>"),
            ["visible"]
        );
        // An unclosed script swallows the rest, never panics.
        assert_eq!(
            paragraphs("<p>before</p><script>never closed <p>x</p>"),
            ["before"]
        );
        assert_eq!(
            paragraphs("<!-- unterminated comment <p>x</p>"),
            Vec::<String>::new()
        );
        assert_eq!(paragraphs("<<>><p>ok</p>"), ["<<>>", "ok"]);
        assert_eq!(
            paragraphs("<p title='a>b' data-x=\"c>d\">quoted</p>"),
            ["quoted"]
        );
        assert_eq!(
            paragraphs("<p title = 'a>b' data-x=\n\"c>d\">spaced</p>"),
            ["spaced"]
        );
    }

    #[test]
    fn a_quote_inside_an_unquoted_value_is_an_ordinary_character() {
        // Browsers read `title=Bob's` as the value `Bob's`; the apostrophe
        // must not open a quoted run that swallows the rest of the page.
        assert_eq!(
            paragraphs(
                "<p>Before</p><a title=Bob's href=/x>link</a><p>After one</p><p>After two</p>"
            ),
            ["Before", "link", "After one", "After two"]
        );
        assert_eq!(
            paragraphs("<img alt=5\"wide src=x.png><p>caption</p><div data-a=it's>tail</div>"),
            ["caption", "tail"]
        );
        // A quote that does start a value still protects a `>` inside it.
        assert_eq!(
            paragraphs("<a title=Bob's data-x='y>z'>link</a><p>after</p>"),
            ["link", "after"]
        );
        // One huge tag of whitespace and stray quotes is still read in
        // linear time.
        let mut tag = String::from("<a ");
        while tag.len() < MAX_EXTRACT_INPUT_BYTES - 64 {
            tag.push_str("   \"   ' =  ");
        }
        tag.push_str(">x");
        let started = std::time::Instant::now();
        let _ = html_to_paragraphs(&tag);
        let limit_ms = if cfg!(debug_assertions) { 2000 } else { 200 };
        assert!(started.elapsed().as_millis() < limit_ms);
    }

    #[test]
    fn a_head_left_open_ends_at_body_content() {
        // Neither </head> nor <body>: both are optional in HTML.
        let page = html_to_paragraphs(
            "<html><head><meta charset=utf-8><title>T</title><p>Visible paragraph one.<p>Two.",
        );
        assert_eq!(page.title.as_deref(), Some("T"));
        assert_eq!(page.paragraphs, ["Visible paragraph one.", "Two."]);
        // Text that is not whitespace ends it too.
        let page = html_to_paragraphs(
            "<!DOCTYPE html><html><head>\n  <title>T</title>\n  <link rel=icon href=x>\n\
             Bare text <b>after</b> the head",
        );
        assert_eq!(page.title.as_deref(), Some("T"));
        assert_eq!(page.paragraphs, ["Bare text after the head"]);
        // Head content before the body content stays dropped.
        assert_eq!(
            paragraphs(
                "<head><title>T</title><style>p{}</style><script>s()</script>\
                 <noscript><p>enable js</p></noscript><meta name=a><div>Body</div><p>More</p>"
            ),
            ["Body", "More"]
        );
        // With <body> and no </head>, and with both, nothing changes.
        assert_eq!(
            paragraphs("<head><title>t</title><meta x><body><p>visible</p>"),
            ["visible"]
        );
        assert_eq!(
            paragraphs("<html><head><title>t</title></head><body><p>visible</p></body></html>"),
            ["visible"]
        );
        // An entity is text too.
        assert_eq!(paragraphs("<head><title>t</title>&copy; 2026"), ["© 2026"]);
    }

    #[test]
    fn comments_doctype_and_processing_instructions_are_skipped() {
        assert_eq!(
            paragraphs("<!DOCTYPE html><?xml version=\"1.0\"?><!-- <p>hidden</p> --><p>shown</p><![CDATA[x]]>"),
            ["shown"]
        );
    }

    #[test]
    fn tables_separate_cells_and_rows() {
        assert_eq!(
            paragraphs(
                "<table><tr><th>Name</th><th>Age</th></tr><tr><td>Ana</td><td>7</td></tr></table>"
            ),
            ["Name Age", "Ana 7"]
        );
    }

    #[test]
    fn uppercase_tags_and_attributes_are_handled() {
        assert_eq!(
            paragraphs("<DIV><P CLASS=x>Upper</P><SCRIPT>bad()</SCRIPT><Style>x{}</STYLE></DIV>"),
            ["Upper"]
        );
    }

    #[test]
    fn plain_text_and_markdown_split_on_blank_lines() {
        let page = plain_to_paragraphs(
            "# Heading\n\nFirst line\nsecond line  \n\n\n  \n- item\n- item 2\n",
        );
        assert_eq!(
            page.paragraphs,
            ["# Heading", "First line\nsecond line", "- item\n- item 2"]
        );
        assert_eq!(
            plain_to_paragraphs("\r\n\r\n").paragraphs,
            Vec::<String>::new()
        );
    }

    #[test]
    fn json_and_xml_pass_through_as_one_block() {
        let page = single_block("  {\"a\": [1,\n 2]}\n\n{\"b\": 3}  ");
        assert_eq!(page.paragraphs, ["{\"a\": [1,\n 2]}\n\n{\"b\": 3}"]);
    }

    #[test]
    fn non_ascii_text_and_truncation_at_char_boundary() {
        assert_eq!(paragraphs("<p>日本語 🦀 text</p>"), ["日本語 🦀 text"]);
        let long = "é".repeat(MAX_EXTRACT_INPUT_BYTES / 2 + 10);
        let page = plain_to_paragraphs(&long);
        assert!(page.input_truncated);
        assert!(page.paragraphs[0].len() <= MAX_EXTRACT_INPUT_BYTES);
    }

    #[test]
    fn four_mib_page_extracts_in_linear_time() {
        let unit = "<div class=\"row\"><p>Some <b>bold</b> text &amp; an entity.</p>\
                    <script>if (a < b) { x = '</p>'; }</script><ul><li>item</li></ul></div>\n";
        let mut page = String::with_capacity(MAX_EXTRACT_INPUT_BYTES);
        while page.len() + unit.len() <= MAX_EXTRACT_INPUT_BYTES {
            page.push_str(unit);
        }
        // Adversarial tail: many '<' and '</' that never close a script.
        page.truncate(MAX_EXTRACT_INPUT_BYTES - 64 * 1024);
        page.push_str("<script>");
        while page.len() + 3 <= MAX_EXTRACT_INPUT_BYTES {
            page.push_str("</s");
        }
        let started = std::time::Instant::now();
        let extracted = html_to_paragraphs(&page);
        let elapsed = started.elapsed();
        assert!(extracted.paragraphs.len() > 10_000);
        assert_eq!(extracted.paragraphs[0], "Some bold text & an entity.");
        // 200 ms is the bound for an optimized build; an unoptimized test
        // build is allowed ten times that.
        let limit_ms = if cfg!(debug_assertions) { 2000 } else { 200 };
        eprintln!(
            "html_text: 4 MiB page extracted in {} ms",
            elapsed.as_millis()
        );
        assert!(
            elapsed.as_millis() < limit_ms,
            "4 MiB page took {} ms",
            elapsed.as_millis()
        );
    }
}
