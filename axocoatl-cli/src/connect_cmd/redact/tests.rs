use super::super::ink_model::{self, Shape};
use super::super::screen::{Screen, Step};
use super::*;

/// A token shaped like the real one: the OAuth prefix and 91 more
/// characters of the token alphabet.
pub(crate) const TOKEN: &str = "sk-ant-oat01-Xq7_vR2mZ9kLp4Tn8Wc1Yb6Hs3Df0Gj5Ae-Ui2Ko7Nl4Mx9Pz1Qw8Er3Ty6Bv0Cs5Dh2Fg7Jk4La9Zx1Vn6Mb3Rt8AA";
/// 108 characters, as long as Anthropic's API keys.
const LONG_TOKEN: &str = "sk-ant-oat01-Bq7_vR2mZ9kLp4Tn8Wc1Yb6Hs3Df0Gj5Ae-Ui2Ko7Nl4Mx9Pz1Qw8Er3Ty6Bv0Cs5Dh2Fg7Jk4La9Zx1Vn6Mb3Rt8Wq2-pAA";

/// Every substring of `token` of at least 12 bytes that `shown` contains,
/// as positions (never the text itself, so a failure does not print it).
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

/// What the user's terminal shows for `bytes`: every row (with the rows
/// that scrolled off and are kept) as text, `.` for empty cells.
pub(crate) fn rendered(bytes: &[u8], cols: u16, rows: u16) -> Vec<String> {
    let mut screen = Screen::new(cols, rows);
    let mut first_row = 0u64;
    let mut last_row = 0u64;
    for &byte in bytes {
        if let Step::Print(print) = screen.feed(byte).step {
            last_row = last_row.max(print.row);
        }
    }
    let span = u64::from(rows) + 16;
    if last_row + 1 > span {
        first_row = last_row + 1 - span;
    }
    (first_row..=last_row.max(u64::from(rows) - 1))
        .map(|row| {
            (0..usize::from(cols))
                .filter_map(|x| match screen.cell(x, row) {
                    Some(cell) if cell.tail => None,
                    Some(cell) if cell.ch == '\0' => Some('.'),
                    Some(cell) => Some(cell.ch),
                    None => Some('?'),
                })
                .collect()
        })
        .collect()
}

