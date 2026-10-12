//! Turning YAML text into the events of one document at a time.
//!
//! `granit-parser` does the parsing. Before it does, [`prepare`] makes the
//! text read as libyaml (through serde_yaml 0.9) read it before Axocoatl
//! 1.3.1, and the loader then adjusts what the parser reports:
//!
//! - NEL (U+0085), LINE SEPARATOR (U+2028) and PARAGRAPH SEPARATOR (U+2029)
//!   are line breaks, as libyaml (YAML 1.1) took them; YAML 1.2 does not.
//!   A NEL reads exactly as a line feed did. libyaml kept a LINE or
//!   PARAGRAPH SEPARATOR inside a value as that character, so a value with
//!   one is refused with its position rather than read differently.
//! - A continuation line of a multi-line quoted string may be indented less
//!   than YAML 1.2 asks, also with a tab (libyaml took it): such lines are
//!   moved right, which does not change the string.
//! - A lone `-` inside `[...]` or `{...}` is the string `-`.
//! - An anchor or alias name holds only letters, digits, `-` and `_`, and
//!   text inside `[...]` or `{...}` does not start with `?` or `:`: libyaml
//!   split such names and took such a `?` or `:` as a key or value
//!   indicator, so these are refused with their position rather than read
//!   differently.
//! - A second byte order mark at the start of the input, which the parser
//!   skips and libyaml read into the text, is refused with its position.
//! - An empty value (`key:`) is the empty string, where the parser reports
//!   `~`, so it reads as `""` into a string, as it always did.
//! - A literal or folded block that ends the input inside its last line, with
//!   no line break after it, has no final line break, where the parser adds
//!   one.
//!
//! Positions are those of libyaml, in the text as given: an event starts at
//! its anchor, tag or block indicator, when it has one, and what [`prepare`]
//! inserted is not counted.

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::sync::Arc;

use granit_parser::{
    ErrorKind, Event as ParserEvent, Marker, Parser, ScalarStyle, ScanError, Scanner, Span,
    StrInput, StructureStyle, TokenType,
};

use super::error::{ErrorImpl, Mark};

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

/// How many bytes [`prepare`] may parse, beyond the first parse, while it
/// makes libyaml's leniencies readable. A document that needs more is left
/// to the parser, which reports where it stopped.
const FIX_BUDGET: usize = 256 << 20;

/// The parser's options: no comments, and no limit on a directive's length,
/// which libyaml did not limit. Collections nest at most 255 deep.
fn options() -> granit_parser::Options {
    granit_parser::options! {
        emit_comments: false,
        max_directive_bytes: usize::MAX,
    }
}

fn parser(source: &str) -> Parser<'_, StrInput<'_>> {
    Parser::new_from_str_with_options(source, options())
}

/// The first error the parser finds in `source`, if any.
fn first_error(source: &str) -> Option<ScanError> {
    parser(source).find_map(Result::err)
}

/// The length of the line break at byte `at`, as libyaml and granit-parser
/// count them: CR LF, CR or LF; 0 when there is none.
fn break_len(bytes: &[u8], at: usize) -> usize {
    match bytes[at] {
        b'\r' if bytes.get(at + 1) == Some(&b'\n') => 2,
        b'\r' | b'\n' => 1,
        _ => 0,
    }
}

/// The byte offset where each line starts, from line 1.
fn line_starts(source: &str) -> Vec<usize> {
    let bytes = source.as_bytes();
    let mut starts = vec![0];
    let mut at = 0;
    while at < bytes.len() {
        match break_len(bytes, at) {
            0 => at += 1,
            len => {
                at += len;
                starts.push(at);
            }
        }
    }
    starts
}

/// The number of line breaks in `text`, and the byte offset just after the
/// last one.
fn breaks(text: &str) -> (usize, Option<usize>) {
    let bytes = text.as_bytes();
    let mut count = 0;
    let mut last = None;
    let mut at = 0;
    while at < bytes.len() {
        match break_len(bytes, at) {
            0 => at += 1,
            len => {
                at += len;
                count += 1;
                last = Some(at);
            }
        }
    }
    (count, last)
}

