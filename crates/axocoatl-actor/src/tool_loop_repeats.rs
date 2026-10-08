//! Notice a tool loop that stopped making progress. In the 1.1.0 eval a solo
//! Agent restated its finished answer through 18 rounds of `bash` `echo "✅ …"`
//! (about 540,000 tokens, no change) until its budget ran out. In the 1.3.0
//! re-smoke two audit workers ran `python3 -c` scripts that printed "final
//! audit confirmation" lines for 39 minutes (80 and 81 `bash` calls), each
//! command a little different, so neither pattern below caught them. Three
//! patterns end the loop: consecutive rounds whose calls only print text,
//! consecutive rounds that repeat the same calls and get the same results,
//! and calls of one kind that keep showing nothing new
//! ([`REPEATED_CALL_LIMIT`] of the last [`REPEAT_WINDOW`] calls with the same
//! tool and arguments, or of the last [`REPEAT_WINDOW`] `bash` commands with
//! the same command pattern). Either way the next request goes without tools
//! and asks for the answer.
//!
//! A read-only Agent (one that can change no file, such as an audit worker)
//! is also watched across kinds: in the 1.3.0 re-smoke audit workers made
//! 60 to 113 `grep` calls, each with different arguments, over files they
//! had already read, so no pattern above caught them. When
//! [`STALE_CALL_LIMIT`] of its last [`STALE_WINDOW`] calls showed nothing
//! new, whatever their tools and arguments, the next request asks once for
//! the final answer and still offers tools ([`ToolLoopRepeats::take_nudge`]);
//! when [`STALE_AFTER_NUDGE_LIMIT`] more calls show nothing new after that,
//! the loop ends like the patterns above.
//!
//! All are deliberately narrow. Reading different files, or running the same
//! test again after an edit (an edit or a write, or a `bash` command that
//! printed nothing, starts the counts of the third pattern again), never
//! counts; neither does polling a terminal, whose output can change without
//! the Agent doing anything.

use axocoatl_core::ToolCallRecord;
use std::collections::{HashMap, HashSet, VecDeque};
use std::hash::{Hash, Hasher};

/// Consecutive rounds whose calls only print text that end the loop.
pub(crate) const NO_OP_ROUND_LIMIT: usize = 3;
/// Consecutive rounds of the same calls with the same results that end it.
pub(crate) const IDENTICAL_ROUND_LIMIT: usize = 4;
/// Calls of one kind that showed nothing new, among the last
/// [`REPEAT_WINDOW`] of that kind, that end it.
pub(crate) const REPEATED_CALL_LIMIT: usize = 8;
/// How many of the latest calls of one kind [`REPEATED_CALL_LIMIT`] counts in.
pub(crate) const REPEAT_WINDOW: usize = 10;
/// Calls of any kind that showed nothing new, among a read-only Agent's
/// last [`STALE_WINDOW`] calls, that ask it once for its answer.
pub(crate) const STALE_CALL_LIMIT: usize = 12;
/// How many of a read-only Agent's latest calls [`STALE_CALL_LIMIT`]
/// counts in.
pub(crate) const STALE_WINDOW: usize = 15;
/// Calls that showed nothing new after that request that end the loop.
pub(crate) const STALE_AFTER_NUDGE_LIMIT: usize = 8;
/// A result shows nothing new when at most one of its lines in this many
/// is new: a line no earlier result of the activation showed and the call's
/// own arguments do not contain (a script that prints its own text).
const NEW_LINE_SHARE: usize = 4;
/// Arguments longer than this are not searched for a result's lines.
const MAX_SEARCHED_ARGUMENT_BYTES: usize = 16 * 1024;
/// Tools whose successful call changes files.
const CHANGING_TOOLS: &[&str] = &["write_file", "edit_file"];

/// Tools that watch something running on its own, so the same call is
/// expected to be repeated while waiting.
const OBSERVATION_TOOLS: &[&str] = &["read_terminal", "list_terminals"];

/// Shell commands with no effect beyond printing (the shell exits after the
/// call, so `cd` changes nothing either).
const NO_OP_COMMANDS: &[&str] = &["echo", "printf", "true", ":", "cd", "pwd"];