/// The display has the program's layout cell for cell: every cell shows
/// what the program wrote, except that each cell of the token shows the
/// mask (or a blank); and no part of `token` is visible anywhere.
pub(crate) fn assert_masked_cell_for_cell(
    input: &[u8],
    shown: &[u8],
    cols: u16,
    rows: u16,
    token: &str,
) {
    let program = rendered(input, cols, rows);
    let display = rendered(shown, cols, rows);
    assert_eq!(program.len(), display.len(), "{cols}x{rows}: row counts");
    let mut masked = 0;
    for (row, (wrote, showed)) in program.iter().zip(&display).enumerate() {
        for (x, (a, b)) in wrote.chars().zip(showed.chars()).enumerate() {
            if a != b {
                masked += 1;
                assert!(
                    is_token_char(a) && (MASK.contains(b) || b == ' '),
                    "{cols}x{rows}: cell {x},{row} differs and is not a masked token cell"
                );
            }
        }
    }
    assert!(masked > 0, "{cols}x{rows}: nothing was masked");
    let all = display.join("\n");
    assert!(
        leaked_positions(all.as_bytes(), token).is_empty(),
        "{cols}x{rows}: token text is visible"
    );
    // The stream itself never carries it either.
    assert!(
        leaked_positions(shown, token).is_empty(),
        "{cols}x{rows}: stream leak"
    );
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

fn run_sized(chunks: &[&[u8]], cols: u16, rows: u16, idle_between: bool) -> (Vec<u8>, TokenFilter) {
    let mut filter = TokenFilter::new(cols, rows);
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

fn run(chunks: &[&[u8]], cols: u16, idle_between: bool) -> (Vec<u8>, TokenFilter) {
    run_sized(chunks, cols, 40, idle_between)
}

fn selected(filter: &TokenFilter) -> Selection {
    select_token(filter.found())
}

fn only_token(filter: &TokenFilter) -> String {
    match selected(filter) {
        Selection::One(token) => token.as_str().to_owned(),
        Selection::None { other } => panic!("no token captured ({other} other)"),
        Selection::Several(count) => panic!("{count} tokens captured"),
    }
}

/// The cells a token of `length` characters shows.
fn masked(length: usize) -> String {
    (0..length)
        .map(|index| char::from(mask_byte(index)))
        .collect()
}

/// Text without its escape sequences.
fn visible(shown: &[u8]) -> String {
    let mut text = String::new();
    let mut screen = Screen::new(1000, 40);
    for &byte in shown {
        if let Step::Print(print) = screen.feed(byte).step {
            text.push(print.ch);
        } else if byte == b'\n' && !screen.in_sequence() {
            text.push('\n');
        }
    }
    text
}

#[test]
fn the_token_is_hidden_and_captured_with_escapes_around_and_inside() {
    let input = stream(TOKEN);
    let (shown, filter) = run(&[&input], 200, false);
    assert_eq!(only_token(&filter), TOKEN);
    assert!(leaked_positions(&shown, TOKEN).is_empty());
    let text = String::from_utf8(shown.clone()).unwrap();
    // The token's cells show the mask, then blanks; the escapes inside it
    // stay where they were, so colors stay balanced and nothing moves.
    let expected = format!(
        "Your OAuth token (valid for 1 year):\r\n\r\n\x1b[1m\x1b[38;5;214m{}\x1b[0m\x1b[38;2;10;200;30m{}\x1b[0m\r\n",
        masked(40),
        " ".repeat(TOKEN.len() - 40)
    );
    assert!(text.contains(&expected), "{text}");
    assert_eq!(text.matches(MASK).count(), 1);
    assert!(text.contains("skip asks sk sk- sk-an ok\r\n"));
    assert!(text.contains("\u{2713} Long-lived"));
    assert_masked_cell_for_cell(&input, &shown, 200, 40, TOKEN);
}

/// Every way of cutting the output in two, three and single bytes gives
/// the same display and the same token.
#[test]
fn every_chunk_split_point_gives_the_same_result() {
    let input = stream(TOKEN);
    let (whole, _) = run(&[&input], 200, false);
    let token_at = input
        .windows(7)
        .position(|window| window == b"sk-ant-")
        .unwrap();
    for split in 0..=input.len() {
        let (a, b) = input.split_at(split);
        let (shown, filter) = run(&[a, b], 200, false);
        assert_eq!(shown, whole, "split at {split}");
        assert_eq!(only_token(&filter), TOKEN, "split at {split}");
    }
    // Three chunks, both cuts around the token.
    for first in token_at.saturating_sub(30)..(token_at + TOKEN.len() + 40).min(input.len()) {
        for second in first..(token_at + TOKEN.len() + 40).min(input.len()) {
            let (shown, filter) = run(
                &[&input[..first], &input[first..second], &input[second..]],
                200,
                false,
            );
            assert_eq!(shown, whole, "split at {first}, {second}");
            assert_eq!(only_token(&filter), TOKEN);
        }
    }
    let bytes: Vec<&[u8]> = input.chunks(1).collect();
    let (shown, filter) = run(&bytes, 200, false);
    assert_eq!(shown, whole);
    assert_eq!(only_token(&filter), TOKEN);
}

/// A quiet program at any split point: what is held back is shown, but
/// never more than `sk-ant` of the token, and the token is still captured
/// whole.
#[test]
fn an_idle_flush_at_every_split_point_never_shows_the_secret() {
    let input = stream(TOKEN);
    for split in 0..=input.len() {
        let (a, b) = input.split_at(split);
        let (shown, filter) = run(&[a, b], 200, true);
        assert!(
            leaked_positions(&shown, TOKEN).is_empty(),
            "split at {split}: {:?}",
            leaked_positions(&shown, TOKEN)
        );
        assert_eq!(only_token(&filter), TOKEN, "split at {split}");
        assert!(!visible(&shown).contains("sk-ant-"), "split at {split}");
    }
    let bytes: Vec<&[u8]> = input.chunks(1).collect();
    let (shown, filter) = run(&bytes, 200, true);
    assert!(leaked_positions(&shown, TOKEN).is_empty());
    assert_eq!(only_token(&filter), TOKEN);
}

/// Escapes of every kind between every pair of characters of the prefix
/// and of the token, at every split point: colors, character sets, cursor
/// save and restore, a move away and back, hyperlinks (OSC 8 with `BEL`
/// and with `ST`), synchronized-update marks, a DCS string, the cursor's
/// visibility.
#[test]
fn escapes_between_every_character_are_transparent() {
    let escapes: [&[u8]; 9] = [
        b"\x1b[0;1;38;5;200m",
        b"\x1b(B",
        b"\x1b7\x1b8",
        b"\x1b[3D\x1b[3C",
        b"\x1b]8;id=x;https://example.test/\x07",
        b"\x1b]8;;\x1b\\",
        b"\x1b[?2026h\x1b[?2026l",
        b"\x1bP+q544e\x1b\\",
        b"\x1b[?25l",
    ];
    let mut input = b"token: \x1b[32m".to_vec();
    for (index, byte) in TOKEN.bytes().enumerate() {
        input.push(byte);
        input.extend_from_slice(escapes[index % escapes.len()]);
    }
    input.extend_from_slice(b"\x1b[0m done\r\n");
    let (whole, filter) = run(&[&input], 200, false);
    assert_eq!(only_token(&filter), TOKEN);
    assert!(leaked_positions(&whole, TOKEN).is_empty());
    assert!(String::from_utf8_lossy(&whole).ends_with("\x1b[0m done\r\n"));
    assert_eq!(
        visible(&whole).trim_end(),
        format!("token: {} done", masked(TOKEN.len()))
    );
    for split in 0..=input.len() {
        let (a, b) = input.split_at(split);
        let (shown, filter) = run(&[a, b], 200, false);
        assert_eq!(shown, whole, "split at {split}");
        assert_eq!(only_token(&filter), TOKEN);
        let (shown, filter) = run(&[a, b], 200, true);
        assert!(
            leaked_positions(&shown, TOKEN).is_empty(),
            "split at {split}"
        );
        assert_eq!(only_token(&filter), TOKEN);
    }
}

/// A prefix split by escapes that are not transparent to a byte filter
/// (an OSC string, the string terminator, a DCS string): what lands in the
/// cells is `sk-ant-…`, so it is hidden.
#[test]
fn a_prefix_split_by_any_escape_is_hidden() {
    for escape in [
        "\x1b]0;title\x07",
        "\x1b]8;;https://example.test/\x1b\\",
        "\x1b\\",
        "\x1bPtmux;\x1b\x1b[0m\x1b\\",
        "\x1b_apc\x1b\\",
        "\x1b[?2026h",
        "\x1b[1;1H\x1b[1;{next}H",
        "\x1b7\x1b[5;5H\x1b8",
    ] {
        for at in 1..PREFIX.len() {
            let escape = escape.replace("{next}", &(at + 1).to_string());
            let escape = escape.as_bytes();
            let mut input = Vec::new();
            input.extend_from_slice(&TOKEN.as_bytes()[..at]);
            input.extend_from_slice(escape);
            input.extend_from_slice(&TOKEN.as_bytes()[at..]);
            input.extend_from_slice(b"\r\n");
            let (shown, filter) = run(&[&input], 200, false);
            assert!(
                leaked_positions(&shown, TOKEN).is_empty(),
                "{escape:?} at {at}"
            );
            assert_eq!(only_token(&filter), TOKEN, "{escape:?} at {at}");
            assert_eq!(visible(&shown).trim_end(), masked(TOKEN.len()).trim_end());
        }
    }
}

/// In a terminal narrower than the token, a program that breaks it with
/// line breaks at the last column (indented or not) shows it hidden and
/// joined.
#[test]
fn a_token_wrapped_with_line_breaks_and_indentation_is_hidden_and_joined() {
    for indent in [0usize, 2, 4] {
        for cols in [20u16, 33, 40, 64, 80] {
            let width = usize::from(cols) - indent;
            let mut input = b"Your OAuth token:\r\n\x1b[33m".to_vec();
            for line in TOKEN.as_bytes().chunks(width) {
                input.extend_from_slice(" ".repeat(indent).as_bytes());
                input.extend_from_slice(line);
                input.extend_from_slice(b"\r\n");
            }
            input.extend_from_slice(b"\x1b[0m\r\nStore this token securely.\r\n");
            for split in 0..=input.len() {
                let (a, b) = input.split_at(split);
                for idle in [false, true] {
                    let (shown, filter) = run(&[a, b], cols, idle);
                    assert!(
                        leaked_positions(&shown, TOKEN).is_empty(),
                        "indent {indent} cols {cols} split {split}"
                    );
                    assert_eq!(only_token(&filter), TOKEN, "cols {cols} split {split}");
                    assert!(visible(&shown).contains("Store this token securely."));
                }
            }
        }
    }
    // A token that ends before the last column is not stored with the next
    // line. That line, starting in the token's column, is masked too (it
    // could be the token's continuation in a narrow box), unless it is in
    // another style or further left or right.
    let mut filter = TokenFilter::new(200, 40);
    let mut shown = Vec::new();
    filter.push(TOKEN.as_bytes(), &mut shown);
    filter.push(b"\r\nNext\r\n \x1b[2mmore\x1b[22m\r\n", &mut shown);
    filter.finish(&mut shown);
    assert_eq!(only_token(&filter), TOKEN);
    let text = visible(&shown);
    assert!(text.contains("\n    \n more"), "{text:?}");
    let mut filter = TokenFilter::new(200, 40);
    let mut shown = Vec::new();
    filter.push(
        format!("\x1b[33m{TOKEN}\x1b[39m\r\n\x1b[2mNext\x1b[22m\r\n").as_bytes(),
        &mut shown,
    );
    filter.finish(&mut shown);
    assert_eq!(only_token(&filter), TOKEN);
    assert!(visible(&shown).contains("\nNext"));
}

/// A program that prints the token as one line in a narrow terminal: the
/// terminal wraps it (no line break in the output).
#[test]
fn a_token_the_terminal_wraps_is_hidden_and_joined() {
    for cols in 20u16..=120 {
        let input = format!("Your token: \x1b[1m{TOKEN}\x1b[0m\r\nStore it.\r\n");
        let (shown, filter) = run(&[input.as_bytes()], cols, false);
        assert_eq!(only_token(&filter), TOKEN, "{cols}");
        assert_masked_cell_for_cell(input.as_bytes(), &shown, cols, 40, TOKEN);
    }
}

/// What Claude Code 2.1.271 writes for `claude setup-token`, at every width
/// from 20 to 300 columns and with and without the tall banner: the token
/// (wrapped by Ink into rows placed by cursor movement) is captured exactly
/// and every one of its cells is masked; everything else is shown as the
/// program wrote it.
#[test]
fn claude_code_rendering_at_every_width_is_masked_and_captured() {
    for token in [TOKEN, LONG_TOKEN] {
        for rows in [24u16, 40] {
            for cols in 20u16..=300 {
                let (before, after) =
                    ink_model::session(token, usize::from(cols), usize::from(rows), Shape::Claude);
                let input = [before.as_slice(), after.as_slice()].concat();
                let (shown, filter) = run_sized(&[&before, &after], cols, rows, true);
                assert_eq!(only_token(&filter), token, "{cols}x{rows}");
                assert_masked_cell_for_cell(&input, &shown, cols, rows, token);
                let display = rendered(&shown, cols, rows).join("\n");
                let first_row = &MASK[..MASK.len().min(usize::from(cols) - 1)];
                assert!(display.contains(first_row), "{cols}x{rows}");
                // Byte by byte, the same.
                let bytes: Vec<&[u8]> = input.chunks(1).collect();
                let (bytewise, filter) = run_sized(&bytes, cols, rows, false);
                assert_eq!(bytewise, shown, "{cols}x{rows}: byte by byte");
                assert_eq!(only_token(&filter), token);
            }
        }
    }
}

/// The token frame cut at every byte, with the program quiet at the cut.
#[test]
fn claude_code_rendering_cut_anywhere_is_masked_and_captured() {
    for cols in [40u16, 80, 109] {
        let (before, after) = ink_model::session(TOKEN, usize::from(cols), 24, Shape::Claude);
        for cut in 0..=after.len() {
            let (a, b) = after.split_at(cut);
            let (shown, filter) = run_sized(&[&before, a, b], cols, 24, true);
            assert!(
                leaked_positions(&shown, TOKEN).is_empty(),
                "{cols}: cut at {cut}"
            );
            assert_eq!(only_token(&filter), TOKEN, "{cols}: cut at {cut}");
            assert!(!visible(&shown).contains("sk-ant-"), "{cols}: cut at {cut}");
        }
    }
}

/// The token in a rounded border box with padding, after a label in the
/// same text (so its first row starts mid-row and its prefix may wrap), and
/// in a box narrower than the terminal: at every width from 30 to 300.
#[test]
fn bordered_labeled_and_narrow_layouts_are_masked_and_captured() {
    for shape in [Shape::Bordered, Shape::Labeled, Shape::Narrow] {
        for cols in 30u16..=300 {
            let (before, after) = ink_model::session(LONG_TOKEN, usize::from(cols), 40, shape);
            let input = [before.as_slice(), after.as_slice()].concat();
            let (shown, filter) = run_sized(&[&before, &after], cols, 40, true);
            assert_eq!(only_token(&filter), LONG_TOKEN, "{shape:?} {cols}");
            assert_masked_cell_for_cell(&input, &shown, cols, 40, LONG_TOKEN);
        }
    }
    // A label that puts the prefix itself across the wrap.
    let label = "x".repeat(70);
    for cols in 72u16..=80 {
        let input = format!("{label} {LONG_TOKEN}\r\nnext\r\n");
        let (shown, filter) = run_sized(&[input.as_bytes()], cols, 40, false);
        assert_eq!(only_token(&filter), LONG_TOKEN, "{cols}");
        assert_masked_cell_for_cell(input.as_bytes(), &shown, cols, 40, LONG_TOKEN);
    }
}

/// When the layout above the token changes, Ink's renderer rewrites only
/// the cells that changed: where the token's new row holds the same
/// character as the row it replaces, nothing is written. The skipped cells
/// are already masked, and the token is read from the model.
#[test]
fn a_token_moved_by_a_layout_change_is_masked_and_captured() {
    for cols in [40usize, 50, 64, 80] {
        let width = cols - 1;
        // A token whose second row repeats its first at many columns.
        let mut token: Vec<u8> = LONG_TOKEN.as_bytes().to_vec();
        for index in 13..token.len().min(width) {
            if index + width < token.len() && index % 3 != 0 {
                token[index + width] = token[index];
            }
        }
        let token = String::from_utf8(token).unwrap();
        let rows = 40;
        let mut renderer = ink_model::Renderer::new(cols, rows);
        let mut input = renderer.start();
        input.extend(renderer.render(&ink_model::waiting(cols, rows)));
        let first = ink_model::success(&token, cols, rows, Shape::Claude, ink_model::GUIDE);
        input.extend(renderer.render(&first));
        // A guide one row shorter moves everything below it up a row.
        let rows_of = |text: &str| ink_model::wrap(&[(text, 0)], cols - 1).len();
        let words: Vec<&str> = ink_model::GUIDE.split(' ').collect();
        let shorter = (1..words.len())
            .rev()
            .map(|count| words[..count].join(" "))
            .find(|text| rows_of(text) + 1 == rows_of(ink_model::GUIDE))
            .unwrap();
        let moved = ink_model::success(&token, cols, rows, Shape::Claude, &shorter);
        assert_eq!(moved.height() + 1, first.height(), "{cols}");
        let update = renderer.render(&moved);
        // Cells of the token's new rows that hold what was there already,
        // which the update does not write.
        let skipped = (0..moved.height())
            .flat_map(|y| (0..cols).map(move |x| (x, y)))
            .filter(|(x, y)| {
                let new = moved.rows[*y][*x];
                new.is_some_and(|cell| cell.style == ink_model::WARNING)
                    && first.rows.get(*y).and_then(|row| row[*x]) == new
            })
            .count();
        assert!(skipped >= 5, "{cols}: {skipped}");
        input.extend(update);
        input.extend(renderer.exit());
        let (shown, filter) = run_sized(&[&input], cols as u16, rows as u16, false);
        assert_eq!(only_token(&filter), token, "{cols}");
        assert_masked_cell_for_cell(&input, &shown, cols as u16, rows as u16, &token);
    }
}

#[test]
fn other_anthropic_keys_are_hidden_but_not_captured() {
    let api_key = "sk-ant-api03-abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
    let input = format!("key {api_key} and ssk-ant-zz and sk-ant- and sk-ant-oat01-short\r\n");
    let (shown, filter) = run(&[input.as_bytes()], 200, false);
    assert!(leaked_positions(&shown, api_key).is_empty());
    let text = String::from_utf8(shown).unwrap();
    assert_eq!(
        text,
        format!(
            "key {} and s{} and {} and {}\r\n",
            masked(api_key.len()),
            masked(9),
            masked(7),
            masked(18)
        )
    );
    assert!(matches!(
        select_token(filter.found()),
        Selection::None { other: 4 }
    ));
}

/// A token inside a hyperlink's target is captured; the hyperlink is not
/// shown (its text is).
#[test]
fn a_token_inside_a_hyperlink_escape_is_hidden() {
    let input = format!("\x1b]8;;https://example.test/?t={TOKEN}\x1b\\link\x1b]8;;\x1b\\\r\n");
    let (shown, filter) = run(&[input.as_bytes()], 200, false);
    assert!(leaked_positions(&shown, TOKEN).is_empty());
    assert_eq!(only_token(&filter), TOKEN);
    assert_eq!(String::from_utf8_lossy(&shown), "link\x1b]8;;\x1b\\\r\n");
}

/// Clipboard writes never reach the terminal, however they are sent; other
/// strings do.
#[test]
fn clipboard_writes_are_dropped_and_other_strings_kept() {
    let clipboard: [&[u8]; 7] = [
        b"\x1b]52;c;aHR0cHM6Ly9leGFtcGxlLnRlc3Q=\x07",
        b"\x1b]52;c;aHR0cHM6Ly9leGFtcGxlLnRlc3Q=\x1b\\",
        b"\x1b]52;c;?\x07",
        b"\x1bPtmux;\x1b\x1b]52;c;aGk=\x07\x1b\\",
        b"\x1b]5522;type=write\x07",
        b"\x1b]1337;Copy=:aGk=\x07",
        b"\x1b]1337;CopyToClipboard=\x07",
    ];
    for write in clipboard {
        let mut input = b"before ".to_vec();
        input.extend_from_slice(write);
        input.extend_from_slice(b"after\r\n");
        for split in 0..=input.len() {
            let (a, b) = input.split_at(split);
            let (shown, _) = run(&[a, b], 80, true);
            assert_eq!(shown, b"before after\r\n", "{write:?} split {split}");
        }
    }
    // A clipboard write broken off by another escape is dropped too.
    let (shown, _) = run(&[b"a\x1b]52;c;aGk=\x1b[1mb\r\n"], 80, false);
    assert_eq!(shown, b"a\x1b[1mb\r\n");
    for kept in [
        &b"\x1b]0;window title\x07"[..],
        b"\x1b]8;id=1;https://claude.ai/oauth/authorize\x07link\x1b]8;;\x07",
        b"\x1bP+q544e\x1b\\",
    ] {
        let mut input = kept.to_vec();
        input.extend_from_slice(b"\r\n");
        let (shown, _) = run(&[&input], 80, false);
        assert_eq!(shown, input);
    }
    // An unterminated string is not shown.
    let (shown, _) = run(&[b"a\x1b]52;c;aGk="], 80, false);
    assert_eq!(shown, b"a");
}

#[test]
fn the_same_token_twice_is_one_and_two_tokens_are_refused() {
    let input = format!("{TOKEN}\r\nexport CLAUDE_CODE_OAUTH_TOKEN={TOKEN}\r\n");
    let (shown, filter) = run(&[input.as_bytes()], 200, false);
    assert!(leaked_positions(&shown, TOKEN).is_empty());
    assert_eq!(only_token(&filter), TOKEN);

    let other = TOKEN.replace("Xq7", "Yy8");
    for cols in [80u16, TOKEN.len() as u16, 200] {
        let input = format!("{TOKEN}\r\n{other}\r\n");
        let (shown, filter) = run(&[input.as_bytes()], cols, false);
        assert!(leaked_positions(&shown, TOKEN).is_empty());
        assert!(leaked_positions(&shown, &other).is_empty());
        assert!(
            matches!(select_token(filter.found()), Selection::Several(2)),
            "{cols}"
        );
    }
}

/// Text after the token is never stored with it: the next row of the
/// output after a token that fills its row exactly, a word after a blank,
/// adjacent punctuation, and the next line in another style.
#[test]
fn text_after_the_token_is_not_part_of_it() {
    let cols = TOKEN.len() as u16;
    let cases: [(String, u16); 5] = [
        (format!("{TOKEN}\r\nStore this token securely.\r\n"), cols),
        (format!("{TOKEN} Store\r\n"), 200),
        (format!("{TOKEN}.\r\n"), 200),
        (
            format!("\x1b[33m{TOKEN}\x1b[39m\r\n\x1b[2mStore\x1b[22m\r\n"),
            cols,
        ),
        (
            format!(" \x1b[33m{TOKEN}\x1b[39m\r\n \x1b[2mUse it\x1b[22m\r\n"),
            cols + 1,
        ),
    ];
    for (input, cols) in cases {
        let (shown, filter) = run(&[input.as_bytes()], cols, false);
        assert_eq!(only_token(&filter), TOKEN, "{input:?}");
        assert!(leaked_positions(&shown, TOKEN).is_empty());
    }
}

#[test]
fn an_overlong_run_is_hidden_and_not_captured() {
    let long = format!("{TOKEN}{}", "a".repeat(MAX_TOKEN_CHARS));
    let (shown, filter) = run(&[long.as_bytes(), b" end"], 1000, false);
    assert_eq!(
        String::from_utf8(shown).unwrap(),
        format!("{} end", masked(long.len()))
    );
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
        let (shown, filter) = run(&[a, b], 200, false);
        assert_eq!(shown, input, "split {split}");
        assert!(filter.found().is_empty());
    }
    let (before, _) = ink_model::session(TOKEN, 80, 24, Shape::Claude);
    let (shown, filter) = run_sized(&[&before], 80, 24, false);
    assert_eq!(shown, before);
    assert!(filter.found().is_empty());
}

/// REP repeating a token's character is written out and masked.
#[test]
fn a_repeated_character_is_masked() {
    let input = format!("{}\x1b[20b done\r\n", &TOKEN[..60]);
    let (shown, filter) = run(&[input.as_bytes()], 200, false);
    let repeated = format!("{}{}", &TOKEN[..60], TOKEN[59..60].repeat(20));
    assert_eq!(only_token(&filter), repeated);
    assert!(!String::from_utf8_lossy(&shown).contains("\x1b[20b"));
    assert_eq!(
        visible(&shown).trim_end(),
        format!("{} done", masked(80)).trim_end()
    );
}

/// A resize while the program draws the token for the old size: every cell
/// is still masked, and the token the program draws again for the new size
/// is the one kept.
#[test]
fn a_resize_while_the_token_is_drawn_never_shows_it() {
    for (old, new) in [(80u16, 60u16), (60, 100), (120, 41)] {
        let (before, after) = ink_model::session(TOKEN, usize::from(old), 24, Shape::Claude);
        let token_at = after
            .windows(7)
            .position(|window| window == b"sk-ant-")
            .unwrap();
        for cut in [token_at + 3, token_at + 20, token_at + 70] {
            let mut filter = TokenFilter::new(old, 24);
            let mut shown = Vec::new();
            filter.push(&before, &mut shown);
            filter.push(&after[..cut], &mut shown);
            filter.resize(new, 24);
            filter.push(&after[cut..], &mut shown);
            filter.flush_idle(&mut shown);
            // The program redraws for the new size.
            let redraw =
                ink_model::resized_session(TOKEN, (usize::from(old), 24), (usize::from(new), 24));
            filter.push(&redraw, &mut shown);
            filter.finish(&mut shown);
            assert!(
                leaked_positions(&shown, TOKEN).is_empty(),
                "{old}->{new} cut {cut}"
            );
            assert!(!visible(&shown).contains("sk-ant-"));
            assert_eq!(only_token(&filter), TOKEN, "{old}->{new} cut {cut}");
        }
    }
}

/// In a terminal smaller than [`super::super::relay::MIN_SIZE`], Claude
/// Code may leave the first rows of the token above the screen, where it
/// never draws them; from that size up, at any width to 300 columns and
/// any height (with and without the tall banner), it draws all of it.
#[test]
fn from_the_smallest_terminal_claude_runs_in_the_whole_token_is_drawn() {
    let (min_cols, min_rows) = super::super::relay::MIN_SIZE;
    for rows in [min_rows, min_rows + 1, 29, 30, 31, 45] {
        for cols in min_cols..=300 {
            let (before, after) = ink_model::session(
                LONG_TOKEN,
                usize::from(cols),
                usize::from(rows),
                Shape::Claude,
            );
            let (shown, filter) = run_sized(&[&before, &after], cols, rows, true);
            assert_eq!(only_token(&filter), LONG_TOKEN, "{cols}x{rows}");
            assert!(
                leaked_positions(&shown, LONG_TOKEN).is_empty(),
                "{cols}x{rows}"
            );
        }
    }
    let (before, after) = ink_model::session(LONG_TOKEN, 30, 12, Shape::Claude);
    let (_, filter) = run_sized(&[&before, &after], 30, 12, true);
    assert!(!matches!(selected(&filter), Selection::One(_)));
}
