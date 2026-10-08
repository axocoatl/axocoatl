//! The streaming filter between `claude setup-token` and the user's
//! terminal: every `sk-ant-…` token the program writes is shown masked, cell
//! for cell, and the tokens it masked are kept, in memory only, for
//! [`select_token`].
//!
//! Claude Code (2.1.271, read from its bundle) draws its sign-in screen with
//! its own Ink renderer: a frame is a grid of cells, and each update writes
//! only the cells that changed, moving the cursor between them (`CR` then
//! `CSI n C` and `CSI n B` to another row, `CSI n G` within one). The token
//! is an Ink `Text` in the `warning` color in a box with one column of left
//! padding, hard-wrapped at the box's width, each wrapped row placed by
//! cursor movement rather than by a line break or spaces. So the filter does
//! not read the token from the byte stream: it runs every byte through a
//! model of the terminal ([`Screen`], the program's size) and reads the
//! token from the cells its characters land in.
//!
//! - **Runs.** A run starts at an `s` and grows by each token character
//!   (`A-Z a-z 0-9 - _`) written in the cell right after the run's last one
//!   (or reached from it by autowrap), or as a wrapped continuation (below).
//!   Escapes of any kind between two characters (colors, hyperlinks and
//!   other OSC strings, DCS strings, cursor movement that lands on the next
//!   cell, synchronized-update marks) do not matter, only where the
//!   characters land. While a run is still the prefix `sk-ant-`, everything
//!   the program writes is held back; when the prefix completes, its
//!   characters are shown as the first cells of [`MASK`], and each later
//!   character of the token as the next cell of the mask (then a blank), so
//!   the screen keeps the program's layout exactly.
//!   [`TokenFilter::flush_idle`] may show a held prefix when the program
//!   goes quiet, but at most `sk-ant` (public) is ever shown.
//! - **Wrapped continuations.** A token character on the row below the
//!   run's last cell continues it when every cell right of that last cell is
//!   blank or a frame character (box drawing, block elements, `|`), every
//!   cell left of it on its row is blank or a frame character (indentation,
//!   padding, a left border), it has the SGR rendition of the run's last
//!   character, and either the run reached the right edge (at most
//!   [`FRAME_MARGIN`] columns of padding and border before the last column)
//!   and the new row starts no further right than the token did, or the new
//!   row starts in the same column as the run's current row (a box narrower
//!   than the terminal). In the second case an unstyled run (Claude Code
//!   styles its token) is masked but not stored: the row may as well be
//!   the next line of plain output. Cells a renderer skipped because they
//!   already held the same character (a token moved by a layout change) are
//!   taken from the model when they are marked as a token's.
//! - **Resizes.** From a resize until the program next moves the cursor to
//!   an absolute position, erases the screen or goes quiet, it may still
//!   draw for the old size: a token character on a run's row or the next
//!   one in the run's rendition continues it, and a token read then counts
//!   only when no other token was read.
//! - **Strings.** OSC, DCS, APC, PM and SOS strings are held until they end.
//!   Clipboard writes (OSC 52, kitty's OSC 5522, iTerm2's `Copy`,
//!   `CopyToClipboard` and `EndCopy`, and a tmux or screen DCS passthrough
//!   of any of them) are never shown, nor is a string holding `sk-ant-`; a
//!   token inside one is still captured.
//! - **Ending a token.** A token ends at the first character that does not
//!   continue it. What it holds is cut where `sk-ant-` starts again (two
//!   tokens printed together are two). A wrapped continuation followed on
//!   its row, after a blank, by more text is the program's next line rather
//!   than the token's tail: it is dropped from what is stored when what is
//!   left is still a whole token. A stored token is the OAuth prefix and
//!   then [`MIN_SECRET_CHARS`] to [`MAX_TOKEN_CHARS`] base64url characters:
//!   Claude Code's bundle does not fix the length (its own secret scanner
//!   matches `sk-ant-(oat|ort)NN-` and at least 20 of them).
//!
//! Nothing here formats a token into a string, an error or a `Debug` value,
//! and every buffer that may hold one is zeroized when it is emptied.

use zeroize::{Zeroize, Zeroizing};

use super::screen::{Cell, Print, Rendition, Screen, Step, StringKind};