/// Whether a line starts with a document marker, `---` or `...` followed by
/// a blank, a line break or the end of the input.
fn is_document_marker(line: &str) -> bool {
    (line.starts_with("---") || line.starts_with("..."))
        && line[3..]
            .chars()
            .next()
            .is_none_or(|next| matches!(next, ' ' | '\t' | '\r' | '\n'))
}

/// The byte offset of the quote that closes the quoted scalar opened at
/// byte `open`.
fn closing_quote(source: &str, open: usize, single: bool) -> Option<usize> {
    let bytes = source.as_bytes();
    let mut at = open + 1;
    while at < bytes.len() {
        match bytes[at] {
            b'\\' if !single => at += 2,
            b'\'' if single => {
                if bytes.get(at + 1) == Some(&b'\'') {
                    at += 2;
                } else {
                    return Some(at);
                }
            }
            b'"' if !single => return Some(at),
            _ => at += 1,
        }
    }
    None
}

/// What the text seems to hold from byte `from` on, which is where a token
/// may start: the quoted scalars, by the byte offsets of their opening and
/// closing quotes, and each lone `-` directly followed by `,`, `[`, `]`,
/// `{` or `}`. A quote or `-` starts a token after a line break, a blank,
/// `[`, `{` or `,`, outside a comment. This is a guess, which the parse
/// confirms.
fn guess_tokens(source: &str, from: usize) -> (Vec<(usize, usize)>, Vec<usize>) {
    let bytes = source.as_bytes();
    let mut quoted = Vec::new();
    let mut dashes = Vec::new();
    let mut token_start = true;
    let mut at = from;
    while at < bytes.len() {
        match bytes[at] {
            b'\n' | b'\r' | b' ' | b'\t' | b'[' | b'{' | b',' => {
                token_start = true;
                at += 1;
            }
            b'#' if token_start => {
                at = source[at..]
                    .find(['\n', '\r'])
                    .map_or(bytes.len(), |end| at + end);
            }
            quote @ (b'"' | b'\'') if token_start => {
                let Some(close) = closing_quote(source, at, quote == b'\'') else {
                    break;
                };
                quoted.push((at, close));
                token_start = false;
                at = close + 1;
            }
            b'|' | b'>' if token_start => {
                // A block scalar: its lines are those after this one that
                // are blank or indented more than its parent node.
                let line_start = source[..at].rfind(['\n', '\r']).map_or(0, |end| end + 1);
                let parent = block_parent_column(&source[line_start..at]);
                at = source[at..]
                    .find(['\n', '\r'])
                    .map_or(bytes.len(), |end| at + end);
                while at < bytes.len() {
                    let next = at + break_len(bytes, at).max(1);
                    let line_end = source[next..]
                        .find(['\n', '\r'])
                        .map_or(bytes.len(), |end| next + end);
                    let line = &source[next..line_end];
                    let spaces = line.bytes().take_while(|byte| *byte == b' ').count();
                    if line.trim_matches([' ', '\t']).is_empty() || spaces > parent {
                        at = line_end;
                    } else {
                        break;
                    }
                }
                token_start = true;
            }
            b'-' if token_start
                && matches!(bytes.get(at + 1), Some(b',' | b'[' | b']' | b'{' | b'}')) =>
            {
                dashes.push(at);
                token_start = false;
                at += 1;
            }
            _ => {
                token_start = false;
                at += 1;
            }
        }
    }
    (quoted, dashes)
}

/// The column of the node holding a block scalar whose indicator follows
/// `line` (the start of its line): the column of the key before it, or of
/// the `-` it is an entry of.
fn block_parent_column(line: &str) -> usize {
    let mut column = line.bytes().take_while(|byte| *byte == b' ').count();
    loop {
        let rest = &line[column..];
        if rest.is_empty() {
            return column;
        }
        match rest.strip_prefix('-') {
            Some(after) if after.starts_with([' ', '\t']) => {
                let blanks = after
                    .bytes()
                    .take_while(|byte| matches!(byte, b' ' | b'\t'))
                    .count();
                if after.len() == blanks {
                    // The block is this entry's value.
                    return column;
                }
                column += 1 + blanks;
            }
            _ => return column,
        }
    }
}

