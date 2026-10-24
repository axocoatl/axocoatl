//! How onboarding asks: one question per change to the machine, answered
//! at the terminal, by `--yes`, or not at all with `--plan`, which only says
//! what it would do.

#[cfg(test)]
use std::collections::VecDeque;
use std::sync::Arc;
#[cfg(test)]
use std::sync::Mutex;

use super::host::{Cmd, Host, Log};

/// How onboarding is answered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Mode {
    /// A person answers at the terminal.
    Interactive,
    /// `--yes`: every question is answered yes; sudo also needs
    /// `--allow-sudo`; the Claude Code and Codex sign-ins are skipped unless
    /// `--connect`, and OpenRouter's browser sign-in always is.
    Yes,
    /// `--plan`: nothing changes; every action is printed instead.
    Plan,
}

/// A scripted answer, for tests of the interactive path.
#[cfg(test)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Answer {
    Yes,
    No,
    Pick(usize),
    Text(String),
    Enter,
}

pub(crate) struct Ui {
    pub(crate) mode: Mode,
    pub(crate) allow_sudo: bool,
    pub(crate) log: Option<Arc<Log>>,
    /// Everything printed, for tests.
    #[cfg(test)]
    pub(crate) printed: Mutex<Vec<String>>,
    #[cfg(test)]
    pub(crate) answers: Mutex<VecDeque<Answer>>,
}

impl Ui {
    pub(crate) fn new(mode: Mode, allow_sudo: bool, log: Option<Arc<Log>>) -> Self {
        Self {
            mode,
            allow_sudo,
            log,
            #[cfg(test)]
            printed: Mutex::new(Vec::new()),
            #[cfg(test)]
            answers: Mutex::new(VecDeque::new()),
        }
    }

    /// An interactive Ui answered by `answers`, in order.
    #[cfg(test)]
    pub(crate) fn scripted(answers: Vec<Answer>) -> Self {
        let ui = Self::new(Mode::Interactive, false, None);
        *ui.answers.lock().unwrap() = answers.into();
        ui
    }

    #[cfg(test)]
    fn next_answer(&self) -> Option<Answer> {
        self.answers.lock().unwrap().pop_front()
    }

    #[cfg(test)]
    pub(crate) fn output(&self) -> String {
        self.printed.lock().unwrap().join("\n")
    }

    pub(crate) fn plan(&self) -> bool {
        self.mode == Mode::Plan
    }

    pub(crate) fn interactive(&self) -> bool {
        self.mode == Mode::Interactive
    }

    /// Print a line (and log it).
    pub(crate) fn say(&self, text: impl AsRef<str>) {
        let text = text.as_ref();
        println!("{text}");
        if let Some(log) = &self.log {
            log.line(text);
        }
        #[cfg(test)]
        self.printed.lock().unwrap().push(text.to_string());
    }

    pub(crate) fn heading(&self, text: &str) {
        self.say(format!("\n{text}"));
    }

    pub(crate) fn ok(&self, text: impl AsRef<str>) {
        self.say(format!("  ✓ {}", text.as_ref()));
    }

    pub(crate) fn warn(&self, text: impl AsRef<str>) {
        self.say(format!("  ! {}", text.as_ref()));
    }

    pub(crate) fn fail(&self, text: impl AsRef<str>) {
        self.say(format!("  ✗ {}", text.as_ref()));
    }

