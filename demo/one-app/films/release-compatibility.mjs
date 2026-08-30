import { createHash } from 'node:crypto';
import { existsSync, lstatSync, readFileSync } from 'node:fs';
import { isDeepStrictEqual } from 'node:util';
import { relative, resolve, sep } from 'node:path';
import { spawnSync } from 'node:child_process';

import {
  provenanceSourceExcludes,
  readJson,
  sha256Buffer,
  sha256File,
} from './film-lib.mjs';

const sha1Pattern = /^[0-9a-f]{40}$/;
const sha256Pattern = /^[0-9a-f]{64}$/;
const classifications = new Set([
  'dependency-lock',
  'documentation',
  'generated-notice',
  'governance',
  'historical-provenance-rewrite',
  'installer',
  'marketing',
  'onboarding-cli',
  'release-control',
  'release-metadata',
  'runtime-build-test-fix',
  'test-coverage',
]);
export const firstCommittedDisclosure =
  'These are the earliest committed provenance declarations. The capture binary bytes are not preserved, so its path, version, and hash remain declarations rather than an independently authenticated binary.';
export const frozenRewriteDisclosure =
  'The frozen v1.0.1 tree contains later source and binary field rewrites made without recapture. This incident attestation audits those twelve source/binary-only changes and does not treat them as capture identity or claim capture with v1.0.1.';
export const requiredProtectedPaths = Object.freeze([
  'Cargo.toml',
  'axocoatl-server/Cargo.toml',
  'axocoatl-server/src',
  'axocoatl-server/static',
  'crates',
  'demo/one-app',
  'packages',
  'sites/marketing/assets/films',
  'sites/marketing/components/ax-product-film.js',
  'sites/marketing/concepts',
  'sites/marketing/index.html',
  'sites/marketing/showcase',
  'sites/marketing/why',
]);
export const requiredSemanticsPreservingRuntimeFixes = Object.freeze([
  'axocoatl-server/src/routes.rs',
  'crates/axocoatl-coordination/src/htn.rs',
  'crates/axocoatl-daemon/src/bootstrap.rs',
  'crates/axocoatl-daemon/src/error.rs',
  'crates/axocoatl-memory/src/checkpoint.rs',
  'crates/axocoatl-memory/src/extract.rs',
]);
const sourceDigestCache = new Map();
const diffCache = new Map();

function fail(message) {
  throw new Error(message);
}

function assert(condition, message) {
  if (!condition) fail(message);
}

function requireObject(value, label) {
  assert(value && typeof value === 'object' && !Array.isArray(value), `${label} must be an object.`);
  return value;
}

function exactKeys(value, keys, label) {
  requireObject(value, label);
  const actual = Object.keys(value).sort();
  const expected = [...keys].sort();
  assert(isDeepStrictEqual(actual, expected), `${label} must contain exactly: ${expected.join(', ')}.`);
}

function requireString(value, label) {
  assert(typeof value === 'string' && value.trim() === value && value.length > 0, `${label} must be a non-empty trimmed string.`);
}

function requireHash(value, pattern, label) {
  assert(pattern.test(value || ''), `${label} has an invalid digest.`);
}

function safeRepoPath(path, label) {
  requireString(path, label);
  assert(!path.startsWith('/') && !path.includes('\\'), `${label} must be POSIX-relative.`);
  const parts = path.split('/');
  assert(parts.every(part => part && part !== '.' && part !== '..'), `${label} is not canonical.`);
  return path;
}

function sortedUnique(values, label) {
  assert(Array.isArray(values), `${label} must be an array.`);
  const sorted = [...values].sort((left, right) => Buffer.compare(Buffer.from(left), Buffer.from(right)));
  assert(isDeepStrictEqual(values, sorted), `${label} must be bytewise sorted.`);
  assert(new Set(values).size === values.length, `${label} must not contain duplicates.`);
}

function validateSourceDeclaration(value, label) {
  exactKeys(value, ['branch', 'head', 'dirty', 'patch_sha256', 'patch_excludes', 'content_sha256'], label);
  requireString(value.branch, `${label}.branch`);
  requireHash(value.head, sha1Pattern, `${label}.head`);
  assert(value.dirty === true, `${label}.dirty must remain true.`);
  requireHash(value.patch_sha256, sha256Pattern, `${label}.patch_sha256`);
  assert(isDeepStrictEqual(value.patch_excludes, [...provenanceSourceExcludes]), `${label}.patch_excludes changed.`);
  requireHash(value.content_sha256, sha256Pattern, `${label}.content_sha256`);
}

