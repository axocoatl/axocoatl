// node --test scripts/release-gate/stub-model-server.test.mjs
import assert from 'node:assert/strict';
import { existsSync, readFileSync } from 'node:fs';
import path from 'node:path';
import { after, before, test } from 'node:test';
import { fileURLToPath } from 'node:url';

import {
  DEFAULT_MODELS, LONG_OUTPUT_PROBE_PROMPT, OLLAMA_VERSION, STOP_PROBE_PROMPT, TOOL_PROBE_PROMPT,
  classify, decide, digestOf, loadFixes, parseFixes, startStubServer, toolHistory, writerStep,
} from './stub-model-server.mjs';

const REPO = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..', '..');
const FIXTURES = path.join(REPO, 'tests', 'fixtures', 'newuser');
const fixes = loadFixes();
const manifest = JSON.parse(readFileSync(path.join(FIXTURES, 'fixtures.json'), 'utf8'));
const WRITER_TOOLS = ['read_file', 'list_dir', 'grep', 'glob', 'write_file', 'edit_file', 'bash']
  .map((name) => ({ type: 'function', function: { name, parameters: { type: 'object' } } }));
const REVIEWER_TOOLS = ['read_file', 'list_dir', 'grep', 'glob']
  .map((name) => ({ type: 'function', function: { name, parameters: { type: 'object' } } }));

const presentFixtures = () => Object.keys(manifest.fixtures)
  .filter((name) => existsSync(path.join(FIXTURES, name)));

/** What read_file answers in a fixture: its JSON result, or an error. */
function readFileResult(fixture, file) {
  const full = path.join(FIXTURES, fixture, file);
  if (!existsSync(full)) return `Tool error: read_file: ${file}: No such file or directory`;
  const content = readFileSync(full, 'utf8');
  return JSON.stringify({ content, truncated: false, returned_bytes: content.length });
}

/**
 * Drive the writer side of a conversation in `fixture` the way the host
 * does: run each tool call against the fixture's files, append the call and
 * its result, and ask again, until the writer answers in text.
 */
function converse(fixture, fixTable = fixes) {
  const messages = [
    { role: 'system', content: 'You own this change.' },
    { role: 'user', content: 'Paging skips the last item when the total is a multiple of the page size' },
  ];
  const files = new Map();
  for (let turn = 0; turn < 20; turn += 1) {
    const step = writerStep(messages, fixTable);
    if (step.kind === 'text') return { step, messages, files, turns: turn + 1 };
    messages.push({ role: 'assistant', content: '', tool_calls: [{ id: `c${turn}`, function: { name: step.name, arguments: step.args } }] });
    let result;
    if (step.name === 'read_file') {
      result = readFileResult(fixture, step.args.path);
    } else if (step.name === 'edit_file') {
      const before = files.get(step.args.path) ?? readFileSync(path.join(FIXTURES, fixture, step.args.path), 'utf8');
      const occurrences = before.split(step.args.old).length - 1;
      assert.equal(occurrences, 1, `edit_file old text must appear once in ${fixture}/${step.args.path}`);
      files.set(step.args.path, before.replace(step.args.old, step.args.new));
      result = JSON.stringify({ ok: true, path: step.args.path, replacements: 1 });
    } else {
      assert.fail(`unexpected tool ${step.name}`);
    }
    messages.push({ role: 'tool', content: result });
  }
  assert.fail(`the writer did not finish in ${fixture}`);
}

