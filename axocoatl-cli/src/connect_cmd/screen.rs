//! A model of the terminal `claude setup-token` draws on: what each byte of
//! its output does to the cursor and to the cells, as the user's terminal
//! (which has the same size) does it. [`super::redact::TokenFilter`] decides
//! from it which written characters form a token, whatever escapes, cursor
//! movements or line breaks separate them in the byte stream.
//!
//! It covers what terminal programs use: printable text with autowrap (the
//! pending wrap at the last column), wide and zero-width characters, C0
//! controls, cursor movement (CUU, CUD, CUF, CUB, CNL, CPL, CHA, HPA, HPR,
//! VPA, VPR, CUP, HVP, tabs, DECSC/DECRC and SCOSC/SCORC), erasure (ED, EL,
//! ECH), insertion and deletion (ICH, DCH, IL, DL), scrolling (IND, RI, NEL,
//! SU, SD, DECSTBM), REP, the origin and autowrap modes, the alternate
//! screen, soft and full reset, and SGR renditions. Strings (OSC, DCS, APC,
//! PM, SOS) are reported byte by byte to the caller, which buffers them.
//!
//! Cells hold what the program wrote, the token included, so every buffer
//! here is zeroized when it is cleared, replaced or dropped.

use std::collections::VecDeque;

use unicode_width::UnicodeWidthChar;
use zeroize::Zeroize;

/// Rows that scrolled off the top that are kept, for continuation checks.
const HISTORY_ROWS: usize = 16;
/// The longest CSI parameter string kept; a longer one is ignored, as
/// terminals ignore it.
const MAX_PARAMS: usize = 64;
/// The most repetitions one REP may ask for.
const MAX_REPEAT: usize = 4096;

/// One character cell.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Cell {
    /// `'\0'` for a cell nothing was written to (or that was erased).
    pub(crate) ch: char,
    /// Written as part of a token (shown masked).
    pub(crate) secret: bool,
    /// The right half of a wide character.
    pub(crate) tail: bool,
}

impl Zeroize for Cell {
    fn zeroize(&mut self) {
        self.ch.zeroize();
        self.secret.zeroize();
        self.tail.zeroize();
    }
}

impl Cell {
    /// Nothing visible: never written, erased, or a space.
    pub(crate) fn is_blank(&self) -> bool {
        !self.tail && matches!(self.ch, '\0' | ' ' | '\u{a0}')
    }
}

/// A color of an SGR rendition.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum Color {
    #[default]
    Default,
    Indexed(u8),
    Rgb(u8, u8, u8),
}

/// The SGR state characters are written with, in canonical form (two
/// sequences that render alike compare equal).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Rendition {
    attributes: u16,
    foreground: Color,
    background: Color,
    underline_color: Color,
}

const BOLD: u16 = 1;
const DIM: u16 = 1 << 1;
const ITALIC: u16 = 1 << 2;
const UNDERLINE: u16 = 1 << 3;
const BLINK: u16 = 1 << 4;
const INVERSE: u16 = 1 << 5;
const HIDDEN: u16 = 1 << 6;
const STRIKE: u16 = 1 << 7;
const OVERLINE: u16 = 1 << 8;
const DOUBLE_UNDERLINE: u16 = 1 << 9;

impl Rendition {
    /// Apply one SGR parameter list (`1;38;5;214`, `38:2::255:193:7`).
    fn apply(&mut self, params: &[u8]) {
        let groups: Vec<Vec<Option<u16>>> = params
            .split(|byte| *byte == b';')
            .map(|group| group.split(|byte| *byte == b':').map(number).collect())
            .collect();
        let mut index = 0;
        while index < groups.len() {
            let group = &groups[index];
            let code = group.first().copied().flatten().unwrap_or(0);
            index += 1;
            match code {
                0 => *self = Self::default(),
                1 => self.attributes |= BOLD,
                2 => self.attributes |= DIM,
                3 => self.attributes |= ITALIC,
                4 => {
                    let style = group.get(1).copied().flatten();
                    if style == Some(0) {
                        self.attributes &= !(UNDERLINE | DOUBLE_UNDERLINE);
                    } else {
                        self.attributes |= UNDERLINE;
                    }
                }
                5 | 6 => self.attributes |= BLINK,
                7 => self.attributes |= INVERSE,
                8 => self.attributes |= HIDDEN,
                9 => self.attributes |= STRIKE,
                21 => self.attributes |= DOUBLE_UNDERLINE,
                22 => self.attributes &= !(BOLD | DIM),
                23 => self.attributes &= !ITALIC,
                24 => self.attributes &= !(UNDERLINE | DOUBLE_UNDERLINE),
                25 => self.attributes &= !BLINK,
                27 => self.attributes &= !INVERSE,
                28 => self.attributes &= !HIDDEN,
                29 => self.attributes &= !STRIKE,
                30..=37 => self.foreground = Color::Indexed((code - 30) as u8),
                39 => self.foreground = Color::Default,
                40..=47 => self.background = Color::Indexed((code - 40) as u8),
                49 => self.background = Color::Default,
                53 => self.attributes |= OVERLINE,
                55 => self.attributes &= !OVERLINE,
                59 => self.underline_color = Color::Default,
                90..=97 => self.foreground = Color::Indexed((code - 90 + 8) as u8),
                100..=107 => self.background = Color::Indexed((code - 100 + 8) as u8),
                38 | 48 | 58 => {
                    let color = if group.len() > 1 {
                        extended_color(&group[1..], true)
                    } else {
                        // `38;5;n` and `38;2;r;g;b`: the rest is in the
                        // following groups.
                        let rest: Vec<Option<u16>> = groups[index..]
                            .iter()
                            .map(|group| group.first().copied().flatten())
                            .collect();
                        let (color, used) = extended_color_semicolons(&rest);
                        index += used;
                        color
                    };
                    if let Some(color) = color {
                        match code {
                            38 => self.foreground = color,
                            48 => self.background = color,
                            _ => self.underline_color = color,
                        }
                    }
                }
                _ => {}
            }
        }
    }
}

