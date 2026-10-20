#!/usr/bin/env node
// A scripted model server for the clean-machine new-user gate
// (docs/RELEASE_GATE.md). It answers like a local Ollama 0.40.6 with its
// cloud models off, and like an OpenAI-compatible API, and it plays both
// sides of an `axocoatl run fix` deterministically:
//
// - the writer (a request that offers edit_file or write_file) reads the
//   probe file of each entry in stub-fixes.json until one holds that entry's
//   `find` text, replaces it with edit_file, then says it is done;
// - the reviewer (any other request with tools, or one that asks for a
//   VERDICT line) approves;
// - Axocoatl's two contract probes get the answers Ollama 0.40.6 gives.
//
// It needs only Node 18 or later, binds only 127.0.0.1, sends nothing
// anywhere, and keeps counters at GET /__gate/stats so the gate can prove a
// refused run used no model calls.
//
// Usage:
//   node stub-model-server.mjs [--port N] [--port-file FILE] [--fixes FILE]
//     [--models a,b] [--version V] [--cloud disabled|enabled|unreported]
//     [--context TOKENS] [--model-size BYTES] [--log FILE]

import { createHash } from 'node:crypto';
import { appendFileSync, readFileSync, writeFileSync } from 'node:fs';
import { createServer } from 'node:http';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

/** The Ollama release native Axocoatl was audited against. */
export const OLLAMA_VERSION = '0.40.6';
/** The stop probe's prompt (`axocoatl_llm_ollama::CONTRACT_STOP_PROBE_PROMPT`). */
export const STOP_PROBE_PROMPT = 'Reply with the single word: ready';
/** The models a new user's onboarding chooses on a 48 GB machine. */
export const DEFAULT_MODELS = ['qwen3-coder:30b', 'gpt-oss:20b'];
export const DEFAULT_CONTEXT = 32768;
/** Reported model size: small, so memory checks pass on any runner. */
export const DEFAULT_MODEL_SIZE = 1024 * 1024 * 1024;
const FIXES_SCHEMA = 'axocoatl.gate-stub-fixes/1';
const CREATED_AT = '2026-10-19T00:00:00Z';
const MAX_BODY_BYTES = 32 * 1024 * 1024;
const HERE = path.dirname(fileURLToPath(import.meta.url));
export const DEFAULT_FIXES = path.join(HERE, 'stub-fixes.json');

// ---------------------------------------------------------------------------
// Fix table