    /// Ask before a change to the machine. `question` shows the exact
    /// command, what it downloads or costs, and what else it affects;
    /// `default` is what Enter answers. With `--yes`, a change that needs
    /// sudo is made only with `--allow-sudo`.
    pub(crate) fn consent(&self, question: &str, default: bool, sudo: bool) -> bool {
        let choices = if default { "[Y/n]" } else { "[y/N]" };
        match self.mode {
            Mode::Plan => {
                self.say(format!("  would ask: {question} {choices}"));
                default
            }
            Mode::Yes => {
                if sudo && !self.allow_sudo {
                    self.say(format!(
                        "  {question}\n    → not done: it needs sudo, and --yes runs sudo only with \
                         --allow-sudo"
                    ));
                    false
                } else {
                    self.say(format!("  {question} yes (--yes)"));
                    true
                }
            }
            Mode::Interactive => {
                if let Some(log) = &self.log {
                    log.line(&format!("? {question} {choices}"));
                }
                #[cfg(test)]
                if let Some(answer) = self.next_answer() {
                    self.printed
                        .lock()
                        .unwrap()
                        .push(format!("? {question} {choices} {answer:?}"));
                    return match answer {
                        Answer::Yes => true,
                        Answer::No => false,
                        Answer::Enter => default,
                        other => panic!("{question}: expected yes or no, got {other:?}"),
                    };
                }
                // "[Y/n] " while it asks, "yes" once answered.
                let answer = dialoguer::Confirm::new()
                    .with_prompt(fit_prompt(question, 7))
                    .default(default)
                    .interact()
                    .unwrap_or(false);
                if let Some(log) = &self.log {
                    log.line(&format!("  answered {}", if answer { "yes" } else { "no" }));
                }
                answer
            }
        }
    }

    /// Choose one of `items`; `--yes` and `--plan` take `default`.
    pub(crate) fn select(&self, prompt: &str, items: &[String], default: usize) -> usize {
        match self.mode {
            Mode::Plan | Mode::Yes => {
                self.say(format!(
                    "  {prompt}: {}",
                    items.get(default).map(String::as_str).unwrap_or_default()
                ));
                default
            }
            Mode::Interactive => {
                #[cfg(test)]
                if let Some(answer) = self.next_answer() {
                    let index = match answer {
                        Answer::Pick(index) => index,
                        Answer::Enter => default,
                        other => panic!("{prompt}: expected a pick, got {other:?}"),
                    };
                    self.printed
                        .lock()
                        .unwrap()
                        .push(format!("? {prompt} → {}", items[index]));
                    return index;
                }
                let term = dialoguer::console::Term::stderr();
                let columns = term.size_checked().map(|(_, columns)| usize::from(columns));
                let (listed, shown) = fit_items(items, columns);
                let cut = !listed.is_empty();
                for row in listed {
                    let _ = term.write_line(&row);
                }
                let index = dialoguer::Select::new()
                    .with_prompt(fit_prompt(prompt, 1))
                    .items(&shown)
                    .default(default)
                    .report(!cut)
                    .interact()
                    .unwrap_or(default);
                // The menu showed the items cut to fit: the answer is said
                // whole, once the menu is gone.
                if cut {
                    let _ = term.write_line(&format!("{prompt}: {}", items[index]));
                }
                if let Some(log) = &self.log {
                    log.line(&format!("? {prompt} → {}", items[index]));
                }
                index
            }
        }
    }

    /// A line of text; `--yes` and `--plan` take `default`. An empty
    /// `default` offers none: Enter then answers with an empty line.
    pub(crate) fn input(&self, prompt: &str, default: &str) -> String {
        match self.mode {
            Mode::Plan | Mode::Yes => {
                self.say(format!("  {prompt}: {default}"));
                default.to_string()
            }
            Mode::Interactive => {
                #[cfg(test)]
                if let Some(answer) = self.next_answer() {
                    return match answer {
                        Answer::Text(text) => text,
                        Answer::Enter => default.to_string(),
                        other => panic!("{prompt}: expected text, got {other:?}"),
                    };
                }
                // ": " and the default in brackets while it asks.
                let mut input = dialoguer::Input::<String>::new()
                    .with_prompt(fit_prompt(prompt, default.chars().count() + 5));
                input = if default.is_empty() {
                    input.allow_empty(true)
                } else {
                    input.default(default.to_string())
                };
                let text = input
                    .interact_text()
                    .unwrap_or_else(|_| default.to_string());
                if let Some(log) = &self.log {
                    log.line(&format!("? {prompt} → {text}"));
                }
                text.trim().to_string()
            }
        }
    }