/// What the user sees in place of a token: its first cells, then blank
/// cells for the rest of the token.
pub(crate) const MASK: &str = "[token hidden by axocoatl]";
/// Every Anthropic credential starts with this; all of them are hidden.
const PREFIX: &[u8] = b"sk-ant-";
/// The prefix of the long-lived OAuth token `claude setup-token` prints.
pub(crate) const OAUTH_PREFIX: &str = "sk-ant-oat01-";
/// The fewest characters after [`OAUTH_PREFIX`] a token is taken to have
/// (Claude Code's own scanner wants at least 20; its tokens have about 95).
const MIN_SECRET_CHARS: usize = 32;
/// The longest token kept; a longer run is still hidden, but not captured.
pub(crate) const MAX_TOKEN_CHARS: usize = 512;
/// How many columns of right padding and border a wrapped row may leave.
const FRAME_MARGIN: usize = 6;
/// The longest control string held; a longer one is dropped whole.
const MAX_STRING: usize = 16 * 1024;

fn is_token_char(ch: char) -> bool {
    ch.is_ascii_alphanumeric() || ch == '-' || ch == '_'
}

/// Box drawing, block elements and `|`: what frames a wrapped token.
fn is_frame(ch: char) -> bool {
    matches!(ch, '|' | '\u{2500}'..='\u{259f}')
}

fn blank_or_frame(cell: Cell) -> bool {
    cell.is_blank() || (!cell.tail && is_frame(cell.ch))
}

fn blank_or_frame_char(ch: char) -> bool {
    matches!(ch, ' ' | '\u{a0}') || is_frame(ch)
}

/// Overwrite what `buffer` holds and empty it, keeping its allocation. Its
/// spare capacity holds nothing to overwrite: every byte it ever held was
/// overwritten this way when it was emptied.
fn wipe(buffer: &mut Vec<u8>) {
    buffer.as_mut_slice().zeroize();
    buffer.clear();
}

/// The byte shown in the `index`-th cell of a token.
fn mask_byte(index: usize) -> u8 {
    MASK.as_bytes().get(index).copied().unwrap_or(b' ')
}

/// One `sk-ant-` token the filter hid.
pub(crate) struct Found {
    value: Zeroizing<String>,
    overlong: bool,
    /// Read while the terminal was being resized, when the program may still
    /// have drawn for the old size.
    suspect: bool,
}

/// Where a run's characters on one row begin.
struct Segment {
    start_x: usize,
    /// Its first character's index in the run's value.
    start: usize,
}

/// A run of token characters in progress: the prefix being matched, or a
/// token.
struct Run {
    token: bool,
    value: Zeroizing<String>,
    overlong: bool,
    /// The column the run started at.
    first_x: usize,
    last: (usize, u64),
    rendition: Rendition,
    segments: Vec<Segment>,
    /// Characters so far, the cells taken from the model included.
    index: usize,
    /// Prefix characters an idle flush showed as they are.
    shown_raw: usize,
    prefix_cells: Vec<(usize, u64)>,
    /// The held prefix characters: their offset in `held` and index.
    held_chars: Vec<(usize, usize)>,
    /// Where in `value` continuations that are masked but not stored
    /// begin (see [`TokenFilter::continues`]).
    detached_from: Option<usize>,
    suspect: bool,
}

impl Run {
    fn push(&mut self, ch: char) {
        if self.value.len() < MAX_TOKEN_CHARS {
            self.value.push(ch);
        } else {
            self.overlong = true;
        }
        self.index += 1;
    }
}

/// What a written character does to the run in progress.
enum Decision {
    /// It continues the run from this column on its row; `false` when the
    /// continuation is masked but not stored.
    Extend(usize, bool),
    Pause,
    Close(Close),
    Afresh,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Close {
    /// More text after a blank on the run's last row.
    TextAfterOnRow,
    Other,
}

/// The streaming redaction filter. See the module documentation.
pub(crate) struct TokenFilter {
    screen: Screen,
    /// The bytes of the escape sequence or character in progress.
    unit: Zeroizing<Vec<u8>>,
    string: Zeroizing<Vec<u8>>,
    string_kind: StringKind,
    string_discard: bool,
    /// Everything written while a prefix is being matched.
    held: Zeroizing<Vec<u8>>,
    run: Option<Run>,
    found: Vec<Found>,
    /// The terminal was resized and the program may still draw for the old
    /// size.
    resizing: bool,
}

impl TokenFilter {
    /// A filter for a program whose terminal is `cols` by `rows`.
    pub(crate) fn new(cols: u16, rows: u16) -> Self {
        Self {
            screen: Screen::new(cols, rows),
            unit: Zeroizing::new(Vec::with_capacity(128)),
            string: Zeroizing::new(Vec::with_capacity(MAX_STRING)),
            string_kind: StringKind::Other,
            string_discard: false,
            held: Zeroizing::new(Vec::with_capacity(8192)),
            run: None,
            found: Vec::new(),
            resizing: false,
        }
    }