fn number(digits: &[u8]) -> Option<u16> {
    if digits.is_empty() || !digits.iter().all(u8::is_ascii_digit) {
        return None;
    }
    let mut value: u32 = 0;
    for digit in digits {
        value = (value * 10 + u32::from(digit - b'0')).min(u32::from(u16::MAX));
    }
    Some(value as u16)
}

fn channel(value: Option<u16>) -> u8 {
    value.unwrap_or(0).min(255) as u8
}

/// `5:n` or `2:[colorspace:]r:g:b` after `38:`.
fn extended_color(sub: &[Option<u16>], colons: bool) -> Option<Color> {
    match sub.first().copied().flatten() {
        Some(5) => Some(Color::Indexed(channel(sub.get(1).copied().flatten()))),
        Some(2) => {
            let rgb = if colons && sub.len() >= 5 {
                &sub[2..5]
            } else {
                &sub[1..sub.len().min(4)]
            };
            Some(Color::Rgb(
                channel(rgb.first().copied().flatten()),
                channel(rgb.get(1).copied().flatten()),
                channel(rgb.get(2).copied().flatten()),
            ))
        }
        _ => None,
    }
}

/// `5;n` or `2;r;g;b` after `38;`: the color and how many groups it used.
fn extended_color_semicolons(rest: &[Option<u16>]) -> (Option<Color>, usize) {
    match rest.first().copied().flatten() {
        Some(5) => (
            Some(Color::Indexed(channel(rest.get(1).copied().flatten()))),
            2.min(rest.len()),
        ),
        Some(2) => (
            Some(Color::Rgb(
                channel(rest.get(1).copied().flatten()),
                channel(rest.get(2).copied().flatten()),
                channel(rest.get(3).copied().flatten()),
            )),
            4.min(rest.len()),
        ),
        _ => (None, 1.min(rest.len())),
    }
}

/// The kinds of control strings.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum StringKind {
    Osc,
    Dcs,
    /// APC, PM and SOS: terminals ignore them.
    Other,
}

/// A character the program wrote.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Print {
    pub(crate) ch: char,
    /// Its column.
    pub(crate) x: usize,
    /// Its row, counted from the first row the screen ever had (rows that
    /// scrolled off the top keep their number).
    pub(crate) row: u64,
    /// The cell of the character printed just before, when this one
    /// continues it on the next row by autowrap (nothing moved the cursor in
    /// between).
    pub(crate) wrapped_from: Option<(usize, u64)>,
}

/// What a byte did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Step {
    /// Nothing visible yet (a sequence in progress) or a control function
    /// that changes no cell.
    None,
    /// A character was written.
    Print(Print),
    /// REP: write the last character this many more times.
    Repeat(usize),
    /// A control string begins (the `ESC` before this byte belongs to it).
    StringStart(StringKind),
    /// A byte of a control string's content (or a pending `ESC` of its
    /// terminator).
    StringByte,
    /// The string's terminator (`BEL`, or the `\` of `ESC \`).
    StringEnd,
}

/// The result of one byte.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Fed {
    /// The `ESC` held as the start of a string terminator turned out to
    /// start a new sequence instead: the string ended before it, and that
    /// `ESC` belongs to the sequence this byte continues.
    pub(crate) string_broken: bool,
    pub(crate) step: Step,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
    Ground,
    Utf8 { need: u8 },
    Escape,
    EscapeIntermediate { first: u8 },
    Csi,
    CsiIgnore,
    String { kind: StringKind, escape: bool },
}

#[derive(Clone, Copy, Debug, Default)]
struct SavedCursor {
    x: usize,
    y: usize,
    pending_wrap: bool,
    rendition: Rendition,
    origin: bool,
}

/// Modes a program may leave set if it is stopped, which the user's
/// terminal must get back.
#[derive(Clone, Copy, Debug, Default)]
struct Modes {
    cursor_hidden: bool,
    bracketed_paste: bool,
    focus_events: bool,
    mouse: bool,
    synchronized: bool,
    modify_other_keys: bool,
    kitty_keyboard: usize,
}

/// The terminal model. See the module documentation.
pub(crate) struct Screen {
    cols: usize,
    rows: usize,
    grid: Vec<Cell>,
    /// The main screen's cells while the alternate screen is shown.
    main: Option<Vec<Cell>>,
    main_cursor: Option<SavedCursor>,
    x: usize,
    y: usize,
    pending_wrap: bool,
    saved: SavedCursor,
    top: usize,
    bottom: usize,
    origin: bool,
    autowrap: bool,
    rendition: Rendition,
    /// Rows that scrolled off the top of the whole screen so far.
    scrolled: u64,
    history: VecDeque<Vec<Cell>>,
    last_print: Option<(usize, u64)>,
    last_char: Option<char>,
    state: State,
    params: Vec<u8>,
    utf8: u32,
    /// Rows moved in a way rows are not numbered for (a scroll region, the
    /// alternate screen, insertion or deletion, a reset, a resize).
    disrupted: bool,
    /// The cursor moved to an absolute position, or the screen was erased.
    repositioned: bool,
    modes: Modes,
}