/// The quoted scalars the parser reads in `source`, by the byte offsets
/// where they start and end, and where each double-quoted `-` starts, up to
/// the first error.
fn quoted_spans(source: &str) -> (Vec<(usize, usize)>, Vec<usize>) {
    let mut quoted = Vec::new();
    let mut dashes = Vec::new();
    for event in parser(source) {
        let Ok((event, span)) = event else {
            break;
        };
        let ParserEvent::Scalar(value, style, ..) = event else {
            continue;
        };
        let (Some(start), Some(end)) = (span.start.byte_offset(), span.end.byte_offset()) else {
            break;
        };
        if matches!(style, ScalarStyle::SingleQuoted | ScalarStyle::DoubleQuoted) {
            quoted.push((start, end));
            if style == ScalarStyle::DoubleQuoted && value == "-" {
                dashes.push(start);
            }
        }
    }
    (quoted, dashes)
}

/// Whether byte `offset` lies inside one of the `quoted` spans, which are
/// in order.
fn inside(quoted: &[(usize, usize)], offset: usize) -> bool {
    let after = quoted.partition_point(|&(start, _)| start < offset);
    after > 0 && quoted[after - 1].1 > offset
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Fix {
    /// Spaces moving a quoted continuation line right.
    Indent,
    /// A quote around a lone `-` in a flow collection.
    Quote,
}

/// Text that [`prepare`] inserted, at a byte offset of the prepared text.
#[derive(Clone, Copy, Debug)]
struct Insertion {
    offset: usize,
    len: usize,
    fix: Fix,
}

/// An insertion placed in the prepared text: its character index and line,
/// and the characters inserted before it, for taking them out of positions.
#[derive(Clone, Copy)]
struct Placed {
    index: usize,
    line: usize,
    len: usize,
    before: usize,
}

/// The input as the parser reads it, and what is needed to report positions
/// and values as libyaml did for the input as given.
pub(crate) struct Prepared<'input> {
    pub(crate) source: Cow<'input, str>,
    starts_with_bom: bool,
    placed: Vec<Placed>,
    /// Byte offsets of the opening quotes put around a lone `-`, ascending.
    quoted_dashes: Vec<usize>,
    /// Byte offsets of the LINE and PARAGRAPH SEPARATORs read as line
    /// feeds, ascending.
    separators: Vec<usize>,
    /// Why the input is refused before it is read, and where.
    pub(crate) refusal: Option<(String, Mark)>,
}

#[derive(Clone)]
struct Work<'input> {
    source: Cow<'input, str>,
    insertions: Vec<Insertion>,
    quoted_dashes: Vec<usize>,
    separators: Vec<usize>,
    budget: usize,
}

/// Make `input` read as libyaml read it; see the module documentation.
pub(crate) fn prepare(input: &str) -> Prepared<'_> {
    let mut work = Work {
        source: Cow::Borrowed(input),
        insertions: Vec::new(),
        quoted_dashes: Vec::new(),
        separators: Vec::new(),
        budget: FIX_BUDGET,
    };
    work.translate_breaks();
    let translated = work.source.clone();
    let separators = work.separators.clone();
    work.fix();
    if !work.insertions.is_empty() && !work.confirmed() {
        // A fix that the parse does not confirm as a quoted continuation
        // line or a lone `-` is not used: the parser reports the input.
        work.source = translated;
        work.separators = separators;
        work.insertions.clear();
        work.quoted_dashes.clear();
    }
    let mut prepared = work.finish();
    prepared.refusal = if prepared.source.starts_with("\u{feff}\u{feff}") {
        // The parser skips them all; libyaml read the second into the text.
        Some((
            "a second byte order mark (U+FEFF) at the start of the input".to_owned(),
            prepared.mark_at_offset('\u{feff}'.len_utf8()),
        ))
    } else {
        prepared.check_tokens()
    };
    prepared
}