    /// Filter `input`, appending what may be shown to `shown`.
    pub(crate) fn push(&mut self, input: &[u8], shown: &mut Vec<u8>) {
        for &byte in input {
            self.byte(byte, shown);
        }
    }

    /// The program's terminal changed size. Until the program next moves
    /// to an absolute position, erases the screen or is quiet, it may still
    /// draw for the old size (see the module documentation).
    pub(crate) fn resize(&mut self, cols: u16, rows: u16) {
        self.screen.resize(cols, rows);
        // A run in progress goes on: rows keep their numbers.
        self.screen.take_disrupted();
        self.resizing = true;
    }

    /// The program has been quiet: show what is held back without giving up
    /// a match in progress (a held prefix is public text).
    pub(crate) fn flush_idle(&mut self, shown: &mut Vec<u8>) {
        self.resizing = false;
        if let Some(run) = self.run.as_mut() {
            if !run.token && !self.held.is_empty() {
                shown.extend_from_slice(&self.held);
                wipe(&mut self.held);
                run.shown_raw = run.index;
                run.held_chars.clear();
            }
        }
    }

    /// The output ended: show what is held back and close a token. An
    /// unfinished escape sequence or string is dropped, so it cannot swallow
    /// what is written to the terminal next.
    pub(crate) fn finish(&mut self, shown: &mut Vec<u8>) {
        self.close(Close::Other, shown);
        wipe(&mut self.unit);
        wipe(&mut self.string);
        self.string_discard = false;
    }

    /// The bytes that put the user's terminal back from modes the program
    /// left set; empty when it left none.
    pub(crate) fn reset_sequence(&self) -> Vec<u8> {
        self.screen.reset_sequence()
    }

    /// The tokens hidden, after [`Self::finish`].
    pub(crate) fn into_found(mut self) -> Vec<Found> {
        std::mem::take(&mut self.found)
    }

    /// The tokens hidden so far (complete ones only; call
    /// [`Self::finish`] first for all of them).
    #[cfg(test)]
    pub(crate) fn found(&self) -> &[Found] {
        &self.found
    }

    fn byte(&mut self, byte: u8, shown: &mut Vec<u8>) {
        let fed = self.screen.feed(byte);
        if fed.string_broken {
            // The `ESC` held as the start of a terminator begins the next
            // sequence: the string ended before it.
            self.string.pop();
            self.end_string(shown);
            self.unit.push(0x1b);
        }
        match fed.step {
            Step::StringStart(kind) => {
                self.string_begin(kind, byte);
                return;
            }
            Step::StringByte => {
                self.string_push(byte);
                return;
            }
            Step::StringEnd => {
                self.string_push(byte);
                self.end_string(shown);
                return;
            }
            _ => {}
        }
        self.unit.push(byte);
        if self.screen.take_disrupted() {
            self.close(Close::Other, shown);
        }
        if self.screen.take_repositioned() {
            // The program moved to an absolute position or erased the
            // screen: it draws for the size it has now.
            self.resizing = false;
        }
        match fed.step {
            Step::Print(print) => {
                let unit = std::mem::take(&mut *self.unit);
                self.on_char(print, &unit, shown);
                *self.unit = unit;
                wipe(&mut self.unit);
            }
            Step::Repeat(count) => {
                // The repetition is written out, so that each character
                // goes through the filter.
                wipe(&mut self.unit);
                for _ in 0..count {
                    let Some(print) = self.screen.repeat_last() else {
                        break;
                    };
                    let mut encoded = [0u8; 4];
                    let bytes =
                        Zeroizing::new(print.ch.encode_utf8(&mut encoded).as_bytes().to_vec());
                    encoded.zeroize();
                    if self.screen.take_disrupted() {
                        self.close(Close::Other, shown);
                    }
                    self.on_char(print, &bytes, shown);
                }
            }
            _ => {
                if !self.screen.in_sequence() {
                    let unit = std::mem::take(&mut *self.unit);
                    self.emit(&unit, shown);
                    *self.unit = unit;
                    wipe(&mut self.unit);
                }
            }
        }
    }

