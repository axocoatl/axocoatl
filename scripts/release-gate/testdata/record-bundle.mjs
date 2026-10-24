// Record bundles for the gate's self-tests and its stand-in axocoatl,
// written as the product writes them (crates/axocoatl-session/src/
// record_bundle.rs, BundleWriter): a header line, one line per section, and
// an end line that counts the section lines and holds the SHA-256 of every
// byte before it. The `history` section is the one a 1.5.0 run wrote
// (record-history-1.5.0.json: the Session export of a passing `run fix` on
// the rust fixture, with its activations' tool streams and repository
// snapshots left out), with the answers it accepted replaced.
// It is test scaffolding, never the product.
import { createHash } from 'node:crypto';
import { readFileSync } from 'node:fs';

/** A bundle's text from its header and `[section, data]` pairs. */
export function bundleText(header, sections) {
  const body = [header, ...sections.map(([section, data]) => ({ section, data }))]
    .map((line) => `${JSON.stringify(line)}\n`)
    .join('');
  const sha256 = createHash('sha256').update(body).digest('hex');
  return `${body}${JSON.stringify({ section: 'end', lines: sections.length, sha256 })}\n`;
}

/** The 1.5.0 history, its two accepted activations (writer, then reviewer)
 *  answering `writer` and `reviewer`. */
export function history(writer, reviewer) {
  const entries = JSON.parse(readFileSync(new URL('record-history-1.5.0.json', import.meta.url), 'utf8'));
  const [first, second] = entries[0].turn.activations;
  for (const [activation, text] of [[first, writer], [second, reviewer]]) {
    activation.output.content.text = text;
    for (const reserved of activation.reserved_outputs) reserved.content.output.text = text;
  }
  return entries;
}

/** A record of a run in `sessionId` that started at commit `repoHead`, with
 *  `verdict` (pass 0, checks_failed 1, needs_attention 2, error 5,
 *  interrupted 6) and `entries` as its history. */
export function record({ runId = 'run-00000000-0000-4000-8000-000000000000', sessionId = 'ses-00000000-0000-4000-8000-000000000000',
  repoHead = '0'.repeat(40), verdict = 'pass', entries = history('Fixed it.', 'VERDICT: APPROVE') } = {}) {
  const exit = { pass: 0, checks_failed: 1, needs_attention: 2, interrupted: 6, error: 5 }[verdict];
  return bundleText(
    { schema: 'axocoatl.record-bundle/2', run_id: runId, session_id: sessionId, created_at_ms: 1792697490606, axocoatl_version: '1.5.0' },
    [
      ['manifest', { schema: 'axocoatl.run-manifest/1', run_id: runId, session_id: sessionId, repo_head: repoHead, dirty_paths: [] }],
      ['outcome', { schema: 'axocoatl.run-outcome/1', run_id: runId, session_id: sessionId, verdict, exit_code: exit }],
      ['history', entries],
    ],
  );
}