/// One kind of call [`REPEATED_CALL_LIMIT`] counts.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum CallKind {
    /// The same tool with the same arguments.
    Same { tool: String, arguments: String },
    /// `bash` commands with the same command pattern ([`command_pattern`]).
    Pattern(String),
}

impl CallKind {
    fn describe(&self) -> String {
        match self {
            Self::Same { tool, .. } => format!("{tool} with the same arguments"),
            Self::Pattern(pattern) => format!("bash `{pattern} …`"),
        }
    }
}

#[derive(Debug, Default)]
pub(crate) struct ToolLoopRepeats {
    last_round: Option<u64>,
    identical_rounds: usize,
    no_op_rounds: usize,
    /// Hashes of every result line the activation's calls have shown.
    seen_lines: HashSet<u64>,
    /// For each kind of call, whether each of its latest calls showed
    /// nothing new, oldest first.
    windows: HashMap<CallKind, VecDeque<bool>>,
    /// Whether the Agent is read-only, so its calls are also counted
    /// across kinds ([`STALE_CALL_LIMIT`]).
    read_only: bool,
    /// Whether each of its latest calls, of any kind, showed nothing new.
    stale: VecDeque<bool>,
    /// Whether it was asked for its answer with tools still offered.
    nudged: bool,
    /// Why the next request asks for it, until taken.
    nudge_due: Option<String>,
    /// Calls that showed nothing new since it was asked.
    stale_after_nudge: usize,
}

impl ToolLoopRepeats {
    /// The guard of an Agent that can change no file: its calls are also
    /// counted across kinds.
    pub(crate) fn read_only() -> Self {
        Self {
            read_only: true,
            ..Self::default()
        }
    }

    /// The same guard with nothing counted yet (new guidance is new work).
    pub(crate) fn fresh(&self) -> Self {
        Self {
            read_only: self.read_only,
            ..Self::default()
        }
    }

    /// Why the next request should ask for the final answer while still
    /// offering tools: once, after [`STALE_CALL_LIMIT`] of a read-only
    /// Agent's last [`STALE_WINDOW`] calls showed nothing new.
    pub(crate) fn take_nudge(&mut self) -> Option<String> {
        self.nudge_due.take()
    }

    /// Record one round's calls and results; returns why the loop should end
    /// once its recent rounds show no progress.
    pub(crate) fn observe(&mut self, round: &[ToolCallRecord]) -> Option<String> {
        if round.is_empty() {
            return None;
        }
        let mut repeated = None;
        for call in round {
            if let Some(reason) = self.observe_call(call) {
                repeated.get_or_insert(reason);
            }
        }
        let fingerprint = round_fingerprint(round);
        let observes = round
            .iter()
            .any(|call| OBSERVATION_TOOLS.contains(&call.tool_name.as_str()));
        self.identical_rounds = if observes {
            0
        } else if self.last_round == Some(fingerprint) {
            self.identical_rounds + 1
        } else {
            1
        };
        self.last_round = Some(fingerprint);
        self.no_op_rounds = if round.iter().all(only_prints) {
            self.no_op_rounds + 1
        } else {
            0
        };
        if self.no_op_rounds >= NO_OP_ROUND_LIMIT {
            return Some(format!(
                "your last {} tool rounds only printed text and changed nothing",
                self.no_op_rounds
            ));
        }
        if self.identical_rounds >= IDENTICAL_ROUND_LIMIT {
            return Some(format!(
                "your last {} tool rounds repeated the same calls and got the same results",
                self.identical_rounds
            ));
        }
        repeated
    }