impl Drop for Screen {
    fn drop(&mut self) {
        self.grid.zeroize();
        if let Some(main) = self.main.as_mut() {
            main.zeroize();
        }
        for row in self.history.iter_mut() {
            row.zeroize();
        }
        self.params.zeroize();
        self.last_char.zeroize();
    }
}

impl Screen {
    pub(crate) fn new(cols: u16, rows: u16) -> Self {
        let cols = usize::from(cols.max(1));
        let rows = usize::from(rows.max(1));
        Self {
            cols,
            rows,
            grid: vec![Cell::default(); cols * rows],
            main: None,
            main_cursor: None,
            x: 0,
            y: 0,
            pending_wrap: false,
            saved: SavedCursor::default(),
            top: 0,
            bottom: rows - 1,
            origin: false,
            autowrap: true,
            rendition: Rendition::default(),
            scrolled: 0,
            history: VecDeque::with_capacity(HISTORY_ROWS + 1),
            last_print: None,
            last_char: None,
            state: State::Ground,
            params: Vec::with_capacity(MAX_PARAMS),
            utf8: 0,
            disrupted: false,
            repositioned: false,
            modes: Modes::default(),
        }
    }

    pub(crate) fn cols(&self) -> usize {
        self.cols
    }

    /// The current SGR rendition.
    pub(crate) fn rendition(&self) -> Rendition {
        self.rendition
    }

    /// Whether a sequence (escape, CSI, UTF-8 or string) is in progress.
    pub(crate) fn in_sequence(&self) -> bool {
        self.state != State::Ground
    }

    /// Whether rows moved since the last call in a way their numbers do not
    /// follow.
    pub(crate) fn take_disrupted(&mut self) -> bool {
        std::mem::take(&mut self.disrupted)
    }

    /// Whether, since the last call, the cursor was moved to an absolute
    /// position (CUP, HVP) or the screen was erased (ED): what a program
    /// does when it redraws.
    pub(crate) fn take_repositioned(&mut self) -> bool {
        std::mem::take(&mut self.repositioned)
    }

    /// The cell at column `x` of row `row` (see [`Print::row`]), while it is
    /// on the screen or among the last rows that scrolled off.
    pub(crate) fn cell(&self, x: usize, row: u64) -> Option<Cell> {
        if row >= self.scrolled {
            let y = usize::try_from(row - self.scrolled).ok()?;
            if y >= self.rows || x >= self.cols {
                return None;
            }
            return Some(self.grid[y * self.cols + x]);
        }
        let back = usize::try_from(self.scrolled - row).ok()?;
        let line = self.history.get(self.history.len().checked_sub(back)?)?;
        line.get(x).copied()
    }

    /// Mark the cell at `x` of `row` as part of a token.
    pub(crate) fn mark_secret(&mut self, x: usize, row: u64) {
        if row >= self.scrolled {
            if let Ok(y) = usize::try_from(row - self.scrolled) {
                if y < self.rows && x < self.cols {
                    self.grid[y * self.cols + x].secret = true;
                }
            }
        }
    }

    /// The bytes that put the user's terminal back from modes the program
    /// left set (it was stopped, or did not clean up); empty when none.
    pub(crate) fn reset_sequence(&self) -> Vec<u8> {
        let mut reset = Vec::new();
        let modes = &self.modes;
        if modes.synchronized {
            reset.extend_from_slice(b"\x1b[?2026l");
        }
        if modes.mouse {
            reset.extend_from_slice(b"\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1015l");
        }
        if modes.focus_events {
            reset.extend_from_slice(b"\x1b[?1004l");
        }
        if modes.bracketed_paste {
            reset.extend_from_slice(b"\x1b[?2004l");
        }
        if modes.kitty_keyboard > 0 {
            reset.extend_from_slice(format!("\x1b[<{}u", modes.kitty_keyboard).as_bytes());
        }
        if modes.modify_other_keys {
            reset.extend_from_slice(b"\x1b[>4m");
        }
        if self.main.is_some() {
            reset.extend_from_slice(b"\x1b[?1049l");
        }
        if self.rendition != Rendition::default() {
            reset.extend_from_slice(b"\x1b[0m");
        }
        if modes.cursor_hidden {
            reset.extend_from_slice(b"\x1b[?25h");
        }
        reset
    }

    /// The user's terminal changed size: so does this one (cells are kept
    /// from the top left, as terminals that do not reflow keep them).
    pub(crate) fn resize(&mut self, cols: u16, rows: u16) {
        let cols = usize::from(cols.max(1));
        let rows = usize::from(rows.max(1));
        if cols == self.cols && rows == self.rows {
            return;
        }
        let resized = |old: &mut Vec<Cell>| -> Vec<Cell> {
            let mut new = vec![Cell::default(); cols * rows];
            for y in 0..rows.min(self.rows) {
                for x in 0..cols.min(self.cols) {
                    new[y * cols + x] = old[y * self.cols + x];
                }
            }
            old.zeroize();
            new
        };
        self.grid = resized(&mut self.grid);
        if let Some(main) = self.main.as_mut() {
            *main = resized(main);
        }
        self.cols = cols;
        self.rows = rows;
        self.x = self.x.min(cols - 1);
        self.y = self.y.min(rows - 1);
        self.pending_wrap = false;
        self.top = 0;
        self.bottom = rows - 1;
        self.saved.x = self.saved.x.min(cols - 1);
        self.saved.y = self.saved.y.min(rows - 1);
        self.last_print = None;
        self.disrupted = true;
    }

