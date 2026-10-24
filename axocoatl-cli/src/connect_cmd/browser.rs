//! Opening the sign-in page on this computer for `axocoatl connect codex`
//! when Codex runs in the login container, where it cannot open a browser
//! here.
//!
//! The relay watches what it shows (after the token filters) for the one
//! address Codex 0.160.1 prints ([`SignInPage`]): the browser sign-in's
//! `https://auth.openai.com/oauth/authorize?…`, or with `--device-auth` the
//! device page `https://auth.openai.com/codex/device`, nothing else (no other
//! host, scheme, port, user part, path or fragment). The first one it sees
//! is opened once, with this computer's opener ([`Opener`]: `open` on macOS,
//! `xdg-open` elsewhere, the first on `PATH`; in WSL, `wslview` first, which
//! opens Windows' browser), as its only argument and without a shell. The
//! address is not secret (Codex's PKCE verifier never leaves the
//! container), but it is never written to a file or a log.

use std::ffi::OsStr;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// What the relay shows once the opener has opened the page.
pub(crate) const OPENED: &str = "Opened the sign-in page in your browser.";

/// The origin both pages are on, exactly.
const ORIGIN: &str = "https://auth.openai.com";
/// The browser sign-in's path (then `?` and its query).
const AUTHORIZE_PATH: &str = "/oauth/authorize";
/// The device sign-in's page, exactly (Codex 0.160.1's
/// `{issuer}/codex/device`).
const DEVICE_PATH: &str = "/codex/device";
/// A longer word is not an address Codex prints (its sign-in address is
/// about 500 bytes).
const MAX_ADDRESS: usize = 4096;
/// How long the opener may take to say whether it opened the page; one that
/// is still running then (a browser started in the foreground) counts as
/// opened.
const OPEN_WAIT: Duration = Duration::from_secs(5);

/// The page Codex prints for a sign-in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SignInPage {
    /// `https://auth.openai.com/oauth/authorize?…`, the browser sign-in.
    Authorize,
    /// `https://auth.openai.com/codex/device`, with `--device-auth`.
    Device,
}

impl SignInPage {
    pub(crate) fn for_sign_in(device_auth: bool) -> Self {
        if device_auth {
            Self::Device
        } else {
            Self::Authorize
        }
    }

    /// Whether `address` is exactly this page: the origin
    /// `https://auth.openai.com` (lowercase, no port, no user part), then
    /// the device page's path and nothing more, or the authorize path, `?`
    /// and a query of form-encoded characters only (as Codex's URL builder
    /// writes it: letters, digits, `-._~*+&=` and `%` with two hex digits;
    /// no fragment).
    pub(crate) fn accepts(self, address: &str) -> bool {
        if address.len() > MAX_ADDRESS {
            return false;
        }
        let Some(rest) = address.strip_prefix(ORIGIN) else {
            return false;
        };
        match self {
            Self::Device => rest == DEVICE_PATH,
            Self::Authorize => rest
                .strip_prefix(AUTHORIZE_PATH)
                .and_then(|rest| rest.strip_prefix('?'))
                .is_some_and(form_encoded_query),
        }
    }
}

/// A non-empty query of form-encoded characters, each `%` followed by two
/// hex digits.
fn form_encoded_query(query: &str) -> bool {
    let bytes = query.as_bytes();
    if bytes.is_empty() {
        return false;
    }
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'%' => {
                let hex = bytes.get(index + 1..index + 3);
                if !hex.is_some_and(|hex| hex.iter().all(u8::is_ascii_hexdigit)) {
                    return false;
                }
                index += 3;
                continue;
            }
            byte if byte.is_ascii_alphanumeric() || b"-._~*+&=".contains(&byte) => {}
            _ => return false,
        }
        index += 1;
    }
    true
}

/// A character an address can hold (RFC 3986). Anything else ends a word.
fn address_char(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || b"-._~:/?#[]@!$&'()*+,;=%".contains(&byte)
}

/// Where the watcher is in an escape sequence.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Escape {
    /// Plain text.
    None,
    /// After `ESC`.
    Start,
    /// After `ESC` and intermediate bytes (`ESC ( B`, for example).
    Intermediate,
    /// In a CSI sequence.
    Csi,
    /// In an OSC, DCS, APC, PM or SOS string; `true` after its `ESC`.
    String(bool),
}