    /// Show `bytes`, or hold them while a prefix is being matched.
    fn emit(&mut self, bytes: &[u8], shown: &mut Vec<u8>) {
        match &self.run {
            Some(run) if !run.token => self.held.extend_from_slice(bytes),
            _ => shown.extend_from_slice(bytes),
        }
    }

    fn on_char(&mut self, print: Print, bytes: &[u8], shown: &mut Vec<u8>) {
        let decision = match &self.run {
            None => Decision::Afresh,
            Some(run) => {
                let (_, last_row) = run.last;
                if is_token_char(print.ch) {
                    match self.continues(run, &print) {
                        Some((from, attached)) => Decision::Extend(from, attached),
                        None => Decision::Close(self.close_reason(&print)),
                    }
                } else if blank_or_frame_char(print.ch)
                    && (print.row == last_row || print.row == last_row + 1)
                {
                    // Padding or a border beside the run, or on the row a
                    // continuation would start on.
                    Decision::Pause
                } else {
                    Decision::Close(self.close_reason(&print))
                }
            }
        };
        match decision {
            Decision::Extend(from, attached) => {
                if self.extend(from, attached, print, bytes, shown) {
                    return;
                }
                // The prefix broke off; the character is read afresh.
            }
            Decision::Pause => {
                self.emit(bytes, shown);
                return;
            }
            Decision::Close(reason) => self.close(reason, shown),
            Decision::Afresh => {}
        }
        if print.ch == char::from(PREFIX[0]) {
            self.start(print, bytes);
            return;
        }
        self.emit(bytes, shown);
    }

    /// Where, on `print`'s row, the run's continuation begins, when `print`
    /// continues the run, and whether the continuation is stored with it.
    /// A continuation found only because the new row starts at the same
    /// column as the run's (a box narrower than the terminal) is stored when
    /// the run is styled, as Claude Code styles its token; unstyled, it is
    /// masked but not stored, since it may as well be the next line of plain
    /// output.
    fn continues(&self, run: &Run, print: &Print) -> Option<(usize, bool)> {
        let (last_x, last_row) = run.last;
        if print.wrapped_from == Some(run.last) {
            return Some((print.x, true));
        }
        if print.row == last_row && print.x == last_x + 1 {
            return Some((print.x, true));
        }
        if self.resizing
            && (print.row == last_row || print.row == last_row + 1)
            && self.screen.rendition() == run.rendition
        {
            // The program may still draw for the old size, where the
            // terminal's geometry no longer says where its next cell is.
            return Some((print.x, true));
        }
        if print.row == last_row {
            // A renderer skips cells that did not change: cells already
            // shown as part of a token in between are taken as they are.
            let skipped = (last_x + 1..print.x).all(|x| {
                self.screen
                    .cell(x, last_row)
                    .is_some_and(|cell| cell.secret && is_token_char(cell.ch))
            });
            return (run.token && print.x > last_x + 1 && skipped).then_some((last_x + 1, true));
        }
        if print.row != last_row + 1 || self.screen.rendition() != run.rendition {
            return None;
        }
        // Cells at the end of the run's row that a renderer skipped are the
        // run's.
        let last_x = last_x + self.trailing_secret(last_x, last_row);
        let mut start = print.x;
        if run.token {
            while start > 0
                && self
                    .screen
                    .cell(start - 1, print.row)
                    .is_some_and(|cell| cell.secret && is_token_char(cell.ch))
            {
                start -= 1;
            }
        }
        let left_clear =
            (0..start).all(|x| self.screen.cell(x, print.row).is_none_or(blank_or_frame));
        if !left_clear {
            return None;
        }
        if self.resizing {
            return Some((start, true));
        }
        let cols = self.screen.cols();
        let right_clear =
            (last_x + 1..cols).all(|x| self.screen.cell(x, last_row).is_none_or(blank_or_frame));
        if !right_clear {
            return None;
        }
        let at_edge = last_x + 1 + FRAME_MARGIN >= cols && start <= run.first_x;
        if at_edge {
            return Some((start, true));
        }
        let aligned = run
            .segments
            .last()
            .is_some_and(|segment| segment.start_x == start);
        let styled = run.rendition != Rendition::default();
        aligned.then_some((start, styled || !run.token))
    }