    /// Process one byte of the program's output.
    pub(crate) fn feed(&mut self, byte: u8) -> Fed {
        let step = match self.state {
            State::Ground => self.ground(byte),
            State::Utf8 { need } => self.utf8_continue(byte, need),
            State::Escape => self.escape(byte),
            State::EscapeIntermediate { first } => self.escape_intermediate(byte, first),
            State::Csi | State::CsiIgnore => self.csi(byte),
            State::String { kind, escape } => {
                return self.string(byte, kind, escape);
            }
        };
        Fed {
            string_broken: false,
            step,
        }
    }

    /// Print `last_char` once more (REP).
    pub(crate) fn repeat_last(&mut self) -> Option<Print> {
        let ch = self.last_char?;
        self.print(ch)
    }

    fn ground(&mut self, byte: u8) -> Step {
        match byte {
            0x1b => {
                self.state = State::Escape;
                Step::None
            }
            0x00..=0x1f | 0x7f => {
                self.control(byte);
                Step::None
            }
            0x20..=0x7e => self.print_step(char::from(byte)),
            0xc2..=0xdf => self.utf8_start(byte & 0x1f, 1),
            0xe0..=0xef => self.utf8_start(byte & 0x0f, 2),
            0xf0..=0xf4 => self.utf8_start(byte & 0x07, 3),
            // A stray continuation or an invalid lead byte shows as U+FFFD.
            _ => self.print_step(char::REPLACEMENT_CHARACTER),
        }
    }

    fn utf8_start(&mut self, bits: u8, need: u8) -> Step {
        self.utf8 = u32::from(bits);
        self.state = State::Utf8 { need };
        Step::None
    }

    fn utf8_continue(&mut self, byte: u8, need: u8) -> Step {
        if byte & 0xc0 != 0x80 {
            // The sequence broke off: a replacement character, then this
            // byte afresh.
            self.state = State::Ground;
            self.utf8 = 0;
            let replaced = self.print_step(char::REPLACEMENT_CHARACTER);
            let next = self.ground(byte);
            return match next {
                Step::None => replaced,
                other => other,
            };
        }
        self.utf8 = (self.utf8 << 6) | u32::from(byte & 0x3f);
        if need > 1 {
            self.state = State::Utf8 { need: need - 1 };
            return Step::None;
        }
        self.state = State::Ground;
        let ch = char::from_u32(self.utf8).unwrap_or(char::REPLACEMENT_CHARACTER);
        self.utf8 = 0;
        self.print_step(ch)
    }

    fn print_step(&mut self, ch: char) -> Step {
        match self.print(ch) {
            Some(print) => Step::Print(print),
            None => Step::None,
        }
    }

    /// Write `ch` at the cursor. Zero-width characters change no cell.
    fn print(&mut self, ch: char) -> Option<Print> {
        let width = ch.width().unwrap_or(0).min(2);
        if width == 0 {
            return None;
        }
        let mut wrapped_from = None;
        if self.pending_wrap {
            self.pending_wrap = false;
            if self.autowrap {
                wrapped_from = self.last_print;
                self.x = 0;
                self.linefeed();
            }
        }
        if width == 2 && self.x + 1 >= self.cols {
            if self.autowrap && self.cols >= 2 {
                wrapped_from = None;
                self.x = 0;
                self.linefeed();
            } else {
                self.x = self.cols.saturating_sub(2);
            }
        }
        let (x, y) = (self.x, self.y);
        self.split_wide(x, y);
        if width == 2 && x + 1 < self.cols {
            self.split_wide(x + 1, y);
        }
        let index = y * self.cols + x;
        self.grid[index] = Cell {
            ch,
            secret: false,
            tail: false,
        };
        if width == 2 && x + 1 < self.cols {
            self.grid[index + 1] = Cell {
                ch: ' ',
                secret: false,
                tail: true,
            };
        }
        let row = self.scrolled + y as u64;
        self.last_print = Some((x, row));
        self.last_char = Some(ch);
        if x + width >= self.cols {
            self.x = self.cols - 1;
            self.pending_wrap = self.autowrap;
        } else {
            self.x = x + width;
        }
        Some(Print {
            ch,
            x,
            row,
            wrapped_from,
        })
    }

    /// Overwriting half of a wide character blanks its other half.
    fn split_wide(&mut self, x: usize, y: usize) {
        let index = y * self.cols + x;
        let cell = self.grid[index];
        if cell.tail && x > 0 {
            self.grid[index - 1] = Cell::default();
        } else if !cell.tail && x + 1 < self.cols && self.grid[index + 1].tail {
            self.grid[index + 1] = Cell::default();
        }
    }

    fn moved(&mut self) {
        self.pending_wrap = false;
        self.last_print = None;
    }

    fn control(&mut self, byte: u8) {
        match byte {
            0x08 => {
                self.moved();
                self.x = self.x.saturating_sub(1);
            }
            0x09 => self.tab_forward(1),
            0x0a..=0x0c => {
                self.moved();
                self.linefeed();
            }
            0x0d => {
                self.moved();
                self.x = 0;
            }
            _ => {}
        }
    }

    fn tab_forward(&mut self, count: usize) {
        self.moved();
        for _ in 0..count {
            self.x = ((self.x / 8 + 1) * 8).min(self.cols - 1);
        }
    }

    fn tab_backward(&mut self, count: usize) {
        self.moved();
        for _ in 0..count {
            self.x = if self.x == 0 { 0 } else { (self.x - 1) / 8 * 8 };
        }
    }

