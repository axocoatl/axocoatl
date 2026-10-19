// What stand-ins for Ollama answer to native Axocoatl's checks of the
// server: the audited version, and the two contract probes Axocoatl sends
// before a Team uses a model (an over-long prompt Ollama 0.40.6 refuses with
// HTTP 400, and a one-token request that stops after one token).

/** The Ollama release native Axocoatl was audited against. */
export const OLLAMA_VERSION = '0.40.6';

/** The stop probe's prompt (`axocoatl_llm_ollama::CONTRACT_STOP_PROBE_PROMPT`). */
const STOP_PROBE_PROMPT = 'Reply with the single word: ready';

/** Whether a parsed `/api/chat` body is one of the contract probes. */
export function isContractProbe(body) {
  if (body?.options?.num_predict !== 1 || body?.truncate !== false) return false;
  const content = body?.messages?.[0]?.content;
  return content === STOP_PROBE_PROMPT || (typeof content === 'string' && content.startsWith('x x '));
}

/**
 * Answer a contract probe on `res` the way Ollama 0.40.6 does and return
 * true, or return false for any other request.
 */
export function answerContractProbe(url, body, res) {
  if (url !== '/api/chat' || !isContractProbe(body)) return false;
  if (body.messages[0].content !== STOP_PROBE_PROMPT) {
    res.writeHead(400, { 'content-type': 'application/json' });
    res.end(JSON.stringify({ error: 'request exceeds the available context size, try increasing it' }));
    return true;
  }
  const record = { model: body.model, created_at: '2026-10-19T00:00:00Z', message: { role: 'assistant', content: 'ready' }, done: true, done_reason: 'length', prompt_eval_count: 14, eval_count: 1 };
  res.writeHead(200, { 'content-type': 'application/x-ndjson' });
  res.end(`${JSON.stringify(record)}\n`);
  return true;
}