    /// Count one call toward [`REPEATED_CALL_LIMIT`]; returns why the loop
    /// should end once a kind of call keeps showing nothing new.
    fn observe_call(&mut self, call: &ToolCallRecord) -> Option<String> {
        if OBSERVATION_TOOLS.contains(&call.tool_name.as_str()) {
            return None;
        }
        let lines = call.result.as_ref().map(result_lines).unwrap_or_default();
        let failed = call
            .result
            .as_ref()
            .is_none_or(|result| result.get("error").is_some());
        let changed = (CHANGING_TOOLS.contains(&call.tool_name.as_str()) && !failed)
            || (call.tool_name == "bash" && lines.is_empty());
        if changed {
            // An edit or a write, or a command that printed nothing and may
            // have changed files, is progress: every count starts again.
            self.windows.clear();
            self.stale.clear();
            self.stale_after_nudge = 0;
            self.seen_lines
                .extend(lines.iter().map(|line| line_hash(line)));
            return None;
        }
        let mut arguments = Vec::new();
        string_leaves(&call.arguments, &mut arguments);
        let arguments = arguments.join("\n");
        let searched = arguments.len() <= MAX_SEARCHED_ARGUMENT_BYTES;
        let new = lines
            .iter()
            .filter(|line| {
                let seen = self.seen_lines.contains(&line_hash(line));
                let own = searched && arguments.contains(line.as_str());
                !(seen || own)
            })
            .count();
        let nothing_new = new * NEW_LINE_SHARE <= lines.len();
        self.seen_lines
            .extend(lines.iter().map(|line| line_hash(line)));
        let mut kinds = vec![CallKind::Same {
            tool: call.tool_name.clone(),
            arguments: call.arguments.to_string(),
        }];
        if call.tool_name == "bash" {
            if let Some(pattern) = call
                .arguments
                .get("command")
                .and_then(serde_json::Value::as_str)
                .and_then(command_pattern)
            {
                kinds.push(CallKind::Pattern(pattern));
            }
        }
        let mut reason = None;
        if self.read_only {
            reason = self.observe_stale(nothing_new);
        }
        for kind in kinds {
            let window = self.windows.entry(kind.clone()).or_default();
            window.push_back(nothing_new);
            if window.len() > REPEAT_WINDOW {
                window.pop_front();
            }
            let repeats = window.iter().filter(|nothing| **nothing).count();
            if repeats >= REPEATED_CALL_LIMIT && reason.is_none() {
                reason = Some(format!(
                    "you repeated the same call ({}) {repeats} times without new results",
                    kind.describe()
                ));
            }
        }
        reason
    }

    /// Count one call of a read-only Agent toward [`STALE_CALL_LIMIT`], or,
    /// once it was asked for its answer, toward [`STALE_AFTER_NUDGE_LIMIT`];
    /// returns why the loop should end.
    fn observe_stale(&mut self, nothing_new: bool) -> Option<String> {
        if self.nudged {
            if nothing_new {
                self.stale_after_nudge += 1;
            }
            return (self.stale_after_nudge >= STALE_AFTER_NUDGE_LIMIT).then(|| {
                format!(
                    "after the host asked for your answer, {} more of your tool calls showed \
                     nothing new",
                    self.stale_after_nudge
                )
            });
        }
        self.stale.push_back(nothing_new);
        if self.stale.len() > STALE_WINDOW {
            self.stale.pop_front();
        }
        let stale = self.stale.iter().filter(|nothing| **nothing).count();
        if stale >= STALE_CALL_LIMIT {
            self.nudged = true;
            self.nudge_due = Some(format!(
                "{stale} of your last {} tool calls showed nothing new",
                self.stale.len()
            ));
        }
        None
    }
}

/// The trimmed, non-empty lines of every string a result holds.
fn result_lines(result: &serde_json::Value) -> Vec<String> {
    let mut strings = Vec::new();
    string_leaves(result, &mut strings);
    strings
        .iter()
        .flat_map(|text| text.lines())
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_owned)
        .collect()
}

fn string_leaves<'a>(value: &'a serde_json::Value, out: &mut Vec<&'a str>) {
    match value {
        serde_json::Value::String(text) => out.push(text),
        serde_json::Value::Array(items) => {
            for item in items {
                string_leaves(item, out);
            }
        }
        serde_json::Value::Object(fields) => {
            for item in fields.values() {
                string_leaves(item, out);
            }
        }
        _ => {}
    }
}

fn line_hash(line: &str) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    line.hash(&mut hasher);
    hasher.finish()
}