    /// Down one row; at the bottom margin the region scrolls up.
    fn linefeed(&mut self) {
        if self.y == self.bottom {
            self.scroll_up(1);
        } else if self.y + 1 < self.rows {
            self.y += 1;
        }
    }

    fn reverse_index(&mut self) {
        self.moved();
        if self.y == self.top {
            self.scroll_down(1);
        } else if self.y > 0 {
            self.y -= 1;
        }
    }

    fn whole_screen(&self) -> bool {
        self.top == 0 && self.bottom == self.rows - 1 && self.main.is_none()
    }

    fn row_range(&self, y: usize) -> std::ops::Range<usize> {
        y * self.cols..(y + 1) * self.cols
    }

    fn clear_row(&mut self, y: usize) {
        let range = self.row_range(y);
        self.grid[range].iter_mut().for_each(Zeroize::zeroize);
    }

    /// Scroll the region up `count` rows (the top rows leave it).
    fn scroll_up(&mut self, count: usize) {
        let count = count.min(self.bottom - self.top + 1);
        if self.whole_screen() {
            for y in 0..count {
                let range = self.row_range(y);
                if self.history.len() == HISTORY_ROWS {
                    if let Some(mut oldest) = self.history.pop_front() {
                        oldest.zeroize();
                    }
                }
                self.history.push_back(self.grid[range].to_vec());
            }
            self.scrolled += count as u64;
        } else {
            self.disrupted = true;
        }
        let (top, bottom, cols) = (self.top, self.bottom, self.cols);
        self.grid[top * cols..(bottom + 1) * cols].rotate_left(count * cols);
        for y in bottom + 1 - count..=bottom {
            self.clear_row(y);
        }
    }

    /// Scroll the region down `count` rows (blank rows enter at its top).
    fn scroll_down(&mut self, count: usize) {
        let count = count.min(self.bottom - self.top + 1);
        self.disrupted = true;
        let (top, bottom, cols) = (self.top, self.bottom, self.cols);
        self.grid[top * cols..(bottom + 1) * cols].rotate_right(count * cols);
        for y in top..top + count {
            self.clear_row(y);
        }
    }

    fn escape(&mut self, byte: u8) -> Step {
        self.state = State::Ground;
        match byte {
            b'[' => {
                self.params.clear();
                self.state = State::Csi;
                Step::None
            }
            b']' => self.start_string(StringKind::Osc),
            b'P' => self.start_string(StringKind::Dcs),
            b'_' | b'^' | b'X' => self.start_string(StringKind::Other),
            0x1b => {
                self.state = State::Escape;
                Step::None
            }
            0x18 | 0x1a => Step::None,
            0x00..=0x1f => {
                // A control inside an escape runs, and the escape goes on.
                self.control(byte);
                self.state = State::Escape;
                Step::None
            }
            0x20..=0x2f => {
                self.state = State::EscapeIntermediate { first: byte };
                Step::None
            }
            b'7' => {
                self.save_cursor();
                Step::None
            }
            b'8' => {
                self.restore_cursor();
                Step::None
            }
            b'D' => {
                self.moved();
                self.linefeed();
                Step::None
            }
            b'E' => {
                self.moved();
                self.x = 0;
                self.linefeed();
                Step::None
            }
            b'M' => {
                self.reverse_index();
                Step::None
            }
            b'c' => {
                self.full_reset();
                Step::None
            }
            _ => Step::None,
        }
    }

    fn escape_intermediate(&mut self, byte: u8, first: u8) -> Step {
        match byte {
            0x20..=0x2f => Step::None,
            0x30..=0x7e => {
                self.state = State::Ground;
                if first == b'#' && byte == b'8' {
                    // DECALN fills the screen with `E`.
                    self.moved();
                    self.disrupted = true;
                    for cell in self.grid.iter_mut() {
                        *cell = Cell {
                            ch: 'E',
                            secret: false,
                            tail: false,
                        };
                    }
                }
                Step::None
            }
            0x18 | 0x1a => {
                self.state = State::Ground;
                Step::None
            }
            0x1b => {
                self.state = State::Escape;
                Step::None
            }
            _ => {
                self.control(byte);
                Step::None
            }
        }
    }

    fn start_string(&mut self, kind: StringKind) -> Step {
        self.state = State::String {
            kind,
            escape: false,
        };
        Step::StringStart(kind)
    }

    fn string(&mut self, byte: u8, kind: StringKind, escape: bool) -> Fed {
        let step = |step| Fed {
            string_broken: false,
            step,
        };
        if escape {
            if byte == b'\\' {
                self.state = State::Ground;
                return step(Step::StringEnd);
            }
            if kind == StringKind::Dcs && byte == 0x1b {
                // A doubled `ESC` is how tmux and screen passthroughs carry
                // an escape sequence inside a DCS string: data.
                self.state = State::String {
                    kind,
                    escape: false,
                };
                return step(Step::StringByte);
            }
            // `ESC` then something else: the string ended, and the `ESC`
            // starts a new sequence that this byte continues.
            self.state = State::Escape;
            let next = self.escape(byte);
            return Fed {
                string_broken: true,
                step: next,
            };
        }
        match byte {
            0x07 if kind == StringKind::Osc => {
                self.state = State::Ground;
                step(Step::StringEnd)
            }
            0x1b => {
                self.state = State::String { kind, escape: true };
                step(Step::StringByte)
            }
            0x18 | 0x1a => {
                self.state = State::Ground;
                step(Step::StringEnd)
            }
            _ => step(Step::StringByte),
        }
    }