impl<'input> Work<'input> {
    /// NEL, LINE SEPARATOR and PARAGRAPH SEPARATOR become line breaks.
    fn translate_breaks(&mut self) {
        if !self.source.contains(['\u{85}', '\u{2028}', '\u{2029}']) {
            return;
        }
        let mut out = String::with_capacity(self.source.len());
        let mut previous = None;
        for c in self.source.chars() {
            // A LF right after a CR joins it into one line break, which is
            // right only where the input had a CR LF: a NEL, LINE or
            // PARAGRAPH SEPARATOR, or a LF, after a CR that stands for
            // another line break becomes a CR too. Content reads a CR as a
            // LF, so no value changes.
            let after_cr = out.ends_with('\r');
            match c {
                '\u{85}' | '\u{2028}' | '\u{2029}' => {
                    if c != '\u{85}' {
                        self.separators.push(out.len());
                    }
                    out.push(if after_cr { '\r' } else { '\n' });
                }
                '\n' if after_cr && previous != Some('\r') => out.push('\r'),
                c => out.push(c),
            }
            previous = Some(c);
        }
        self.source = Cow::Owned(out);
    }

    fn spend(&mut self, bytes: usize) -> bool {
        match self.budget.checked_sub(bytes) {
            Some(rest) => {
                self.budget = rest;
                true
            }
            None => false,
        }
    }

    /// Fix what the parser stops at, while it is one of libyaml's
    /// leniencies: there, and wherever else the text seems to need the same
    /// fix, as far as the parse confirms it.
    fn fix(&mut self) {
        let mut previous = None;
        loop {
            // The first parse is not counted.
            if previous.is_some() && !self.spend(self.source.len()) {
                return;
            }
            let Some(error) = first_error(&self.source) else {
                return;
            };
            let at = (error.kind().clone(), error.marker().index());
            if previous.as_ref() == Some(&at) {
                return;
            }
            previous = Some(at);
            let fixed = match error.kind() {
                ErrorKind::InvalidQuotedScalarIndent | ErrorKind::TabInIndentation => {
                    self.reindent(error.marker().line())
                }
                ErrorKind::PlainScalarStartsWithDashFlowIndicator => error
                    .marker()
                    .byte_offset()
                    .is_some_and(|offset| self.quote_dashes(offset)),
                _ => false,
            };
            if !fixed {
                return;
            }
        }
    }

    /// Move the continuation lines of the quoted scalar that line `line`
    /// (from 1) continues, from that line on, at least as far right as the
    /// scalar's opening quote, and those of the quoted scalars that seem to
    /// follow, as far as the parse confirms them. The parser asks such lines
    /// to be indented more than the block holding the scalar, which starts
    /// left of the quote.
    fn reindent(&mut self, line: usize) -> bool {
        let starts = line_starts(&self.source);
        let Some(&line_start) = starts.get(line.wrapping_sub(1)) else {
            return false;
        };
        // Parsing the input up to that line ends inside the quoted scalar,
        // and the parser names where it opened. The spaces after it keep the
        // end of the input right of the scalar's block (which starts left of
        // the quote, on a line before), so it is not taken for this line's
        // indentation.
        let longest = starts[..line]
            .windows(2)
            .map(|pair| self.source[pair[0]..pair[1]].chars().count())
            .max()
            .unwrap_or(0);
        let mut prefix = String::with_capacity(line_start + longest + 1);
        prefix.push_str(&self.source[..line_start]);
        prefix.extend(std::iter::repeat_n(' ', longest + 1));
        if !self.spend(prefix.len()) {
            return false;
        }
        let open = match first_error(&prefix) {
            Some(error) if *error.kind() == ErrorKind::UnclosedQuotedScalar => {
                error.marker().byte_offset()
            }
            _ => None,
        };
        let Some(open) = open else {
            return false;
        };
        let single = match self.source.as_bytes().get(open) {
            Some(b'\'') => true,
            Some(b'"') => false,
            _ => return false,
        };
        let Some(close) = closing_quote(&self.source, open, single) else {
            return false;
        };
        let mut edits = Vec::new();
        self.continuation_edits(&starts, open, close, line - 1, &mut edits);
        let certain = edits.len();
        if certain == 0 {
            return false;
        }
        for (open, close) in guess_tokens(&self.source, close + 1).0 {
            self.continuation_edits(&starts, open, close, 0, &mut edits);
        }
        let keep = if edits.len() > certain {
            self.confirmed_indents(&edits).max(certain)
        } else {
            certain
        };
        self.apply(&edits[..keep]);
        true
    }

