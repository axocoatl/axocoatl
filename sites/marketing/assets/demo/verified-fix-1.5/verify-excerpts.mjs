#!/usr/bin/env node
// No network, inference, writes, or dependencies. With --repo, verify against frozen Git evidence.
import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { readFileSync } from 'node:fs';
import { dirname, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { execFileSync } from 'node:child_process';

const directory = dirname(fileURLToPath(import.meta.url));
const args = process.argv.slice(2);
assert(args.length === 0 || (args.length === 2 && args[0] === '--repo'),
  'Usage: node verify-excerpts.mjs [--repo /path/to/axocoatl]');
const repository = args.length ? resolve(args[1]) : null;
const hash = (bytes) => createHash('sha256').update(bytes).digest('hex');
const provenance = JSON.parse(readFileSync(resolve(directory, 'provenance.json')));
const excerptBytes = readFileSync(resolve(directory, 'excerpts.json'));
const { excerpts } = JSON.parse(excerptBytes);
const frozen = '19e3c89ca7b4d02cd742bccd671f7274e68b95d7';
const candidate = 'b222302f32639c9211255ab4f818c3262bce856a';
const prefix = `release-evidence/1.5.0/${candidate}/`;
assert.equal(provenance.frozen_evidence_commit, frozen);
assert.equal(provenance.candidate_commit, candidate);
assert.equal(provenance.run_id, 'run-a2c57675-d55d-4abb-9993-d0c27d4a6fd3');
assert.equal(hash(excerptBytes), provenance.excerpts_sha256);
assert.deepEqual(excerpts.map((entry) => entry.id),
  ['request', 'patch', 'checks-review', 'original-tests', 'receipt']);
const sources = new Map();
for (const source of provenance.sources) {
  assert(source.path.startsWith(prefix) && !source.path.split('/').includes('..'));
  assert(!sources.has(source.path), `duplicate source: ${source.path}`);
  sources.set(source.path, source);
}
for (const excerpt of excerpts) {
  assert(sources.has(excerpt.source));
  assert.equal(hash(excerpt.text), excerpt.sha256, excerpt.id);
  assert(excerpt.lines.length === 2 && excerpt.lines.every(Number.isInteger));
  assert(excerpt.lines[0] > 0 && excerpt.lines[1] >= excerpt.lines[0]);
  assert(!/\/Users\/|\/private\/|Bearer\s|[?&](?:code|token)=/i.test(excerpt.text),
    `private location or authentication material in ${excerpt.id}`);
}
if (repository) {
  const readFrozen = (path) => execFileSync('git', ['show', `${frozen}:${path}`],
    { cwd: repository, maxBuffer: 8 * 1024 * 1024 });
  const manifest = JSON.parse(readFrozen(`${prefix}manifest.json`));
  const inventory = new Map(manifest.files.map((file) => [prefix + file.path, file]));
  for (const source of sources.values()) {
    const entry = inventory.get(source.path);
    assert(entry, `not in signed inventory: ${source.path}`);
    const bytes = readFrozen(source.path);
    assert.equal(hash(bytes), source.sha256, source.path);
    assert.equal(bytes.length, source.bytes, source.path);
    assert.equal(source.sha256, entry.sha256, source.path);
    assert.equal(source.bytes, entry.bytes, source.path);
    source.bytesRead = bytes;
  }
  for (const excerpt of excerpts) {
    const text = sources.get(excerpt.source).bytesRead.toString('utf8');
    const lines = text.match(/[^\n]*\n|[^\n]+$/g) || [];
    assert(excerpt.lines[1] <= lines.length);
    assert.equal(lines.slice(excerpt.lines[0] - 1, excerpt.lines[1]).join(''),
      excerpt.text, `nonliteral excerpt: ${excerpt.id}`);
  }
  const assessment = JSON.parse(readFrozen(`${prefix}C1-local/acceptance/readme-node-deps/assessment.json`));
  assert.equal(assessment.result, 'ACCEPTED');
  assert.equal(assessment.original_tests_unchanged, true);
  assert.equal(assessment.all_other_tracked_files_unchanged, true);
  assert.equal(assessment.host_test.exit_code, 0);
  assert.equal(assessment.verification.receipt_exit, 0);
  assert.equal(assessment.verification.record_exit, 0);
  assert.equal(assessment.negative_evidence.tool_errors.length, 2);
  assert.equal(assessment.negative_evidence.provider_retries.length, 0);
  console.log(`Verified ${excerpts.length} literal excerpts and ${sources.size} source hashes against the frozen signed evidence inventory.`);
} else {
  console.log(`Verified ${excerpts.length} excerpt asset hashes. Use --repo to check frozen source bytes and signed inventory membership.`);
}