    /// A secret typed at the terminal, never shown or logged; `None` without
    /// a terminal or when left empty.
    pub(crate) fn secret(&self, prompt: &str) -> Option<zeroize::Zeroizing<String>> {
        if self.mode != Mode::Interactive {
            return None;
        }
        #[cfg(test)]
        if let Some(answer) = self.next_answer() {
            return match answer {
                Answer::Text(text) if !text.is_empty() => Some(zeroize::Zeroizing::new(text)),
                _ => None,
            };
        }
        // ": [hidden]" once answered.
        dialoguer::Password::new()
            .with_prompt(fit_prompt(prompt, 10))
            .allow_empty_password(true)
            .interact()
            .ok()
            .filter(|key| !key.trim().is_empty())
            .map(|key| zeroize::Zeroizing::new(key.trim().to_string()))
    }

    /// Wait for the person to do something by hand and press Enter. `false`
    /// without a terminal: the step stays left.
    pub(crate) fn wait_for_enter(&self, instruction: &str) -> bool {
        match self.mode {
            Mode::Plan => {
                self.say(format!("  would ask you to: {instruction}"));
                false
            }
            Mode::Yes => false,
            Mode::Interactive => {
                self.say(format!("  {instruction}"));
                #[cfg(test)]
                if let Some(answer) = self.next_answer() {
                    return answer == Answer::Enter || answer == Answer::Yes;
                }
                dialoguer::Input::<String>::new()
                    .with_prompt("Press Enter when done")
                    .allow_empty(true)
                    .interact_text()
                    .is_ok()
            }
        }
    }

    /// Run a change after consent; with `--plan`, only say so.
    pub(crate) async fn execute(&self, host: &Host, cmd: &Cmd) -> Result<(), String> {
        if self.plan() {
            self.say(format!("    would run: {}", cmd.display()));
            return Ok(());
        }
        // Never sudo under --yes without --allow-sudo, whatever asked.
        if cmd.uses_sudo() && self.mode == Mode::Yes && !self.allow_sudo {
            return Err(format!(
                "`{}` needs sudo, which --yes runs only with --allow-sudo",
                cmd.display()
            ));
        }
        if let Some(machine) = &cmd.starts_machine {
            if let Some(socket) = host.clear_stale_machine_socket(machine).await {
                self.say(format!(
                    "  Removed {}, the API socket {machine} left when it did not stop cleanly \
                     (Podman would not start it while it was there)",
                    socket.display()
                ));
            }
        }
        self.say(format!("  $ {}", cmd.display()));
        host.run(cmd).await
    }

    /// [`Ui::execute`], returning the end of what the command printed (empty
    /// with `--plan`).
    pub(crate) async fn execute_showing_output(
        &self,
        host: &Host,
        cmd: &Cmd,
    ) -> Result<String, String> {
        if self.plan() {
            self.say(format!("    would run: {}", cmd.display()));
            return Ok(String::new());
        }
        if cmd.uses_sudo() && self.mode == Mode::Yes && !self.allow_sudo {
            return Err(format!(
                "`{}` needs sudo, which --yes runs only with --allow-sudo",
                cmd.display()
            ));
        }
        self.say(format!("  $ {}", cmd.display()));
        host.run_showing_output(cmd)
            .await
            .map_err(|(error, why)| match why.is_empty() {
                true => error,
                false => format!("{error}: {why}"),
            })
    }

    /// Run several changes in order, stopping at the first that fails.
    pub(crate) async fn execute_all(&self, host: &Host, cmds: &[Cmd]) -> Result<(), String> {
        for cmd in cmds {
            self.execute(host, cmd).await?;
        }
        Ok(())
    }
}

/// `a && b` for a question.
pub(crate) fn joined(cmds: &[Cmd]) -> String {
    cmds.iter()
        .map(Cmd::display)
        .collect::<Vec<_>>()
        .join(" && ")
}

