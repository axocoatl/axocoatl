import assert from 'node:assert/strict';
import { execFileSync, spawnSync } from 'node:child_process';
import { mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { dirname, join, resolve } from 'node:path';
import test from 'node:test';

import { loadPortfolio, readJson, repoRoot, sha256File } from './film-lib.mjs';
import {
  assertSourceBinaryOnlyRewrite,
  canonicalDeltaDigest,
  filmArtifactSet,
  gitDiffEntries,
  revisionIdentity,
  sourceContentDigestAtRevision,
  validateCompatibilityAttestation,
  verifyReleaseCompatibilityDocument,
} from './release-compatibility.mjs';

const attestationPath = resolve(repoRoot, 'demo/one-app/films/compatibility/v1.0.1.json');
const attestation = JSON.parse(readFileSync(attestationPath, 'utf8'));
const baselineCommit = '60b1bce3d5293495409b0ef2c8f7fff1b4ba1e4b';

function git(root, ...args) {
  return execFileSync('git', args, { cwd: root, encoding: 'utf8', stdio: ['ignore', 'pipe', 'pipe'] }).trim();
}

function revisionClone(t, revision, name) {
  const parent = mkdtempSync(join(tmpdir(), `axocoatl-v101-film-${name}-`));
  const root = join(parent, name);
  t.after(() => rmSync(parent, { recursive: true, force: true }));
  execFileSync('git', ['clone', '--quiet', '--shared', '--no-checkout', repoRoot, root]);
  git(root, 'checkout', '--quiet', '--detach', revision);
  return root;
}

function changed(document, mutate) {
  const copy = structuredClone(document);
  mutate(copy);
  return copy;
}

test('v1.0.1 incident compatibility restores first-committed declarations and binds all 55 paths', (t) => {
  const releaseRoot = revisionClone(t, attestation.release.commit, 'release-source');
  const portfolio = loadPortfolio(releaseRoot);
  const options = { releaseRoot, controlRoot: repoRoot, portfolio };

  const result = verifyReleaseCompatibilityDocument(attestation, options);
  assert.deepEqual(result, {
    tag: 'v1.0.1',
    commit: 'e82902bdfabb0541e466d5f98f0013cea36bdeab',
    films: 12,
    changedPaths: 55,
    nonRecordingPaths: 43,
    provenanceRewrites: 12,
    artifacts: 153,
  });
  assert.equal(
    sourceContentDigestAtRevision(releaseRoot, attestation.first_committed_provenance.materialized_source.commit),
    '27ceddcd7c6f177767025f7c15a2cf2f9bb95fde19df365f3a18b05afc07cfdf',
  );
  assert.equal(
    sourceContentDigestAtRevision(releaseRoot, attestation.release.commit),
    'a5644419d0f51bcf9a012ed8bccd4a57c6674f517700d0503858ddeaf814c023',
  );
  assert.equal(
    gitDiffEntries(
      releaseRoot,
      attestation.first_committed_provenance.materialized_source.commit,
      attestation.release.commit,
    ).length,
    55,
  );

  const command = spawnSync(process.execPath, [
    resolve(repoRoot, 'demo/one-app/films/verify-film-set.mjs'),
    '--release-compatibility',
    attestationPath,
    '--release-root',
    releaseRoot,
  ], { cwd: tmpdir(), encoding: 'utf8' });
  assert.equal(command.status, 0, command.stderr || command.stdout);
  assert.match(command.stdout, /55 exact changed paths: 43 non-recording \+ 12 audited source\/binary-only rewrites/);
  assert.match(command.stdout, /not captured with the patch binary/);

  const ordinarySourceBound = spawnSync(process.execPath, [
    resolve(releaseRoot, 'demo/one-app/films/verify-film-set.mjs'),
    '--source-bound',
  ], { cwd: tmpdir(), encoding: 'utf8' });
  assert.notEqual(ordinarySourceBound.status, 0, 'ordinary source-bound verification must remain strict');
  assert.match(ordinarySourceBound.stderr, /source content differs from the recorded checkout/);

  const baselineRoot = revisionClone(t, baselineCommit, 'first-provenance-commit');
  const baselineSourceBound = spawnSync(process.execPath, [
    resolve(baselineRoot, 'demo/one-app/films/verify-film-set.mjs'),
    '--source-bound',
  ], { cwd: tmpdir(), encoding: 'utf8' });
  assert.equal(baselineSourceBound.status, 0, baselineSourceBound.stderr || baselineSourceBound.stdout);

  assert.throws(
    () => verifyReleaseCompatibilityDocument(changed(attestation, value => {
      value.release.commit = 'f9214961efd0408b7fc312f19c6ea8a907cb3303';
    }), options),
    /HEAD does not match/,
  );

  const laterBaseline = '38d19386637c6850446668d651b7e7201dd88886';
  assert.throws(
    () => verifyReleaseCompatibilityDocument(changed(attestation, value => {
      value.first_committed_provenance.materialized_source.commit = laterBaseline;
      value.first_committed_provenance.materialized_source.tree = revisionIdentity(releaseRoot, laterBaseline).tree;
      value.first_committed_provenance.materialized_source.source_content_sha256 =
        sourceContentDigestAtRevision(releaseRoot, laterBaseline);
      value.first_committed_provenance.source.content_sha256 =
        value.first_committed_provenance.materialized_source.source_content_sha256;
    }), options),
    /not the first provenance commit directly after the declared source head/,
  );

  assert.throws(
    () => verifyReleaseCompatibilityDocument(changed(attestation, value => {
      value.first_committed_provenance.source.head = '38d19386637c6850446668d651b7e7201dd88886';
    }), options),
    /not the first provenance commit directly after|restored source declaration differs/,
  );
  assert.throws(
    () => verifyReleaseCompatibilityDocument(changed(attestation, value => {
      value.first_committed_provenance.binary.sha256 = '0'.repeat(64);
    }), options),
    /restored binary declaration differs/,
  );
  assert.throws(
    () => verifyReleaseCompatibilityDocument(changed(attestation, value => {
      value.first_committed_provenance.files[0].first_sha256 = '0'.repeat(64);
    }), options),
    /restored first-committed provenance bytes changed/,
  );

  const tamperedControl = join(mkdtempSync(join(tmpdir(), 'axocoatl-v101-film-control-')), 'control');
  t.after(() => rmSync(dirname(tamperedControl), { recursive: true, force: true }));
  const controlPortfolio = resolve(tamperedControl, attestation.portfolio.path);
  mkdirSync(dirname(controlPortfolio), { recursive: true });
  writeFileSync(controlPortfolio, readFileSync(resolve(repoRoot, attestation.portfolio.path)));
  const blobMismatch = structuredClone(attestation);
  blobMismatch.first_committed_provenance.binary.version = 'tampered-declaration';
  for (const file of blobMismatch.first_committed_provenance.files) {
    const target = resolve(tamperedControl, file.path);
    mkdirSync(dirname(target), { recursive: true });
    const provenance = readJson(resolve(repoRoot, file.path));
    provenance.binary.version = 'tampered-declaration';
    writeFileSync(target, `${JSON.stringify(provenance, null, 2)}\n`);
    file.first_sha256 = sha256File(target);
  }
  blobMismatch.portfolio.restored_artifact_set_sha256 =
    filmArtifactSet(releaseRoot, portfolio, tamperedControl).sha256;
  assert.throws(
    () => verifyReleaseCompatibilityDocument(blobMismatch, {
      releaseRoot,
      controlRoot: tamperedControl,
      portfolio,
    }),
    /first-committed Git blob digest differs|do not match the first-committed Git blob/,
  );
  assert.throws(
    () => verifyReleaseCompatibilityDocument(changed(attestation, value => {
      value.portfolio.restored_artifact_set_sha256 = '0'.repeat(64);
    }), options),
    /restored film artifact-set bytes changed/,
  );
  assert.throws(
    () => verifyReleaseCompatibilityDocument(changed(attestation, value => {
      value.delta.entries[0].new_blob = '0'.repeat(40);
      value.delta.entries_sha256 = canonicalDeltaDigest(value.delta.entries);
    }), options),
    /release delta changed/,
  );
  assert.throws(
    () => validateCompatibilityAttestation(changed(attestation, value => {
      [value.delta.entries[0], value.delta.entries[1]] = [value.delta.entries[1], value.delta.entries[0]];
    })),
    /bytewise sorted/,
  );
  assert.throws(
    () => validateCompatibilityAttestation(changed(attestation, value => {
      value.protected.paths = [];
    })),
    /verifier-required filmed product surface/,
  );
  assert.throws(
    () => verifyReleaseCompatibilityDocument(changed(attestation, value => {
      value.runtime_changed_paths = value.runtime_changed_paths.filter(path => path !== 'Cargo.lock');
    }), options),
    /runtime_changed_paths no longer matches/,
  );
  assert.throws(
    () => validateCompatibilityAttestation(changed(attestation, value => {
      value.protected.excluded_paths.push('../escape');
    })),
    /bytewise sorted|canonical|only the 12 audited/,
  );

  const firstFile = attestation.first_committed_provenance.files[0].path;
  const restored = readJson(resolve(repoRoot, firstFile));
  const beyondSourceBinary = structuredClone(restored);
  beyondSourceBinary.source = structuredClone(attestation.frozen_release_provenance_rewrite.source);
  beyondSourceBinary.binary = structuredClone(attestation.frozen_release_provenance_rewrite.binary);
  beyondSourceBinary.capture.theme = beyondSourceBinary.capture.theme === 'light' ? 'dark' : 'light';
  assert.throws(
    () => assertSourceBinaryOnlyRewrite(restored, beyondSourceBinary, firstFile),
    /changed beyond the audited source\/binary rewrite/,
  );

  const capture = resolve(releaseRoot, 'demo/one-app/films/source/session-workbench/capture.json');
  writeFileSync(capture, `${readFileSync(capture, 'utf8')} `);
  assert.throws(
    () => verifyReleaseCompatibilityDocument(attestation, options),
    /clean frozen checkout/,
  );
});