/// Finds the first address [`SignInPage::accepts`] in a stream of terminal
/// output, as the user sees it: a word of address characters, which color
/// changes (SGR) and strings such as a hyperlink's (`OSC 8`, whose hidden
/// target is never read) do not split, and which any other character or
/// control ends. Any other escape sequence (a cursor movement, an erase)
/// right after a word drops it: the program may have cut the address there,
/// so it is not opened, and a word after one starts afresh.
pub(crate) struct LinkWatcher {
    page: SignInPage,
    word: Vec<u8>,
    /// The current word grew past [`MAX_ADDRESS`]: it is not one.
    overlong: bool,
    escape: Escape,
    found: bool,
}

impl LinkWatcher {
    pub(crate) fn new(page: SignInPage) -> Self {
        Self {
            page,
            word: Vec::new(),
            overlong: false,
            escape: Escape::None,
            found: false,
        }
    }

    /// Read `bytes`; the page's address the first time a whole one has
    /// been seen, and never again.
    pub(crate) fn push(&mut self, bytes: &[u8]) -> Option<String> {
        let mut first = None;
        for &byte in bytes {
            if let Some(address) = self.step(byte) {
                first.get_or_insert(address);
            }
        }
        first
    }

    fn step(&mut self, byte: u8) -> Option<String> {
        match self.escape {
            Escape::None => {
                if byte == 0x1b {
                    self.escape = Escape::Start;
                    None
                } else if address_char(byte) {
                    if self.word.len() < MAX_ADDRESS {
                        self.word.push(byte);
                    } else {
                        self.overlong = true;
                    }
                    None
                } else {
                    self.end_word()
                }
            }
            Escape::Start => match byte {
                b'[' => {
                    self.escape = Escape::Csi;
                    None
                }
                b']' | b'P' | b'_' | b'^' | b'X' => {
                    self.escape = Escape::String(false);
                    None
                }
                0x20..=0x2f => {
                    self.escape = Escape::Intermediate;
                    None
                }
                _ => {
                    self.escape = Escape::None;
                    self.drop_word();
                    None
                }
            },
            Escape::Intermediate => {
                if !(0x20..=0x2f).contains(&byte) {
                    self.escape = Escape::None;
                    self.drop_word();
                }
                None
            }
            Escape::Csi => {
                if (0x40..=0x7e).contains(&byte) {
                    self.escape = Escape::None;
                    // A color change is inside the word; a cursor movement
                    // or an erase drops it.
                    if byte != b'm' {
                        self.drop_word();
                    }
                } else if !(0x20..=0x3f).contains(&byte) {
                    self.escape = Escape::None;
                    self.drop_word();
                }
                None
            }
            Escape::String(after_escape) => {
                self.escape = match byte {
                    0x07 => Escape::None,
                    b'\\' if after_escape => Escape::None,
                    0x1b => Escape::String(true),
                    _ => Escape::String(false),
                };
                None
            }
        }
    }

    /// Forget the current word without looking at it.
    fn drop_word(&mut self) {
        self.word.clear();
        self.overlong = false;
    }

    fn end_word(&mut self) -> Option<String> {
        let word = std::mem::take(&mut self.word);
        let overlong = std::mem::replace(&mut self.overlong, false);
        if self.found || overlong || word.is_empty() {
            return None;
        }
        let address = String::from_utf8(word).ok()?;
        if self.page.accepts(&address) {
            self.found = true;
            Some(address)
        } else {
            None
        }
    }
}

/// This computer's program that opens an address in the user's browser.
#[derive(Clone, Debug)]
pub(crate) struct Opener {
    program: PathBuf,
}

