//! Turning YAML text into the events of one document at a time.
//!
//! `granit-parser` does the parsing. Three things are adjusted so a document
//! reads as it did with libyaml (through serde_yaml 0.9) before Axocoatl
//! 1.3.1:
//!
//! - an empty value (`key:`) is the empty string, where the parser reports
//!   `~`, so it reads as `""` into a string, as it always did;
//! - a literal or folded block that ends the input without a final line
//!   break has no final line break, where the parser adds one;
//! - a continuation line of a multi-line quoted string may be indented less
//!   than YAML 1.2 asks (libyaml took it), which [`lenient_source`] handles.
//!
//! Positions are those of libyaml: an event starts at its anchor, tag or
//! block indicator, when it has one.

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::sync::Arc;

use granit_parser::{
    ErrorKind, Event as ParserEvent, Marker, Parser, ScalarStyle, Span, StructureStyle,
};

use super::error::{self, ErrorImpl, Mark};

/// The style a scalar was written in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Style {
    Plain,
    SingleQuoted,
    DoubleQuoted,
    Literal,
    Folded,
}

#[derive(Debug)]
pub(crate) struct Scalar {
    pub(crate) value: String,
    pub(crate) style: Style,
    /// The resolved tag, such as `tag:yaml.org,2002:str` for `!!str` or
    /// `!Variant` for a local tag.
    pub(crate) tag: Option<String>,
}

#[derive(Debug)]
pub(crate) enum Event {
    /// The index of the anchored event.
    Alias(usize),
    Scalar(Scalar),
    SequenceStart(Option<String>),
    SequenceEnd,
    MappingStart(Option<String>),
    MappingEnd,
    /// An empty stream.
    Void,
}

pub(crate) struct Document {
    pub(crate) events: Vec<(Event, Mark)>,
    pub(crate) error: Option<Arc<ErrorImpl>>,
    /// Anchor id to the index of its event.
    pub(crate) aliases: BTreeMap<usize, usize>,
}

/// The longest stretch of under-indented quoted continuation lines that
/// [`lenient_source`] re-indents in one document.
const MAX_REINDENTED_LINES: usize = 256;

fn parser(source: &str) -> Parser<'_, granit_parser::StrInput<'_>> {
    Parser::new_from_str_with_options(source, granit_parser::options! { emit_comments: false })
}

/// The input, with every continuation line of a multi-line quoted scalar
/// that YAML 1.2 finds under-indented moved right far enough.
///
/// libyaml accepted such lines, so configurations written for Axocoatl
/// before 1.3.1 may have them. Moving them does not change the string: a
/// quoted scalar drops the leading white space of its continuation lines.
/// Only lines the parser reports as under-indented continuations are
/// touched; any other input is returned unchanged.
pub(crate) fn lenient_source(input: &str) -> Cow<'_, str> {
    let mut source = Cow::Borrowed(input);
    let mut last_line = None;
    let mut pad = 0usize;
    let mut lines = 0;
    loop {
        let Some(line) = first_underindented_quoted_line(&source) else {
            return source;
        };
        // The same line again: it needs more room than was given.
        pad = if last_line == Some(line) {
            pad * 2
        } else {
            lines += 1;
            1
        };
        last_line = Some(line);
        if lines > MAX_REINDENTED_LINES || pad > 1024 {
            // Leave the rest to the parser, which reports the line.
            return source;
        }
        let Some(offset) = line_start(&source, line) else {
            return source;
        };
        let mut owned = source.into_owned();
        owned.insert_str(offset, &" ".repeat(pad));
        source = Cow::Owned(owned);
    }
}

/// The line (from 1) of the first quoted-scalar continuation line the
/// parser finds under-indented, if parsing stops there.
fn first_underindented_quoted_line(source: &str) -> Option<usize> {
    for event in parser(source) {
        match event {
            Ok(_) => {}
            Err(error) => {
                return matches!(error.kind(), ErrorKind::InvalidQuotedScalarIndent)
                    .then(|| error.marker().line());
            }
        }
    }
    None
}

/// The byte offset where line `line` (from 1) starts.
fn line_start(source: &str, line: usize) -> Option<usize> {
    if line == 1 {
        return Some(0);
    }
    source
        .match_indices('\n')
        .nth(line - 2)
        .map(|(offset, _)| offset + 1)
}

pub(crate) struct Loader<'input> {
    source: &'input str,
    parser: Option<Parser<'input, granit_parser::StrInput<'input>>>,
    document_count: usize,
    /// Byte offset where the previous event ended.
    previous_end: usize,
}

