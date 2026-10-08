//! Tests only: how Claude Code 2.1.271 draws `claude setup-token`'s screen,
//! read from the JavaScript bundled in its binary, so the tests can produce
//! the bytes the real CLI writes at any terminal size.
//!
//! - **Layout** (`setupTokenHandler` and `ConsoleOAuthFlow`): a column with
//!   gap 1 holding the welcome line (the 17-row banner when the terminal has
//!   at least 30 rows), then a box with `paddingLeft: 1` and gap 1 holding the
//!   bold guide text and the sign-in flow. While it waits for the sign-in,
//!   the flow shows the sign-in URL outdented by one column (dim, an OSC 8
//!   hyperlink) and the paste prompt; once it has the token, a box with
//!   `paddingTop: 1` and gap 1 holding the success line, "Your OAuth token
//!   (valid for 1 year):", the token in the `warning` color, and two dim
//!   lines. Every `Text` wraps with wrap-ansi's hard wrap (`hard: true,
//!   trim: false`), so the token is broken into rows as wide as the box
//!   (the terminal's width minus 1), each starting at column 1.
//! - **Output** (`LogUpdate.render`, `zb`, `Gs`, `Ub`, `dbn`): frames are
//!   cell grids. The first frame is written row by row, `CR LF` between
//!   rows and `CSI n G` over empty cells. Each later frame writes only the
//!   cells that changed, moving with `CR` + `CSI n C` + `CSI n B`/`CSI n A`
//!   between rows and `CSI n G` within a row; a cell that empties is written
//!   as a space, or `CSI K` past a row's last content; rows that grow are
//!   written as in the first frame; a frame that shrinks clears the rows it
//!   leaves (`CSI 2K`, `CSI 1A`); a change above the viewport while
//!   shrinking, or a new width, clears the viewport (`CSI H`, then `CSI 2K
//!   CSI 1B` per row, `CSI H`) and redraws from the first row that fits.
//!   Every frame is wrapped in synchronized-update marks (`CSI ? 2026 h/l`);
//!   styles are SGR open and close codes, hyperlinks OSC 8 ended by `BEL`.

#![allow(dead_code)]

/// SGR open and close codes of each style the screens use.
const STYLES: [(&str, &str); 8] = [
    ("", ""),
    ("\x1b[38;2;215;119;87m", "\x1b[39m"),
    ("\x1b[2m", "\x1b[22m"),
    ("\x1b[1m", "\x1b[22m"),
    ("\x1b[38;2;255;193;7m", "\x1b[39m"),
    ("\x1b[38;2;78;186;101m", "\x1b[39m"),
    ("\x1b[38;2;136;136;136m", "\x1b[39m"),
    ("\x1b[38;5;214m", "\x1b[39m"),
];
pub const PLAIN: u8 = 0;
pub const CLAUDE: u8 = 1;
pub const DIM: u8 = 2;
pub const BOLD: u8 = 3;
pub const WARNING: u8 = 4;
pub const SUCCESS: u8 = 5;
pub const BORDER: u8 = 6;
pub const ORANGE: u8 = 7;

/// The sign-in URL the waiting screen shows (link 1).
pub const URL: &str = "https://claude.ai/oauth/authorize?code=true&client_id=9d1c250a-e61b-44d9-88ed-5944d1962f5e&response_type=code&redirect_uri=https%3A%2F%2Fplatform.claude.com%2Foauth%2Fcode%2Fcallback&scope=user%3Ainference&code_challenge=E9MelhoaQ2DZ8YUTxlQRmeaKwvqGkq0bPcF3NnZsA4w&code_challenge_method=S256&state=uyV0cBRkHnzT5WqL1tXp8JdgM2aOeIfC7hrN6vYs3Kj";
pub const GUIDE: &str = "This will guide you through long-lived (1-year) auth token setup for your Claude account. Claude subscription required.";
/// What the waiting screen ends with (the test types Enter when it shows).
pub const PROMPT: &str = "Paste code here if prompted > ";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cell {
    pub ch: char,
    pub style: u8,
    pub link: u8,
}

/// One frame: rows of cells, `None` where nothing is drawn.
#[derive(Clone, Debug, Default)]
pub struct Frame {
    pub width: usize,
    pub rows: Vec<Vec<Option<Cell>>>,
}

impl Frame {
    pub fn new(width: usize) -> Self {
        Self {
            width,
            rows: Vec::new(),
        }
    }

    pub fn height(&self) -> usize {
        self.rows.len()
    }

    fn cell(&self, x: usize, y: usize) -> Option<Cell> {
        self.rows
            .get(y)
            .and_then(|row| row.get(x).copied().flatten())
    }