    /// Add to `edits` the spaces that move the continuation lines of the
    /// quoted scalar from byte `open` to byte `close`, from the line at
    /// `starts[from]` on, at least as far right as its opening quote. Lines
    /// starting with a document marker stay: libyaml refused those too.
    fn continuation_edits(
        &self,
        starts: &[usize],
        open: usize,
        close: usize,
        from: usize,
        edits: &mut Vec<(usize, usize, Fix)>,
    ) {
        let open_line = starts.partition_point(|&start| start <= open) - 1;
        let column = self.source[starts[open_line]..open].chars().count();
        for &start in &starts[from.max(open_line + 1)..] {
            if start > close {
                break;
            }
            let rest = &self.source[start..];
            if is_document_marker(rest) {
                continue;
            }
            let spaces = rest.bytes().take_while(|byte| *byte == b' ').count();
            if spaces < column {
                edits.push((start, column - spaces, Fix::Indent));
            }
        }
    }

    /// How many of `edits` (spaces, ascending), from the first, the parse of
    /// the text with all of them places inside quoted scalars.
    fn confirmed_indents(&mut self, edits: &[(usize, usize, Fix)]) -> usize {
        let mut candidate = self.clone();
        candidate.apply(edits);
        if !self.spend(candidate.source.len()) {
            return 0;
        }
        let (quoted, _) = quoted_spans(&candidate.source);
        let mut before = 0;
        edits
            .iter()
            .take_while(|&&(offset, count, _)| {
                let confirmed = inside(&quoted, offset + before);
                before += count;
                confirmed
            })
            .count()
    }

    /// Put quotes around the lone `-` at byte `at`, which the parser refuses
    /// inside a flow collection and libyaml read as the string `-`, and
    /// around those that seem to follow, as far as the parse confirms them.
    fn quote_dashes(&mut self, at: usize) -> bool {
        if self.source.as_bytes().get(at) != Some(&b'-') {
            return false;
        }
        let mut dashes = vec![at];
        dashes.extend(guess_tokens(&self.source, at + 1).1);
        let quotes = |dashes: &[usize]| {
            dashes
                .iter()
                .flat_map(|&dash| [(dash, 1, Fix::Quote), (dash + 1, 1, Fix::Quote)])
                .collect::<Vec<_>>()
        };
        let keep = if dashes.len() > 1 {
            let mut candidate = self.clone();
            candidate.apply(&quotes(&dashes));
            if self.spend(candidate.source.len()) {
                let (_, quoted) = quoted_spans(&candidate.source);
                // Each quoted `-` moved right by the two quotes before it.
                dashes
                    .iter()
                    .enumerate()
                    .take_while(|&(index, &dash)| quoted.binary_search(&(dash + 2 * index)).is_ok())
                    .count()
                    .max(1)
            } else {
                1
            }
        } else {
            1
        };
        let kept = &dashes[..keep];
        self.apply(&quotes(kept));
        for (index, &dash) in kept.iter().enumerate() {
            // The opening quote is where the `-` was.
            let opening = dash + 2 * index;
            let position = self
                .quoted_dashes
                .partition_point(|&quoted| quoted < opening);
            self.quoted_dashes.insert(position, opening);
        }
        true
    }

    /// Insert spaces, or a quote, at each `(offset, count, fix)`, offsets
    /// ascending and distinct, and move every recorded offset with the text.
    fn apply(&mut self, edits: &[(usize, usize, Fix)]) {
        let mut out = String::with_capacity(
            self.source.len() + edits.iter().map(|(_, count, _)| count).sum::<usize>(),
        );
        let mut from = 0;
        for &(offset, count, fix) in edits {
            out.push_str(&self.source[from..offset]);
            let text = match fix {
                Fix::Indent => ' ',
                Fix::Quote => '"',
            };
            out.extend(std::iter::repeat_n(text, count));
            from = offset;
        }
        out.push_str(&self.source[from..]);
        self.source = Cow::Owned(out);

        // Text inserted at or before an offset moves it.
        let mut shifted = Vec::with_capacity(edits.len());
        let mut total = 0;
        for &(offset, count, _) in edits {
            total += count;
            shifted.push((offset, total));
        }
        let shift = |offset: usize| {
            let applied = shifted.partition_point(|&(at, _)| at <= offset);
            if applied == 0 {
                0
            } else {
                shifted[applied - 1].1
            }
        };
        for insertion in &mut self.insertions {
            insertion.offset += shift(insertion.offset);
        }
        for dash in &mut self.quoted_dashes {
            *dash += shift(*dash);
        }
        for separator in &mut self.separators {
            *separator += shift(*separator);
        }
        let mut before = 0;
        for &(offset, count, fix) in edits {
            self.insertions.push(Insertion {
                offset: offset + before,
                len: count,
                fix,
            });
            before += count;
        }
        self.insertions.sort_by_key(|insertion| insertion.offset);
    }