test('every fixture on disk is in fixtures.json and has exactly one stub fix that matches its files', () => {
  const onDisk = presentFixtures();
  assert.ok(onDisk.length >= 4, `expected the new-user fixtures, found ${onDisk.join(', ')}`);
  for (const name of onDisk) {
    const entries = fixes.filter((fix) => fix.fixture === name);
    assert.equal(entries.length, 1, `${name} needs exactly one entry in stub-fixes.json`);
    const [fix] = entries;
    const probe = path.join(FIXTURES, name, fix.probe);
    assert.ok(existsSync(probe), `${name}: probe ${fix.probe} does not exist`);
    const content = readFileSync(probe, 'utf8');
    assert.equal(content.split(fix.find).length - 1, 1, `${name}: "${fix.find}" must appear exactly once in ${fix.probe}`);
    assert.ok(existsSync(path.join(FIXTURES, name, 'README.md')), `${name} needs a README.md stating the expected fix`);
    const readme = readFileSync(path.join(FIXTURES, name, 'README.md'), 'utf8');
    if (name !== 'node-plain') {
      assert.match(readme, /## The expected fix/, `${name}: README must state the expected fix`);
      assert.ok(readme.includes(fix.replace.trim()) || readme.includes(fix.replace.split(' ').at(-1)), `${name}: README's fix should show the stub's replacement`);
    }
  }
  for (const fix of fixes) {
    assert.ok(manifest.fixtures[fix.fixture], `stub fix for ${fix.fixture} names no fixture in fixtures.json`);
  }
});

test('the stub writer reads probes in order, fixes the fixture it recognises, and then says it is done', () => {
  for (const name of presentFixtures()) {
    const { step, files, turns } = converse(name);
    const fix = fixes.find((candidate) => candidate.fixture === name);
    assert.equal(step.kind, 'text', name);
    assert.match(step.text, /^Fixed /, `${name}: ${step.text}`);
    assert.equal(step.fixture, name);
    assert.deepEqual([...files.keys()], [fix.probe], `${name}: only the probe file changes`);
    assert.ok(files.get(fix.probe).includes(fix.replace), name);
    const probes = [...new Set(fixes.map((candidate) => candidate.probe))];
    assert.ok(turns <= probes.indexOf(fix.probe) + 3, `${name} took ${turns} turns`);
  }
});

test('the stub writer gives up with a pointer when no fixture matches', () => {
  const { step } = converse('node-deps', parseFixes(JSON.stringify({
    schema: 'axocoatl.gate-stub-fixes/1',
    fixes: [{ fixture: 'none', probe: 'nowhere.txt', find: 'never', replace: 'ever' }],
  })));
  assert.equal(step.kind, 'text');
  assert.match(step.text, /No fixture this stub knows matched/);
  assert.match(step.text, /stub-fixes\.json/);
});

test('a failed edit is reported instead of claimed as fixed', () => {
  const fix = fixes[0];
  const messages = [
    { role: 'user', content: 'task' },
    { role: 'assistant', tool_calls: [{ id: 'a', function: { name: 'edit_file', arguments: { path: fix.probe, old: fix.find, new: fix.replace } } }] },
    { role: 'tool', content: 'Tool error: edit_file: the old text was not found' },
  ];
  const step = writerStep(messages, fixes);
  assert.equal(step.kind, 'text');
  assert.match(step.text, /failed/);
});

test('review findings sent back after the edit are answered in an ADJUDICATIONS block', () => {
  const fix = fixes[0];
  const messages = [
    { role: 'user', content: 'task' },
    { role: 'assistant', tool_calls: [{ id: 'a', function: { name: 'edit_file', arguments: { path: fix.probe, old: fix.find, new: fix.replace } } }] },
    { role: 'tool', content: JSON.stringify({ ok: true }) },
    { role: 'assistant', content: 'Fixed.' },
    { role: 'user', content: 'The reviewer found:\nF1 src/paginate.js:15 something\nF2 other\nAnswer every finding in an ADJUDICATIONS block.' },
  ];
  const step = writerStep(messages, fixes);
  assert.match(step.text, /^ADJUDICATIONS\n```json\n/);
  const answers = JSON.parse(step.text.split('\n').slice(2, -1).join('\n'));
  assert.deepEqual(answers.map((answer) => [answer.id, answer.decision]), [['F1', 'accept'], ['F2', 'accept']]);
});

test('requests are told apart: writer, reviewer, contract probe and anything else', () => {
  assert.equal(classify({ tools: WRITER_TOOLS, messages: [{ role: 'user', content: 'x' }] }), 'writer');
  assert.equal(classify({ tools: REVIEWER_TOOLS, messages: [{ role: 'user', content: 'x' }] }), 'reviewer');
  assert.equal(classify({ messages: [{ role: 'user', content: 'Give VERDICT: APPROVE or VERDICT: CHANGES' }] }), 'reviewer');
  assert.equal(classify({ truncate: false, options: { num_predict: 1 }, messages: [{ role: 'user', content: STOP_PROBE_PROMPT }] }), 'probe');
  assert.equal(classify({ truncate: false, options: { num_predict: 1 }, messages: [{ role: 'user', content: 'x x x x' }] }), 'probe');
  assert.equal(classify({ truncate: false, options: { num_predict: 64 }, messages: [{ role: 'user', content: LONG_OUTPUT_PROBE_PROMPT }] }), 'probe');
  // The tool-call probe offers a tool, and is still a probe, not a reviewer.
  assert.equal(classify({ truncate: false, options: { num_predict: 512 }, tools: [{ type: 'function', function: { name: 'report_ready' } }], messages: [{ role: 'user', content: TOOL_PROBE_PROMPT }] }), 'probe');
  assert.equal(classify({ messages: [{ role: 'user', content: TOOL_PROBE_PROMPT }] }), 'other');
  assert.equal(classify({ messages: [{ role: 'user', content: 'hello' }] }), 'other');
  const review = decide({ tools: REVIEWER_TOOLS, messages: [{ role: 'user', content: 'review' }] }, fixes);
  assert.match(review.text, /^VERDICT: APPROVE\n/);
});

test('OpenAI-shaped history (string arguments, tool_call_id) is read like Ollama history', () => {
  const history = toolHistory([
    { role: 'assistant', content: null, tool_calls: [
      { id: 'one', type: 'function', function: { name: 'read_file', arguments: '{"path":"a.js"}' } },
      { id: 'two', type: 'function', function: { name: 'read_file', arguments: '{"path":"b.js"}' } },
    ] },
    { role: 'tool', tool_call_id: 'two', content: 'B' },
    { role: 'tool', tool_call_id: 'one', content: 'A' },
  ]);
  assert.deepEqual(history.map((call) => [call.name, call.args.path, call.result]), [['read_file', 'a.js', 'A'], ['read_file', 'b.js', 'B']]);
});

test('a fix table that could not match reliably is refused', () => {
  const table = (fix) => JSON.stringify({ schema: 'axocoatl.gate-stub-fixes/1', fixes: [fix] });
  assert.throws(() => parseFixes(table({ fixture: 'a', probe: 'x.js', find: 'say "hi"', replace: 'y' })), /quotes/);
  assert.throws(() => parseFixes(table({ fixture: 'a', probe: '../x.js', find: 'a', replace: 'b' })), /relative path/);
  assert.throws(() => parseFixes(table({ fixture: 'a', probe: 'x.js', find: 'a', replace: 'a' })), /equals/);
  assert.throws(() => parseFixes(JSON.stringify({ schema: 'other', fixes: [] })), /schema/);
});

let stub;
before(async () => { stub = await startStubServer({ fixes }); });
after(async () => { await new Promise((resolve) => stub.server.close(resolve)); });

async function call(route, body) {
  const response = await fetch(`${stub.baseUrl}${route}`, body === undefined ? {} : {
    method: 'POST', headers: { 'content-type': 'application/json' }, body: JSON.stringify(body),
  });
  return { status: response.status, type: response.headers.get('content-type'), text: await response.text() };
}

test('the server reports an audited, cloud-off Ollama with local GGUF models that load with a 32k context', async () => {
  assert.deepEqual(JSON.parse((await call('/api/version')).text), { version: OLLAMA_VERSION });
  assert.equal(JSON.parse((await call('/api/status')).text).cloud.disabled, true);
  const tags = JSON.parse((await call('/api/tags')).text).models;
  assert.deepEqual(tags.map((model) => model.name), DEFAULT_MODELS);
  for (const model of tags) {
    assert.equal(model.model, model.name);
    assert.match(model.digest, /^[0-9a-f]{64}$/);
    assert.equal(model.digest, digestOf(model.name));
  }
  const shown = JSON.parse((await call('/api/show', { model: 'qwen3-coder:30b' })).text);
  assert.equal(shown.details.format, 'gguf');
  assert.deepEqual(shown.capabilities, ['completion', 'tools']);
  assert.equal((await call('/api/show', { model: 'missing:1b' })).status, 404);
  const load = JSON.parse((await call('/api/generate', { model: 'gpt-oss:20b', stream: false, shift: false })).text);
  assert.deepEqual(Object.keys(load).sort(), ['created_at', 'done', 'done_reason', 'model', 'response']);
  assert.equal(load.done_reason, 'load');
  assert.equal(load.response, '');
  const loaded = JSON.parse((await call('/api/ps')).text).models;
  for (const model of loaded) {
    assert.equal(model.context_length, 32768);
    assert.equal(model.runner, 'llamacpp');
    assert.equal(model.details.format, 'gguf');
  }
});

test('the contract probes get Ollama 0.40 answers and are counted apart from model calls', async () => {
  await call('/__gate/reset', {});
  const overLong = await call('/api/chat', { model: 'qwen3-coder:30b', truncate: false, shift: false, stream: true, options: { num_ctx: 32768, num_predict: 1 }, messages: [{ role: 'user', content: 'x x x x x' }] });
  assert.equal(overLong.status, 400);
  assert.ok(JSON.parse(overLong.text).error);
  const stop = await call('/api/chat', { model: 'qwen3-coder:30b', truncate: false, shift: false, stream: true, options: { num_ctx: 32768, num_predict: 1 }, messages: [{ role: 'user', content: STOP_PROBE_PROMPT }] });
  const record = JSON.parse(stop.text.trim());
  assert.equal(record.done_reason, 'length');
  assert.equal(record.eval_count, 1);
  // The long answer stops at its limit and reports exactly that many tokens.
  const long = await call('/api/chat', { model: 'qwen3-coder:30b', truncate: false, shift: false, stream: true, options: { num_ctx: 32768, num_predict: 64 }, messages: [{ role: 'user', content: LONG_OUTPUT_PROBE_PROMPT }] });
  const longRecords = long.text.trim().split('\n').map((line) => JSON.parse(line));
  assert.equal(longRecords.at(-1).done_reason, 'length');
  assert.equal(longRecords.at(-1).eval_count, 64);
  assert.ok(longRecords.filter((entry) => entry.message.content).length <= 64);
  // Asked to call the offered tool, the model calls it, with a call id.
  const tool = await call('/api/chat', { model: 'gpt-oss:20b', truncate: false, shift: false, stream: true, options: { num_ctx: 32768, num_predict: 512 }, tools: [{ type: 'function', function: { name: 'report_ready' } }], messages: [{ role: 'user', content: TOOL_PROBE_PROMPT }] });
  const call0 = JSON.parse(tool.text.trim()).message.tool_calls[0];
  assert.ok(call0.id);
  assert.deepEqual([call0.function.name, call0.function.arguments], ['report_ready', { word: 'ready' }]);
  const stats = JSON.parse((await call('/__gate/stats')).text);
  assert.equal(stats.probes, 4);
  assert.equal(stats.writer + stats.reviewer + stats.other, 0);
});

test('a writer chat streams one NDJSON record with a native tool call, and the stats count it', async () => {
  await call('/__gate/reset', {});
  const answer = await call('/api/chat', { model: 'qwen3-coder:30b', stream: true, tools: WRITER_TOOLS, messages: [{ role: 'user', content: 'Paging skips the last item' }] });
  assert.equal(answer.status, 200);
  assert.equal(answer.type, 'application/x-ndjson');
  const lines = answer.text.trim().split('\n').map((line) => JSON.parse(line));
  assert.equal(lines.length, 1);
  const [record] = lines;
  assert.equal(record.done, true);
  assert.equal(record.done_reason, 'stop');
  assert.equal(record.message.tool_calls[0].function.name, 'read_file');
  assert.deepEqual(record.message.tool_calls[0].function.arguments, { path: fixes[0].probe });
  assert.match(record.message.tool_calls[0].id, /^gate-call-\d+$/);
  assert.ok(record.prompt_eval_count > 0 && record.eval_count > 0);
  const stats = JSON.parse((await call('/__gate/stats')).text);
  assert.equal(stats.writer, 1);
  assert.equal((await call('/api/chat', { model: 'unknown:7b', messages: [] })).status, 404);
});

test('the OpenAI-compatible endpoint answers with string arguments, and streams SSE ending in [DONE]', async () => {
  const models = JSON.parse((await call('/v1/models')).text);
  assert.deepEqual(models.data.map((model) => model.id), DEFAULT_MODELS);
  const plain = JSON.parse((await call('/v1/chat/completions', { model: 'gpt-oss:20b', tools: WRITER_TOOLS, messages: [{ role: 'user', content: 'task' }] })).text);
  assert.equal(plain.choices[0].finish_reason, 'tool_calls');
  assert.equal(typeof plain.choices[0].message.tool_calls[0].function.arguments, 'string');
  assert.deepEqual(JSON.parse(plain.choices[0].message.tool_calls[0].function.arguments), { path: fixes[0].probe });
  assert.ok(plain.usage.total_tokens > 0);
  const streamed = await call('/api/v1/chat/completions', { model: 'gpt-oss:20b', stream: true, tools: REVIEWER_TOOLS, messages: [{ role: 'user', content: 'review' }] });
  assert.equal(streamed.type, 'text/event-stream');
  const events = streamed.text.trim().split('\n\n');
  assert.equal(events.at(-1), 'data: [DONE]');
  const chunks = events.slice(0, -1).map((event) => JSON.parse(event.replace(/^data: /, '')));
  const content = chunks.flatMap((chunk) => chunk.choices.map((choice) => choice.delta?.content ?? '')).join('');
  assert.match(content, /^VERDICT: APPROVE/);
  assert.ok(chunks.some((chunk) => chunk.choices[0]?.finish_reason === 'stop'));
  assert.ok(chunks.some((chunk) => chunk.usage?.total_tokens > 0));
});

test('cloud mode and the version can be set to test refusals', async () => {
  const enabled = await startStubServer({ fixes, cloud: 'enabled', version: '0.20.6' });
  try {
    const status = await (await fetch(`${enabled.baseUrl}/api/status`)).json();
    assert.equal(status.cloud.disabled, false);
    assert.equal((await (await fetch(`${enabled.baseUrl}/api/version`)).json()).version, '0.20.6');
  } finally {
    await new Promise((resolve) => enabled.server.close(resolve));
  }
});