    fn csi(&mut self, byte: u8) -> Step {
        match byte {
            0x20..=0x3f => {
                if self.params.len() < MAX_PARAMS {
                    self.params.push(byte);
                } else {
                    self.state = State::CsiIgnore;
                }
                Step::None
            }
            0x40..=0x7e => {
                let ignored = self.state == State::CsiIgnore;
                self.state = State::Ground;
                let step = if ignored {
                    Step::None
                } else {
                    self.dispatch_csi(byte)
                };
                self.params.clear();
                step
            }
            0x18 | 0x1a => {
                self.state = State::Ground;
                self.params.clear();
                Step::None
            }
            0x1b => {
                self.state = State::Escape;
                self.params.clear();
                Step::None
            }
            _ => {
                // A control inside a CSI sequence runs, and it goes on.
                self.control(byte);
                Step::None
            }
        }
    }

    /// The numeric parameters, `0` for an empty one.
    fn numbers(params: &[u8]) -> Vec<usize> {
        params
            .split(|byte| *byte == b';')
            .map(|part| {
                let digits: Vec<u8> = part
                    .iter()
                    .copied()
                    .take_while(u8::is_ascii_digit)
                    .collect();
                number(&digits).map(usize::from).unwrap_or(0)
            })
            .collect()
    }

    fn dispatch_csi(&mut self, final_byte: u8) -> Step {
        let params = std::mem::take(&mut self.params);
        let private = params
            .first()
            .copied()
            .filter(|byte| b"<=>?".contains(byte));
        let intermediates: Vec<u8> = params
            .iter()
            .copied()
            .filter(|byte| (0x20..=0x2f).contains(byte))
            .collect();
        let body: Vec<u8> = params
            .iter()
            .copied()
            .skip(usize::from(private.is_some()))
            .filter(|byte| (0x30..=0x3b).contains(byte))
            .collect();
        self.params = params;
        let numbers = Self::numbers(&body);
        let first = numbers.first().copied().unwrap_or(0);
        let count = first.max(1);
        let second = numbers.get(1).copied().unwrap_or(0);
        if !intermediates.is_empty() {
            if intermediates == b"!" && final_byte == b'p' && private.is_none() {
                self.soft_reset();
            }
            return Step::None;
        }
        match private {
            Some(b'?') => {
                if matches!(final_byte, b'h' | b'l') {
                    for mode in numbers {
                        self.private_mode(mode, final_byte == b'h');
                    }
                } else if final_byte == b'J' {
                    self.erase_display(first);
                } else if final_byte == b'K' {
                    self.erase_line(first);
                }
                return Step::None;
            }
            Some(b'>') => {
                if final_byte == b'u' {
                    self.modes.kitty_keyboard = self.modes.kitty_keyboard.saturating_add(1);
                } else if final_byte == b'm' && first == 4 {
                    self.modes.modify_other_keys = second > 0;
                }
                return Step::None;
            }
            Some(b'<') => {
                if final_byte == b'u' {
                    self.modes.kitty_keyboard = self.modes.kitty_keyboard.saturating_sub(count);
                }
                return Step::None;
            }
            Some(_) => return Step::None,
            None => {}
        }
        match final_byte {
            b'A' => self.move_vertical(-(count as isize)),
            b'B' | b'e' => self.move_vertical(count as isize),
            b'C' | b'a' => self.move_to_column(self.x.saturating_add(count)),
            b'D' => self.move_to_column(self.x.saturating_sub(count)),
            b'E' => {
                self.move_vertical(count as isize);
                self.x = 0;
            }
            b'F' => {
                self.move_vertical(-(count as isize));
                self.x = 0;
            }
            b'G' | b'`' => self.move_to_column(count - 1),
            b'H' | b'f' => {
                self.repositioned = true;
                self.move_to(second.max(1) - 1, count - 1);
            }
            b'd' => {
                let column = self.x;
                self.move_to(column, count - 1);
            }
            b'I' => self.tab_forward(count),
            b'Z' => self.tab_backward(count),
            b'J' => {
                self.repositioned = true;
                self.erase_display(first);
            }
            b'K' => self.erase_line(first),
            b'X' => {
                self.moved();
                let (x, y) = (self.x, self.y);
                for column in x..(x + count).min(self.cols) {
                    self.grid[y * self.cols + column].zeroize();
                }
            }
            b'@' => self.insert_characters(count),
            b'P' => self.delete_characters(count),
            b'L' => self.shift_lines(count, true),
            b'M' => self.shift_lines(count, false),
            b'S' => {
                self.moved();
                self.scroll_up(count);
            }
            b'T' => {
                self.moved();
                self.scroll_down(count);
            }
            b'r' => self.set_region(first, second),
            b's' if body.is_empty() => self.save_cursor(),
            b'u' if body.is_empty() => self.restore_cursor(),
            b'm' => self.rendition.apply(&body),
            b'b' => return Step::Repeat(count.min(MAX_REPEAT)),
            _ => {}
        }
        Step::None
    }

    fn move_vertical(&mut self, delta: isize) {
        self.moved();
        let (low, high) = if self.y >= self.top && self.y <= self.bottom {
            (self.top, self.bottom)
        } else {
            (0, self.rows - 1)
        };
        let target = self.y as isize + delta;
        self.y = target.clamp(low as isize, high as isize) as usize;
    }

    fn move_to_column(&mut self, x: usize) {
        self.moved();
        self.x = x.min(self.cols - 1);
    }

    fn move_to(&mut self, x: usize, y: usize) {
        self.moved();
        self.x = x.min(self.cols - 1);
        self.y = if self.origin {
            (self.top + y).min(self.bottom)
        } else {
            y.min(self.rows - 1)
        };
    }

