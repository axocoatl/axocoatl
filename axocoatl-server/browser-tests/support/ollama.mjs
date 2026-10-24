// What stand-ins for Ollama answer to native Axocoatl's checks of the
// server: the audited version, and the contract probes Axocoatl sends before
// a Team uses a model, answered as Ollama 0.40 does
// (`axocoatl_llm_ollama::ContractProbe`): an over-long prompt refused with
// HTTP 400, a one-token request that stops after one token, a long answer
// that stops at its limit, and a request to call the offered tool that calls
// it.

/** The Ollama release native Axocoatl was audited against. */
export const OLLAMA_VERSION = '0.40.6';

/** The probes' prompts (`axocoatl_llm_ollama::ContractProbe`). */
const STOP_PROBE_PROMPT = 'Reply with the single word: ready';
const LONG_OUTPUT_PROBE_PROMPT = 'Write the numbers from 1 to 300, one per line. Write only the numbers.';
const TOOL_PROBE_PROMPT = 'Call the report_ready tool with word set to ready.';

/** Which probe a parsed `/api/chat` body is: `over-long`, `stop`, `long`, `tool`, or null. */
function contractProbe(body) {
  if (body?.truncate !== false) return null;
  const content = body?.messages?.[0]?.content;
  if (content === STOP_PROBE_PROMPT) return 'stop';
  if (content === LONG_OUTPUT_PROBE_PROMPT) return 'long';
  if (content === TOOL_PROBE_PROMPT) return 'tool';
  return typeof content === 'string' && content.startsWith('x x ') ? 'over-long' : null;
}

/** Whether a parsed `/api/chat` body is one of the contract probes. */
export function isContractProbe(body) {
  return contractProbe(body) !== null;
}

/**
 * Answer a contract probe on `res` the way Ollama 0.40 does and return
 * true, or return false for any other request.
 */
export function answerContractProbe(url, body, res) {
  const probe = url === '/api/chat' ? contractProbe(body) : null;
  if (probe === null) return false;
  if (probe === 'over-long') {
    res.writeHead(400, { 'content-type': 'application/json' });
    res.end(JSON.stringify({ error: 'request exceeds the available context size, try increasing it' }));
    return true;
  }
  const record = (message, fields) => ({ model: body.model, created_at: '2026-10-19T00:00:00Z', message: { role: 'assistant', ...message }, ...fields });
  const records = {
    stop: [record({ content: 'ready' }, { done: true, done_reason: 'length', prompt_eval_count: 14, eval_count: 1 })],
    long: [
      record({ content: '1\n2\n3\n' }, { done: false }),
      record({ content: '' }, { done: true, done_reason: 'length', prompt_eval_count: 33, eval_count: body.options?.num_predict }),
    ],
    tool: [record(
      { content: '', tool_calls: [{ id: 'call_probe', function: { index: 0, name: 'report_ready', arguments: { word: 'ready' } } }] },
      { done: true, done_reason: 'stop', prompt_eval_count: 138, eval_count: 31 },
    )],
  }[probe];
  res.writeHead(200, { 'content-type': 'application/x-ndjson' });
  res.end(records.map((entry) => `${JSON.stringify(entry)}\n`).join(''));
  return true;
}
