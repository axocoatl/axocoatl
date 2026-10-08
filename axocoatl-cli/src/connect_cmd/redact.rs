//! The streaming filter between `claude setup-token` and the user's
//! terminal: every `sk-ant-…` token in the program's output is replaced by
//! [`MASK`] before it is shown, and the tokens it replaced are kept, in
//! memory only, for [`select_token`].
//!
//! The filter works on raw bytes as they arrive, in chunks of any size:
//!
//! - A run of bytes that could still become the start of a token
//!   (`s`, `sk`, … `sk-ant`) is held back until the next byte decides it.
//!   [`TokenFilter::flush_idle`] may show a held run when the program goes
//!   quiet, but keeps matching, so at most `sk-ant` (public, and shorter than
//!   any secret part) is ever shown before the mask.
//! - CSI escape sequences (`ESC [ … final`, colors) and character-set
//!   designations (`ESC ( B`) are transparent: one inside a token or its
//!   prefix neither ends it nor becomes part of it. Any other escape (an OSC
//!   introducer, the string terminator `ESC \`) ends a token; the text of
//!   an OSC string, such as a hyperlink's target, is filtered like output. Escapes inside a token are still shown after the
//!   mask, so colors stay balanced; escapes inside the held prefix are
//!   dropped.
//! - A token that reaches the last column of the program's terminal and
//!   continues after a line break (how a narrow terminal wraps it) is one
//!   token: the continuation is hidden too, and joined.
//!
//! Nothing here formats a token into a string, an error or a `Debug` value.

use zeroize::Zeroizing;

/// What the user sees in place of a token.
pub(crate) const MASK: &str = "[token hidden by axocoatl]";
/// Every Anthropic credential starts with this; all of them are hidden.
const PREFIX: &[u8] = b"sk-ant-";
/// The prefix of the long-lived OAuth token `claude setup-token` prints.
pub(crate) const OAUTH_PREFIX: &str = "sk-ant-oat01-";
/// The fewest characters after [`OAUTH_PREFIX`] a token is taken to have.
const MIN_SECRET_CHARS: usize = 32;
/// The longest token kept; a longer run is still hidden, but not captured.
const MAX_TOKEN_CHARS: usize = 512;
/// At most this many spaces of indentation before a wrapped continuation.
const MAX_WRAP_INDENT: usize = 8;