function validateBinaryDeclaration(value, label) {
  exactKeys(value, ['path', 'version', 'sha256'], label);
  safeRepoPath(value.path, `${label}.path`);
  requireString(value.version, `${label}.version`);
  requireHash(value.sha256, sha256Pattern, `${label}.sha256`);
}

function withoutSourceAndBinary(value) {
  const copy = structuredClone(value);
  delete copy.source;
  delete copy.binary;
  return copy;
}

export function assertSourceBinaryOnlyRewrite(firstCommitted, frozenRelease, label = 'provenance') {
  assert(
    isDeepStrictEqual(withoutSourceAndBinary(firstCommitted), withoutSourceAndBinary(frozenRelease)),
    `${label}: frozen provenance changed beyond the audited source/binary rewrite.`,
  );
}

function runGit(root, args, options = {}) {
  const result = spawnSync('git', args, {
    cwd: root,
    encoding: options.encoding === null ? null : 'utf8',
    input: options.input,
    maxBuffer: 256 * 1024 * 1024,
  });
  if (result.error) fail(`git ${args.join(' ')} could not run: ${result.error.message}`);
  if (result.status !== 0) {
    const detail = String(result.stderr || result.stdout || '').trim();
    fail(`git ${args.join(' ')} failed (${result.status})${detail ? `: ${detail}` : ''}`);
  }
  return result.stdout;
}

function parseTree(root, revision) {
  const raw = runGit(root, ['ls-tree', '-rz', '--full-tree', '-r', revision], { encoding: null });
  return String(raw)
    .split('\0')
    .filter(Boolean)
    .map(record => {
      const tab = record.indexOf('\t');
      assert(tab > 0, `Malformed Git tree record for ${revision}.`);
      const [mode, type, object] = record.slice(0, tab).split(' ');
      const path = record.slice(tab + 1);
      assert(type === 'blob', `Film source tree may contain only blobs: ${path} is ${type}.`);
      assert(['100644', '100755', '120000'].includes(mode), `Unsupported Git mode ${mode} for ${path}.`);
      safeRepoPath(path, `Git tree path ${path}`);
      requireHash(object, sha1Pattern, `Git object for ${path}`);
      return { mode, object, path };
    });
}

function readBlobBatch(root, objects) {
  const unique = [...new Set(objects)];
  if (!unique.length) return new Map();
  const output = runGit(root, ['cat-file', '--batch'], {
    encoding: null,
    input: Buffer.from(`${unique.join('\n')}\n`),
  });
  const blobs = new Map();
  let offset = 0;
  for (const requested of unique) {
    const lineEnd = output.indexOf(0x0a, offset);
    assert(lineEnd >= 0, `Missing cat-file header for ${requested}.`);
    const header = output.subarray(offset, lineEnd).toString('utf8');
    const [object, type, rawSize] = header.split(' ');
    assert(object === requested && type === 'blob' && /^\d+$/.test(rawSize || ''), `Invalid cat-file header for ${requested}.`);
    const size = Number(rawSize);
    const start = lineEnd + 1;
    const end = start + size;
    assert(end < output.length && output[end] === 0x0a, `Truncated cat-file body for ${requested}.`);
    blobs.set(requested, output.subarray(start, end));
    offset = end + 1;
  }
  assert(offset === output.length, 'Unexpected bytes after the Git blob batch.');
  return blobs;
}

export function revisionIdentity(root, revision) {
  const commit = String(runGit(root, ['rev-parse', `${revision}^{commit}`])).trim();
  const tree = String(runGit(root, ['rev-parse', `${revision}^{tree}`])).trim();
  requireHash(commit, sha1Pattern, `${revision} commit`);
  requireHash(tree, sha1Pattern, `${revision} tree`);
  return { commit, tree };
}

export function sourceContentDigestAtRevision(root, revision) {
  const identity = revisionIdentity(root, revision);
  const cacheKey = `${resolve(root)}\0${identity.commit}`;
  if (sourceDigestCache.has(cacheKey)) return sourceDigestCache.get(cacheKey);
  const entries = parseTree(root, identity.commit)
    .filter(entry => !provenanceSourceExcludes.some(prefix => entry.path.startsWith(prefix)))
    .sort((left, right) => Buffer.compare(Buffer.from(left.path), Buffer.from(right.path)));
  const blobs = readBlobBatch(root, entries.map(entry => entry.object));
  const digest = createHash('sha256');
  digest.update('axocoatl-source-content-v1\0');
  for (const entry of entries) {
    const type = entry.mode === '120000' ? 'symlink' : entry.mode === '100755' ? 'file+x' : 'file';
    const contentHash = sha256Buffer(blobs.get(entry.object));
    digest.update(`${type}\0${entry.path}\0${contentHash}\n`);
  }
  const value = digest.digest('hex');
  sourceDigestCache.set(cacheKey, value);
  return value;
}

