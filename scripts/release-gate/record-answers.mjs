#!/usr/bin/env node
// The answers a run accepted, read from its record, for the new-user gate's
// check that no accepted output holds tool-call markup a model wrote as text
// (`<function=` or `</tool_call>`; lib.sh gate_markup_check,
// docs/RELEASE_GATE.md). It reads the record file alone: no daemon, no
// token.
//
// The record must be whole: its end line counts the section lines and holds
// the SHA-256 of every byte before it, as `axocoatl record verify` checks
// (crates/axocoatl-session/src/record_bundle.rs).
//
// A record's `history` section is the run's Session export when the run
// ended (bootstrap_loadout_runs.rs, loadout_session_export): a list of
// entries tagged by `history_version` (session_history.rs,
// SessionHistoryEntry), or, for a Session with legacy turns only, a bare
// list of SessionTurn (turn_ledger.rs). Superseded turns are not in it.
//   execution_v2  {turn: ExecutionTurnView}: an accepted answer is an
//                 activation whose `currently_accepted` is true; its
//                 `output` is {status: "available", content: {text, kind}}
//                 (execution_content.rs). Partial, reserved and streamed
//                 output, and a failed activation's, was never accepted and
//                 is not read.
//   legacy_v1     {turn: SessionTurn}: `final_output`, and each output in
//                 `agent_outputs` that is not superseded.
// An accepted activation whose output the record does not hold, an entry of
// another kind, or a history the record marks unavailable cannot be
// checked, so it fails rather than counting as clean.
//
// A run whose verdict (the `outcome` section) is pass or checks_failed
// accepted at least its writer's answer, so the check needs at least one
// answer there. A run that needs attention, was interrupted or ended in an
// error may have accepted none.
//
// Usage:
//   node record-answers.mjs RECORD        one line; exit 0: every accepted
//                                         answer was read and none holds
//                                         markup; 1: one does; 2: the
//                                         answers cannot be read, or there
//                                         are fewer than the verdict needs
//   node record-answers.mjs --repo-head RECORD
//                                         the commit the run started from
//                                         (manifest repo_head); exit 2 when
//                                         the record names none

import { createHash } from 'node:crypto';
import { readFileSync } from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

/** Tool-call markup a model wrote as text instead of calling the tool. */
export const MARKUP = /<function=|<\/tool_call>/;

/** The verdicts whose run accepted at least the writer's answer. */
const ANSWERED = new Set(['pass', 'checks_failed']);

/** A whole record's sections, `[{ section, data }]`, after checking its
 *  header and end line; throws when the record is not whole. */
export function readRecord(bytes) {
  const buffer = Buffer.isBuffer(bytes) ? bytes : Buffer.from(bytes);
  if (!buffer.length) throw new Error('the record is empty');
  if (buffer[buffer.length - 1] !== 0x0a) throw new Error('the record is cut off (no end line)');
  const endStart = buffer.lastIndexOf(0x0a, buffer.length - 2) + 1;
  if (endStart === 0) throw new Error('the record has a header and nothing else');
  const parse = (slice, what) => {
    try {
      return JSON.parse(slice.toString('utf8'));
    } catch {
      throw new Error(`${what} is not JSON`);
    }
  };
  const end = parse(buffer.subarray(endStart, buffer.length - 1), 'the end line');
  const lines = buffer.subarray(0, endStart).toString('utf8').split('\n').slice(0, -1);
  if (end?.section !== 'end') throw new Error('the record has no end line: it is cut off');
  if (end.lines !== lines.length - 1) {
    throw new Error(`the end line counts ${end.lines} section lines; the record has ${lines.length - 1}`);
  }
  const digest = createHash('sha256').update(buffer.subarray(0, endStart)).digest('hex');
  if (end.sha256 !== digest) throw new Error('the record does not match its end line\'s digest: it was changed');
  const header = parse(Buffer.from(lines[0]), 'line 1');
  if (!/^axocoatl\.record-bundle\//.test(String(header?.schema))) {
    throw new Error('the record does not start with a record-bundle header');
  }
  return lines.slice(1).map((line, index) => parse(Buffer.from(line), `line ${index + 2}`));
}

function only(sections, name) {
  const found = sections.filter((entry) => entry?.section === name);
  if (found.length !== 1) throw new Error(`the record has ${found.length} ${name} sections, not one`);
  return found[0].data;
}