    /// How many cells right of `x` on `row` are a token's, in a row (cells a
    /// renderer skipped because they did not change).
    fn trailing_secret(&self, x: usize, row: u64) -> usize {
        (x + 1..self.screen.cols())
            .take_while(|column| {
                self.screen
                    .cell(*column, row)
                    .is_some_and(|cell| cell.secret && is_token_char(cell.ch))
            })
            .count()
    }

    /// Append to the run the cells right of its last one that a renderer
    /// skipped.
    fn take_trailing(&mut self) {
        let Some((last_x, last_row)) = self
            .run
            .as_ref()
            .filter(|run| run.token)
            .map(|run| run.last)
        else {
            return;
        };
        let count = self.trailing_secret(last_x, last_row);
        let Some(run) = self.run.as_mut() else {
            return;
        };
        for x in last_x + 1..=last_x + count {
            if let Some(cell) = self.screen.cell(x, last_row) {
                run.push(cell.ch);
                run.last = (x, last_row);
            }
        }
    }

    /// Add `print` (and the cells from `from` before it) to the run. `false`
    /// when it does not match the prefix: the run was closed.
    fn extend(
        &mut self,
        from: usize,
        attached: bool,
        print: Print,
        bytes: &[u8],
        shown: &mut Vec<u8>,
    ) -> bool {
        let mismatch = self.run.as_ref().is_some_and(|run| {
            !run.token && PREFIX.get(run.index).map(|byte| char::from(*byte)) != Some(print.ch)
        });
        if mismatch {
            self.close(Close::Other, shown);
            return false;
        }
        let rendition = self.screen.rendition();
        if self.run.as_ref().is_some_and(|run| print.row != run.last.1) {
            self.take_trailing();
        }
        let suspect = self.resizing;
        let Some(run) = self.run.as_mut() else {
            return false;
        };
        run.suspect |= suspect;
        if print.row != run.last.1 {
            run.segments.push(Segment {
                start_x: from,
                start: run.value.len(),
            });
        }
        if !attached && run.detached_from.is_none() {
            run.detached_from = Some(run.value.len());
        }
        for x in from..print.x {
            let ch = self
                .screen
                .cell(x, print.row)
                .map(|cell| cell.ch)
                .unwrap_or('\0');
            run.push(ch);
        }
        run.last = (print.x, print.row);
        run.rendition = rendition;
        if run.token {
            run.push(print.ch);
            let index = run.index - 1;
            let mask = mask_byte(index.saturating_sub(run.shown_raw));
            self.screen.mark_secret(print.x, print.row);
            shown.push(mask);
            return true;
        }
        run.held_chars.push((self.held.len(), run.index));
        run.prefix_cells.push((print.x, print.row));
        run.push(print.ch);
        let complete = run.index == PREFIX.len();
        self.held.extend_from_slice(bytes);
        if complete {
            self.complete_prefix(shown);
        }
        true
    }

    fn start(&mut self, print: Print, bytes: &[u8]) {
        let mut value = Zeroizing::new(String::with_capacity(MAX_TOKEN_CHARS));
        value.push(print.ch);
        wipe(&mut self.held);
        self.held.extend_from_slice(bytes);
        self.run = Some(Run {
            token: false,
            value,
            overlong: false,
            first_x: print.x,
            last: (print.x, print.row),
            rendition: self.screen.rendition(),
            segments: vec![Segment {
                start_x: print.x,
                start: 0,
            }],
            index: 1,
            shown_raw: 0,
            prefix_cells: vec![(print.x, print.row)],
            held_chars: vec![(0, 0)],
            detached_from: None,
            suspect: self.resizing,
        });
    }

    /// `sk-ant-` is complete: what was held is shown, the prefix as the
    /// first cells of the mask.
    fn complete_prefix(&mut self, shown: &mut Vec<u8>) {
        let Some(run) = self.run.as_mut() else {
            return;
        };
        run.token = true;
        for (offset, index) in run.held_chars.drain(..) {
            if let Some(byte) = self.held.get_mut(offset) {
                *byte = mask_byte(index.saturating_sub(run.shown_raw));
            }
        }
        shown.extend_from_slice(&self.held);
        wipe(&mut self.held);
        for (x, row) in run.prefix_cells.drain(..) {
            self.screen.mark_secret(x, row);
        }
        run.suspect |= self.resizing;
    }