export function gitDiffEntries(root, baseline, release) {
  const baselineCommit = revisionIdentity(root, baseline).commit;
  const releaseCommit = revisionIdentity(root, release).commit;
  const cacheKey = `${resolve(root)}\0${baselineCommit}\0${releaseCommit}`;
  if (diffCache.has(cacheKey)) return structuredClone(diffCache.get(cacheKey));
  const raw = runGit(root, [
    'diff-tree', '-r', '--no-commit-id', '--no-renames', '--raw', '-z', baselineCommit, releaseCommit,
  ], { encoding: null });
  const fields = String(raw).split('\0').filter(Boolean);
  assert(fields.length % 2 === 0, 'Malformed raw Git diff output.');
  const entries = [];
  for (let index = 0; index < fields.length; index += 2) {
    const header = fields[index];
    const path = fields[index + 1];
    const parts = header.split(' ');
    assert(parts.length === 5 && parts[0].startsWith(':'), `Malformed raw Git diff header: ${header}`);
    const oldMode = parts[0].slice(1);
    const newMode = parts[1];
    const oldObject = parts[2];
    const newObject = parts[3];
    const status = parts[4];
    assert(['A', 'D', 'M'].includes(status), `Unsupported Git delta status ${status} for ${path}.`);
    safeRepoPath(path, `Changed path ${path}`);
    entries.push({
      path,
      status,
      old_mode: oldMode === '000000' ? null : oldMode,
      new_mode: newMode === '000000' ? null : newMode,
      old_blob: /^0{40}$/.test(oldObject) ? null : oldObject,
      new_blob: /^0{40}$/.test(newObject) ? null : newObject,
    });
  }
  entries.sort((left, right) => Buffer.compare(Buffer.from(left.path), Buffer.from(right.path)));
  diffCache.set(cacheKey, entries);
  return structuredClone(entries);
}

export function canonicalDeltaDigest(entries) {
  const digest = createHash('sha256');
  digest.update('axocoatl-film-release-delta-v1\0');
  for (const entry of entries) {
    digest.update([
      entry.status,
      entry.path,
      entry.old_mode ?? '-',
      entry.new_mode ?? '-',
      entry.old_blob ?? '-',
      entry.new_blob ?? '-',
      entry.classification,
    ].join('\0'));
    digest.update('\n');
  }
  return digest.digest('hex');
}

function gitPathObject(root, revision, path) {
  return String(runGit(root, ['rev-parse', `${revision}:${path}`])).trim();
}

function gitPathExists(root, revision, path) {
  const result = spawnSync('git', ['cat-file', '-e', `${revision}:${path}`], {
    cwd: root,
    encoding: 'utf8',
  });
  if (result.error) fail(`git cat-file could not run: ${result.error.message}`);
  if (result.status === 0) return true;
  if (result.status === 128) return false;
  const detail = String(result.stderr || result.stdout || '').trim();
  fail(`git cat-file -e failed (${result.status})${detail ? `: ${detail}` : ''}`);
}

export function protectedObjectsDigest(root, revision, paths) {
  const digest = createHash('sha256');
  digest.update('axocoatl-film-protected-paths-v1\0');
  for (const path of paths) {
    const object = gitPathObject(root, revision, path);
    requireHash(object, sha1Pattern, `${revision}:${path}`);
    const type = String(runGit(root, ['cat-file', '-t', object])).trim();
    assert(['blob', 'tree'].includes(type), `${revision}:${path} must identify a blob or tree.`);
    digest.update(`${path}\0${type}\0${object}\n`);
  }
  return digest.digest('hex');
}

export function protectedContentDigest(root, revision, paths, excludedPaths) {
  const excluded = new Set(excludedPaths);
  const entries = parseTree(root, revision)
    .filter(entry => paths.some(path => entry.path === path || entry.path.startsWith(`${path}/`)))
    .filter(entry => !excluded.has(entry.path))
    .sort((left, right) => Buffer.compare(Buffer.from(left.path), Buffer.from(right.path)));
  assert(entries.length > 0, 'protected film surface must contain at least one Git blob.');
  const digest = createHash('sha256');
  digest.update('axocoatl-film-protected-content-v1\0');
  for (const entry of entries) digest.update(`${entry.mode}\0${entry.path}\0${entry.object}\n`);
  return { count: entries.length, sha256: digest.digest('hex') };
}