/// A shell command's pattern: the program it runs (its file name), with its
/// first argument when that is an option (`python3 -c`, `grep -n`). Comment
/// lines, `cd <dir> &&` and variable assignments before it are skipped.
pub(crate) fn command_pattern(command: &str) -> Option<String> {
    let body: Vec<&str> = command
        .lines()
        .map(str::trim)
        .skip_while(|line| line.is_empty() || line.starts_with('#'))
        .collect();
    let body = body.join("\n");
    let words: Vec<&str> = body.split_whitespace().collect();
    let mut at = 0;
    while at < words.len() {
        let word = words[at];
        if word == "cd"
            && words
                .get(at + 2)
                .is_some_and(|next| *next == "&&" || *next == ";")
        {
            at += 3;
        } else if word.split_once('=').is_some_and(|(name, _)| {
            !name.is_empty()
                && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
                && !name.starts_with(|c: char| c.is_ascii_digit())
        }) {
            at += 1;
        } else {
            break;
        }
    }
    let program = words.get(at)?.rsplit('/').next()?;
    if program.is_empty() {
        return None;
    }
    let program: String = program.chars().take(64).collect();
    Some(
        match words.get(at + 1).filter(|word| word.starts_with('-')) {
            Some(option) => {
                let option: String = option.split('=').next()?.chars().take(16).collect();
                format!("{program} {option}")
            }
            None => program,
        },
    )
}

fn round_fingerprint(round: &[ToolCallRecord]) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    for call in round {
        call.tool_name.hash(&mut hasher);
        call.arguments.to_string().hash(&mut hasher);
        call.result
            .as_ref()
            .map(serde_json::Value::to_string)
            .hash(&mut hasher);
    }
    hasher.finish()
}

/// A `bash` call whose command can only print text.
fn only_prints(call: &ToolCallRecord) -> bool {
    call.tool_name == "bash"
        && call
            .arguments
            .get("command")
            .and_then(serde_json::Value::as_str)
            .is_some_and(shell_only_prints)
}