/// The prompt for a dialoguer question, after printing all but its last
/// row. dialoguer redraws an answered prompt by clearing only the row the
/// cursor is on, so a prompt wider than the terminal left its first rows
/// on the screen and printed them again. `reserve` is the room the prompt's
/// row needs after it (the `[Y/n] ` of a question). Without a terminal size
/// the prompt is returned whole. Every dialoguer prompt goes through it
/// (`every_long_prompt_is_fitted_to_the_terminal`).
pub(crate) fn fit_prompt(prompt: &str, reserve: usize) -> String {
    let term = dialoguer::console::Term::stderr();
    let columns = term.size_checked().map(|(_, columns)| usize::from(columns));
    let (rows, last) = prompt_rows(prompt, reserve, columns);
    for row in rows {
        let _ = term.write_line(&row);
    }
    last
}

/// [`fit_prompt`] for a terminal `columns` wide: the rows to print first,
/// and the last row, which dialoguer shows with `reserve` after it.
fn prompt_rows(prompt: &str, reserve: usize, columns: Option<usize>) -> (Vec<String>, String) {
    let Some(columns) = columns else {
        return (Vec::new(), prompt.to_string());
    };
    let mut rows = wrap_rows(prompt, columns.saturating_sub(reserve + 1));
    let last = rows.pop().unwrap_or_default();
    (rows, last)
}

/// The items of a menu as a terminal `columns` wide shows them, and the
/// rows to print before it. dialoguer redraws a menu by clearing the rows
/// it counts, one per item (it counts a wrapped item short), so an item
/// wider than the terminal left stale rows above the answer. When every
/// item fits, they are shown as they are; else each is printed whole,
/// numbered and wrapped, and the menu shows the numbered items cut to fit.
/// Without a terminal size the items are shown whole.
pub(crate) fn fit_items(items: &[String], columns: Option<usize>) -> (Vec<String>, Vec<String>) {
    let Some(columns) = columns else {
        return (Vec::new(), items.to_vec());
    };
    // "> " before an item, and the last column left free.
    let room = columns.saturating_sub(3).max(20);
    if items.iter().all(|item| item.chars().count() <= room) {
        return (Vec::new(), items.to_vec());
    }
    let mut listed = Vec::new();
    let mut shown = Vec::new();
    for (index, item) in items.iter().enumerate() {
        let number = format!("{}. ", index + 1);
        for (row, text) in wrap_rows(item, columns.saturating_sub(number.len() + 3))
            .into_iter()
            .enumerate()
        {
            let lead = if row == 0 {
                number.clone()
            } else {
                " ".repeat(number.len())
            };
            listed.push(format!("  {lead}{text}"));
        }
        let whole = format!("{number}{item}");
        shown.push(if whole.chars().count() <= room {
            whole
        } else {
            let cut: String = whole.chars().take(room.saturating_sub(1)).collect();
            format!("{}…", cut.trim_end())
        });
    }
    (listed, shown)
}