function releasePath(root, path) {
  safeRepoPath(path, `Release path ${path}`);
  const absolute = resolve(root, path);
  const back = relative(root, absolute);
  assert(back && back !== '..' && !back.startsWith(`..${sep}`), `Release path escapes the checkout: ${path}`);
  assert(existsSync(absolute), `Release artifact is missing: ${path}`);
  const stat = lstatSync(absolute);
  assert(stat.isFile() && !stat.isSymbolicLink(), `Release artifact must be a regular non-symlink file: ${path}`);
  return absolute;
}

export function filmArtifactSet(root, portfolio, provenanceRoot = root) {
  const paths = new Set([
    'demo/one-app/films/portfolio.json',
    'demo/one-app/films/SHOT-MANIFEST.md',
  ]);
  const provenancePaths = new Set(portfolio.films.map(film => film.provenance));
  for (const film of portfolio.films) {
    paths.add(film.scenario);
    paths.add(film.provenance);
    paths.add(film.media.mp4);
    paths.add(film.media.poster);
    const provenance = readJson(releasePath(provenanceRoot, film.provenance));
    paths.add(provenance.capture.record);
    paths.add(provenance.edit.timeline);
    paths.add(provenance.edit.stage_record);
    paths.add(provenance.evidence.record);
    for (const keyframe of provenance.capture.keyframes) paths.add(keyframe.path);
  }
  const ordered = [...paths].sort((left, right) => Buffer.compare(Buffer.from(left), Buffer.from(right)));
  const digest = createHash('sha256');
  digest.update('axocoatl-film-artifact-set-v1\0');
  for (const path of ordered) {
    const artifactRoot = provenancePaths.has(path) ? provenanceRoot : root;
    digest.update(`${path}\0${sha256File(releasePath(artifactRoot, path))}\n`);
  }
  return { count: ordered.length, sha256: digest.digest('hex') };
}

function isRuntimePath(path) {
  return path === 'Cargo.toml' ||
    path === 'Cargo.lock' ||
    path.startsWith('axocoatl-cli/') ||
    path === 'axocoatl-server/Cargo.toml' ||
    path.startsWith('axocoatl-server/src/') ||
    path.startsWith('axocoatl-server/static/') ||
    path.startsWith('crates/') ||
    path.startsWith('packages/');
}