/// Whether a shell command is a list of simple commands (joined by `;`,
/// `&&`, `||` or newlines) that each start with a no-op command, with no
/// redirection, pipe, background job, grouping or command substitution
/// anywhere outside single quotes. Anything unrecognized counts as work.
pub(crate) fn shell_only_prints(command: &str) -> bool {
    let mut chars = command.chars().peekable();
    // Whether the next word starts a simple command, and the current word.
    let mut at_command_start = true;
    let mut word = String::new();
    let mut in_word = false;
    let end_word = |word: &mut String, in_word: &mut bool, at_command_start: &mut bool| {
        if *in_word {
            let ok = !*at_command_start || NO_OP_COMMANDS.contains(&word.as_str());
            *at_command_start = false;
            *in_word = false;
            word.clear();
            ok
        } else {
            true
        }
    };
    while let Some(c) = chars.next() {
        match c {
            '\'' => {
                in_word = true;
                loop {
                    match chars.next() {
                        Some('\'') => break,
                        Some(c) => word.push(c),
                        None => return false,
                    }
                }
            }
            '"' => {
                in_word = true;
                loop {
                    match chars.next() {
                        Some('"') => break,
                        Some('\\') => match chars.next() {
                            Some(c) => word.push(c),
                            None => return false,
                        },
                        Some('`') => return false,
                        Some('$') if chars.peek() == Some(&'(') => return false,
                        Some(c) => word.push(c),
                        None => return false,
                    }
                }
            }
            '\\' => match chars.next() {
                // A line continuation joins lines; anything else is literal.
                Some('\n') => {}
                Some(c) => {
                    in_word = true;
                    word.push(c);
                }
                None => return false,
            },
            ' ' | '\t' => {
                if !end_word(&mut word, &mut in_word, &mut at_command_start) {
                    return false;
                }
            }
            ';' | '\n' => {
                if !end_word(&mut word, &mut in_word, &mut at_command_start) {
                    return false;
                }
                at_command_start = true;
            }
            '&' | '|' => {
                // Only `&&` and `||` join commands; `&` and `|` do work.
                if chars.next() != Some(c)
                    || !end_word(&mut word, &mut in_word, &mut at_command_start)
                {
                    return false;
                }
                at_command_start = true;
            }
            '>' | '<' | '(' | ')' | '{' | '}' | '`' => return false,
            '$' if chars.peek() == Some(&'(') => return false,
            '#' if !in_word => {
                // A comment runs to the end of the line.
                for c in chars.by_ref() {
                    if c == '\n' {
                        break;
                    }
                }
                at_command_start = true;
            }
            c => {
                in_word = true;
                word.push(c);
            }
        }
    }
    end_word(&mut word, &mut in_word, &mut at_command_start)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn call(tool: &str, arguments: serde_json::Value, result: serde_json::Value) -> ToolCallRecord {
        ToolCallRecord {
            tool_name: tool.to_string(),
            arguments,
            result: Some(result),
        }
    }

    fn bash(command: &str) -> ToolCallRecord {
        call(
            "bash",
            json!({ "command": command }),
            json!({"stdout": "ok\n", "exit_code": 0}),
        )
    }

    #[test]
    fn commands_that_only_print_are_recognized() {
        for command in [
            "echo done",
            "cd /workspace/repo && echo \"✅ buildManifest collision detection working\"",
            "echo 'a | b > c; d'",
            "echo \"tests: 8 pass | 0 fail -> ok\"",
            "printf '%s\\n' done; true",
            ":",
            "pwd\necho $HOME",
            "true || echo never  # a comment > here",
            "",
        ] {
            assert!(shell_only_prints(command), "{command:?}");
        }
        for command in [
            "echo x > notes.md",
            "echo x >> notes.md",
            "echo $(rm -rf lib)",
            "echo \"$(rm -rf lib)\"",
            "echo `id`",
            "echo \"`id`\"",
            "cd lib && npm test",
            "echo a | tee b",
            "echo a &",
            "X=1 echo a",
            "sleep 1; echo a",
            "(echo a)",
            "{ echo a; }",
            "cat <<EOF\nx\nEOF",
            "echo \"unterminated",
            "node -e \"console.log(1)\"",
        ] {
            assert!(!shell_only_prints(command), "{command:?}");
        }
    }

    /// The eval's loop: each round restated the answer in a new `echo`.
    #[test]
    fn rounds_that_only_print_end_the_loop() {
        let mut repeats = ToolLoopRepeats::default();
        assert_eq!(repeats.observe(&[bash("npm test")]), None);
        assert_eq!(repeats.observe(&[bash("cd /w && echo \"✅ fixed\"")]), None);
        assert_eq!(
            repeats.observe(&[bash("echo \"✅ fixed, verified\"")]),
            None
        );
        let reason = repeats
            .observe(&[bash("echo 'Final confirmation: fixed'")])
            .unwrap();
        assert_eq!(
            reason,
            "your last 3 tool rounds only printed text and changed nothing"
        );

        // Any real call in between starts the count again. (Run longer, the
        // same echo and the same test showing nothing new would end the loop
        // as repeated calls; see `a_kind_of_call_that_shows_nothing_new_ends_the_loop`.)
        let mut repeats = ToolLoopRepeats::default();
        for _ in 0..2 {
            assert_eq!(repeats.observe(&[bash("echo checking")]), None);
            assert_eq!(repeats.observe(&[bash("echo checking")]), None);
            assert_eq!(
                repeats.observe(&[bash("echo checking"), bash("npm test")]),
                None
            );
        }
    }

    #[test]
    fn the_same_calls_with_the_same_results_end_the_loop() {
        let read = || {
            call(
                "read_file",
                json!({"path": "lib/a.js"}),
                json!({"content": "x"}),
            )
        };
        let mut repeats = ToolLoopRepeats::default();
        for _ in 1..IDENTICAL_ROUND_LIMIT {
            assert_eq!(repeats.observe(&[read()]), None);
        }
        assert_eq!(
            repeats.observe(&[read()]).unwrap(),
            "your last 4 tool rounds repeated the same calls and got the same results"
        );
    }

    #[test]
    fn progress_between_repeats_never_ends_the_loop() {
        let mut repeats = ToolLoopRepeats::default();
        let test = || {
            call(
                "bash",
                json!({"command": "npm test"}),
                json!({"stdout": "fail 4", "exit_code": 1}),
            )
        };
        // Edit, test, edit, test: the same test run each time is progress.
        for index in 0..20 {
            let edit = call(
                "edit_file",
                json!({"path": "lib/a.js", "old": "x", "new": format!("y{index}")}),
                json!({"replaced": 1}),
            );
            assert_eq!(repeats.observe(&[edit]), None);
            assert_eq!(repeats.observe(&[test()]), None);
        }
        // Reading a different file each round.
        for index in 0..20 {
            let read = call(
                "read_file",
                json!({"path": format!("lib/{index}.js")}),
                json!({"content": "x"}),
            );
            assert_eq!(repeats.observe(&[read]), None);
        }
        // The same call whose result changes, such as a growing log.
        for index in 0..20 {
            let tail = call(
                "bash",
                json!({"command": "tail -n 5 build.log"}),
                json!({"stdout": format!("step {index}"), "exit_code": 0}),
            );
            assert_eq!(repeats.observe(&[tail]), None);
        }
        // Polling a terminal while something else runs.
        for _ in 0..20 {
            let poll = call(
                "read_terminal",
                json!({"id": "t1"}),
                json!({"output": "", "running": true}),
            );
            assert_eq!(repeats.observe(&[poll]), None);
        }
    }

    fn printed(command: &str, stdout: &str) -> ToolCallRecord {
        call(
            "bash",
            json!({ "command": command }),
            json!({"stdout": stdout, "stderr": "", "exit_code": 0}),
        )
    }

    /// The 1.3.0 re-smoke's audit workers: `python3 -c` scripts, each a
    /// little different, printing their own "final confirmation" lines.
    #[test]
    fn a_kind_of_call_that_shows_nothing_new_ends_the_loop() {
        let mut repeats = ToolLoopRepeats::default();
        let read = call(
            "read_file",
            json!({"path": "auth/tokens.py"}),
            json!({"content": "import hmac\n\ndef is_valid(signature, expected):\n    return signature != expected\n"}),
        );
        assert_eq!(repeats.observe(&[read]), None);
        let mut ended = None;
        for index in 0..20 {
            let lines = [
                format!("=== FINAL AUDIT CONFIRMATION {index} ==="),
                "Area: auth".to_string(),
                "File: auth/tokens.py".to_string(),
                "Location: Line 4".to_string(),
                "return signature != expected".to_string(),
            ];
            let script: String = lines
                .iter()
                .map(|line| format!("print('{line}')\n"))
                .collect();
            let command = format!("python3 -c \"\n# Final audit confirmation\n{script}\"");
            let stdout = lines.join("\n") + "\n";
            if let Some(reason) = repeats.observe(&[printed(&command, &stdout)]) {
                ended = Some((index, reason));
                break;
            }
        }
        let (index, reason) = ended.expect("the loop ends");
        assert_eq!(index, REPEATED_CALL_LIMIT - 1, "{reason}");
        assert_eq!(
            reason,
            "you repeated the same call (bash `python3 -c …`) 8 times without new results"
        );

        // The same two calls in turn: no two rounds alike, nothing new.
        let mut repeats = ToolLoopRepeats::default();
        let file = || {
            call(
                "read_file",
                json!({"path": "billing/pagination.py"}),
                json!({"content": "def get_page(items, page):\n    return items[page:page + 11]\n"}),
            )
        };
        let listing = || {
            call(
                "list_dir",
                json!({"path": "billing"}),
                json!({"listing": "pagination.py\n__init__.py\n"}),
            )
        };
        let mut calls = 0;
        let reason = loop {
            calls += 1;
            let round = if calls % 2 == 1 { file() } else { listing() };
            if let Some(reason) = repeats.observe(&[round]) {
                break reason;
            }
            assert!(calls < 40, "the alternation never ended");
        };
        // read_file's ninth call: its first showed the file.
        assert_eq!(calls, 17);
        assert_eq!(
            reason,
            "you repeated the same call (read_file with the same arguments) 8 times without new \
             results"
        );
    }

    #[test]
    fn new_results_and_changes_keep_a_kind_of_call_going() {
        // Each run of the same test after an edit, even with the same output.
        let mut repeats = ToolLoopRepeats::default();
        for index in 0..30 {
            let edit = call(
                "edit_file",
                json!({"path": "lib/a.js", "old": "x", "new": format!("y{index}")}),
                json!({"replaced": 1}),
            );
            assert_eq!(repeats.observe(&[edit]), None);
            assert_eq!(repeats.observe(&[printed("npm test", "fail 4\n")]), None);
            assert_eq!(
                repeats.observe(&[printed("grep -n x lib/a.js", "3:x\n")]),
                None
            );
        }
        // A command that printed nothing may have changed files.
        let mut repeats = ToolLoopRepeats::default();
        for index in 0..30 {
            let sed = format!("sed -i 's/a/b{index}/' lib/a.js");
            assert_eq!(repeats.observe(&[printed(&sed, "")]), None);
            assert_eq!(repeats.observe(&[printed("npm test", "fail 4\n")]), None);
        }
        // The same kind of command, each showing something new.
        let mut repeats = ToolLoopRepeats::default();
        for index in 0..30 {
            let command = format!("grep -n 'needle{index}' -r src");
            let stdout = format!("src/file{index}.rs:12: needle{index} in place\n");
            assert_eq!(repeats.observe(&[printed(&command, &stdout)]), None);
        }
        // A computed result is new even when the script names it.
        let mut repeats = ToolLoopRepeats::default();
        for index in 0..30 {
            let command = format!("python3 -c \"print(sum(range({index})))\"");
            let stdout = format!("{}\n", (0..index).sum::<usize>() + 1_000_000);
            assert_eq!(repeats.observe(&[printed(&command, &stdout)]), None);
        }
    }

    /// A recorded activation's tool calls, grouped into its rounds.
    fn recorded_rounds(name: &str) -> Vec<Vec<ToolCallRecord>> {
        let path = format!(
            "{}/../axocoatl-session/tests/fixtures/answers/{name}",
            env!("CARGO_MANIFEST_DIR")
        );
        let text = std::fs::read_to_string(&path).unwrap_or_else(|error| panic!("{path}: {error}"));
        let mut rounds: Vec<(u64, Vec<ToolCallRecord>)> = Vec::new();
        for line in text.lines() {
            let call: serde_json::Value = serde_json::from_str(line).unwrap();
            let round = call["round"].as_u64().unwrap();
            let record = ToolCallRecord {
                tool_name: call["tool"].as_str().unwrap().to_owned(),
                arguments: call["arguments"].clone(),
                result: Some(call["result"].clone()),
            };
            match rounds.last_mut() {
                Some((last, calls)) if *last == round => calls.push(record),
                _ => rounds.push((round, vec![record])),
            }
        }
        rounds.into_iter().map(|(_, calls)| calls).collect()
    }

    /// After how many calls something happened, and why.
    type At = Option<(usize, String)>;

    /// Feed `rounds` to `repeats`; returns after how many calls the nudge
    /// came and after how many the loop ended, with why.
    fn replay(repeats: &mut ToolLoopRepeats, rounds: &[Vec<ToolCallRecord>]) -> (At, At) {
        let (mut calls, mut nudged, mut ended) = (0, None, None);
        for round in rounds {
            calls += round.len();
            if let Some(reason) = repeats.observe(round) {
                ended.get_or_insert((calls, reason));
            }
            if let Some(reason) = repeats.take_nudge() {
                assert!(nudged.is_none(), "one nudge");
                nudged = Some((calls, reason));
            }
            if ended.is_some() {
                break;
            }
        }
        (nudged, ended)
    }

    /// resmoke10 out3's auth worker: one 671-byte file, read at its second
    /// call, then 113 `grep` calls, each with different arguments. The
    /// patterns above ended it only at its 127th call; read-only, it is
    /// asked for its answer after its 22nd and ended after its 31st. Its
    /// finding (the timing compare) was in the read at call 2.
    #[test]
    fn a_read_only_agent_whose_calls_of_any_kind_show_nothing_new_is_asked_then_ended() {
        let rounds = recorded_rounds("audit-resmoke10-out3-worker-auth-calls.jsonl");
        assert_eq!(rounds.iter().map(Vec::len).sum::<usize>(), 127);
        let (nudged, ended) = replay(&mut ToolLoopRepeats::read_only(), &rounds);
        assert_eq!(
            nudged,
            Some((
                22,
                "12 of your last 15 tool calls showed nothing new".to_owned()
            ))
        );
        assert_eq!(
            ended,
            Some((
                31,
                "after the host asked for your answer, 8 more of your tool calls showed nothing \
                 new"
                .to_owned()
            ))
        );
        let (nudged, ended) = replay(&mut ToolLoopRepeats::default(), &rounds);
        assert_eq!(nudged, None, "only a read-only Agent is nudged");
        assert_eq!(
            ended,
            Some((
                127,
                "you repeated the same call (read_file with the same arguments) 8 times without \
                 new results"
                    .to_owned()
            ))
        );
    }

    /// resmoke10 out4's planner: 50 calls across the repository (listings,
    /// globs, reads, four of them failing), most of them showing something
    /// new. It is never asked.
    #[test]
    fn a_read_only_agent_exploring_is_never_asked() {
        let rounds = recorded_rounds("audit-resmoke10-out4-planner-calls.jsonl");
        assert_eq!(rounds.iter().map(Vec::len).sum::<usize>(), 50);
        assert_eq!(
            replay(&mut ToolLoopRepeats::read_only(), &rounds),
            (None, None)
        );
    }

    #[test]
    fn a_change_starts_the_read_only_counts_again() {
        let grep = |index: usize| {
            call(
                "grep",
                json!({"pattern": format!("p{index}"), "path": "lib"}),
                json!({"matches": "lib/a.js:1:x"}),
            )
        };
        // Eleven of twelve stale, then a change: the window starts again.
        let mut repeats = ToolLoopRepeats::read_only();
        for index in 0..12 {
            assert_eq!(repeats.observe(&[grep(index)]), None);
        }
        assert_eq!(repeats.take_nudge(), None);
        assert_eq!(
            repeats.observe(&[printed("sed -i s/x/y/ lib/a.js", "")]),
            None
        );
        let read = call(
            "read_file",
            json!({"path": "lib/a.js"}),
            json!({"content": "y"}),
        );
        assert_eq!(repeats.observe(&[read]), None);
        for index in 0..11 {
            assert_eq!(repeats.observe(&[grep(index)]), None);
            assert_eq!(repeats.take_nudge(), None);
        }
        assert_eq!(repeats.observe(&[grep(11)]), None);
        assert_eq!(
            repeats.take_nudge().as_deref(),
            Some("12 of your last 13 tool calls showed nothing new")
        );
        assert_eq!(repeats.take_nudge(), None, "asked once");
        // After the request, a call that shows something new does not count.
        for index in 0..7 {
            assert_eq!(repeats.observe(&[grep(index)]), None);
        }
        let new = call(
            "read_file",
            json!({"path": "lib/b.js"}),
            json!({"content": "something new"}),
        );
        assert_eq!(repeats.observe(&[new]), None);
        assert!(repeats.observe(&[grep(7)]).is_some());
        // The same guard without counting: what `fresh` keeps.
        assert!(repeats.fresh().read_only);
        assert!(!ToolLoopRepeats::default().fresh().read_only);
    }

    #[test]
    fn command_patterns_name_the_program_and_its_first_option() {
        for (command, pattern) in [
            ("python3 -c \"print(1)\"", Some("python3 -c")),
            ("/usr/bin/python3 -c 'x'", Some("python3 -c")),
            (
                "# Final check\npython3 -c \"\nprint(1)\"",
                Some("python3 -c"),
            ),
            ("cd /workspace/repo && grep -n x lib", Some("grep -n")),
            ("LC_ALL=C sort -u list", Some("sort -u")),
            ("npm test", Some("npm")),
            ("git --no-pager log", Some("git --no-pager")),
            ("grep --include=*.py -r x .", Some("grep --include")),
            ("", None),
            ("# only a comment", None),
        ] {
            assert_eq!(command_pattern(command).as_deref(), pattern, "{command:?}");
        }
    }
}