impl Opener {
    /// The opener's name: `open` on macOS, `xdg-open` elsewhere.
    pub(crate) fn name() -> &'static str {
        if cfg!(target_os = "macos") {
            "open"
        } else {
            "xdg-open"
        }
    }

    /// The first [`Opener::name`] on `path`, if there is one; in WSL,
    /// `wslview` first.
    pub(crate) fn find(path: Option<&OsStr>) -> Option<Self> {
        Self::find_for(path, in_wsl())
    }

    /// [`Opener::find`] in WSL (`wsl`) or not. WSL usually has no Linux
    /// browser for `xdg-open`, so `wslview` (wslu, which Ubuntu's WSL image
    /// ships) opens the page in Windows' browser, which reaches the
    /// distribution's `127.0.0.1` through WSL's localhost forwarding.
    /// Without either the address is only printed, as callers do anyway.
    pub(crate) fn find_for(path: Option<&OsStr>, wsl: bool) -> Option<Self> {
        let names: &[&str] = if wsl {
            &["wslview", Self::name()]
        } else {
            &[Self::name()]
        };
        names.iter().find_map(|name| {
            super::find_program(name, None, path, None, "")
                .ok()
                .map(|program| Self { program })
        })
    }

    /// Open `address` in the background: the opener runs with the address
    /// as its one argument (no shell), nothing of the terminal (its output
    /// is discarded, so it cannot draw over the sign-in) and in a process
    /// group of its own, so a Ctrl-C meant for Axocoatl does not reach a
    /// browser it starts. The receiver gets whether the page was opened.
    pub(crate) fn open(&self, address: &str) -> mpsc::Receiver<bool> {
        let (sender, receiver) = mpsc::channel();
        let mut command = Command::new(&self.program);
        command
            .arg(address)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0);
        std::thread::spawn(move || {
            let Ok(mut child) = command.spawn() else {
                let _ = sender.send(false);
                return;
            };
            let deadline = Instant::now() + OPEN_WAIT;
            loop {
                match child.try_wait() {
                    Ok(Some(status)) => {
                        let _ = sender.send(status.success());
                        return;
                    }
                    Ok(None) if Instant::now() < deadline => {
                        std::thread::sleep(Duration::from_millis(20));
                    }
                    Ok(None) => {
                        let _ = sender.send(true);
                        // Reaped whenever it ends; never killed, since it
                        // may be the browser itself.
                        let _ = child.wait();
                        return;
                    }
                    Err(_) => {
                        let _ = sender.send(false);
                        return;
                    }
                }
            }
        });
        receiver
    }
}

/// This is Windows Subsystem for Linux.
fn in_wsl() -> bool {
    cfg!(target_os = "linux")
        && crate::onboard::detect::wsl_from(
            &std::fs::read_to_string("/proc/sys/kernel/osrelease").unwrap_or_default(),
            std::env::var("WSL_DISTRO_NAME").ok().as_deref(),
        )
        .is_some()
}

/// The relay's part: watch what is shown for the page, open it once, and
/// once the opener has opened it, say so on the user's terminal.
pub(crate) struct OpenOnce<'a> {
    watcher: LinkWatcher,
    opener: &'a Opener,
    opening: Option<mpsc::Receiver<bool>>,
    /// The last byte shown ended a line.
    at_line_start: bool,
}

impl<'a> OpenOnce<'a> {
    pub(crate) fn new(page: SignInPage, opener: &'a Opener) -> Self {
        Self {
            watcher: LinkWatcher::new(page),
            opener,
            opening: None,
            at_line_start: true,
        }
    }

    /// `shown` is about to be shown: start opening the page the first time
    /// its address is in it.
    pub(crate) fn watch(&mut self, shown: &[u8]) {
        if let Some(&last) = shown.last() {
            self.at_line_start = last == b'\n';
        }
        if let Some(address) = self.watcher.push(shown) {
            self.opening = Some(self.opener.open(&address));
        }
    }

    /// Whether the opener has not answered yet.
    #[cfg(test)]
    pub(crate) fn waiting(&self) -> bool {
        self.opening.is_some()
    }

    /// The line to show (the terminal is raw: `\r\n`), once the opener has
    /// opened the page; nothing when it could not, since the address Codex
    /// printed is still on screen.
    pub(crate) fn note(&mut self) -> Option<Vec<u8>> {
        let opened = match self.opening.as_ref()?.try_recv() {
            Ok(opened) => opened,
            Err(mpsc::TryRecvError::Empty) => return None,
            Err(mpsc::TryRecvError::Disconnected) => false,
        };
        self.opening = None;
        if !opened {
            return None;
        }
        let mut note = Vec::with_capacity(OPENED.len() + 4);
        if !self.at_line_start {
            note.extend_from_slice(b"\r\n");
        }
        note.extend_from_slice(OPENED.as_bytes());
        note.extend_from_slice(b"\r\n");
        self.at_line_start = true;
        Some(note)
    }
}