/** Parse and check a stub-fixes.json document. */
export function parseFixes(text) {
  const document = JSON.parse(text);
  if (document?.schema !== FIXES_SCHEMA) {
    throw new Error(`stub fixes: schema must be ${FIXES_SCHEMA}`);
  }
  if (!Array.isArray(document.fixes) || document.fixes.length === 0) {
    throw new Error('stub fixes: "fixes" must be a non-empty array');
  }
  const seen = new Set();
  return document.fixes.map((fix, index) => {
    const where = `stub fixes: entry ${index + 1}`;
    for (const key of ['fixture', 'probe', 'find', 'replace']) {
      if (typeof fix?.[key] !== 'string' || fix[key] === '') {
        throw new Error(`${where}: "${key}" must be a non-empty string`);
      }
    }
    if (seen.has(fix.fixture)) throw new Error(`${where}: fixture ${fix.fixture} is listed twice`);
    seen.add(fix.fixture);
    if (/["\\\n\r]/.test(fix.find)) {
      throw new Error(`${where}: "find" must not contain quotes, backslashes or newlines`);
    }
    if (fix.find === fix.replace) throw new Error(`${where}: "replace" equals "find"`);
    if (path.isAbsolute(fix.probe) || fix.probe.split('/').includes('..')) {
      throw new Error(`${where}: "probe" must be a relative path inside the repository`);
    }
    return { fixture: fix.fixture, probe: fix.probe, find: fix.find, replace: fix.replace };
  });
}

export function loadFixes(file = DEFAULT_FIXES) {
  return parseFixes(readFileSync(file, 'utf8'));
}

// ---------------------------------------------------------------------------
// Requests

/** The 64-hex digest the stub reports for `model`. */
export function digestOf(model) {
  return createHash('sha256').update(`axocoatl-gate-stub:${model}`).digest('hex');
}

/** Whether a parsed `/api/chat` body is one of Axocoatl's contract probes. */
export function isContractProbe(body) {
  if (body?.options?.num_predict !== 1 || body?.truncate !== false) return false;
  const content = body?.messages?.[0]?.content;
  return content === STOP_PROBE_PROMPT || (typeof content === 'string' && content.startsWith('x x '));
}

function textOf(content) {
  if (typeof content === 'string') return content;
  if (Array.isArray(content)) {
    return content.map((part) => (typeof part === 'string' ? part : part?.text ?? '')).join('');
  }
  return '';
}

function toolNames(body) {
  return (body?.tools || []).map((tool) => tool?.function?.name ?? tool?.name).filter(Boolean);
}

function argumentsOf(call) {
  const raw = call?.function?.arguments ?? call?.arguments ?? {};
  if (typeof raw !== 'string') return raw && typeof raw === 'object' ? raw : {};
  try {
    const parsed = JSON.parse(raw);
    return parsed && typeof parsed === 'object' ? parsed : {};
  } catch {
    return {};
  }
}

/**
 * The tool calls of a conversation in order, each with the result the host
 * sent back (`undefined` while unanswered). Works for Ollama's message shape
 * (arguments as an object, results in order) and OpenAI's (arguments as a
 * JSON string, results naming `tool_call_id`).
 */
export function toolHistory(messages = []) {
  const calls = [];
  for (const message of messages) {
    if (message?.role === 'assistant' && Array.isArray(message.tool_calls)) {
      for (const call of message.tool_calls) {
        calls.push({
          id: call?.id ?? '',
          name: call?.function?.name ?? call?.name ?? '',
          args: argumentsOf(call),
          result: undefined,
        });
      }
    } else if (message?.role === 'tool') {
      const content = textOf(message.content);
      const byId = message.tool_call_id
        ? calls.find((call) => call.result === undefined && call.id === message.tool_call_id)
        : undefined;
      const pending = byId ?? calls.find((call) => call.result === undefined);
      if (pending) pending.result = content;
    }
  }
  return calls;
}

/** `writer`, `reviewer`, `probe` or `other`. */
export function classify(body) {
  if (isContractProbe(body)) return 'probe';
  const tools = toolNames(body);
  if (tools.includes('edit_file') || tools.includes('write_file')) return 'writer';
  const text = (body?.messages || []).map((message) => textOf(message?.content)).join('\n');
  if (tools.length > 0 || /VERDICT/.test(text)) return 'reviewer';
  return 'other';
}

function normalizePath(value) {
  return String(value ?? '').replace(/^\.\//, '').replace(/^\/+/, '');
}

/** A tool result's file content, when it is read_file's JSON, else the raw text. */
function resultTexts(result) {
  const texts = [String(result ?? '')];
  try {
    const parsed = JSON.parse(texts[0]);
    if (typeof parsed?.content === 'string') texts.push(parsed.content);
  } catch {
    // not JSON: the raw text is all there is
  }
  return texts;
}

function editSucceeded(result) {
  try {
    const parsed = JSON.parse(String(result ?? ''));
    return parsed?.ok === true;
  } catch {
    return /"ok"\s*:\s*true/.test(String(result ?? ''));
  }
}

/**
 * What the writer does next, from the conversation so far: a tool call
 * `{ kind: 'call', name, args, fixture? }` or a final answer
 * `{ kind: 'text', text, fixture? }`.
 */
export function writerStep(messages, fixes) {
  const history = toolHistory(messages);
  const edit = [...history].reverse().find((call) => call.name === 'edit_file' || call.name === 'write_file');
  if (edit) {
    const fix = fixes.find((candidate) => normalizePath(candidate.probe) === normalizePath(edit.args.path));
    const fixture = fix?.fixture;
    if (edit.result !== undefined && !editSucceeded(edit.result)) {
      return { kind: 'text', fixture, text: `The edit of ${edit.args.path} failed: ${String(edit.result).slice(0, 400)}` };
    }
    const last = [...(messages || [])].reverse().find((message) => message?.role === 'user');
    const findings = [...new Set((textOf(last?.content).match(/\bF\d+\b/g) || []))];
    if (/ADJUDICATIONS/.test(textOf(last?.content)) && findings.length > 0) {
      const answers = findings.map((id) => ({ id, decision: 'accept', reason: 'The last page now holds every item left after its start.' }));
      return { kind: 'text', fixture, text: `ADJUDICATIONS\n\`\`\`json\n${JSON.stringify(answers)}\n\`\`\`` };
    }
    return {
      kind: 'text',
      fixture,
      text: `Fixed ${edit.args.path}: the last page is now sized by the items left after its start, so it is full when the total is a multiple of the page size.`,
    };
  }
  for (const fix of fixes) {
    const read = history.find((call) => call.name === 'read_file'
      && normalizePath(call.args.path) === normalizePath(fix.probe) && call.result !== undefined);
    if (read && resultTexts(read.result).some((text) => text.includes(fix.find))) {
      return { kind: 'call', fixture: fix.fixture, name: 'edit_file', args: { path: fix.probe, old: fix.find, new: fix.replace } };
    }
  }
  const tried = new Set(history.filter((call) => call.name === 'read_file').map((call) => normalizePath(call.args.path)));
  const next = fixes.map((fix) => fix.probe).find((probe) => !tried.has(normalizePath(probe)));
  if (next) return { kind: 'call', name: 'read_file', args: { path: next } };
  return {
    kind: 'text',
    text: `No fixture this stub knows matched this repository (read: ${[...tried].join(', ')}). Add an entry to scripts/release-gate/stub-fixes.json.`,
  };
}

/** The answer to one chat request: a step plus the role it played. */
export function decide(body, fixes) {
  const kind = classify(body);
  if (kind === 'writer') return { role: 'writer', ...writerStep(body.messages, fixes) };
  if (kind === 'reviewer') {
    return { role: 'reviewer', kind: 'text', text: 'VERDICT: APPROVE\nThe change sizes the last page by the items left after its start; nothing must change.' };
  }
  return { role: 'other', kind: 'text', text: 'ready' };
}

function estimateTokens(text) {
  return Math.max(1, Math.ceil(String(text).length / 4));
}

// ---------------------------------------------------------------------------
// Server

/** Make (not start) a stub server. */
export function createStubServer(options = {}) {
  const fixes = options.fixes ?? loadFixes(options.fixesFile ?? DEFAULT_FIXES);
  const models = options.models ?? DEFAULT_MODELS;
  const version = options.version ?? OLLAMA_VERSION;
  const cloud = options.cloud ?? 'disabled';
  const context = options.context ?? DEFAULT_CONTEXT;
  const modelSize = options.modelSize ?? DEFAULT_MODEL_SIZE;
  const logFile = options.logFile;
  const stats = { requests: 0, chats: 0, writer: 0, reviewer: 0, probes: 0, other: 0, pulls: 0, edits: [], unknown: [] };
  let callCounter = 0;

  const log = (entry) => {
    if (!logFile) return;
    try {
      appendFileSync(logFile, `${JSON.stringify({ at: new Date().toISOString(), ...entry })}\n`);
    } catch {
      // logging is best effort
    }
  };
  const known = (model) => models.includes(model) || models.includes(`${model}:latest`)
    || (String(model).endsWith(':latest') && models.includes(String(model).slice(0, -':latest'.length)));
  const json = (res, status, value) => {
    res.writeHead(status, { 'content-type': 'application/json' });
    res.end(JSON.stringify(value));
  };
  const details = { format: 'gguf', family: 'stub', parameter_size: '1B', quantization_level: 'Q4_K_M' };
  const tagRecord = (model) => ({ name: model, model, modified_at: CREATED_AT, size: modelSize, digest: digestOf(model), details });
  const nextCallId = () => {
    callCounter += 1;
    return `gate-call-${callCounter}`;
  };
  const count = (role) => {
    stats.chats += 1;
    if (role === 'writer') stats.writer += 1;
    else if (role === 'reviewer') stats.reviewer += 1;
    else if (role === 'probe') stats.probes += 1;
    else stats.other += 1;
  };

  function ollamaChat(body, res) {
    if (isContractProbe(body)) {
      count('probe');
      log({ path: '/api/chat', role: 'probe', model: body.model });
      if (body.messages[0].content !== STOP_PROBE_PROMPT) {
        json(res, 400, { error: 'request exceeds the available context size, try increasing it' });
        return;
      }
      const record = { model: body.model, created_at: CREATED_AT, message: { role: 'assistant', content: 'ready' }, done: true, done_reason: 'length', prompt_eval_count: 14, eval_count: 1 };
      res.writeHead(200, { 'content-type': 'application/x-ndjson' });
      res.end(`${JSON.stringify(record)}\n`);
      return;
    }
    if (!known(body?.model)) {
      json(res, 404, { error: `model '${body?.model}' not found` });
      return;
    }
    const step = decide(body, fixes);
    count(step.role);
    if (step.kind === 'call' && step.name === 'edit_file') stats.edits.push({ fixture: step.fixture, path: step.args.path });
    log({ path: '/api/chat', role: step.role, model: body.model, step: step.kind === 'call' ? `${step.name} ${step.args.path}` : 'text', fixture: step.fixture });
    const message = step.kind === 'call'
      ? { role: 'assistant', content: '', tool_calls: [{ id: nextCallId(), function: { name: step.name, arguments: step.args } }] }
      : { role: 'assistant', content: step.text };
    const prompt = (body.messages || []).map((entry) => textOf(entry?.content)).join('\n');
    const record = {
      model: body.model, created_at: CREATED_AT, message, done: true, done_reason: 'stop',
      prompt_eval_count: estimateTokens(prompt), eval_count: estimateTokens(JSON.stringify(message)),
    };
    if (body.stream === false) {
      json(res, 200, record);
      return;
    }
    res.writeHead(200, { 'content-type': 'application/x-ndjson' });
    res.end(`${JSON.stringify(record)}\n`);
  }

  function openaiChat(body, res) {
    if (!known(body?.model)) {
      json(res, 404, { error: { message: `model '${body?.model}' not found`, type: 'invalid_request_error' } });
      return;
    }
    const step = decide(body, fixes);
    count(step.role);
    if (step.kind === 'call' && step.name === 'edit_file') stats.edits.push({ fixture: step.fixture, path: step.args.path });
    log({ path: '/v1/chat/completions', role: step.role, model: body.model, step: step.kind === 'call' ? `${step.name} ${step.args.path}` : 'text', fixture: step.fixture });
    const id = `chatcmpl-gate-${stats.chats}`;
    const created = 1792368000;
    const toolCalls = step.kind === 'call'
      ? [{ id: nextCallId(), type: 'function', function: { name: step.name, arguments: JSON.stringify(step.args) } }]
      : undefined;
    const content = step.kind === 'call' ? null : step.text;
    const finish = step.kind === 'call' ? 'tool_calls' : 'stop';
    const prompt = (body.messages || []).map((entry) => textOf(entry?.content)).join('\n');
    const usage = { prompt_tokens: estimateTokens(prompt), completion_tokens: estimateTokens(JSON.stringify(toolCalls ?? content)) };
    usage.total_tokens = usage.prompt_tokens + usage.completion_tokens;
    if (!body.stream) {
      json(res, 200, {
        id, object: 'chat.completion', created, model: body.model,
        choices: [{ index: 0, message: { role: 'assistant', content, ...(toolCalls ? { tool_calls: toolCalls } : {}) }, finish_reason: finish }],
        usage,
      });
      return;
    }
    const chunk = (delta, finishReason = null, extra = {}) => `data: ${JSON.stringify({
      id, object: 'chat.completion.chunk', created, model: body.model,
      choices: delta === null ? [] : [{ index: 0, delta, finish_reason: finishReason }], ...extra,
    })}\n\n`;
    let stream = chunk({ role: 'assistant', content: '' });
    stream += toolCalls
      ? chunk({ tool_calls: toolCalls.map((call, index) => ({ index, ...call })) })
      : chunk({ content });
    stream += chunk({}, finish);
    stream += chunk(null, null, { usage });
    stream += 'data: [DONE]\n\n';
    res.writeHead(200, { 'content-type': 'text/event-stream', 'cache-control': 'no-cache' });
    res.end(stream);
  }

  async function handle(req, res) {
    stats.requests += 1;
    const url = new URL(req.url, 'http://127.0.0.1');
    const route = `${req.method} ${url.pathname}`;
    let raw = '';
    let size = 0;
    for await (const chunk of req) {
      size += chunk.length;
      if (size > MAX_BODY_BYTES) {
        json(res, 413, { error: 'request body too large' });
        return;
      }
      raw += chunk;
    }
    let body = null;
    if (raw) {
      try {
        body = JSON.parse(raw);
      } catch {
        json(res, 400, { error: 'request body is not JSON' });
        return;
      }
    }
    switch (route) {
      case 'GET /':
      case 'HEAD /':
        res.writeHead(200, { 'content-type': 'text/plain' });
        res.end('Ollama is running');
        return;
      case 'GET /api/version':
        json(res, 200, { version });
        return;
      case 'GET /api/status':
        json(res, 200, cloud === 'unreported' ? {} : { cloud: { disabled: cloud === 'disabled', source: cloud === 'disabled' ? 'config' : '' } });
        return;
      case 'GET /api/tags':
        json(res, 200, { models: models.map(tagRecord) });
        return;
      case 'GET /api/ps':
        json(res, 200, {
          models: models.map((model) => ({ ...tagRecord(model), expires_at: '2099-01-01T00:00:00Z', size_vram: modelSize, context_length: context, runner: 'llamacpp' })),
        });
        return;
      case 'POST /api/show':
        if (!known(body?.model ?? body?.name)) {
          json(res, 404, { error: `model '${body?.model ?? body?.name}' not found` });
          return;
        }
        json(res, 200, {
          modified_at: CREATED_AT, details, capabilities: ['completion', 'tools'],
          model_info: { 'general.architecture': 'stub', 'general.context_length': context },
        });
        return;
      case 'POST /api/generate':
        if (!known(body?.model)) {
          json(res, 404, { error: `model '${body?.model}' not found` });
          return;
        }
        if (!body?.prompt) {
          json(res, 200, { model: body.model, created_at: CREATED_AT, response: '', done: true, done_reason: 'load' });
          return;
        }
        json(res, 200, { model: body.model, created_at: CREATED_AT, response: 'ready', done: true, done_reason: 'stop', prompt_eval_count: estimateTokens(body.prompt), eval_count: 1 });
        return;
      case 'POST /api/pull':
        stats.pulls += 1;
        log({ path: '/api/pull', model: body?.model ?? body?.name });
        res.writeHead(200, { 'content-type': 'application/x-ndjson' });
        res.end(`${JSON.stringify({ status: 'success' })}\n`);
        return;
      case 'POST /api/chat':
        ollamaChat(body, res);
        return;
      case 'GET /v1/models':
      case 'GET /api/v1/models':
        json(res, 200, { object: 'list', data: models.map((id) => ({ id, object: 'model', created: 1792368000, owned_by: 'axocoatl-gate-stub' })) });
        return;
      case 'POST /v1/chat/completions':
      case 'POST /api/v1/chat/completions':
        openaiChat(body, res);
        return;
      case 'GET /__gate/stats':
        json(res, 200, stats);
        return;
      case 'POST /__gate/reset':
        Object.assign(stats, { requests: 0, chats: 0, writer: 0, reviewer: 0, probes: 0, other: 0, pulls: 0, edits: [], unknown: [] });
        json(res, 200, { ok: true });
        return;
      default:
        stats.unknown.push(route);
        log({ path: url.pathname, method: req.method, unknown: true });
        json(res, 404, { error: `the gate stub does not serve ${route}` });
    }
  }

  const server = createServer((req, res) => {
    handle(req, res).catch((error) => {
      if (!res.headersSent) json(res, 500, { error: String(error?.message || error) });
      else res.end();
    });
  });
  server.stats = stats;
  return server;
}

/** Start a stub on 127.0.0.1 and resolve its base URL. */
export async function startStubServer(options = {}) {
  const server = createStubServer(options);
  await new Promise((resolve, reject) => {
    server.once('error', reject);
    server.listen(options.port ?? 0, '127.0.0.1', resolve);
  });
  const { port } = server.address();
  return { server, port, baseUrl: `http://127.0.0.1:${port}` };
}

function parseArguments(argv) {
  const options = {};
  const take = (index, flag) => {
    const value = argv[index + 1];
    if (value === undefined) throw new Error(`${flag} needs a value`);
    return value;
  };
  for (let index = 0; index < argv.length; index += 1) {
    const flag = argv[index];
    switch (flag) {
      case '--port': options.port = Number(take(index, flag)); index += 1; break;
      case '--port-file': options.portFile = take(index, flag); index += 1; break;
      case '--fixes': options.fixesFile = take(index, flag); index += 1; break;
      case '--models': options.models = take(index, flag).split(',').map((model) => model.trim()).filter(Boolean); index += 1; break;
      case '--version': options.version = take(index, flag); index += 1; break;
      case '--cloud': options.cloud = take(index, flag); index += 1; break;
      case '--context': options.context = Number(take(index, flag)); index += 1; break;
      case '--model-size': options.modelSize = Number(take(index, flag)); index += 1; break;
      case '--log': options.logFile = take(index, flag); index += 1; break;
      default: throw new Error(`unknown argument ${flag}`);
    }
  }
  if (options.port !== undefined && (!Number.isInteger(options.port) || options.port < 0 || options.port > 65535)) {
    throw new Error('--port must be 0 to 65535');
  }
  if (options.cloud && !['disabled', 'enabled', 'unreported'].includes(options.cloud)) {
    throw new Error('--cloud must be disabled, enabled or unreported');
  }
  if (options.context !== undefined && (!Number.isInteger(options.context) || options.context < 1)) {
    throw new Error('--context must be a positive integer');
  }
  return options;
}

if (process.argv[1] && path.resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  let options;
  try {
    options = parseArguments(process.argv.slice(2));
  } catch (error) {
    console.error(`stub-model-server: ${error.message}`);
    process.exit(2);
  }
  const { server, baseUrl, port } = await startStubServer(options);
  if (options.portFile) writeFileSync(options.portFile, `${port}\n`);
  console.log(`stub-model-server: listening on ${baseUrl} (Ollama ${options.version ?? OLLAMA_VERSION}, cloud ${options.cloud ?? 'disabled'})`);
  const stop = () => server.close(() => process.exit(0));
  process.on('SIGTERM', stop);
  process.on('SIGINT', stop);
}