    fn ensure(&mut self, height: usize) {
        while self.rows.len() < height {
            self.rows.push(vec![None; self.width]);
        }
    }

    fn put(&mut self, x: usize, y: usize, ch: char, style: u8, link: u8) {
        self.ensure(y + 1);
        if x < self.width {
            self.rows[y][x] = Some(Cell { ch, style, link });
        }
    }

    /// The text of row `y`, `.` for empty cells.
    pub fn row_text(&self, y: usize) -> String {
        (0..self.width)
            .map(|x| self.cell(x, y).map(|cell| cell.ch).unwrap_or('.'))
            .collect()
    }
}

/// wrap-ansi with `hard: true, trim: false`, then Ink's removal of the
/// space a wrapped row starts with: the rows of `spans` in `width` columns.
pub fn wrap(spans: &[(&str, u8)], width: usize) -> Vec<Vec<(char, u8)>> {
    let width = width.max(1);
    let mut styled: Vec<(char, u8)> = Vec::new();
    for (text, style) in spans {
        styled.extend(text.chars().map(|ch| (ch, *style)));
    }
    let words: Vec<&[(char, u8)]> = styled.split(|(ch, _)| *ch == ' ').collect();
    let mut rows: Vec<Vec<(char, u8)>> = vec![Vec::new()];
    for (index, word) in words.iter().enumerate() {
        let mut length = rows.last().unwrap().len();
        if index != 0 {
            if length >= width {
                rows.push(Vec::new());
                length = 0;
            }
            let style = word.first().map(|(_, style)| *style).unwrap_or(PLAIN);
            rows.last_mut().unwrap().push((' ', style));
            length += 1;
        }
        if word.len() > width {
            let remaining = width.saturating_sub(length);
            let this_line =
                1 + (word.len() as isize - remaining as isize - 1).div_euclid(width as isize);
            let next_line = ((word.len() - 1) / width) as isize;
            if next_line < this_line {
                rows.push(Vec::new());
            }
            let mut visible = rows.last().unwrap().len();
            for (position, character) in word.iter().enumerate() {
                if visible < width {
                    rows.last_mut().unwrap().push(*character);
                } else {
                    rows.push(vec![*character]);
                    visible = 0;
                }
                visible += 1;
                if visible == width && position + 1 < word.len() {
                    rows.push(Vec::new());
                    visible = 0;
                }
            }
            continue;
        }
        if length + word.len() > width && length > 0 && !word.is_empty() {
            rows.push(Vec::new());
        }
        rows.last_mut().unwrap().extend_from_slice(word);
    }
    for row in rows.iter_mut().skip(1) {
        if row.first().is_some_and(|(ch, _)| *ch == ' ') {
            row.remove(0);
        }
    }
    rows
}

/// Places blocks of text from the top down.
pub struct Layout {
    pub frame: Frame,
    pub y: usize,
}

impl Layout {
    pub fn new(width: usize) -> Self {
        Self {
            frame: Frame::new(width),
            y: 0,
        }
    }

    /// A `Text` at column `x`, `width` wide; its rows advance `y`.
    pub fn text(&mut self, x: usize, width: usize, spans: &[(&str, u8)], link: u8) {
        for row in wrap(spans, width) {
            self.frame.ensure(self.y + 1);
            for (offset, (ch, style)) in row.into_iter().enumerate() {
                self.frame.put(x + offset, self.y, ch, style, link);
            }
            self.y += 1;
        }
    }

    pub fn gap(&mut self, rows: usize) {
        self.y += rows;
        self.frame.ensure(self.y);
    }

    /// A `Text` in a rounded border box with one column of padding on each
    /// side, as wide as the frame.
    pub fn bordered(&mut self, spans: &[(&str, u8)]) {
        let width = self.frame.width;
        let inner = width.saturating_sub(4).max(1);
        let horizontal = |left: char, right: char, frame: &mut Frame, y: usize| {
            frame.put(0, y, left, BORDER, 0);
            for x in 1..width - 1 {
                frame.put(x, y, '─', BORDER, 0);
            }
            frame.put(width - 1, y, right, BORDER, 0);
        };
        horizontal('╭', '╮', &mut self.frame, self.y);
        self.y += 1;
        for row in wrap(spans, inner) {
            self.frame.put(0, self.y, '│', BORDER, 0);
            for (offset, (ch, style)) in row.into_iter().enumerate() {
                self.frame.put(2 + offset, self.y, ch, style, 0);
            }
            self.frame.put(width - 1, self.y, '│', BORDER, 0);
            self.y += 1;
        }
        horizontal('╰', '╯', &mut self.frame, self.y);
        self.y += 1;
    }