    fn close_reason(&self, print: &Print) -> Close {
        let Some(run) = &self.run else {
            return Close::Other;
        };
        let (last_x, last_row) = run.last;
        let after_blank = print.row == last_row
            && print.x > last_x + 1
            && (last_x + 1..print.x).all(|x| {
                self.screen
                    .cell(x, last_row)
                    .is_none_or(|cell| cell.is_blank())
            });
        if after_blank {
            Close::TextAfterOnRow
        } else {
            Close::Other
        }
    }

    fn close(&mut self, reason: Close, shown: &mut Vec<u8>) {
        self.take_trailing();
        let Some(run) = self.run.take() else {
            return;
        };
        if !run.token {
            // Not a token after all: what was held is shown as it is.
            shown.extend_from_slice(&self.held);
            wipe(&mut self.held);
            return;
        }
        self.finalize(run, reason);
    }

    /// Keep the token(s) a closed run holds.
    fn finalize(&mut self, run: Run, reason: Close) {
        let suspect = run.suspect || self.resizing;
        if run.overlong {
            self.found.push(Found {
                value: run.value,
                overlong: true,
                suspect,
            });
            return;
        }
        let detached = run.detached_from.unwrap_or(run.value.len());
        let mut end = detached;
        if reason == Close::TextAfterOnRow && run.segments.len() > 1 && end == run.value.len() {
            if let Some(last) = run.segments.last() {
                if last.start >= OAUTH_PREFIX.len() + MIN_SECRET_CHARS {
                    end = last.start;
                }
            }
        }
        self.keep_pieces(&run.value[..end], true, suspect);
        // Of what was masked but not stored, only a token of its own (one
        // that starts with the prefix) is kept.
        self.keep_pieces(&run.value[detached..], false, suspect);
    }

    /// Keep `value` cut where a token starts again inside it; with
    /// `whole_first`, its first piece too even if it is not a prefix.
    fn keep_pieces(&mut self, value: &str, whole_first: bool, suspect: bool) {
        if value.is_empty() {
            return;
        }
        let mut starts: Vec<usize> = vec![0];
        starts.extend(
            (1..value.len()).filter(|index| value.as_bytes()[*index..].starts_with(PREFIX)),
        );
        for (number, start) in starts.iter().enumerate() {
            let piece_text = &value[*start..starts.get(number + 1).copied().unwrap_or(value.len())];
            if number == 0 && !whole_first && !piece_text.as_bytes().starts_with(PREFIX) {
                continue;
            }
            let mut piece = Zeroizing::new(String::with_capacity(piece_text.len()));
            piece.push_str(piece_text);
            self.found.push(Found {
                value: piece,
                overlong: false,
                suspect,
            });
        }
    }

    fn string_begin(&mut self, kind: StringKind, byte: u8) {
        wipe(&mut self.string);
        self.string_discard = false;
        self.string_kind = kind;
        let unit = std::mem::take(&mut *self.unit);
        self.string.extend_from_slice(&unit);
        *self.unit = unit;
        wipe(&mut self.unit);
        self.string_push(byte);
    }

    fn string_push(&mut self, byte: u8) {
        if self.string_discard {
            return;
        }
        if self.string.len() >= MAX_STRING {
            self.string_discard = true;
            wipe(&mut self.string);
            return;
        }
        self.string.push(byte);
    }

    /// A control string ended: it is shown unless it writes the clipboard
    /// or holds a token (which is captured).
    fn end_string(&mut self, shown: &mut Vec<u8>) {
        let mut string = std::mem::take(&mut *self.string);
        let discard = std::mem::take(&mut self.string_discard);
        if !discard && !string.is_empty() {
            let has_token = self.capture_in_string(&string);
            if !has_token && !is_clipboard(self.string_kind, &string) {
                self.emit(&string, shown);
            }
        }
        wipe(&mut string);
        *self.string = string;
    }