export function validateCompatibilityAttestation(attestation) {
  exactKeys(attestation, [
    'schema_version', 'kind', 'release', 'first_committed_provenance',
    'frozen_release_provenance_rewrite', 'portfolio', 'delta', 'protected',
    'runtime_changed_paths', 'semantics_preserving_runtime_fixes',
  ], 'attestation');
  assert(attestation.schema_version === 1, 'attestation.schema_version must be 1.');
  assert(attestation.kind === 'axocoatl-film-release-compatibility', 'attestation.kind is invalid.');

  exactKeys(attestation.release, ['tag', 'tag_object', 'commit', 'tree', 'source_content_sha256'], 'release');
  assert(/^v\d+\.\d+\.\d+$/.test(attestation.release.tag || ''), 'release.tag must be an exact semantic version tag.');
  requireHash(attestation.release.tag_object, sha1Pattern, 'release.tag_object');
  requireHash(attestation.release.commit, sha1Pattern, 'release.commit');
  requireHash(attestation.release.tree, sha1Pattern, 'release.tree');
  requireHash(attestation.release.source_content_sha256, sha256Pattern, 'release.source_content_sha256');

  const first = attestation.first_committed_provenance;
  exactKeys(first, ['disclosure', 'materialized_source', 'source', 'binary', 'files'], 'first_committed_provenance');
  assert(first.disclosure === firstCommittedDisclosure, 'first-committed provenance disclosure changed.');
  exactKeys(first.materialized_source, ['commit', 'tree', 'source_content_sha256'], 'first_committed_provenance.materialized_source');
  requireHash(first.materialized_source.commit, sha1Pattern, 'first_committed_provenance.materialized_source.commit');
  requireHash(first.materialized_source.tree, sha1Pattern, 'first_committed_provenance.materialized_source.tree');
  requireHash(first.materialized_source.source_content_sha256, sha256Pattern, 'first_committed_provenance.materialized_source.source_content_sha256');
  validateSourceDeclaration(first.source, 'first_committed_provenance.source');
  validateBinaryDeclaration(first.binary, 'first_committed_provenance.binary');
  assert(
    first.materialized_source.source_content_sha256 === first.source.content_sha256,
    'materialized source digest must equal the first-committed declared content digest.',
  );
  assert(Array.isArray(first.files) && first.files.length === 12, 'first_committed_provenance.files must contain 12 entries.');
  sortedUnique(first.files.map(file => file.path), 'first-committed provenance paths');
  for (const [index, file] of first.files.entries()) {
    exactKeys(file, ['path', 'first_blob', 'first_sha256', 'frozen_release_blob', 'frozen_release_sha256'], `first_committed_provenance.files[${index}]`);
    safeRepoPath(file.path, `first_committed_provenance.files[${index}].path`);
    assert(
      file.path.startsWith('demo/one-app/films/provenance/') && file.path.endsWith('.json'),
      `first_committed_provenance.files[${index}].path is outside the provenance directory.`,
    );
    requireHash(file.first_blob, sha1Pattern, `first_committed_provenance.files[${index}].first_blob`);
    requireHash(file.first_sha256, sha256Pattern, `first_committed_provenance.files[${index}].first_sha256`);
    requireHash(file.frozen_release_blob, sha1Pattern, `first_committed_provenance.files[${index}].frozen_release_blob`);
    requireHash(file.frozen_release_sha256, sha256Pattern, `first_committed_provenance.files[${index}].frozen_release_sha256`);
    assert(file.first_blob !== file.frozen_release_blob, `${file.path}: frozen release must retain the audited rewrite.`);
  }

  const rewrite = attestation.frozen_release_provenance_rewrite;
  exactKeys(rewrite, ['disclosure', 'source', 'binary'], 'frozen_release_provenance_rewrite');
  assert(rewrite.disclosure === frozenRewriteDisclosure, 'frozen-release rewrite disclosure changed.');
  validateSourceDeclaration(rewrite.source, 'frozen_release_provenance_rewrite.source');
  validateBinaryDeclaration(rewrite.binary, 'frozen_release_provenance_rewrite.binary');
  assert(!isDeepStrictEqual(rewrite.source, first.source), 'frozen source rewrite must differ from the first-committed declaration.');
  assert(!isDeepStrictEqual(rewrite.binary, first.binary), 'frozen binary rewrite must differ from the first-committed declaration.');

  exactKeys(attestation.portfolio, [
    'path', 'sha256', 'film_count', 'artifact_count',
    'frozen_artifact_set_sha256', 'restored_artifact_set_sha256',
  ], 'portfolio');
  assert(attestation.portfolio.path === 'demo/one-app/films/portfolio.json', 'portfolio.path is not canonical.');
  requireHash(attestation.portfolio.sha256, sha256Pattern, 'portfolio.sha256');
  assert(attestation.portfolio.film_count === 12, 'portfolio.film_count must be 12.');
  assert(Number.isInteger(attestation.portfolio.artifact_count) && attestation.portfolio.artifact_count > 0, 'portfolio.artifact_count must be positive.');
  requireHash(attestation.portfolio.frozen_artifact_set_sha256, sha256Pattern, 'portfolio.frozen_artifact_set_sha256');
  requireHash(attestation.portfolio.restored_artifact_set_sha256, sha256Pattern, 'portfolio.restored_artifact_set_sha256');

  exactKeys(attestation.delta, [
    'entry_count', 'non_recording_path_count', 'provenance_rewrite_path_count', 'entries_sha256', 'entries',
  ], 'delta');
  requireHash(attestation.delta.entries_sha256, sha256Pattern, 'delta.entries_sha256');
  assert(Array.isArray(attestation.delta.entries) && attestation.delta.entries.length > 0, 'delta.entries must not be empty.');
  assert(attestation.delta.entry_count === 55, 'delta.entry_count must be the complete 55-path history delta.');
  assert(attestation.delta.non_recording_path_count === 43, 'delta.non_recording_path_count must be 43.');
  assert(attestation.delta.provenance_rewrite_path_count === 12, 'delta.provenance_rewrite_path_count must be 12.');
  assert(attestation.delta.entries.length === attestation.delta.entry_count, 'delta.entries length does not match delta.entry_count.');
  sortedUnique(attestation.delta.entries.map(entry => entry.path), 'delta entry paths');
  for (const [index, entry] of attestation.delta.entries.entries()) {
    exactKeys(entry, ['path', 'status', 'old_mode', 'new_mode', 'old_blob', 'new_blob', 'classification'], `delta.entries[${index}]`);
    safeRepoPath(entry.path, `delta.entries[${index}].path`);
    assert(['A', 'D', 'M'].includes(entry.status), `delta.entries[${index}].status is invalid.`);
    for (const [key, value] of [['old_mode', entry.old_mode], ['new_mode', entry.new_mode]]) {
      assert(value === null || /^(100644|100755|120000)$/.test(value), `delta.entries[${index}].${key} is invalid.`);
    }
    for (const [key, value] of [['old_blob', entry.old_blob], ['new_blob', entry.new_blob]]) {
      assert(value === null || sha1Pattern.test(value), `delta.entries[${index}].${key} is invalid.`);
    }
    assert(classifications.has(entry.classification), `delta.entries[${index}].classification is invalid.`);
    assert((entry.status === 'A') === (entry.old_blob === null && entry.old_mode === null), `delta.entries[${index}] has an invalid added-file identity.`);
    assert((entry.status === 'D') === (entry.new_blob === null && entry.new_mode === null), `delta.entries[${index}] has an invalid deleted-file identity.`);
  }
  assert(canonicalDeltaDigest(attestation.delta.entries) === attestation.delta.entries_sha256, 'delta.entries_sha256 does not match the declared entries.');
  const provenancePaths = first.files.map(file => file.path);
  const classifiedProvenancePaths = attestation.delta.entries
    .filter(entry => entry.classification === 'historical-provenance-rewrite')
    .map(entry => entry.path);
  assert(isDeepStrictEqual(classifiedProvenancePaths, provenancePaths), 'delta must classify exactly the 12 audited provenance rewrites.');
  assert(
    attestation.delta.entries.length - classifiedProvenancePaths.length === attestation.delta.non_recording_path_count,
    'delta non-recording path count does not match the complete delta.',
  );

  exactKeys(attestation.protected, ['paths', 'excluded_paths', 'entry_count', 'content_sha256'], 'protected');
  sortedUnique(attestation.protected.paths, 'protected.paths');
  for (const [index, path] of attestation.protected.paths.entries()) safeRepoPath(path, `protected.paths[${index}]`);
  assert(
    isDeepStrictEqual(attestation.protected.paths, [...requiredProtectedPaths]),
    'protected.paths must equal the verifier-required filmed product surface.',
  );
  sortedUnique(attestation.semantics_preserving_runtime_fixes, 'semantics_preserving_runtime_fixes');
  assert(
    isDeepStrictEqual(attestation.semantics_preserving_runtime_fixes, [...requiredSemanticsPreservingRuntimeFixes]),
    'semantics_preserving_runtime_fixes must equal the reviewed six-path set.',
  );
  for (const [index, path] of attestation.semantics_preserving_runtime_fixes.entries()) {
    safeRepoPath(path, `semantics_preserving_runtime_fixes[${index}]`);
  }
  const expectedProtectedExclusions = [...provenancePaths, ...attestation.semantics_preserving_runtime_fixes]
    .sort((left, right) => Buffer.compare(Buffer.from(left), Buffer.from(right)));
  sortedUnique(attestation.protected.excluded_paths, 'protected.excluded_paths');
  assert(
    isDeepStrictEqual(attestation.protected.excluded_paths, expectedProtectedExclusions),
    'protected.excluded_paths must contain only the 12 audited provenance rewrites and six reviewed runtime fixes.',
  );
  assert(
    attestation.protected.excluded_paths.every(path => attestation.protected.paths.some(root => path === root || path.startsWith(`${root}/`))),
    'every protected exclusion must be inside the required protected surface.',
  );
  assert(Number.isInteger(attestation.protected.entry_count) && attestation.protected.entry_count > 0, 'protected.entry_count must be positive.');
  requireHash(attestation.protected.content_sha256, sha256Pattern, 'protected.content_sha256');

  sortedUnique(attestation.runtime_changed_paths, 'runtime_changed_paths');
  assert(attestation.runtime_changed_paths.length > 0, 'runtime_changed_paths must not be empty.');
  for (const [index, path] of attestation.runtime_changed_paths.entries()) safeRepoPath(path, `runtime_changed_paths[${index}]`);
  assert(
    attestation.semantics_preserving_runtime_fixes.every(path => attestation.runtime_changed_paths.includes(path)),
    'every reviewed runtime fix must appear in runtime_changed_paths.',
  );
  const classifiedRuntimeFixes = attestation.delta.entries
    .filter(entry => entry.classification === 'runtime-build-test-fix')
    .map(entry => entry.path);
  assert(
    isDeepStrictEqual(classifiedRuntimeFixes, attestation.semantics_preserving_runtime_fixes),
    'delta must classify exactly the six reviewed runtime build/test fixes.',
  );
  return attestation;
}