    pub fn finish(mut self) -> Frame {
        self.frame.ensure(self.y);
        self.frame.rows.truncate(self.y);
        self.frame
    }
}

/// The welcome: one line, or the 58-column banner from 30 rows up.
fn welcome(layout: &mut Layout, rows: usize) {
    let width = layout.frame.width;
    if rows < 30 {
        layout.text(
            0,
            width,
            &[("Welcome to Claude Code ", CLAUDE), ("v2.1.271", DIM)],
            0,
        );
        return;
    }
    let banner = [
        "Welcome to Claude Code v2.1.271 ",
        "..........................................................",
        "                                                          ",
        "     *                                       █████▓▓░     ",
        "                                 *         ███▓░     ░░   ",
        "            ░░░░░░                        ███▓░           ",
        "    ░░░   ░░░░░░░░░░                      ███▓░           ",
        "   ░░░░░░░░░░░░░░░░░░░    *                ██▓░░      ▓   ",
        "                                             ░▓▓███▓▓░    ",
        " *                                 ░░░░                   ",
        "                                 ░░░░░░░░                 ",
        "                               ░░░░░░░░░░░░░░░░           ",
        "       █████████                                       *  ",
        "      ██▄█████▄██                        *                ",
        "       █████████      *                                   ",
        ".......█ █   █ █..........................................",
        "",
    ];
    for line in banner.iter().take(16) {
        layout.frame.ensure(layout.y + 1);
        for (x, ch) in line.chars().enumerate().take(58.min(width)) {
            let style = if ch == '█' { ORANGE } else { PLAIN };
            if ch != ' ' || x < line.trim_end().chars().count() {
                layout.frame.put(x, layout.y, ch, style, 0);
            }
        }
        layout.y += 1;
    }
}

/// The screen before the sign-in finishes.
pub fn waiting(cols: usize, rows: usize) -> Frame {
    let mut layout = Layout::new(cols);
    welcome(&mut layout, rows);
    layout.gap(1);
    layout.text(1, cols - 1, &[(GUIDE, BOLD)], 0);
    layout.gap(1);
    // The URL box is outdented to column 0.
    layout.text(
        1,
        cols - 1,
        &[("Browser didn't open? Use the url below to sign in ", DIM)],
        0,
    );
    layout.gap(1);
    layout.text(0, cols, &[(URL, DIM)], 1);
    layout.gap(1);
    layout.gap(1);
    layout.text(1, cols - 1, &[(PROMPT, PLAIN)], 0);
    layout.finish()
}

/// What the success screen shows the token in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Shape {
    /// As Claude Code 2.1.271 does.
    Claude,
    /// In a rounded border box with padding.
    Bordered,
    /// After a label in the same `Text`.
    Labeled,
    /// In a box 12 columns narrower than the terminal, without a border.
    Narrow,
}

/// The screen once the token was made; `guide` replaces the guide text
/// (a shorter one moves the token up a row).
pub fn success(token: &str, cols: usize, rows: usize, shape: Shape, guide: &str) -> Frame {
    let mut layout = Layout::new(cols);
    welcome(&mut layout, rows);
    layout.gap(1);
    layout.text(1, cols - 1, &[(guide, BOLD)], 0);
    layout.gap(1);
    // The token box: paddingTop 1, gap 1.
    layout.gap(1);
    layout.text(
        1,
        cols - 1,
        &[(
            "\u{2713} Long-lived authentication token created successfully!",
            SUCCESS,
        )],
        0,
    );
    layout.gap(1);
    layout.text(
        1,
        cols - 1,
        &[("Your OAuth token (valid for 1 year):", PLAIN)],
        0,
    );
    layout.gap(1);
    match shape {
        Shape::Claude => layout.text(1, cols - 1, &[(token, WARNING)], 0),
        Shape::Bordered => layout.bordered(&[(token, WARNING)]),
        Shape::Labeled => layout.text(1, cols - 1, &[("Token: ", PLAIN), (token, WARNING)], 0),
        Shape::Narrow => layout.text(1, cols.saturating_sub(13).max(8), &[(token, WARNING)], 0),
    }
    layout.gap(1);
    layout.text(
        1,
        cols - 1,
        &[(
            "Store this token securely. You won't be able to see it again.",
            DIM,
        )],
        0,
    );
    layout.gap(1);
    layout.text(
        1,
        cols - 1,
        &[(
            "Use this token by setting: export CLAUDE_CODE_OAUTH_TOKEN=<token>",
            DIM,
        )],
        0,
    );
    // The flow's (empty) status box after one more gap.
    layout.gap(1);
    layout.finish()
}