    fn erase_display(&mut self, mode: usize) {
        self.moved();
        let cursor = self.y * self.cols + self.x;
        let range = match mode {
            0 => cursor..self.grid.len(),
            1 => 0..cursor + 1,
            2 => 0..self.grid.len(),
            3 => {
                for row in self.history.iter_mut() {
                    row.zeroize();
                }
                self.history.clear();
                return;
            }
            _ => return,
        };
        self.grid[range].iter_mut().for_each(Zeroize::zeroize);
    }

    fn erase_line(&mut self, mode: usize) {
        self.moved();
        let start = self.y * self.cols;
        let range = match mode {
            0 => start + self.x..start + self.cols,
            1 => start..start + self.x + 1,
            2 => start..start + self.cols,
            _ => return,
        };
        self.grid[range].iter_mut().for_each(Zeroize::zeroize);
    }

    fn insert_characters(&mut self, count: usize) {
        self.moved();
        self.disrupted = true;
        let start = self.y * self.cols + self.x;
        let end = (self.y + 1) * self.cols;
        let count = count.min(end - start);
        self.grid[start..end].rotate_right(count);
        self.grid[start..start + count]
            .iter_mut()
            .for_each(Zeroize::zeroize);
    }

    fn delete_characters(&mut self, count: usize) {
        self.moved();
        self.disrupted = true;
        let start = self.y * self.cols + self.x;
        let end = (self.y + 1) * self.cols;
        let count = count.min(end - start);
        self.grid[start..end].rotate_left(count);
        self.grid[end - count..end]
            .iter_mut()
            .for_each(Zeroize::zeroize);
    }

    /// IL and DL: the rows from the cursor's to the bottom margin move down
    /// (`insert`) or up by `count`, and the rows that empty are blank.
    fn shift_lines(&mut self, count: usize, insert: bool) {
        if self.y < self.top || self.y > self.bottom {
            return;
        }
        self.moved();
        self.disrupted = true;
        let (y, bottom, cols) = (self.y, self.bottom, self.cols);
        let count = count.min(bottom - y + 1);
        let region = &mut self.grid[y * cols..(bottom + 1) * cols];
        let cleared = if insert {
            region.rotate_right(count * cols);
            y..y + count
        } else {
            region.rotate_left(count * cols);
            bottom + 1 - count..bottom + 1
        };
        for row in cleared {
            self.clear_row(row);
        }
        self.x = 0;
    }

    fn set_region(&mut self, top: usize, bottom: usize) {
        let top = top.max(1) - 1;
        let bottom = if bottom == 0 {
            self.rows
        } else {
            bottom.min(self.rows)
        } - 1;
        if top < bottom {
            self.top = top;
            self.bottom = bottom;
            self.move_to(0, 0);
        }
    }

    fn save_cursor(&mut self) {
        self.saved = SavedCursor {
            x: self.x,
            y: self.y,
            pending_wrap: self.pending_wrap,
            rendition: self.rendition,
            origin: self.origin,
        };
    }

    fn restore_cursor(&mut self) {
        let saved = self.saved;
        self.moved();
        self.x = saved.x.min(self.cols - 1);
        self.y = saved.y.min(self.rows - 1);
        self.pending_wrap = saved.pending_wrap;
        self.rendition = saved.rendition;
        self.origin = saved.origin;
    }

    fn soft_reset(&mut self) {
        self.origin = false;
        self.autowrap = true;
        self.top = 0;
        self.bottom = self.rows - 1;
        self.rendition = Rendition::default();
        self.saved = SavedCursor::default();
        self.modes.cursor_hidden = false;
    }

    fn full_reset(&mut self) {
        self.moved();
        self.disrupted = true;
        if let Some(mut main) = self.main.take() {
            main.zeroize();
        }
        self.main_cursor = None;
        self.grid.iter_mut().for_each(Zeroize::zeroize);
        for row in self.history.iter_mut() {
            row.zeroize();
        }
        self.history.clear();
        self.x = 0;
        self.y = 0;
        self.soft_reset();
        self.modes = Modes::default();
    }

    fn private_mode(&mut self, mode: usize, set: bool) {
        match mode {
            6 => {
                self.origin = set;
                self.move_to(0, 0);
            }
            7 => {
                self.autowrap = set;
                if !set {
                    self.pending_wrap = false;
                }
            }
            25 => self.modes.cursor_hidden = !set,
            47 | 1047 | 1049 => self.alternate_screen(mode, set),
            1048 => {
                if set {
                    self.save_cursor();
                } else {
                    self.restore_cursor();
                }
            }
            1000 | 1002 | 1003 | 1005 | 1006 | 1015 | 1016 => self.modes.mouse = set,
            1004 => self.modes.focus_events = set,
            2004 => self.modes.bracketed_paste = set,
            2026 => self.modes.synchronized = set,
            _ => {}
        }
    }

