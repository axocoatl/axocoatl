//! Notice a tool loop that stopped making progress. In the 1.1.0 eval a solo
//! Agent restated its finished answer through 18 rounds of `bash` `echo "✅ …"`
//! (about 540,000 tokens, no change) until its budget ran out. Two patterns
//! end the loop: consecutive rounds whose calls only print text, and
//! consecutive rounds that repeat the same calls and get the same results.
//! Either way the next request goes without tools and asks for the answer.
//!
//! Both are deliberately narrow. Reading different files, or running the same
//! test again after an edit (the edit is a different round in between), never
//! counts; neither does polling a terminal, whose output can change without
//! the Agent doing anything.

use axocoatl_core::ToolCallRecord;
use std::hash::{Hash, Hasher};

/// Consecutive rounds whose calls only print text that end the loop.
pub(crate) const NO_OP_ROUND_LIMIT: usize = 3;
/// Consecutive rounds of the same calls with the same results that end it.
pub(crate) const IDENTICAL_ROUND_LIMIT: usize = 4;

/// Tools that watch something running on its own, so the same call is
/// expected to be repeated while waiting.
const OBSERVATION_TOOLS: &[&str] = &["read_terminal", "list_terminals"];

/// Shell commands with no effect beyond printing (the shell exits after the
/// call, so `cd` changes nothing either).
const NO_OP_COMMANDS: &[&str] = &["echo", "printf", "true", ":", "cd", "pwd"];

#[derive(Debug, Default)]
pub(crate) struct ToolLoopRepeats {
    last_round: Option<u64>,
    identical_rounds: usize,
    no_op_rounds: usize,
}

impl ToolLoopRepeats {
    /// Record one round's calls and results; returns why the loop should end
    /// once its recent rounds show no progress.
    pub(crate) fn observe(&mut self, round: &[ToolCallRecord]) -> Option<String> {
        if round.is_empty() {
            return None;
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
        None
    }
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

        // Any real call in between starts the count again.
        let mut repeats = ToolLoopRepeats::default();
        for _ in 0..5 {
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
}