/// Claude Code's main-screen renderer: the bytes for each frame.
pub struct Renderer {
    cols: usize,
    viewport: usize,
    previous: Frame,
    /// The cursor, in frame coordinates (`x == cols` after the last column).
    x: usize,
    y: usize,
    style: u8,
    link: u8,
    out: Vec<u8>,
}

fn hyperlink(link: u8) -> String {
    if link == 0 {
        "\x1b]8;;\x07".to_string()
    } else {
        format!("\x1b]8;id=1f2e3d;{URL}\x07")
    }
}

impl Renderer {
    pub fn new(cols: usize, viewport: usize) -> Self {
        Self {
            cols,
            viewport,
            previous: Frame::new(cols),
            x: 0,
            y: 0,
            style: PLAIN,
            link: 0,
            out: Vec::new(),
        }
    }

    /// What the renderer writes when it starts.
    pub fn start(&mut self) -> Vec<u8> {
        b"\x1b7\x1b[r\x1b8\x1b[?25h\x1b[?2004h\x1b[?1004h".to_vec()
    }

    /// What it writes when Claude Code exits.
    pub fn exit(&mut self) -> Vec<u8> {
        b"\x1b[?25h\x1b[?1004l\x1b[?2004l".to_vec()
    }

    fn push(&mut self, text: &str) {
        self.out.extend_from_slice(text.as_bytes());
    }

    fn set_style(&mut self, style: u8) {
        if style == self.style {
            return;
        }
        let close = STYLES[usize::from(self.style)].1;
        let open = STYLES[usize::from(style)].0;
        self.push(close);
        self.push(open);
        self.style = style;
    }

    fn set_link(&mut self, link: u8) {
        if link != self.link {
            let sequence = hyperlink(link);
            self.push(&sequence);
            self.link = link;
        }
    }

    /// `Gs`: move to `(x, y)`.
    fn go(&mut self, x: usize, y: usize) {
        let dy = y as isize - self.y as isize;
        if self.x >= self.cols || dy != 0 {
            self.push("\r");
            if x > 0 {
                self.push(&format!("\x1b[{x}C"));
            }
            if dy < 0 {
                self.push(&format!("\x1b[{}A", -dy));
            } else if dy > 0 {
                self.push(&format!("\x1b[{dy}B"));
            }
        } else if x != self.x {
            self.push(&format!("\x1b[{}G", x + 1));
        }
        self.x = x;
        self.y = y;
    }

    /// `Ub`: write one cell's character.
    fn write(&mut self, ch: char) {
        let mut encoded = [0u8; 4];
        self.out
            .extend_from_slice(ch.encode_utf8(&mut encoded).as_bytes());
        if self.x >= self.cols {
            self.x = 1;
            self.y += 1;
        } else {
            self.x += 1;
        }
    }

    /// `zb`: write rows `from..to` of `frame` whole.
    fn rows(&mut self, frame: &Frame, from: usize, to: usize) {
        for y in from..to {
            if self.y < y {
                self.push("\r");
                for _ in 0..y - self.y {
                    self.push("\n");
                }
                self.x = 0;
                self.y = y;
            }
            for x in 0..self.cols {
                if let Some(cell) = frame.cell(x, y) {
                    self.go(x, y);
                    self.set_link(cell.link);
                    self.set_style(cell.style);
                    self.write(cell.ch);
                }
            }
            self.set_style(PLAIN);
            self.set_link(0);
            self.push("\r\n");
            self.x = 0;
            self.y = y + 1;
        }
        self.set_style(PLAIN);
        self.set_link(0);
    }

    /// `Wr`: clear the viewport and redraw from the first row that fits.
    fn full_reset(&mut self, frame: &Frame, offscreen: usize) {
        let first = offscreen.min((frame.height() + 1).saturating_sub(self.viewport));
        self.push("\x1b[H");
        for _ in 0..self.viewport {
            self.push("\x1b[2K\x1b[1B");
        }
        self.push("\x1b[H");
        self.x = 0;
        self.y = first;
        self.rows(frame, first, frame.height());
    }

    /// The bytes that turn the previous frame into `next`.
    pub fn render(&mut self, next: &Frame) -> Vec<u8> {
        self.out.clear();
        self.push("\x1b[?2026h");
        let previous = std::mem::replace(&mut self.previous, Frame::new(self.cols));
        if previous.width != 0 && previous.width != next.width {
            // A new width: the whole frame again.
            self.cols = next.width;
            let offscreen = previous.height().saturating_sub(self.viewport)
                + usize::from(previous.height() >= self.viewport);
            self.full_reset(next, offscreen);
        } else {
            self.diff(&previous, next);
        }
        self.push("\x1b[?2026l");
        self.previous = next.clone();
        std::mem::take(&mut self.out)
    }