/// `text` in rows of at most `width` characters (at least 20), broken
/// between words; a word longer than a row is split.
pub(crate) fn wrap_rows(text: &str, width: usize) -> Vec<String> {
    let width = width.max(20);
    let mut rows = Vec::new();
    for paragraph in text.split('\n') {
        let mut row = String::new();
        for word in paragraph.split(' ').filter(|word| !word.is_empty()) {
            let mut word = word.to_string();
            loop {
                let used = row.chars().count();
                let length = word.chars().count();
                let gap = usize::from(used > 0);
                if used + gap + length <= width {
                    if gap == 1 {
                        row.push(' ');
                    }
                    row.push_str(&word);
                    break;
                }
                if used > 0 {
                    rows.push(std::mem::take(&mut row));
                    continue;
                }
                // A word longer than a whole row: split it.
                let head: String = word.chars().take(width).collect();
                word = word.chars().skip(width).collect();
                rows.push(head);
            }
        }
        rows.push(row);
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn yes_answers_every_question_but_sudo_needs_allow_sudo() {
        let ui = Ui::new(Mode::Yes, false, None);
        assert!(ui.consent(
            "Install Podman now with `brew install podman`?",
            true,
            false
        ));
        assert!(ui.consent("Switch it to rootless?", false, false));
        assert!(!ui.consent(
            "Install Podman with `sudo apt-get install -y podman`?",
            true,
            true
        ));
        assert!(ui.output().contains("--allow-sudo"));
        let ui = Ui::new(Mode::Yes, true, None);
        assert!(ui.consent("Install with sudo?", true, true));
        assert_eq!(ui.select("Who writes", &["a".into(), "b".into()], 1), 1);
        assert_eq!(ui.input("Model", "qwen/qwen3-coder"), "qwen/qwen3-coder");
        assert!(ui.secret("Key").is_none());
        assert!(!ui.wait_for_enter("Open Ollama"));
    }

    /// A prompt wider than the terminal is broken between words into rows
    /// that fit, so dialoguer only ever redraws the last one.
    #[test]
    fn a_long_prompt_is_broken_into_rows_that_fit() {
        let question = "Check the sandbox with one throwaway container? This pulls \
                        docker.io/library/alpine:3.20 (about 4 MB), the image Sessions start \
                        from, and runs it rootless with your user mapped in and no network.";
        for width in [40, 72, 152] {
            let rows = wrap_rows(question, width);
            assert!(rows.len() > 1, "{width}: {rows:?}");
            assert!(
                rows.iter().all(|row| row.chars().count() <= width),
                "{width}: {rows:?}"
            );
            assert_eq!(rows.join(" "), question);
        }
        assert_eq!(wrap_rows(question, 400), [question]);
        assert_eq!(wrap_rows("Install Podman?", 80), ["Install Podman?"]);
        // A word longer than a row (a path) is split, never wider.
        let path = format!("Write {}? yes", "/x".repeat(40));
        let rows = wrap_rows(&path, 30);
        assert!(rows.iter().all(|row| row.chars().count() <= 30), "{rows:?}");
        assert_eq!(rows.concat().replace(' ', ""), path.replace(' ', ""));
    }

    /// A question wider than the terminal leaves only its last row to
    /// dialoguer, with room for `[Y/n] `: `axocoatl connect openrouter`'s
    /// questions too.
    #[test]
    fn a_long_question_leaves_dialoguer_one_row_that_fits() {
        use std::time::Duration;
        for question in [
            crate::connect_cmd::openrouter::keep_waiting_question(Duration::from_secs(600)),
            crate::connect_cmd::openrouter::BYOK_QUESTION.to_string(),
        ] {
            for columns in [60, 80, 100] {
                let (rows, last) = prompt_rows(&question, 7, Some(columns));
                assert!(!rows.is_empty(), "{columns}: {question}");
                assert!(last.chars().count() + 7 < columns, "{columns}: {last}");
                assert!(
                    rows.iter().all(|row| row.chars().count() < columns),
                    "{columns}: {rows:?}"
                );
                assert_eq!(format!("{} {last}", rows.join(" ")), question);
            }
            assert_eq!(
                prompt_rows(&question, 7, None),
                (Vec::new(), question.clone())
            );
            assert_eq!(prompt_rows(&question, 7, Some(400)), (Vec::new(), question));
        }
    }

    /// Every dialoguer prompt in the CLI is fitted to the terminal
    /// ([`fit_prompt`]), but a short fixed one: a prompt that wraps is
    /// printed twice when dialoguer redraws it.
    #[test]
    fn every_long_prompt_is_fitted_to_the_terminal() {
        fn sources(directory: &std::path::Path, found: &mut Vec<std::path::PathBuf>) {
            for entry in std::fs::read_dir(directory).unwrap().flatten() {
                let path = entry.path();
                if path.is_dir() {
                    sources(&path, found);
                } else if path.extension().is_some_and(|extension| extension == "rs") {
                    found.push(path);
                }
            }
        }
        let mut files = Vec::new();
        sources(
            &std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src"),
            &mut files,
        );
        // Not written whole, so this test is not one of the calls.
        let call = concat!(".with", "_prompt(");
        let mut prompts = 0;
        let mut unfitted = Vec::new();
        for file in files {
            let source = std::fs::read_to_string(&file).unwrap();
            let joined: String = source.split_whitespace().collect::<Vec<_>>().join(" ");
            for (index, _) in joined.match_indices(call) {
                prompts += 1;
                let argument = &joined[index + call.len()..];
                let short = argument.starts_with('"')
                    && argument[1..]
                        .find('"')
                        .is_some_and(|end| end < 40 && argument[1 + end..].starts_with("\")"));
                if !argument.starts_with("fit_prompt(") && !short {
                    let shown: String = argument.chars().take(60).collect();
                    unfitted.push(format!("{}: {shown}", file.display()));
                }
            }
        }
        assert!(prompts >= 6, "the scan found {prompts} prompts");
        assert!(
            unfitted.is_empty(),
            "prompts not fitted:\n{}",
            unfitted.join("\n")
        );
    }

    /// A menu whose items are wider than the terminal shows each in full,
    /// numbered and wrapped, then a menu of the numbered items cut to fit,
    /// so dialoguer, which counts one row per item, never leaves a stale
    /// row; one that fits is shown as it is.
    #[test]
    fn menu_items_wider_than_the_terminal_are_listed_and_cut_to_fit() {
        let items: Vec<String> = [
            "Claude Code (your Claude Pro or Max plan; claude found at \
             /Users/alex/.local/bin/claude)",
            "Codex (your ChatGPT plan, or an OpenAI API key)",
            "A hosted model (OpenRouter credits)",
        ]
        .map(String::from)
        .to_vec();
        let (listed, shown) = fit_items(&items, Some(60));
        assert!(
            listed.iter().all(|row| row.chars().count() <= 60),
            "{listed:?}"
        );
        assert!(
            shown.iter().all(|item| item.chars().count() <= 57),
            "{shown:?}"
        );
        assert_eq!(
            listed[0],
            "  1. Claude Code (your Claude Pro or Max plan; claude found"
        );
        assert_eq!(
            listed
                .join(" ")
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" "),
            "1. Claude Code (your Claude Pro or Max plan; claude found at \
             /Users/alex/.local/bin/claude) 2. Codex (your ChatGPT plan, or an OpenAI API \
             key) 3. A hosted model (OpenRouter credits)"
        );
        assert_eq!(
            shown,
            [
                "1. Claude Code (your Claude Pro or Max plan; claude foun…",
                "2. Codex (your ChatGPT plan, or an OpenAI API key)",
                "3. A hosted model (OpenRouter credits)",
            ]
        );
        // Wide enough, or no terminal size: as they are, nothing listed.
        assert_eq!(fit_items(&items, Some(160)), (Vec::new(), items.clone()));
        assert_eq!(fit_items(&items, None), (Vec::new(), items));
    }

    #[tokio::test]
    async fn plan_mode_runs_nothing_and_says_what_it_would() {
        let shims = super::super::host::testing::Shims::new();
        shims.add("brew", "exit 0");
        let host = shims.host(true);
        let ui = Ui::new(Mode::Plan, false, None);
        assert!(ui.consent("Install Podman?", true, false));
        assert!(!ui.consent("Switch it to rootless?", false, false));
        ui.execute(&host, &Cmd::new("brew", &["install", "podman"]))
            .await
            .unwrap();
        assert!(shims.calls().is_empty());
        let output = ui.output();
        assert!(
            output.contains("would ask: Install Podman? [Y/n]"),
            "{output}"
        );
        assert!(
            output.contains("would run: brew install podman"),
            "{output}"
        );
    }
}