fn is_token_char(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_'
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Escape {
    None,
    /// Right after `ESC`.
    Start,
    /// After `ESC` and intermediate bytes (`ESC (`).
    Intermediate,
    /// Inside `ESC [`.
    Csi,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Mode {
    /// Plain output; `matched` bytes of [`PREFIX`] seen so far.
    Normal,
    /// Inside a token: nothing visible is shown.
    Token,
    /// A token ended at the last column with a line break: a token character
    /// next continues it.
    WrapGap,
}

/// One `sk-ant-` token the filter hid.
pub(crate) struct Found {
    value: Zeroizing<String>,
    overlong: bool,
}

/// The streaming redaction filter. See the module documentation.
pub(crate) struct TokenFilter {
    cols: usize,
    col: usize,
    escape: Escape,
    mode: Mode,
    /// Bytes held back: a possible prefix (Normal) or a line break and
    /// indentation (WrapGap). Never a token character after the prefix.
    pending: Zeroizing<Vec<u8>>,
    matched: usize,
    /// The held prefix was already shown by [`Self::flush_idle`].
    prefix_shown: bool,
    token: Zeroizing<String>,
    overlong: bool,
    token_end_col: usize,
    gap_breaks: usize,
    gap_indent: usize,
    found: Vec<Found>,
}

impl TokenFilter {
    /// A filter for a program whose terminal is `cols` columns wide.
    pub(crate) fn new(cols: u16) -> Self {
        Self {
            cols: usize::from(cols.max(1)),
            col: 0,
            escape: Escape::None,
            mode: Mode::Normal,
            pending: Zeroizing::new(Vec::with_capacity(64)),
            matched: 0,
            prefix_shown: false,
            token: Zeroizing::new(String::with_capacity(MAX_TOKEN_CHARS)),
            overlong: false,
            token_end_col: 0,
            gap_breaks: 0,
            gap_indent: 0,
            found: Vec::new(),
        }
    }

    /// Filter `input`, appending what may be shown to `shown`.
    pub(crate) fn push(&mut self, input: &[u8], shown: &mut Vec<u8>) {
        for &byte in input {
            self.byte(byte, shown);
        }
    }

    /// The program has been quiet: show what is held back without giving up
    /// a match in progress (a held prefix is public text).
    pub(crate) fn flush_idle(&mut self, shown: &mut Vec<u8>) {
        match self.mode {
            Mode::Normal if self.matched > 0 && !self.prefix_shown => {
                shown.extend_from_slice(&self.pending);
                self.pending.clear();
                self.prefix_shown = true;
            }
            Mode::WrapGap => {
                shown.extend_from_slice(&self.pending);
                self.pending.clear();
            }
            _ => {}
        }
    }

    /// The output ended: show what is held back and close a token.
    pub(crate) fn finish(&mut self, shown: &mut Vec<u8>) {
        match self.mode {
            Mode::Normal => {
                if self.matched > 0 && !self.prefix_shown {
                    shown.extend_from_slice(&self.pending);
                }
            }
            Mode::Token => self.end_token(),
            Mode::WrapGap => {
                self.end_token();
                shown.extend_from_slice(&self.pending);
            }
        }
        self.pending.clear();
        self.matched = 0;
        self.prefix_shown = false;
        self.mode = Mode::Normal;
    }

    /// The tokens hidden, after [`Self::finish`].
    pub(crate) fn into_found(self) -> Vec<Found> {
        self.found
    }

    /// The tokens hidden so far (complete ones only; call
    /// [`Self::finish`] first for all of them).
    #[cfg(test)]
    pub(crate) fn found(&self) -> &[Found] {
        &self.found
    }

    /// Where an escape byte goes: held with a held prefix or line break,
    /// otherwise shown.
    fn escape_sink<'a>(&'a mut self, shown: &'a mut Vec<u8>) -> &'a mut Vec<u8> {
        let held = match self.mode {
            Mode::Normal => self.matched > 0 && !self.prefix_shown,
            Mode::WrapGap => true,
            Mode::Token => false,
        };
        if held {
            &mut self.pending
        } else {
            shown
        }
    }

    fn advance_column(&mut self, byte: u8) {
        match byte {
            b'\r' | b'\n' => self.col = 0,
            0x08 => self.col = self.col.saturating_sub(1),
            b'\t' => self.col = ((self.col / 8 + 1) * 8).min(self.cols),
            0x00..=0x1f | 0x7f => {}
            // UTF-8 continuation bytes do not move the cursor.
            0x80..=0xbf => {}
            _ => {
                if self.col >= self.cols {
                    self.col = 0;
                }
                self.col += 1;
            }
        }
    }

    fn byte(&mut self, byte: u8, shown: &mut Vec<u8>) {
        match self.escape {
            Escape::Start | Escape::Intermediate => {
                let introduced = self.escape == Escape::Start;
                match byte {
                    b'[' if introduced => self.escape = Escape::Csi,
                    // Intermediate bytes (`ESC ( B`): the sequence goes on.
                    0x20..=0x2f => self.escape = Escape::Intermediate,
                    0x30..=0x7e => {
                        self.escape = Escape::None;
                        self.escape_sink(shown).push(byte);
                        // A character-set designation (`ESC ( B`) is
                        // transparent like CSI. Any other escape, such as
                        // the string terminator `ESC \` or an OSC
                        // introducer, ends a token or a prefix in progress;
                        // the text of an OSC string is filtered like any
                        // other output.
                        if introduced {
                            self.opaque_escape(shown);
                        }
                        return;
                    }
                    // Anything else aborts the sequence and is read afresh.
                    _ => {
                        self.escape = Escape::None;
                        return self.byte(byte, shown);
                    }
                }
                self.escape_sink(shown).push(byte);
                return;
            }
            Escape::Csi => {
                match byte {
                    0x20..=0x3f => {}
                    0x40..=0x7e => self.escape = Escape::None,
                    _ => {
                        self.escape = Escape::None;
                        return self.byte(byte, shown);
                    }
                }
                self.escape_sink(shown).push(byte);
                return;
            }
            Escape::None => {}
        }
        if byte == 0x1b {
            self.escape = Escape::Start;
            self.escape_sink(shown).push(byte);
            return;
        }
        match self.mode {
            Mode::Normal => self.normal(byte, shown),
            Mode::Token => self.in_token(byte, shown),
            Mode::WrapGap => self.wrap_gap(byte, shown),
        }
    }

    /// An escape that is not transparent ended: it closes a token, a held
    /// line break or a prefix in progress (the escape itself was already
    /// shown or held).
    fn opaque_escape(&mut self, shown: &mut Vec<u8>) {
        match self.mode {
            Mode::Token => self.end_token(),
            Mode::WrapGap => {
                self.end_token();
                shown.extend_from_slice(&self.pending);
            }
            Mode::Normal => {
                if self.matched > 0 && !self.prefix_shown {
                    shown.extend_from_slice(&self.pending);
                }
            }
        }
        self.pending.clear();
        self.matched = 0;
        self.prefix_shown = false;
        self.mode = Mode::Normal;
    }

    fn normal(&mut self, byte: u8, shown: &mut Vec<u8>) {
        if byte == PREFIX[self.matched] {
            self.advance_column(byte);
            self.matched += 1;
            if self.prefix_shown {
                // The prefix's last `-` is never shown: the mask follows.
                if self.matched < PREFIX.len() {
                    shown.push(byte);
                }
            } else {
                self.pending.push(byte);
            }
            if self.matched == PREFIX.len() {
                self.start_token(shown);
            }
            return;
        }
        if self.matched > 0 {
            // No proper suffix of a partial prefix starts the prefix again
            // (`s` occurs only first), so the held bytes are plain output and
            // this byte is read afresh.
            if !self.prefix_shown {
                shown.extend_from_slice(&self.pending);
            }
            self.pending.clear();
            self.matched = 0;
            self.prefix_shown = false;
            return self.normal(byte, shown);
        }
        self.advance_column(byte);
        shown.push(byte);
    }

    fn start_token(&mut self, shown: &mut Vec<u8>) {
        shown.extend_from_slice(MASK.as_bytes());
        // Escapes held inside the prefix are dropped with it.
        self.pending.clear();
        self.matched = 0;
        self.prefix_shown = false;
        self.token.clear();
        self.token.push_str("sk-ant-");
        self.overlong = false;
        self.token_end_col = self.col;
        self.mode = Mode::Token;
    }

    fn in_token(&mut self, byte: u8, shown: &mut Vec<u8>) {
        if is_token_char(byte) {
            self.advance_column(byte);
            self.token_end_col = self.col;
            if self.token.len() < MAX_TOKEN_CHARS {
                self.token.push(char::from(byte));
            } else {
                self.overlong = true;
            }
            return;
        }
        if matches!(byte, b'\r' | b'\n') && self.token_end_col >= self.cols {
            self.mode = Mode::WrapGap;
            self.gap_breaks = 1;
            self.gap_indent = 0;
            self.advance_column(byte);
            self.pending.push(byte);
            return;
        }
        self.end_token();
        self.mode = Mode::Normal;
        self.normal(byte, shown);
    }

    fn wrap_gap(&mut self, byte: u8, shown: &mut Vec<u8>) {
        let continues = match byte {
            b'\r' | b'\n' if self.gap_breaks < 2 && self.gap_indent == 0 => {
                self.gap_breaks += 1;
                None
            }
            b' ' if self.gap_indent < MAX_WRAP_INDENT => {
                self.gap_indent += 1;
                None
            }
            _ => Some(is_token_char(byte)),
        };
        match continues {
            None => {
                self.advance_column(byte);
                self.pending.push(byte);
            }
            Some(true) => {
                shown.extend_from_slice(&self.pending);
                self.pending.clear();
                self.mode = Mode::Token;
                self.in_token(byte, shown);
            }
            Some(false) => {
                self.end_token();
                shown.extend_from_slice(&self.pending);
                self.pending.clear();
                self.mode = Mode::Normal;
                self.normal(byte, shown);
            }
        }
    }

    fn end_token(&mut self) {
        let value = std::mem::replace(
            &mut self.token,
            Zeroizing::new(String::with_capacity(MAX_TOKEN_CHARS)),
        );
        self.found.push(Found {
            value,
            overlong: self.overlong,
        });
        self.overlong = false;
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

/// Whether `value` looks like a whole `claude setup-token` token: the OAuth
/// prefix, then at least [`MIN_SECRET_CHARS`] token characters.
fn plausible(found: &Found) -> bool {
    let value = found.value.as_str();
    !found.overlong
        && value.len() >= OAUTH_PREFIX.len() + MIN_SECRET_CHARS
        && value.len() <= MAX_TOKEN_CHARS
        && value.starts_with(OAUTH_PREFIX)
        && value.bytes().all(is_token_char)
}

/// The one token to store, from everything [`TokenFilter`] hid.
pub(crate) fn select_token(found: &[Found]) -> Selection {
    let mut distinct: Vec<&Found> = Vec::new();
    let mut other = 0;
    for candidate in found {
        if !plausible(candidate) {
            other += 1;
            continue;
        }
        if !distinct
            .iter()
            .any(|seen| seen.value.as_str() == candidate.value.as_str())
        {
            distinct.push(candidate);
        }
    }
    match distinct.as_slice() {
        [] => Selection::None { other },
        [one] => Selection::One(Zeroizing::new(one.value.as_str().to_owned())),
        several => Selection::Several(several.len()),
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A token shaped like the real one: the OAuth prefix and 95 more
    /// characters of the token alphabet.
    pub(crate) const TOKEN: &str = "sk-ant-oat01-Xq7_vR2mZ9kLp4Tn8Wc1Yb6Hs3Df0Gj5Ae-Ui2Ko7Nl4Mx9Pz1Qw8Er3Ty6Bv0Cs5Dh2Fg7Jk4La9Zx1Vn6Mb3Rt8AA";

    /// Every substring of `token` of at least 12 bytes that `shown`
    /// contains, as positions (never the text itself, so a failure does not
    /// print it).
    pub(crate) fn leaked_positions(shown: &[u8], token: &str) -> Vec<(usize, usize)> {
        let token = token.as_bytes();
        let mut leaks = Vec::new();
        for start in 0..token.len().saturating_sub(11) {
            let window = &token[start..start + 12];
            if shown.windows(12).any(|candidate| candidate == window) {
                leaks.push((start, start + 12));
            }
        }
        leaks
    }

    /// The plain sequence the fake `claude setup-token` prints, with the
    /// token colored and a color change inside it.
    fn stream(token: &str) -> Vec<u8> {
        let (head, tail) = token.split_at(40);
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"\x1b[2J\x1b[H Welcome to Claude Code\r\n");
        bytes.extend_from_slice(b"Browser didn't open? Use the url below to sign in.\r\n");
        bytes.extend_from_slice(b"Paste code here if prompted > \r\n\r\n");
        bytes.extend_from_slice(
            b"\xe2\x9c\x93 Long-lived authentication token created successfully!\r\n\r\n",
        );
        bytes.extend_from_slice(b"Your OAuth token (valid for 1 year):\r\n\r\n");
        bytes.extend_from_slice(b"\x1b[1m\x1b[38;5;214m");
        bytes.extend_from_slice(head.as_bytes());
        bytes.extend_from_slice(b"\x1b[0m\x1b[38;2;10;200;30m");
        bytes.extend_from_slice(tail.as_bytes());
        bytes.extend_from_slice(b"\x1b[0m\r\n\r\n");
        bytes.extend_from_slice(
            b"Store this token securely. Use it with export CLAUDE_CODE_OAUTH_TOKEN=<token>\r\n",
        );
        bytes.extend_from_slice(b"skip asks sk sk- sk-an ok\r\n");
        bytes
    }

    fn run(chunks: &[&[u8]], cols: u16, idle_between: bool) -> (Vec<u8>, TokenFilter) {
        let mut filter = TokenFilter::new(cols);
        let mut shown = Vec::new();
        for (index, chunk) in chunks.iter().enumerate() {
            filter.push(chunk, &mut shown);
            if idle_between && index + 1 < chunks.len() {
                filter.flush_idle(&mut shown);
            }
        }
        filter.finish(&mut shown);
        (shown, filter)
    }

    fn only_token(filter: &TokenFilter) -> String {
        match select_token(filter.found()) {
            Selection::One(token) => token.as_str().to_owned(),
            Selection::None { other } => panic!("no token captured ({other} other)"),
            Selection::Several(count) => panic!("{count} tokens captured"),
        }
    }

    fn strip_mask(shown: &[u8]) -> String {
        String::from_utf8_lossy(shown).replace(MASK, "")
    }

    #[test]
    fn the_token_is_hidden_and_captured_with_escapes_around_and_inside() {
        let input = stream(TOKEN);
        let (shown, filter) = run(&[&input], 1000, false);
        assert_eq!(only_token(&filter), TOKEN);
        assert!(leaked_positions(&shown, TOKEN).is_empty());
        let text = String::from_utf8(shown.clone()).unwrap();
        assert_eq!(text.matches(MASK).count(), 1, "{}", strip_mask(&shown));
        // Everything else comes through unchanged, including the escapes
        // inside the token (after the mask) and look-alikes of the prefix.
        assert!(text.contains("Your OAuth token (valid for 1 year):\r\n\r\n\x1b[1m\x1b[38;5;214m[token hidden by axocoatl]\x1b[0m\x1b[38;2;10;200;30m\x1b[0m\r\n"));
        assert!(text.contains("skip asks sk sk- sk-an ok\r\n"));
        assert!(text.contains("\u{2713} Long-lived"));
    }

    /// Every way of cutting the output in two, three and single bytes gives
    /// the same display and the same token.
    #[test]
    fn every_chunk_split_point_gives_the_same_result() {
        let input = stream(TOKEN);
        let (whole, _) = run(&[&input], 1000, false);
        let token_at = input
            .windows(7)
            .position(|window| window == b"sk-ant-")
            .unwrap();
        for split in 0..=input.len() {
            let (a, b) = input.split_at(split);
            let (shown, filter) = run(&[a, b], 1000, false);
            assert_eq!(shown, whole, "split at {split}");
            assert_eq!(only_token(&filter), TOKEN, "split at {split}");
        }
        // Three chunks, both cuts around the token.
        for first in token_at.saturating_sub(30)..(token_at + TOKEN.len() + 40).min(input.len()) {
            for second in first..(token_at + TOKEN.len() + 40).min(input.len()) {
                let (shown, filter) = run(
                    &[&input[..first], &input[first..second], &input[second..]],
                    1000,
                    false,
                );
                assert_eq!(shown, whole, "split at {first}, {second}");
                assert_eq!(only_token(&filter), TOKEN);
            }
        }
        let bytes: Vec<&[u8]> = input.chunks(1).collect();
        let (shown, filter) = run(&bytes, 1000, false);
        assert_eq!(shown, whole);
        assert_eq!(only_token(&filter), TOKEN);
    }

    /// A quiet program at any split point: what is held back is shown, but
    /// never more than `sk-ant` of the token, and the token is still
    /// captured whole.
    #[test]
    fn an_idle_flush_at_every_split_point_never_shows_the_secret() {
        let input = stream(TOKEN);
        for split in 0..=input.len() {
            let (a, b) = input.split_at(split);
            let (shown, filter) = run(&[a, b], 1000, true);
            assert!(
                leaked_positions(&shown, TOKEN).is_empty(),
                "split at {split}: {:?}",
                leaked_positions(&shown, TOKEN)
            );
            assert_eq!(only_token(&filter), TOKEN, "split at {split}");
            assert!(!strip_mask(&shown).contains("sk-ant-"), "split at {split}");
        }
        let bytes: Vec<&[u8]> = input.chunks(1).collect();
        let (shown, filter) = run(&bytes, 1000, true);
        assert!(leaked_positions(&shown, TOKEN).is_empty());
        assert_eq!(only_token(&filter), TOKEN);
    }

    /// Escapes between every pair of characters of the prefix and of the
    /// token, at every split point.
    #[test]
    fn escapes_between_every_character_are_transparent() {
        let mut input = b"token: \x1b[32m".to_vec();
        for (index, byte) in TOKEN.bytes().enumerate() {
            input.push(byte);
            if index % 3 == 0 {
                input.extend_from_slice(b"\x1b[0;1;38;5;200m");
            } else if index % 3 == 1 {
                input.extend_from_slice(b"\x1b(B");
            }
        }
        input.extend_from_slice(b"\x1b[0m done\r\n");
        let (whole, filter) = run(&[&input], 1000, false);
        assert_eq!(only_token(&filter), TOKEN);
        assert!(leaked_positions(&whole, TOKEN).is_empty());
        assert!(String::from_utf8_lossy(&whole).ends_with("\x1b[0m done\r\n"));
        for split in 0..=input.len() {
            let (a, b) = input.split_at(split);
            let (shown, filter) = run(&[a, b], 1000, false);
            assert_eq!(shown, whole, "split at {split}");
            assert_eq!(only_token(&filter), TOKEN);
            let (shown, filter) = run(&[a, b], 1000, true);
            assert!(
                leaked_positions(&shown, TOKEN).is_empty(),
                "split at {split}"
            );
            assert_eq!(only_token(&filter), TOKEN);
        }
    }

    /// In a terminal narrower than the token, the program wraps it at the
    /// last column: the continuation lines are hidden and joined.
    #[test]
    fn a_token_wrapped_at_the_last_column_is_hidden_and_joined() {
        for cols in [20u16, 33, 40, 64, 80] {
            let width = usize::from(cols);
            let mut input = b"Your OAuth token:\r\n\x1b[33m".to_vec();
            for (index, line) in TOKEN.as_bytes().chunks(width).enumerate() {
                if index > 0 {
                    input.extend_from_slice(b"\r\n");
                }
                input.extend_from_slice(line);
            }
            input.extend_from_slice(b"\x1b[0m\r\n\r\nStore this token securely.\r\n");
            for split in 0..=input.len() {
                let (a, b) = input.split_at(split);
                for idle in [false, true] {
                    let (shown, filter) = run(&[a, b], cols, idle);
                    assert!(
                        leaked_positions(&shown, TOKEN).is_empty(),
                        "cols {cols} split {split}"
                    );
                    assert_eq!(only_token(&filter), TOKEN, "cols {cols} split {split}");
                    assert!(strip_mask(&shown).contains("Store this token securely."));
                }
            }
        }
        // A token that ends before the last column is not joined with the
        // next line.
        let mut filter = TokenFilter::new(200);
        let mut shown = Vec::new();
        filter.push(TOKEN.as_bytes(), &mut shown);
        filter.push(b"\r\nNext\r\n", &mut shown);
        filter.finish(&mut shown);
        assert_eq!(only_token(&filter), TOKEN);
        assert!(strip_mask(&shown).contains("\r\nNext\r\n"));
    }

    #[test]
    fn other_anthropic_keys_are_hidden_but_not_captured() {
        let api_key = "sk-ant-api03-abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
        let input = format!("key {api_key} and ssk-ant-zz and sk-ant- and sk-ant-oat01-short\r\n");
        let (shown, filter) = run(&[input.as_bytes()], 1000, false);
        assert!(leaked_positions(&shown, api_key).is_empty());
        let text = String::from_utf8(shown).unwrap();
        assert_eq!(text.matches(MASK).count(), 4, "{text}");
        assert!(text.starts_with("key [token hidden by axocoatl] and s[token hidden"));
        assert!(matches!(
            select_token(filter.found()),
            Selection::None { other: 4 }
        ));
    }

    #[test]
    fn a_token_inside_a_hyperlink_escape_is_hidden() {
        let input = format!("\x1b]8;;https://example.test/?t={TOKEN}\x1b\\link\x1b]8;;\x1b\\\r\n");
        let (shown, filter) = run(&[input.as_bytes()], 1000, false);
        assert!(leaked_positions(&shown, TOKEN).is_empty());
        assert_eq!(only_token(&filter), TOKEN);
        assert!(String::from_utf8_lossy(&shown).contains("link"));
    }

    #[test]
    fn the_same_token_twice_is_one_and_two_tokens_are_refused() {
        let input = format!("{TOKEN}\r\nexport CLAUDE_CODE_OAUTH_TOKEN={TOKEN}\r\n");
        let (shown, filter) = run(&[input.as_bytes()], 1000, false);
        assert!(leaked_positions(&shown, TOKEN).is_empty());
        assert_eq!(only_token(&filter), TOKEN);

        let other = TOKEN.replace("Xq7", "Yy8");
        let input = format!("{TOKEN}\r\n{other}\r\n");
        let (shown, filter) = run(&[input.as_bytes()], 1000, false);
        assert!(leaked_positions(&shown, TOKEN).is_empty());
        assert!(leaked_positions(&shown, &other).is_empty());
        assert!(matches!(
            select_token(filter.found()),
            Selection::Several(2)
        ));
    }

    #[test]
    fn an_overlong_run_is_hidden_and_not_captured() {
        let long = format!("{TOKEN}{}", "a".repeat(MAX_TOKEN_CHARS));
        let (shown, filter) = run(&[long.as_bytes(), b" end"], 1000, false);
        assert_eq!(String::from_utf8(shown).unwrap(), format!("{MASK} end"));
        assert!(matches!(
            select_token(filter.found()),
            Selection::None { other: 1 }
        ));
    }

    #[test]
    fn output_without_a_token_passes_through_unchanged_and_ends_flushed() {
        let input = b"\x1b[31mskill\x1b[0m sk-an\x1b[1mt done s".to_vec();
        for split in 0..=input.len() {
            let (a, b) = input.split_at(split);
            let (shown, filter) = run(&[a, b], 1000, false);
            assert_eq!(shown, input, "split {split}");
            assert!(filter.found().is_empty());
        }
    }
}