    /// Change the viewport (a resize); the next render redraws.
    pub fn resize(&mut self, cols: usize, viewport: usize) {
        self.previous.width = self.previous.width.max(1);
        if cols == self.cols {
            self.previous.width = 0;
        }
        self.viewport = viewport;
        let _ = cols;
    }

    fn diff(&mut self, previous: &Frame, next: &Frame) {
        let (old_height, new_height) = (previous.height(), next.height());
        let fills = old_height >= self.viewport;
        let below = self.y >= old_height && fills;
        let offscreen = old_height.saturating_sub(self.viewport) + usize::from(fills);
        let growing = new_height > old_height;
        let shrinking = new_height < old_height;
        if below && shrinking && new_height <= self.viewport {
            return self.full_reset(next, offscreen);
        }
        if shrinking {
            let gone = old_height - new_height;
            if gone > self.viewport {
                return self.full_reset(next, offscreen);
            }
            for row in 0..gone {
                self.push("\x1b[2K");
                if row + 1 < gone {
                    self.push("\x1b[1A");
                }
            }
            self.push("\x1b[G\x1b[1A");
            self.x = 0;
            self.y -= gone;
        }
        let extra = usize::from(below);
        let hidden = if growing {
            (old_height + extra).saturating_sub(self.viewport)
        } else {
            (old_height.max(new_height) + extra).saturating_sub(self.viewport)
        };
        for y in 0..old_height.max(new_height) {
            if growing && y >= old_height {
                break;
            }
            let last_content = (0..self.cols).rev().find(|x| next.cell(*x, y).is_some());
            let mut erased = false;
            for x in 0..self.cols {
                let (old, new) = (previous.cell(x, y), next.cell(x, y));
                if old == new {
                    continue;
                }
                if y < hidden {
                    if shrinking {
                        self.out.clear();
                        self.push("\x1b[?2026h");
                        return self.full_reset(next, offscreen);
                    }
                    continue;
                }
                match new {
                    Some(cell) => {
                        self.go(x, y);
                        self.set_link(cell.link);
                        self.set_style(cell.style);
                        self.write(cell.ch);
                    }
                    None => {
                        if last_content.is_none_or(|last| x > last) {
                            if !erased {
                                erased = true;
                                self.go(x, y);
                                self.set_style(PLAIN);
                                self.set_link(0);
                                self.push("\x1b[K");
                            }
                        } else {
                            self.go(x, y);
                            self.set_style(PLAIN);
                            self.set_link(0);
                            self.write(' ');
                        }
                    }
                }
            }
            self.set_link(0);
        }
        self.set_style(PLAIN);
        self.set_link(0);
        if growing {
            self.rows(next, old_height, new_height);
        }
        if self.y < new_height {
            self.push("\r");
            for _ in 0..new_height - self.y {
                self.push("\n");
            }
        } else if self.y != new_height || self.x != 0 {
            self.push("\r");
            let up = self.y - new_height;
            if up > 0 {
                self.push(&format!("\x1b[{up}A"));
            }
        }
        self.x = 0;
        self.y = new_height;
    }
}

/// What `claude setup-token` writes: up to the paste prompt, and after the
/// sign-in (the token screen and the exit).
pub fn session(token: &str, cols: usize, rows: usize, shape: Shape) -> (Vec<u8>, Vec<u8>) {
    let mut renderer = Renderer::new(cols, rows);
    let mut before = renderer.start();
    before.extend(renderer.render(&waiting(cols, rows)));
    let mut after = renderer.render(&success(token, cols, rows, shape, GUIDE));
    after.extend(renderer.exit());
    (before, after)
}

/// What it writes after the sign-in when the terminal was resized to
/// `cols` by `rows` while it waited: the waiting screen redrawn for the new
/// size, then the token screen.
pub fn resized_session(token: &str, old: (usize, usize), new: (usize, usize)) -> Vec<u8> {
    let mut renderer = Renderer::new(old.0, old.1);
    renderer.render(&waiting(old.0, old.1));
    renderer.viewport = new.1;
    let mut after = renderer.render(&waiting(new.0, new.1));
    after.extend(renderer.render(&success(token, new.0, new.1, Shape::Claude, GUIDE)));
    after.extend(renderer.exit());
    after
}