    /// Whether every fix is confirmed by the parse: every moved line lies
    /// inside a quoted scalar, and every quoted `-` reads as one.
    fn confirmed(&mut self) -> bool {
        if !self.spend(self.source.len()) {
            return false;
        }
        let (quoted, dashes) = quoted_spans(&self.source);
        self.insertions
            .iter()
            .filter(|insertion| insertion.fix == Fix::Indent)
            .all(|insertion| inside(&quoted, insertion.offset))
            && self
                .quoted_dashes
                .iter()
                .all(|dash| dashes.binary_search(dash).is_ok())
    }

    fn finish(self) -> Prepared<'input> {
        let mut placed = Vec::with_capacity(self.insertions.len());
        let mut index = 0;
        let mut line = 1;
        let mut from = 0;
        let mut before = 0;
        for insertion in &self.insertions {
            let between = &self.source[from..insertion.offset];
            index += between.chars().count();
            line += breaks(between).0;
            // A CR LF split by the offset is not possible: insertions follow
            // a whole line break or sit inside a line.
            from = insertion.offset;
            placed.push(Placed {
                index,
                line,
                len: insertion.len,
                before,
            });
            before += insertion.len;
        }
        Prepared {
            starts_with_bom: self.source.starts_with('\u{feff}'),
            source: self.source,
            placed,
            quoted_dashes: self.quoted_dashes,
            separators: self.separators,
            refusal: None,
        }
    }
}

impl Prepared<'_> {
    /// The position of a parser marker in the prepared text. libyaml counted
    /// a byte order mark at the start of the input as a column of the first
    /// line.
    fn raw_mark(&self, marker: &Marker) -> Mark {
        let mut mark = Mark::from_parser(marker);
        if mark.line == 1 && self.starts_with_bom {
            mark.column += 1;
        }
        mark
    }

    /// The position in the input of a position in the prepared text: what
    /// was inserted before it is not counted.
    fn original(&self, mut mark: Mark) -> Mark {
        if self.placed.is_empty() {
            return mark;
        }
        let removed = |placed: &[Placed], index: usize| -> usize {
            let count = placed.partition_point(|insertion| insertion.index < index);
            if count == 0 {
                return 0;
            }
            let last = placed[count - 1];
            last.before + last.len.min(index - last.index)
        };
        let before_mark = removed(&self.placed, mark.index);
        let line_first = self
            .placed
            .partition_point(|insertion| insertion.line < mark.line);
        let before_line = match self.placed.get(line_first) {
            Some(first) if first.index < mark.index => first.before,
            _ => before_mark,
        };
        mark.index -= before_mark;
        mark.column -= before_mark - before_line;
        mark
    }

    /// The position in the input of a parser marker.
    fn mark(&self, marker: &Marker) -> Mark {
        self.original(self.raw_mark(marker))
    }

    /// The position in the input of byte `offset` of the prepared text.
    fn mark_at_offset(&self, offset: usize) -> Mark {
        let before = &self.source[..offset];
        let (count, last) = breaks(before);
        let line_start = last.unwrap_or(0);
        // Counted from the text, a byte order mark is already a column.
        let column = before[line_start..].chars().count() + 1;
        self.original(Mark {
            index: before.chars().count(),
            line: count + 1,
            column,
        })
    }

    /// What libyaml split or read differently in anchor and alias names, and
    /// in a `?` inside a flow collection: refused, with the position.
    fn check_tokens(&self) -> Option<(String, Mark)> {
        if !self.source.contains(['&', '*', '?', ':']) {
            return None;
        }
        let refuse_name = |name: &str, marker: &Marker| {
            (!name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'))
            .then(|| {
                (
                    format!(
                        "an anchor or alias name may hold only letters, digits, `-` and `_`, \
                         not `{name}`"
                    ),
                    self.mark(marker),
                )
            })
        };
        let mut depth = 0usize;
        for token in Scanner::with_options(StrInput::new(&self.source), options()) {
            let token = match token {
                Ok(token) => token,
                Err(error) => {
                    // The parser may stop at such a name before it is read:
                    // say what is wrong with it.
                    let rest = error
                        .marker()
                        .byte_offset()
                        .and_then(|offset| self.source.get(offset..))
                        .and_then(|rest| rest.strip_prefix(['&', '*']))?;
                    let end = rest
                        .find([' ', '\t', '\r', '\n', ',', '[', ']', '{', '}', '\u{feff}'])
                        .unwrap_or(rest.len());
                    return refuse_name(&rest[..end], error.marker());
                }
            };
            let (span, kind) = token.into_parts();
            match kind {
                TokenType::FlowSequenceStart | TokenType::FlowMappingStart => depth += 1,
                TokenType::FlowSequenceEnd | TokenType::FlowMappingEnd => {
                    depth = depth.saturating_sub(1);
                }
                TokenType::Anchor(name) | TokenType::Alias(name) => {
                    if let Some(refusal) = refuse_name(&name, &span.start) {
                        return Some(refusal);
                    }
                }
                TokenType::Scalar(ScalarStyle::Plain, value)
                    if depth > 0 && value.starts_with(['?', ':']) =>
                {
                    let indicator = if value.starts_with('?') {
                        "write `? key` for a key"
                    } else {
                        "write `: value` for a value"
                    };
                    return Some((
                        format!(
                            "`{value}` inside `[...]` or `{{...}}`: {indicator}, or quote the text"
                        ),
                        self.mark(&span.start),
                    ));
                }
                _ => {}
            }
        }
        None
    }
}