function legacyAnswers(turn, answers) {
  if (turn.superseded) return;
  const where = `legacy turn ${turn.id}`;
  if (typeof turn.final_output === 'string') answers.push({ where, text: turn.final_output });
  for (const output of turn.agent_outputs ?? []) {
    if (!output.superseded && typeof output.output === 'string') {
      answers.push({ where: `${where}, ${output.agent_id}`, text: output.output });
    }
  }
}

/** Every accepted answer in a `history` section: `[{ where, text }]`. */
export function acceptedAnswers(history) {
  if (!Array.isArray(history)) {
    if (history && typeof history === 'object' && 'unavailable' in history) {
      throw new Error(`the record's history is unavailable: ${history.unavailable}`);
    }
    throw new Error('the record\'s history is not a list of turns');
  }
  const answers = [];
  history.forEach((entry, index) => {
    const version = entry?.history_version;
    if (version === 'execution_v2') {
      const turn = entry.turn;
      if (!turn || !Array.isArray(turn.activations)) throw new Error(`history entry ${index + 1} (execution_v2) has no activations`);
      if (turn.superseded) return;
      for (const activation of turn.activations) {
        if (activation?.currently_accepted !== true) continue;
        const node = activation.activation?.activation?.node_id ?? '?';
        const name = activation.definition_name?.content ?? node;
        const where = `turn ${turn.turn_id}, ${name}`;
        const output = activation.output;
        if (output?.status !== 'available' || typeof output.content?.text !== 'string') {
          throw new Error(`${where}: the accepted activation's output is not in the record (${output?.status ?? 'no output'})`);
        }
        answers.push({ where, text: output.content.text });
      }
    } else if (version === 'legacy_v1') {
      if (!entry.turn || typeof entry.turn !== 'object') throw new Error(`history entry ${index + 1} (legacy_v1) has no turn`);
      legacyAnswers(entry.turn, answers);
    } else if (version === undefined && typeof entry?.user_input === 'string') {
      legacyAnswers(entry, answers);
    } else {
      throw new Error(`history entry ${index + 1} is of an unknown kind (history_version ${JSON.stringify(version)})`);
    }
  });
  return answers;
}

/** The gate's verdict on a record: `{ status, line }` (see Usage). */
export function checkRecord(bytes) {
  let answers;
  let verdict;
  try {
    const sections = readRecord(bytes);
    verdict = only(sections, 'outcome')?.verdict;
    if (typeof verdict !== 'string') throw new Error('the record\'s outcome has no verdict');
    answers = acceptedAnswers(only(sections, 'history'));
  } catch (error) {
    return { status: 2, line: `cannot read the run's accepted answers: ${error.message}` };
  }
  const bad = answers.filter((answer) => MARKUP.test(answer.text));
  if (bad.length) {
    const first = bad[0];
    const at = first.text.search(MARKUP);
    const excerpt = JSON.stringify(first.text.slice(Math.max(0, at - 60), at + 140));
    return { status: 1, line: `${bad.length} of ${answers.length} accepted answers hold tool-call markup; ${first.where}: ${excerpt}` };
  }
  if (!answers.length && ANSWERED.has(verdict)) {
    return { status: 2, line: `the record holds no accepted answer, but a run whose verdict is ${verdict} accepted its writer's` };
  }
  return { status: 0, line: `${answers.length} accepted answers in the record (verdict ${verdict}) hold no tool-call markup` };
}

/** The commit the run started from (manifest repo_head). */
export function repoHead(bytes) {
  const head = only(readRecord(bytes), 'manifest')?.repo_head;
  if (typeof head !== 'string' || !/^[0-9a-f]{40,64}$/.test(head)) throw new Error('the record names no commit the run started from');
  return head;
}

function main(argv) {
  const head = argv[0] === '--repo-head';
  const files = head ? argv.slice(1) : argv;
  if (files.length !== 1) {
    console.log('usage: record-answers.mjs [--repo-head] RECORD');
    return 2;
  }
  let bytes;
  try {
    bytes = readFileSync(files[0]);
  } catch (error) {
    console.log(`cannot read the run's accepted answers: ${error.code === 'ENOENT' ? `${files[0]} does not exist` : error.message}`);
    return 2;
  }
  if (head) {
    try {
      console.log(repoHead(bytes));
      return 0;
    } catch (error) {
      console.log(error.message);
      return 2;
    }
  }
  const { status, line } = checkRecord(bytes);
  console.log(line);
  return status;
}

if (process.argv[1] && path.resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  process.exit(main(process.argv.slice(2)));
}