export function verifyReleaseCompatibilityDocument(attestation, { releaseRoot, controlRoot, portfolio }) {
  validateCompatibilityAttestation(attestation);
  const root = resolve(releaseRoot);
  const control = resolve(controlRoot);
  assert(existsSync(root) && lstatSync(root).isDirectory() && !lstatSync(root).isSymbolicLink(), 'release root must be a regular directory.');
  assert(existsSync(control) && lstatSync(control).isDirectory() && !lstatSync(control).isSymbolicLink(), 'control root must be a regular directory.');
  const status = runGit(root, ['status', '--porcelain=v1', '--untracked-files=all', '-z'], { encoding: null });
  assert(status.length === 0, 'release root must be a clean frozen checkout.');

  const head = revisionIdentity(root, 'HEAD');
  assert(head.commit === attestation.release.commit && head.tree === attestation.release.tree, 'release root HEAD does not match the attested release commit/tree.');
  const tagRef = `refs/tags/${attestation.release.tag}`;
  const tagObject = String(runGit(root, ['rev-parse', tagRef])).trim();
  assert(tagObject === attestation.release.tag_object, 'release tag object changed.');
  assert(String(runGit(root, ['cat-file', '-t', tagObject])).trim() === 'tag', 'release tag must remain annotated.');
  const tagIdentity = revisionIdentity(root, tagRef);
  assert(tagIdentity.commit === attestation.release.commit && tagIdentity.tree === attestation.release.tree, 'release tag moved from the attested commit/tree.');
  assert(
    sourceContentDigestAtRevision(root, attestation.release.commit) === attestation.release.source_content_sha256,
    'release source-content digest changed.',
  );

  const first = attestation.first_committed_provenance;
  const materialized = revisionIdentity(root, first.materialized_source.commit);
  assert(materialized.tree === first.materialized_source.tree, 'first-committed materialized-source tree changed.');
  assert(
    sourceContentDigestAtRevision(root, materialized.commit) === first.materialized_source.source_content_sha256,
    'first-committed materialized-source digest changed.',
  );
  const materializedParents = String(runGit(root, ['rev-list', '--parents', '-n', '1', materialized.commit])).trim().split(' ');
  assert(
    materializedParents.length === 2 && materializedParents[1] === first.source.head,
    'materialized-source baseline is not the first provenance commit directly after the declared source head.',
  );
  runGit(root, ['merge-base', '--is-ancestor', materialized.commit, attestation.release.commit]);

  assert(portfolio.films.length === attestation.portfolio.film_count, 'release portfolio film count changed.');
  assert(
    sha256File(releasePath(root, attestation.portfolio.path)) === attestation.portfolio.sha256,
    'release portfolio bytes changed.',
  );
  assert(
    sha256File(releasePath(control, attestation.portfolio.path)) === attestation.portfolio.sha256,
    'control portfolio bytes changed.',
  );

  const expectedProvenancePaths = portfolio.films.map(film => film.provenance)
    .sort((left, right) => Buffer.compare(Buffer.from(left), Buffer.from(right)));
  assert(
    isDeepStrictEqual(first.files.map(file => file.path), expectedProvenancePaths),
    'first-committed provenance file set no longer matches the portfolio.',
  );
  for (const file of first.files) {
    const controlFile = releasePath(control, file.path);
    const frozenFile = releasePath(root, file.path);
    const firstBlob = runGit(root, ['cat-file', 'blob', file.first_blob], { encoding: null });
    assert(sha256File(controlFile) === file.first_sha256, `${file.path}: restored first-committed provenance bytes changed.`);
    assert(sha256Buffer(firstBlob) === file.first_sha256, `${file.path}: first-committed Git blob digest differs from the attested restored bytes.`);
    assert(
      isDeepStrictEqual(firstBlob, readFileSync(controlFile)),
      `${file.path}: restored provenance bytes do not match the first-committed Git blob.`,
    );
    assert(sha256File(frozenFile) === file.frozen_release_sha256, `${file.path}: frozen release provenance bytes changed.`);
    assert(!gitPathExists(root, first.source.head, file.path), `${file.path}: provenance already existed before the declared first commit.`);
    assert(gitPathObject(root, materialized.commit, file.path) === file.first_blob, `${file.path}: first-committed provenance Git blob changed.`);
    assert(gitPathObject(root, attestation.release.commit, file.path) === file.frozen_release_blob, `${file.path}: frozen release provenance Git blob changed.`);
    const restored = readJson(controlFile);
    const frozen = readJson(frozenFile);
    assert(isDeepStrictEqual(restored.source, first.source), `${file.path}: restored source declaration differs from the first commit.`);
    assert(isDeepStrictEqual(restored.binary, first.binary), `${file.path}: restored binary declaration differs from the first commit.`);
    assert(
      isDeepStrictEqual(frozen.source, attestation.frozen_release_provenance_rewrite.source),
      `${file.path}: frozen source rewrite differs from the audited value.`,
    );
    assert(
      isDeepStrictEqual(frozen.binary, attestation.frozen_release_provenance_rewrite.binary),
      `${file.path}: frozen binary rewrite differs from the audited value.`,
    );
    assertSourceBinaryOnlyRewrite(restored, frozen, file.path);
  }

  const frozenArtifacts = filmArtifactSet(root, portfolio);
  const restoredArtifacts = filmArtifactSet(root, portfolio, control);
  assert(frozenArtifacts.count === attestation.portfolio.artifact_count, 'frozen film artifact-set membership changed.');
  assert(restoredArtifacts.count === attestation.portfolio.artifact_count, 'restored film artifact-set membership changed.');
  assert(frozenArtifacts.sha256 === attestation.portfolio.frozen_artifact_set_sha256, 'frozen film artifact-set bytes changed.');
  assert(restoredArtifacts.sha256 === attestation.portfolio.restored_artifact_set_sha256, 'restored film artifact-set bytes changed.');

  const actual = gitDiffEntries(root, materialized.commit, attestation.release.commit);
  assert(actual.length === attestation.delta.entries.length, 'release delta path count changed.');
  for (const [index, expected] of attestation.delta.entries.entries()) {
    const { classification: _classification, ...recordedIdentity } = expected;
    assert(isDeepStrictEqual(actual[index], recordedIdentity), `release delta changed at ${expected.path}.`);
  }
  assert(canonicalDeltaDigest(attestation.delta.entries) === attestation.delta.entries_sha256, 'release delta digest changed.');
  const runtimeChanges = actual.filter(entry => isRuntimePath(entry.path)).map(entry => entry.path);
  assert(isDeepStrictEqual(runtimeChanges, attestation.runtime_changed_paths), 'runtime_changed_paths no longer matches the exact release delta.');
  const provenanceRewritePaths = actual
    .filter(entry => entry.path.startsWith('demo/one-app/films/provenance/'))
    .map(entry => entry.path);
  assert(isDeepStrictEqual(provenanceRewritePaths, first.files.map(file => file.path)), 'complete delta has an unaudited provenance change.');
  assert(actual.length - provenanceRewritePaths.length === attestation.delta.non_recording_path_count, 'complete delta no longer has exactly 43 non-recording paths.');

  const baselineProtected = protectedContentDigest(
    root, materialized.commit, attestation.protected.paths, attestation.protected.excluded_paths,
  );
  const releaseProtected = protectedContentDigest(
    root, attestation.release.commit, attestation.protected.paths, attestation.protected.excluded_paths,
  );
  assert(
    isDeepStrictEqual(baselineProtected, releaseProtected),
    'required protected film surface changed outside the audited provenance rewrites and six reviewed runtime fixes.',
  );
  assert(releaseProtected.count === attestation.protected.entry_count, 'protected film-surface entry count changed.');
  assert(releaseProtected.sha256 === attestation.protected.content_sha256, 'protected film-surface content digest changed.');
  return {
    tag: attestation.release.tag,
    commit: attestation.release.commit,
    films: portfolio.films.length,
    changedPaths: actual.length,
    nonRecordingPaths: actual.length - provenanceRewritePaths.length,
    provenanceRewrites: provenanceRewritePaths.length,
    artifacts: restoredArtifacts.count,
  };
}

export function verifyReleaseCompatibility(attestationPath, options) {
  const absolute = resolve(attestationPath);
  assert(existsSync(absolute), `Compatibility attestation does not exist: ${absolute}`);
  const stat = lstatSync(absolute);
  assert(stat.isFile() && !stat.isSymbolicLink(), `Compatibility attestation must be a regular non-symlink file: ${absolute}`);
  return verifyReleaseCompatibilityDocument(JSON.parse(readFileSync(absolute, 'utf8')), options);
}