pub(crate) struct Loader<'input> {
    prepared: &'input Prepared<'input>,
    source: &'input str,
    parser: Option<Parser<'input, StrInput<'input>>>,
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
    pub(crate) fn new(prepared: &'input Prepared<'input>) -> Self {
        let source: &'input str = &prepared.source;
        Loader {
            prepared,
            source,
            parser: Some(parser(source)),
            document_count: 0,
            previous_end: 0,
        }
    }

    /// The position, in the prepared text, of byte `offset`, which lies
    /// before `content` on its line or an earlier one.
    fn mark_before(&self, offset: usize, content: &Marker) -> Option<Mark> {
        let content_offset = byte_offset(content)?;
        let between = self.source.get(offset..content_offset)?;
        let index = content.index().checked_sub(between.chars().count())?;
        let (count, last) = breaks(between);
        let line = content.line().checked_sub(count)?;
        let column = match last {
            // On the content's line: count back from the content.
            None => self
                .prepared
                .raw_mark(content)
                .column
                .checked_sub(between.chars().count())?,
            // On an earlier line: count from the start of the line `offset`
            // is on.
            Some(_) => {
                let line_start = self.source[..offset]
                    .rfind(['\n', '\r'])
                    .map_or(0, |at| at + 1);
                self.source[line_start..offset].chars().count() + 1
            }
        };
        Some(Mark {
            index,
            line,
            column,
        })
    }

    /// Where a node starts, as libyaml marks it: at its anchor, tag or block
    /// scalar indicator, when it has one, else at its content.
    fn node_mark(&self, span: &Span, has_properties: bool) -> Mark {
        let content = self.prepared.raw_mark(&span.start);
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
                        if matches!(c, '\n' | '\r') {
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

    fn syntax_error(&mut self, message: String, mark: Mark) -> Arc<ErrorImpl> {
        self.parser = None;
        Arc::new(ErrorImpl::Syntax(message, mark))
    }

    pub(crate) fn next_document(&mut self) -> Option<Document> {
        let first = self.document_count == 0;
        self.document_count += 1;

        let mut document = Document {
            events: Vec::new(),
            error: None,
            aliases: BTreeMap::new(),
        };

        let prepared = self.prepared;
        if first {
            if let Some((message, mark)) = &prepared.refusal {
                document.error = Some(self.syntax_error(message.clone(), *mark));
                return Some(document);
            }
        }

        loop {
            let parser = self.parser.as_mut()?;
            let (event, span) = match parser.next() {
                Some(Ok(next)) => next,
                Some(Err(scan_error)) => {
                    let mark = self.prepared.mark(scan_error.marker());
                    document.error = Some(if *scan_error.kind() == ErrorKind::UnknownAnchor {
                        self.parser = None;
                        Arc::new(ErrorImpl::UnknownAnchor(mark))
                    } else {
                        self.syntax_error(scan_error.info(), mark)
                    });
                    return Some(document);
                }
                None => {
                    self.parser = None;
                    if first && document.events.is_empty() {
                        let mark = self.prepared.mark_at_offset(self.source.len());
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
                        document
                            .events
                            .push((Event::Void, self.prepared.mark(&span.start)));
                    }
                    return Some(document);
                }
                // An implicit start carries the span of the first token, which
                // may be the root node's anchor or tag: it must not move
                // `previous_end` past them.
                ParserEvent::DocumentStart(false, _) => continue,
                ParserEvent::DocumentStart(..) => None,
                ParserEvent::DocumentEnd => {
                    self.previous_end = byte_offset(&span.end).unwrap_or(self.previous_end);
                    return Some(document);
                }
                ParserEvent::Alias(anchor_id) => {
                    let mark = self.prepared.raw_mark(&span.start);
                    match document.aliases.get(&anchor_id) {
                        Some(index) => Some((Event::Alias(*index), mark)),
                        None => {
                            self.parser = None;
                            document.error = Some(Arc::new(ErrorImpl::UnknownAnchor(
                                self.prepared.original(mark),
                            )));
                            return Some(document);
                        }
                    }
                }
                ParserEvent::Scalar(value, style, anchor_id, tag) => {
                    let (start, end) = (byte_offset(&span.start), byte_offset(&span.end));
                    if let (Some(start), Some(end)) = (start, end) {
                        let separators = &prepared.separators;
                        let first_inside = separators.partition_point(|&at| at < start);
                        if let Some(&at) = separators.get(first_inside).filter(|&&at| at < end) {
                            let mark = self.prepared.mark_at_offset(at);
                            let message = "a LINE SEPARATOR (U+2028) or PARAGRAPH SEPARATOR \
                                           (U+2029) inside a value: write it as `\\L` or `\\P` \
                                           in a double-quoted string";
                            document.error = Some(self.syntax_error(message.to_owned(), mark));
                            return Some(document);
                        }
                    }
                    let mut style = match style {
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
                    if style == Style::DoubleQuoted
                        && value == "-"
                        && start.is_some_and(|start| {
                            self.prepared.quoted_dashes.binary_search(&start).is_ok()
                        })
                    {
                        // A lone `-` in a flow collection, quoted by `prepare`.
                        style = Style::Plain;
                    }
                    if matches!(style, Style::Literal | Style::Folded)
                        && value.ends_with('\n')
                        && end == Some(self.source.len())
                        && !self.source.ends_with(['\n', '\r'])
                        // A last line indented less than the block's content
                        // is blank and not part of it: the line break before
                        // it is the block's.
                        && span.end.col() >= span.start.col()
                    {
                        // The input ends inside the block's last line, without
                        // the line break the parser adds.
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
                ParserEvent::SequenceEnd => {
                    Some((Event::SequenceEnd, self.prepared.raw_mark(&span.start)))
                }
                ParserEvent::MappingStart(_, anchor_id, tag) => {
                    let mark = self.node_mark(&span, anchor_id != 0 || tag.is_some());
                    if anchor_id != 0 {
                        document.aliases.insert(anchor_id, document.events.len());
                    }
                    Some((Event::MappingStart(tag_string(tag.as_deref())), mark))
                }
                ParserEvent::MappingEnd => {
                    Some((Event::MappingEnd, self.prepared.raw_mark(&span.start)))
                }
                // An event kind this module does not know: refuse the document
                // rather than skip what it might hold.
                _ => {
                    let mark = self.prepared.mark(&span.start);
                    document.error =
                        Some(self.syntax_error("unsupported YAML construct".into(), mark));
                    return Some(document);
                }
            };
            if let Some(end) = byte_offset(&span.end) {
                if !empty_span || end > self.previous_end {
                    self.previous_end = end.max(self.previous_end);
                }
            }
            if let Some((event, mark)) = event {
                document.events.push((event, self.prepared.original(mark)));
            }
        }
    }
}