fn tag_string(tag: Option<&granit_parser::Tag>) -> Option<String> {
    tag.map(|tag| format!("{}{}", tag.handle(), tag.suffix()))
}

fn byte_offset(marker: &Marker) -> Option<usize> {
    marker.byte_offset()
}

impl<'input> Loader<'input> {
    pub(crate) fn new(source: &'input str) -> Self {
        Loader {
            source,
            parser: Some(parser(source)),
            document_count: 0,
            previous_end: 0,
        }
    }

    /// The position of a parser marker. libyaml counted a byte order mark
    /// at the start of the input as a column of the first line.
    fn mark(&self, marker: &Marker) -> Mark {
        let mut mark = Mark::from_parser(marker);
        if mark.line == 1 && self.source.starts_with('\u{feff}') {
            mark.column += 1;
        }
        mark
    }

    /// The mark at byte `offset`, which lies before `content` on its line
    /// or an earlier one.
    fn mark_before(&self, offset: usize, content: &Marker) -> Option<Mark> {
        let content_offset = byte_offset(content)?;
        let between = self.source.get(offset..content_offset)?;
        let index = content.index().checked_sub(between.chars().count())?;
        let breaks = between.matches('\n').count();
        let line = content.line().checked_sub(breaks)?;
        let line_start = self.source[..offset].rfind('\n').map_or(0, |at| at + 1);
        let column = self.source[line_start..offset].chars().count() + 1;
        // Counted from the source, a byte order mark is already a column.
        Some(Mark {
            index,
            line,
            column,
        })
    }

    /// Where a node starts, as libyaml marks it: at its anchor, tag or block
    /// scalar indicator, when it has one, else at its content.
    fn node_mark(&self, span: &Span, has_properties: bool) -> Mark {
        let content = self.mark(&span.start);
        if !has_properties {
            return content;
        }
        let from = self.previous_end;
        let Some(region) = byte_offset(&span.start).and_then(|to| self.source.get(from..to)) else {
            return content;
        };
        let mut chars = region.char_indices();
        let mut previous_blank = true;
        while let Some((at, c)) = chars.next() {
            match c {
                ' ' | '\t' | '\r' | '\n' => {
                    previous_blank = true;
                    continue;
                }
                '#' if previous_blank => {
                    // A comment runs to the end of the line.
                    for (_, c) in chars.by_ref() {
                        if c == '\n' {
                            break;
                        }
                    }
                    previous_blank = true;
                    continue;
                }
                '-' | '?' | ':' | ',' | '[' | '{' | '.' => {
                    previous_blank = false;
                    continue;
                }
                _ => {
                    return self.mark_before(from + at, &span.start).unwrap_or(content);
                }
            }
        }
        content
    }

    /// A block sequence starts at its first `-`, which the parser places
    /// after the indicator for a sequence that is not indented under its key.
    fn block_sequence_mark(&self, span: &Span) -> Option<Mark> {
        let start = byte_offset(&span.start)?;
        let rest = self.source.get(start..)?;
        let mut rest_chars = rest.chars();
        if rest_chars.next() == Some('-')
            && rest_chars
                .next()
                .is_none_or(|next| matches!(next, ' ' | '\t' | '\n' | '\r'))
        {
            // The parser already points at the `-`.
            return None;
        }
        let before = self.source.get(..start)?;
        let trimmed = before.trim_end_matches([' ', '\t']);
        let dash = trimmed.strip_suffix('-')?;
        if !(dash.is_empty() || dash.ends_with([' ', '\t', '\n', '\r'])) {
            return None;
        }
        self.mark_before(dash.len(), &span.start)
    }