    fn alternate_screen(&mut self, mode: usize, set: bool) {
        if set == self.main.is_some() {
            return;
        }
        self.disrupted = true;
        self.moved();
        if set {
            if mode == 1049 {
                self.save_cursor();
            }
            let blank = vec![Cell::default(); self.cols * self.rows];
            self.main = Some(std::mem::replace(&mut self.grid, blank));
            self.main_cursor = Some(self.saved);
        } else {
            if let Some(main) = self.main.take() {
                let mut alternate = std::mem::replace(&mut self.grid, main);
                alternate.zeroize();
            }
            if mode == 1049 {
                if let Some(saved) = self.main_cursor.take() {
                    self.saved = saved;
                }
                self.restore_cursor();
            }
        }
        self.top = 0;
        self.bottom = self.rows - 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed(screen: &mut Screen, bytes: &[u8]) -> Vec<Print> {
        let mut prints = Vec::new();
        for &byte in bytes {
            if let Step::Print(print) = screen.feed(byte).step {
                prints.push(print);
            }
        }
        prints
    }

    fn row_text(screen: &Screen, row: u64) -> String {
        (0..screen.cols())
            .map(|x| screen.cell(x, row).map(|cell| cell.ch).unwrap_or('?'))
            .map(|ch| if ch == '\0' { '.' } else { ch })
            .collect()
    }

    #[test]
    fn printing_wraps_at_the_last_column_only_when_the_next_character_comes() {
        let mut screen = Screen::new(5, 3);
        let prints = feed(&mut screen, b"abcde");
        assert_eq!(prints.last().unwrap().x, 4);
        assert_eq!(row_text(&screen, 0), "abcde");
        let prints = feed(&mut screen, b"f");
        assert_eq!(
            prints[0],
            Print {
                ch: 'f',
                x: 0,
                row: 1,
                wrapped_from: Some((4, 0))
            }
        );
        // A carriage return at the pending wrap stays on the row.
        let mut screen = Screen::new(5, 3);
        let prints = feed(&mut screen, b"abcde\r\nf");
        assert_eq!(prints.last().unwrap().wrapped_from, None);
        assert_eq!(prints.last().unwrap().row, 1);
    }

    #[test]
    fn cursor_movement_and_erasure_follow_the_terminal() {
        let mut screen = Screen::new(10, 4);
        feed(&mut screen, b"0123456789\r\x1b[1C\x1b[2Bxy\x1b[1;5Hz");
        assert_eq!(row_text(&screen, 0), "0123z56789");
        feed(&mut screen, b"\x1b[3G\x1b[K");
        assert_eq!(row_text(&screen, 0), "01........");
        assert_eq!(row_text(&screen, 2), ".xy.......");
        let mut screen = Screen::new(10, 4);
        feed(&mut screen, b"abc\x1b[2D\x1b[1@Z\x1b[1P");
        assert_eq!(row_text(&screen, 0), "aZc.......");
    }

    #[test]
    fn rows_keep_their_numbers_when_the_screen_scrolls() {
        let mut screen = Screen::new(4, 3);
        let prints = feed(&mut screen, b"a\r\nb\r\nc\r\nd\r\ne");
        let rows: Vec<u64> = prints.iter().map(|print| print.row).collect();
        assert_eq!(rows, vec![0, 1, 2, 3, 4]);
        assert_eq!(screen.cell(0, 1).unwrap().ch, 'b');
        assert_eq!(screen.cell(0, 4).unwrap().ch, 'e');
        assert!(!screen.take_disrupted());
        // A scroll region moves rows their numbers do not follow.
        feed(&mut screen, b"\x1b[1;2r\x1b[2;1H\n");
        assert!(screen.take_disrupted());
    }

    #[test]
    fn renditions_compare_by_what_they_render() {
        let mut a = Rendition::default();
        a.apply(b"38;2;255;193;7");
        let mut b = Rendition::default();
        b.apply(b"1");
        b.apply(b"22;38:2::255:193:7");
        assert_eq!(a, b);
        let mut c = Rendition::default();
        c.apply(b"38;5;214");
        assert_ne!(a, c);
        c.apply(b"39");
        assert_eq!(c, Rendition::default());
        let mut d = Rendition::default();
        d.apply(b"2");
        d.apply(b"");
        assert_eq!(d, Rendition::default());
    }

    #[test]
    fn strings_are_reported_and_a_broken_terminator_starts_a_new_sequence() {
        let mut screen = Screen::new(10, 2);
        assert_eq!(screen.feed(0x1b).step, Step::None);
        assert_eq!(screen.feed(b']').step, Step::StringStart(StringKind::Osc));
        assert_eq!(screen.feed(b'8').step, Step::StringByte);
        assert_eq!(screen.feed(0x07).step, Step::StringEnd);
        assert!(!screen.in_sequence());
        for &byte in b"\x1b]52;c;eA==" {
            screen.feed(byte);
        }
        assert_eq!(screen.feed(0x1b).step, Step::StringByte);
        let fed = screen.feed(b'[');
        assert!(fed.string_broken);
        assert!(screen.in_sequence());
        assert_eq!(screen.feed(b'm').step, Step::None);
        assert!(!screen.in_sequence());
    }

    #[test]
    fn wide_characters_take_two_cells_and_combining_marks_none() {
        let mut screen = Screen::new(6, 2);
        let prints = feed(&mut screen, "a\u{4e2d}b\u{301}c".as_bytes());
        let columns: Vec<usize> = prints.iter().map(|print| print.x).collect();
        assert_eq!(columns, vec![0, 1, 3, 4]);
    }

    #[test]
    fn modes_left_set_are_reset() {
        let mut screen = Screen::new(10, 2);
        feed(&mut screen, b"\x1b[?25l\x1b[?2004h\x1b[>1u\x1b[1m");
        let reset = String::from_utf8(screen.reset_sequence()).unwrap();
        assert_eq!(reset, "\x1b[?2004l\x1b[<1u\x1b[0m\x1b[?25h");
        feed(&mut screen, b"\x1b[?25h\x1b[?2004l\x1b[<u\x1b[0m");
        assert!(screen.reset_sequence().is_empty());
    }
}
