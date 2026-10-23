// node --test scripts/release-gate/record-answers.test.mjs
//
// The answers a run accepted, read from its record as a 1.5.0 run writes it
// (testdata/record-history-1.5.0.json), for the gate's tool-call markup
// check: markup in an accepted answer fails, a record that cannot be read
// or holds no answer its verdict promises fails, and output that was never
// accepted is not read.
import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import { mkdtempSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import path from 'node:path';
import { after, test } from 'node:test';
import { fileURLToPath } from 'node:url';

import { acceptedAnswers, checkRecord, readRecord, repoHead } from './record-answers.mjs';
import { bundleText, history, record } from './testdata/record-bundle.mjs';

const HERE = path.dirname(fileURLToPath(import.meta.url));
const scratch = mkdtempSync(path.join(tmpdir(), 'axocoatl-record-answers-'));
after(() => rmSync(scratch, { recursive: true, force: true }));

const SLIP = 'Now I will edit the file.\n<function=edit_file>\n<parameter=path>\nsrc/lib.rs\n</parameter>\n</function>\n</tool_call>';

test('a 1.5.0 record: the writer\'s and the reviewer\'s accepted answers are read, and clean ones pass', () => {
  const entries = history('Fixed src/lib.rs.', 'VERDICT: APPROVE\nNothing must change.');
  assert.deepEqual(acceptedAnswers(entries).map((answer) => answer.text), ['Fixed src/lib.rs.', 'VERDICT: APPROVE\nNothing must change.']);
  assert.match(acceptedAnswers(entries)[0].where, /^turn turn-[0-9a-f-]+, writer$/);
  const { status, line } = checkRecord(record({ entries }));
  assert.equal(status, 0);
  assert.equal(line, '2 accepted answers in the record (verdict pass) hold no tool-call markup');
});

test('markup in an accepted answer fails, naming where', () => {
  let result = checkRecord(record({ entries: history(SLIP, 'VERDICT: APPROVE') }));
  assert.equal(result.status, 1);
  assert.match(result.line, /^1 of 2 accepted answers hold tool-call markup; turn turn-[0-9a-f-]+, writer: ".*<function=edit_file>/);
  result = checkRecord(record({ entries: history('Fixed it.', 'VERDICT: APPROVE </tool_call>') }));
  assert.equal(result.status, 1);
  assert.match(result.line, /reviewer: ".*<\/tool_call>/);
  // A run whose checks failed is checked the same way.
  assert.equal(checkRecord(record({ verdict: 'checks_failed', entries: history(SLIP, 'x') })).status, 1);
});

test('output that was never accepted is not read: a refused activation\'s', () => {
  const entries = history('Fixed it.', SLIP);
  entries[0].turn.activations[1].currently_accepted = false;
  entries[0].turn.activations[1].activation.state = 'refused';
  assert.equal(checkRecord(record({ verdict: 'needs_attention', entries })).status, 0);
  const superseded = history(SLIP, SLIP);
  superseded[0].turn.superseded = true;
  assert.deepEqual(acceptedAnswers(superseded), []);
});

test('an answer a review revision replaced was accepted, and is read', () => {
  // Round 1 slipped and was sent back; round 2 passed.
  const entries = history(SLIP, 'VERDICT: CHANGES');
  for (const activation of entries[0].turn.activations) {
    activation.currently_accepted = false;
    activation.activation.state = 'superseded';
  }
  const round2 = history('Fixed it.', 'VERDICT: APPROVE')[0].turn.activations;
  entries[0].turn.activations.push(...round2);
  const result = checkRecord(record({ verdict: 'pass', entries }));
  assert.equal(result.status, 1);
  assert.match(result.line, /<function=|<\/tool_call>/);
  // A replaced activation whose output the record lacks cannot be checked.
  entries[0].turn.activations[0].output = { status: 'missing' };
  assert.equal(checkRecord(record({ verdict: 'pass', entries })).status, 2);
});

test('a pass or checks_failed run with no accepted answer cannot be checked; a run that needs attention may have none', () => {
  const none = () => {
    const entries = history('a', 'b');
    for (const activation of entries[0].turn.activations) activation.currently_accepted = false;
    return entries;
  };
  for (const verdict of ['pass', 'checks_failed']) {
    const { status, line } = checkRecord(record({ verdict, entries: none() }));
    assert.equal(status, 2, verdict);
    assert.match(line, new RegExp(`no accepted answer, but a run whose verdict is ${verdict}`));
  }
  for (const verdict of ['needs_attention', 'error', 'interrupted']) {
    assert.equal(checkRecord(record({ verdict, entries: none() })).line, `0 accepted answers in the record (verdict ${verdict}) hold no tool-call markup`);
  }
  assert.equal(checkRecord(record({ verdict: 'pass', entries: [] })).status, 2);
});

test('a history the gate cannot read fails closed', () => {
  const missing = history('a', 'b');
  missing[0].turn.activations[0].output = { status: 'missing', reference: 'content-0' };
  const cases = {
    'the accepted activation\'s output is not in the record (missing)': missing,
    'history is unavailable: session not found': { unavailable: 'session not found' },
    'not a list of turns': 'the export as text',
    // The shape GET /api/sessions/<id>/messages serves is not a record's.
    'has no activations': [{ history_version: 'execution_v2', content: history('a', 'b')[0].turn }],
    'unknown kind (history_version "execution_v3")': [{ history_version: 'execution_v3', turn: {} }],
  };
  for (const [reason, entries] of Object.entries(cases)) {
    const { status, line } = checkRecord(record({ entries }));
    assert.equal(status, 2, reason);
    assert.ok(line.startsWith('cannot read the run\'s accepted answers: ') && line.includes(reason), line);
  }
});

test('legacy turns: the final answer and each agent output that was not superseded', () => {
  const turn = { id: 'turn-1', user_input: 'fix it', final_output: 'Done.', agent_outputs: [
    { agent_id: 'writer', output: SLIP, superseded: true, recorded_at: 1 },
    { agent_id: 'reviewer', output: 'VERDICT: APPROVE', recorded_at: 2 },
  ] };
  assert.deepEqual(acceptedAnswers([{ history_version: 'legacy_v1', turn }]).map((answer) => answer.where),
    ['legacy turn turn-1', 'legacy turn turn-1, reviewer']);
  assert.equal(checkRecord(record({ entries: [turn] })).status, 0);
  assert.equal(checkRecord(record({ entries: [{ ...turn, final_output: SLIP }] })).status, 1);
});

test('only a whole record is read: its end line counts the lines and holds their digest', () => {
  const whole = record();
  assert.equal(readRecord(Buffer.from(whole)).map((entry) => entry.section).join(' '), 'manifest outcome history');
  const reason = (text) => checkRecord(Buffer.from(text)).line;
  assert.match(reason(whole.replace('Fixed it.', 'Fixed it!')), /does not match its end line's digest/);
  assert.match(reason(whole.slice(0, whole.lastIndexOf('{"section":"end"'))), /no end line/);
  assert.match(reason(whole.slice(0, -10)), /cut off/);
  assert.match(reason(''), /empty/);
  assert.match(reason(bundleText({ schema: 'other' }, [['outcome', { verdict: 'pass' }]])), /record-bundle header/);
  const two = bundleText({ schema: 'axocoatl.record-bundle/2' }, [['outcome', { verdict: 'pass' }], ['history', []], ['history', []]]);
  assert.match(reason(two), /2 history sections, not one/);
  assert.match(reason(bundleText({ schema: 'axocoatl.record-bundle/2' }, [['outcome', {}], ['history', []]])), /outcome has no verdict/);
});

test('the commit the run started from comes from the manifest', () => {
  assert.equal(repoHead(Buffer.from(record({ repoHead: 'a'.repeat(40) }))), 'a'.repeat(40));
  assert.throws(() => repoHead(Buffer.from(record({ repoHead: null }))), /names no commit/);
});

test('the command prints one line and exits 0, 1 or 2', () => {
  const run = (...args) => spawnSync(process.execPath, [path.join(HERE, 'record-answers.mjs'), ...args], { encoding: 'utf8' });
  const file = (name, text) => {
    const target = path.join(scratch, name);
    writeFileSync(target, text);
    return target;
  };
  const clean = file('clean.jsonl', record({ repoHead: 'b'.repeat(40) }));
  assert.deepEqual([run(clean).status, run(clean).stdout], [0, '2 accepted answers in the record (verdict pass) hold no tool-call markup\n']);
  assert.equal(run(file('slip.jsonl', record({ entries: history(SLIP, 'ok') }))).status, 1);
  const absent = run(path.join(scratch, 'absent.jsonl'));
  assert.deepEqual([absent.status, absent.stdout.trim()], [2, `cannot read the run's accepted answers: ${path.join(scratch, 'absent.jsonl')} does not exist`]);
  assert.deepEqual([run('--repo-head', clean).status, run('--repo-head', clean).stdout], [0, `${'b'.repeat(40)}\n`]);
  assert.equal(run().status, 2);
});