    pub(crate) fn next_document(&mut self) -> Option<Document> {
        let first = self.document_count == 0;
        self.document_count += 1;

        let mut document = Document {
            events: Vec::new(),
            error: None,
            aliases: BTreeMap::new(),
        };

        loop {
            let parser = self.parser.as_mut()?;
            let (event, span) = match parser.next() {
                Some(Ok(next)) => next,
                Some(Err(scan_error)) => {
                    let mark = self.mark(scan_error.marker());
                    document.error = Some(Arc::new(ErrorImpl::Syntax(scan_error.info(), mark)));
                    self.parser = None;
                    return Some(document);
                }
                None => {
                    self.parser = None;
                    if first && document.events.is_empty() {
                        let end = self.source.chars().count();
                        let line = self.source.matches('\n').count() + 1;
                        let line_start = self.source.rfind('\n').map_or(0, |at| at + 1);
                        let column = self.source[line_start..].chars().count() + 1;
                        let mark = Mark {
                            index: end,
                            line,
                            column,
                        };
                        document.events.push((Event::Void, mark));
                    }
                    return first.then_some(document);
                }
            };
            let empty_span = span.start == span.end;
            let event = match event {
                ParserEvent::StreamStart | ParserEvent::Comment(..) => None,
                ParserEvent::StreamEnd => {
                    self.parser = None;
                    if !first {
                        return None;
                    }
                    if document.events.is_empty() {
                        document.events.push((Event::Void, self.mark(&span.start)));
                    }
                    return Some(document);
                }
                ParserEvent::DocumentStart(..) => None,
                ParserEvent::DocumentEnd => {
                    self.previous_end = byte_offset(&span.end).unwrap_or(self.previous_end);
                    return Some(document);
                }
                ParserEvent::Alias(anchor_id) => {
                    let mark = self.mark(&span.start);
                    match document.aliases.get(&anchor_id) {
                        Some(index) => Some((Event::Alias(*index), mark)),
                        None => {
                            document.error =
                                Some(error::new(ErrorImpl::UnknownAnchor(mark)).shared());
                            self.parser = None;
                            return Some(document);
                        }
                    }
                }
                ParserEvent::Scalar(value, style, anchor_id, tag) => {
                    let style = match style {
                        ScalarStyle::Plain => Style::Plain,
                        ScalarStyle::SingleQuoted => Style::SingleQuoted,
                        ScalarStyle::DoubleQuoted => Style::DoubleQuoted,
                        ScalarStyle::Literal => Style::Literal,
                        ScalarStyle::Folded => Style::Folded,
                    };
                    let mut value = value.into_owned();
                    if style == Style::Plain && empty_span && tag.is_none() && value == "~" {
                        // An empty node, which the parser reports as `~`.
                        value.clear();
                    }
                    if matches!(style, Style::Literal | Style::Folded)
                        && value.ends_with('\n')
                        && byte_offset(&span.end) == Some(self.source.len())
                        && !self.source.ends_with(['\n', '\r'])
                    {
                        // The input ends inside the block, without the line
                        // break the parser adds.
                        value.pop();
                    }
                    let properties = anchor_id != 0
                        || tag.is_some()
                        || matches!(style, Style::Literal | Style::Folded);
                    let mark = self.node_mark(&span, properties);
                    if anchor_id != 0 {
                        document.aliases.insert(anchor_id, document.events.len());
                    }
                    let tag = tag_string(tag.as_deref());
                    Some((Event::Scalar(Scalar { value, style, tag }), mark))
                }
                ParserEvent::SequenceStart(style, anchor_id, tag) => {
                    let mut mark = self.node_mark(&span, anchor_id != 0 || tag.is_some());
                    if style == StructureStyle::Block && anchor_id == 0 && tag.is_none() {
                        mark = self.block_sequence_mark(&span).unwrap_or(mark);
                    }
                    if anchor_id != 0 {
                        document.aliases.insert(anchor_id, document.events.len());
                    }
                    Some((Event::SequenceStart(tag_string(tag.as_deref())), mark))
                }
                ParserEvent::SequenceEnd => Some((Event::SequenceEnd, self.mark(&span.start))),
                ParserEvent::MappingStart(_, anchor_id, tag) => {
                    let mark = self.node_mark(&span, anchor_id != 0 || tag.is_some());
                    if anchor_id != 0 {
                        document.aliases.insert(anchor_id, document.events.len());
                    }
                    Some((Event::MappingStart(tag_string(tag.as_deref())), mark))
                }
                ParserEvent::MappingEnd => Some((Event::MappingEnd, self.mark(&span.start))),
                // An event kind this module does not know: refuse the document
                // rather than skip what it might hold.
                _ => {
                    let mark = self.mark(&span.start);
                    document.error = Some(Arc::new(ErrorImpl::Syntax(
                        "unsupported YAML construct".into(),
                        mark,
                    )));
                    self.parser = None;
                    return Some(document);
                }
            };
            if let Some(end) = byte_offset(&span.end) {
                if !empty_span || end > self.previous_end {
                    self.previous_end = end.max(self.previous_end);
                }
            }
            if let Some(event) = event {
                document.events.push(event);
            }
        }
    }
}