    /// Capture every token in a control string; whether there was one.
    fn capture_in_string(&mut self, string: &[u8]) -> bool {
        let mut any = false;
        let mut index = 0;
        while let Some(offset) = string[index..]
            .windows(PREFIX.len())
            .position(|window| window == PREFIX)
        {
            any = true;
            let start = index + offset;
            let length = string[start..]
                .iter()
                .take_while(|byte| is_token_char(char::from(**byte)))
                .count();
            let overlong = length > MAX_TOKEN_CHARS;
            let mut value = Zeroizing::new(String::with_capacity(MAX_TOKEN_CHARS));
            for byte in &string[start..start + length.min(MAX_TOKEN_CHARS)] {
                value.push(char::from(*byte));
            }
            self.found.push(Found {
                value,
                overlong,
                suspect: false,
            });
            index = start + length.max(PREFIX.len());
        }
        any
    }
}

/// Whether a control string writes the clipboard: OSC 52, kitty's OSC 5522,
/// iTerm2's `Copy`, `CopyToClipboard` and `EndCopy`, or a DCS passthrough
/// (tmux, screen) of one of them.
fn is_clipboard(kind: StringKind, string: &[u8]) -> bool {
    let payload = string.get(2..).unwrap_or_default();
    let payload = payload
        .strip_suffix(b"\x1b\\")
        .or_else(|| payload.strip_suffix(b"\x07"))
        .unwrap_or(payload);
    let contains = |needle: &[u8]| payload.windows(needle.len()).any(|window| window == needle);
    match kind {
        StringKind::Osc => {
            let command = payload
                .split(|byte| *byte == b';')
                .next()
                .unwrap_or_default();
            match command {
                b"52" | b"5522" => true,
                b"1337" => {
                    let rest = payload.get(5..).unwrap_or_default();
                    [&b"Copy="[..], b"CopyToClipboard", b"EndCopy"]
                        .iter()
                        .any(|start| rest.starts_with(start))
                }
                _ => false,
            }
        }
        StringKind::Dcs => [&b"]52;"[..], b"]5522;", b"]1337;Copy", b"]1337;EndCopy"]
            .iter()
            .any(|needle| contains(needle)),
        StringKind::Other => false,
    }
}

/// What the output held.
pub(crate) enum Selection {
    /// Exactly one Claude Code OAuth token (it may have been printed more
    /// than once).
    One(Zeroizing<String>),
    /// No Claude Code OAuth token; `other` other `sk-ant-` tokens were hidden.
    None { other: usize },
    /// This many different Claude Code OAuth tokens.
    Several(usize),
}

/// Whether `found` has the form of a whole `claude setup-token` token: the
/// OAuth prefix, then at least [`MIN_SECRET_CHARS`] base64url characters.
fn plausible(found: &Found) -> bool {
    let value = found.value.as_str();
    !found.overlong
        && value.len() >= OAUTH_PREFIX.len() + MIN_SECRET_CHARS
        && value.len() <= MAX_TOKEN_CHARS
        && value.starts_with(OAUTH_PREFIX)
        && value.chars().all(is_token_char)
}

fn distinct<'a>(candidates: impl Iterator<Item = &'a Found>) -> Vec<&'a Found> {
    let mut distinct: Vec<&Found> = Vec::new();
    for candidate in candidates {
        if !distinct
            .iter()
            .any(|seen| seen.value.as_str() == candidate.value.as_str())
        {
            distinct.push(candidate);
        }
    }
    distinct
}

/// The one token to store, from everything [`TokenFilter`] hid. Tokens read
/// while the terminal was being resized (the program may have drawn them
/// for the old size) count only when no other token was read, and then the
/// last one does (the program's redraw for the new size).
pub(crate) fn select_token(found: &[Found]) -> Selection {
    let plausible_ones: Vec<&Found> = found.iter().filter(|found| plausible(found)).collect();
    let other = found.len() - plausible_ones.len();
    let settled = distinct(
        plausible_ones
            .iter()
            .copied()
            .filter(|found| !found.suspect),
    );
    let chosen = if settled.is_empty() {
        plausible_ones.last().copied().into_iter().collect()
    } else {
        settled
    };
    match chosen.as_slice() {
        [] => Selection::None { other },
        [one] => Selection::One(Zeroizing::new(one.value.as_str().to_owned())),
        several => Selection::Several(several.len()),
    }
}

#[cfg(test)]
pub(crate) mod tests;
