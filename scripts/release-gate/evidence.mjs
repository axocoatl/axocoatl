#!/usr/bin/env node
// Release evidence of the clean-machine new-user gate (docs/RELEASE_GATE.md):
// the formats, the bundle manifest, its signature check, and the release
// decision. No dependencies beyond Node 18.
//
// A bundle is release-evidence/<version>/<commit>/ holding
//   <path>.json          one file per gate path (A-x86, C1-local, D1, ...)
//   <path>/...           that path's transcripts, receipts and logs
//   <path>/ci-run.json   for a path CI records (Tier A and B): the GitHub
//                        run collect-evidence.sh took it from, which must be
//                        a successful push run of ci.yml on the commit
//   manifest.json        every other file with its SHA-256, and each path's
//                        pass/fail
//   manifest.dsse.json   manifest.json's exact bytes in a DSSE envelope,
//                        signed with `axocoatl receipt sign-evidence` by a
//                        receipt key listed in .axocoatl/gate-keys
//
// Subcommands (used by the gate scripts):
//   add-step --file F --name N --status pass|fail|skip [--exit N]
//            [--seconds S] [--bytes N] [--detail TEXT] [--optional]
//   path-file --steps F --out FILE --path P --tier T --version V --commit C
//            [--started ISO] [--finished ISO] [--json KEY=FILE]...
//            [--attach REL]...
//   manifest <bundle-dir> --version V --commit C
//   verify-bundle <bundle-dir> --keys FILE [--version V] [--commit C]
//            [--required docs/RELEASE_GATE.md --root DIR]
//            without --required every path in the bundle must pass; with
//            it, the bundle passes when it verifies and every required path
//            passes with every step that file requires of it (other failing
//            paths are warnings), as release.yml decides
//   compare <previous-path.json> <path.json>   time and download vs the
//            previous release, flagging growth over 20%

import { execFileSync } from 'node:child_process';
import { createHash, createPublicKey, verify as verifySignature } from 'node:crypto';
import {
  appendFileSync, existsSync, lstatSync, readdirSync, readFileSync, writeFileSync,
} from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

export const MANIFEST_SCHEMA = 'axocoatl.release-evidence/1';
export const PATH_SCHEMA = 'axocoatl.release-evidence.path/1';
export const PAYLOAD_TYPE = 'application/vnd.axocoatl.release-evidence+json';
export const MANIFEST_FILE = 'manifest.json';
export const ENVELOPE_FILE = 'manifest.dsse.json';
export const EVIDENCE_ROOT = 'release-evidence';
export const REQUIRED_START = '<!-- release-gate:required-paths:start -->';
export const REQUIRED_END = '<!-- release-gate:required-paths:end -->';
export const STEPS_START = '<!-- release-gate:required-steps:start -->';
export const STEPS_END = '<!-- release-gate:required-steps:end -->';
export const SIXTEEN_GB_CONDITION = '(when the public copy names 16 GB)';
export const CI_RUN_SCHEMA = 'axocoatl.release-evidence.ci-run/1';
export const CI_RUN_FILE = 'ci-run.json';
export const CI_WORKFLOW = '.github/workflows/ci.yml';
// The tiers CI records: Tier A (Linux) and Tier B (macOS plumbing).
const CI_TIERS = new Set(['A', 'B']);
const MAX_FILE_BYTES = 64 * 1024 * 1024;
const PATH_NAME = /^[A-Za-z0-9][A-Za-z0-9-]{0,63}$/;
const COMMIT = /^[0-9a-f]{40}$/;
const VERSION = /^\d+\.\d+\.\d+$/;
const STEP_STATUSES = new Set(['pass', 'fail', 'skip']);

export const sha256 = (bytes) => createHash('sha256').update(bytes).digest('hex');

/** DSSE's pre-authentication encoding, byte for byte as Axocoatl signs it. */
export function pae(payloadType, payload) {
  const type = Buffer.from(payloadType, 'utf8');
  return Buffer.concat([
    Buffer.from(`DSSEv1 ${type.length} `, 'utf8'), type,
    Buffer.from(` ${payload.length} `, 'utf8'), payload,
  ]);
}

function decodeBase64(text) {
  if (typeof text !== 'string' || !/^[A-Za-z0-9+/_-]*={0,2}$/.test(text)) {
    throw new Error('not base64');
  }
  return Buffer.from(text.replace(/-/g, '+').replace(/_/g, '/'), 'base64');
}

/** One `ed25519 <base64> [comment]` line, as `axocoatl receipt key` prints it. */
export function parseKeyLine(line) {
  const trimmed = line.trim();
  if (!trimmed || trimmed.startsWith('#')) return null;
  const [algorithm, encoded] = trimmed.split(/\s+/);
  if (algorithm !== 'ed25519') throw new Error(`a key line starts with ed25519, not ${JSON.stringify(algorithm.slice(0, 40))}`);
  const raw = decodeBase64(encoded ?? '');
  if (raw.length !== 32) throw new Error('an Ed25519 public key is 32 bytes');
  return {
    raw,
    keyId: `sha256:${sha256(raw)}`,
    shortId: sha256(raw).slice(0, 16),
    object: createPublicKey({ key: { kty: 'OKP', crv: 'Ed25519', x: raw.toString('base64url') }, format: 'jwk' }),
  };
}

/** Every key of a gate-keys file; a line that is not a key is an error naming it. */
export function parseKeys(text) {
  const keys = [];
  text.split('\n').forEach((line, index) => {
    try {
      const key = parseKeyLine(line);
      if (key) keys.push(key);
    } catch (error) {
      throw new Error(`line ${index + 1}: ${error.message}`);
    }
  });
  return keys;
}

/**
 * Open a signed manifest: the payload bytes and the gate key that signed
 * them, or a reason.
 */
export function openEnvelope(envelopeText, keys) {
  let envelope;
  try {
    envelope = JSON.parse(envelopeText);
  } catch (error) {
    return { ok: false, reason: `${ENVELOPE_FILE} is not JSON: ${error.message}` };
  }
  if (envelope?.payloadType !== PAYLOAD_TYPE) {
    return { ok: false, reason: `${ENVELOPE_FILE} has payload type ${JSON.stringify(envelope?.payloadType)}, not ${PAYLOAD_TYPE}` };
  }
  let payload;
  try {
    payload = decodeBase64(envelope.payload);
  } catch {
    return { ok: false, reason: `${ENVELOPE_FILE}'s payload is not base64` };
  }
  if (!Array.isArray(envelope.signatures) || envelope.signatures.length === 0) {
    return { ok: false, reason: `${ENVELOPE_FILE} has no signature` };
  }
  const message = pae(PAYLOAD_TYPE, payload);
  for (const signature of envelope.signatures) {
    let bytes;
    try {
      bytes = decodeBase64(signature?.sig);
    } catch {
      continue;
    }
    for (const key of keys) {
      if (signature.keyid && signature.keyid !== key.keyId) continue;
      if (verifySignature(null, message, key.object, bytes)) return { ok: true, payload, key };
    }
  }
  return { ok: false, reason: `no signature in ${ENVELOPE_FILE} verifies with a key in the gate keys` };
}

/** Every regular file under `dir`, as `/`-separated paths relative to it. */
function listFiles(dir, base = dir, out = [], problems = []) {
  for (const entry of readdirSync(dir, { withFileTypes: true })) {
    const full = path.join(dir, entry.name);
    const relative = path.relative(base, full).split(path.sep).join('/');
    const stat = lstatSync(full);
    if (stat.isSymbolicLink()) problems.push(`${relative} is a symbolic link`);
    else if (stat.isDirectory()) listFiles(full, base, out, problems);
    else if (stat.isFile()) out.push(relative);
    else problems.push(`${relative} is not a regular file`);
  }
  return { files: out.sort(), problems };
}

function plainRelative(file) {
  return typeof file === 'string' && file !== '' && !file.startsWith('/') && !file.includes('\\')
    && file.split('/').every((part) => part !== '' && part !== '.' && part !== '..');
}

/** A path file's pass as its steps say: every required step passed. */
export function stepsPass(steps) {
  return Array.isArray(steps) && steps.length > 0
    && steps.every((step) => step?.required === false || step?.status === 'pass');
}

/** Problems of one path file's parsed content. */
export function checkPathEvidence(name, evidence, { version, commit }) {
  const problems = [];
  if (evidence?.schema !== PATH_SCHEMA) problems.push(`${name}: schema is not ${PATH_SCHEMA}`);
  if (evidence?.path !== name) problems.push(`${name}: the file is evidence of ${JSON.stringify(evidence?.path)}`);
  if (version && evidence?.version !== version) problems.push(`${name}: version ${JSON.stringify(evidence?.version)}, not ${version}`);
  if (commit && evidence?.commit !== commit) problems.push(`${name}: commit ${JSON.stringify(evidence?.commit)}, not ${commit}`);
  if (!Array.isArray(evidence?.steps) || evidence.steps.length === 0) {
    problems.push(`${name}: no steps were recorded`);
    return problems;
  }
  for (const step of evidence.steps) {
    if (typeof step?.name !== 'string' || !STEP_STATUSES.has(step?.status)) {
      problems.push(`${name}: a step has no name or an unknown status`);
    }
  }
  const failed = evidence.steps.filter((step) => step?.required !== false && step?.status !== 'pass');
  if (evidence.pass !== true) problems.push(`${name}: did not pass`);
  if (evidence.pass === true && failed.length > 0) {
    problems.push(`${name}: says it passed, but required steps did not`);
  }
  for (const step of failed) {
    problems.push(`${name}: step ${JSON.stringify(step?.name)} ${step?.status === 'skip' ? 'was skipped' : 'failed'}${step?.detail ? `: ${String(step.detail).slice(0, 200)}` : ''}`);
  }
  return problems;
}

/**
 * Problems of a CI tier's path (Tier A or B): its `<path>/ci-run.json`, which
 * collect-evidence.sh writes from the GitHub run it downloaded the path
 * from, must name a successful push run of ci.yml on the bundle's commit. A
 * path from a red run does not count: CI is green before a tag.
 */
export function checkCiRun(name, run, { commit }) {
  const problems = [];
  const where = `${name}/${CI_RUN_FILE}`;
  if (run?.schema !== CI_RUN_SCHEMA) {
    return [`${where}: schema is not ${CI_RUN_SCHEMA}`];
  }
  const label = `${name}: CI run ${JSON.stringify(run.id)}`;
  if (run.workflow_path !== CI_WORKFLOW) problems.push(`${label} is a run of ${JSON.stringify(run.workflow_path)}, not ${CI_WORKFLOW}`);
  if (run.event !== 'push') problems.push(`${label} is a ${JSON.stringify(run.event)} run, not a push run`);
  if (typeof run.repository !== 'string' || run.repository !== run.head_repository) {
    problems.push(`${label} ran ${JSON.stringify(run.head_repository)}'s code in ${JSON.stringify(run.repository)}`);
  }
  if (commit && run.head_sha !== commit) problems.push(`${label} ran commit ${JSON.stringify(run.head_sha)}, not ${commit}`);
  if (run.status !== 'completed' || run.conclusion !== 'success') {
    problems.push(`${label} concluded ${JSON.stringify(run.conclusion ?? run.status)}, not success: a path from a red CI run does not count`);
  }
  return problems;
}

/**
 * Verify one bundle directory. Returns `{ problems, paths }`: `paths` maps
 * each path the signed manifest lists to whether its evidence passed.
 */
export function verifyBundle(dir, { keys, version, commit }) {
  const problems = [];
  const paths = new Map();
  const envelopePath = path.join(dir, ENVELOPE_FILE);
  if (!existsSync(envelopePath)) return { problems: [`${ENVELOPE_FILE} is missing: the bundle is not signed`], paths };
  const opened = openEnvelope(readFileSync(envelopePath, 'utf8'), keys);
  if (!opened.ok) return { problems: [opened.reason], paths };
  const manifestPath = path.join(dir, MANIFEST_FILE);
  if (existsSync(manifestPath) && !readFileSync(manifestPath).equals(opened.payload)) {
    problems.push(`${MANIFEST_FILE} differs from the signed manifest in ${ENVELOPE_FILE}`);
  }
  let manifest;
  try {
    manifest = JSON.parse(opened.payload.toString('utf8'));
  } catch (error) {
    return { problems: [...problems, `the signed manifest is not JSON: ${error.message}`], paths };
  }
  if (manifest?.schema !== MANIFEST_SCHEMA) problems.push(`the manifest's schema is not ${MANIFEST_SCHEMA}`);
  if (version && manifest?.version !== version) problems.push(`the manifest is for version ${JSON.stringify(manifest?.version)}, not ${version}`);
  if (commit && manifest?.commit !== commit) problems.push(`the manifest is for commit ${JSON.stringify(manifest?.commit)}, not ${commit}`);
  const listed = new Map();
  for (const entry of Array.isArray(manifest?.files) ? manifest.files : []) {
    if (!plainRelative(entry?.path) || !/^[0-9a-f]{64}$/.test(entry?.sha256 ?? '')) {
      problems.push(`the manifest lists an invalid file entry ${JSON.stringify(entry?.path)}`);
      continue;
    }
    listed.set(entry.path, entry.sha256);
  }
  const { files, problems: shapeProblems } = listFiles(dir);
  problems.push(...shapeProblems);
  for (const file of files) {
    if (file === MANIFEST_FILE || file === ENVELOPE_FILE) continue;
    if (!listed.has(file)) problems.push(`${file} is in the bundle but not in the signed manifest`);
  }
  const contents = new Map();
  for (const [file, digest] of listed) {
    const full = path.join(dir, ...file.split('/'));
    if (!existsSync(full)) {
      problems.push(`${file} is in the signed manifest but missing`);
      continue;
    }
    const stat = lstatSync(full);
    if (!stat.isFile() || stat.size > MAX_FILE_BYTES) {
      problems.push(`${file} is not a regular file of at most ${MAX_FILE_BYTES} bytes`);
      continue;
    }
    const bytes = readFileSync(full);
    if (sha256(bytes) !== digest) problems.push(`${file} changed after the manifest was signed`);
    else contents.set(file, bytes);
  }
  const declared = manifest?.paths && typeof manifest.paths === 'object' ? manifest.paths : {};
  for (const [name, entry] of Object.entries(declared)) {
    if (!PATH_NAME.test(name) || !plainRelative(entry?.file) || !listed.has(entry.file)) {
      problems.push(`the manifest's path ${JSON.stringify(name)} names no listed file`);
      continue;
    }
    const bytes = contents.get(entry.file);
    if (!bytes) {
      paths.set(name, { pass: false, problems: [`${name}: its evidence file could not be read`] });
      continue;
    }
    let evidence;
    try {
      evidence = JSON.parse(bytes.toString('utf8'));
    } catch (error) {
      paths.set(name, { pass: false, problems: [`${name}: ${entry.file} is not JSON: ${error.message}`] });
      continue;
    }
    const pathProblems = checkPathEvidence(name, evidence, { version: manifest?.version, commit: manifest?.commit });
    if (entry.pass !== evidence.pass) pathProblems.push(`${name}: the manifest and ${entry.file} disagree on pass`);
    if (CI_TIERS.has(evidence?.tier)) {
      const runFile = `${name}/${CI_RUN_FILE}`;
      const runBytes = contents.get(runFile);
      if (!listed.has(runFile)) {
        pathProblems.push(`${name}: Tier ${evidence.tier} evidence comes only from a CI run, and ${runFile} is missing (collect it with collect-evidence.sh --ci-run)`);
      } else if (runBytes) {
        let run;
        try {
          run = JSON.parse(runBytes.toString('utf8'));
        } catch (error) {
          run = null;
          pathProblems.push(`${runFile} is not JSON: ${error.message}`);
        }
        if (run) pathProblems.push(...checkCiRun(name, run, { commit: manifest?.commit }));
      } else {
        pathProblems.push(`${name}: ${runFile} could not be read`);
      }
    }
    paths.set(name, { pass: pathProblems.length === 0, problems: pathProblems, steps: evidence.steps });
  }
  return { problems, paths, key: opened.key, manifest };
}

/** The required paths docs/RELEASE_GATE.md lists, with their conditions. */
export function parseRequiredPaths(markdown) {
  const start = markdown.indexOf(REQUIRED_START);
  const end = markdown.indexOf(REQUIRED_END);
  if (start < 0 || end < start) throw new Error(`the required-paths block (${REQUIRED_START} ... ${REQUIRED_END}) is missing`);
  const required = [];
  for (const line of markdown.slice(start + REQUIRED_START.length, end).split('\n')) {
    const match = line.match(/^- `([^`]+)`(.*)$/);
    if (!match) continue;
    if (!PATH_NAME.test(match[1])) throw new Error(`invalid gate path name ${JSON.stringify(match[1])}`);
    required.push({ name: match[1], when: match[2].includes(SIXTEEN_GB_CONDITION) ? 'sixteen-gb' : 'always' });
  }
  if (required.length === 0) throw new Error('the required-paths block lists no path');
  return required;
}

/**
 * The steps docs/RELEASE_GATE.md requires of each path, as a Map of path name
 * to step names: the evidence each sentence of the public text relies on, so
 * a path whose script skipped or never recorded one cannot pass. The block
 * lists a path as `- \`NAME\`` and each of its steps under it as
 * `  - \`STEP\``.
 */
export function parseRequiredSteps(markdown) {
  const start = markdown.indexOf(STEPS_START);
  const end = markdown.indexOf(STEPS_END);
  if (start < 0 || end < start) throw new Error(`the required-steps block (${STEPS_START} ... ${STEPS_END}) is missing`);
  const steps = new Map();
  let current = null;
  for (const line of markdown.slice(start + STEPS_START.length, end).split('\n')) {
    const pathLine = line.match(/^- `([^`]+)`\s*$/);
    const stepLine = line.match(/^\s+- `([^`]+)`\s*$/);
    if (pathLine) {
      if (!PATH_NAME.test(pathLine[1])) throw new Error(`invalid gate path name ${JSON.stringify(pathLine[1])}`);
      current = pathLine[1];
      if (steps.has(current)) throw new Error(`${current} is listed twice in the required-steps block`);
      steps.set(current, []);
    } else if (stepLine) {
      if (!current) throw new Error(`the required step ${JSON.stringify(stepLine[1])} is under no path`);
      steps.get(current).push(stepLine[1]);
    } else if (line.trim() !== '') {
      throw new Error(`the required-steps block has a line that is neither a path nor a step: ${JSON.stringify(line.slice(0, 80))}`);
    }
  }
  return steps;
}

/**
 * Why `steps` (a path file's) does not show every step in `required`: each
 * must be recorded as required and passed, and so must every other record
 * of it.
 */
export function requiredStepProblems(name, steps, required) {
  const problems = [];
  const recorded = Array.isArray(steps) ? steps : [];
  for (const wanted of required) {
    const found = recorded.filter((step) => step?.name === wanted);
    if (found.length === 0) {
      problems.push(`${name}: step ${JSON.stringify(wanted)} was not recorded, and the release's public text relies on it`);
    } else if (!found.every((step) => step.status === 'pass' && step.required !== false)) {
      const step = found.find((each) => each.status !== 'pass' || each.required === false);
      const what = step.required === false ? 'was recorded as optional' : step.status === 'skip' ? 'was skipped' : 'failed';
      problems.push(`${name}: step ${JSON.stringify(wanted)} ${what}${step.detail ? `: ${String(step.detail).slice(0, 200)}` : ''}`);
    }
  }
  return problems;
}

/** Public copy that may make a claim: the files the 16 GB condition reads. */
export function publicCopyFiles(root) {
  const out = [];
  const add = (relative) => { if (existsSync(path.join(root, relative))) out.push(relative); };
  add('README.md');
  add('llms.txt');
  add('docs/CLAIMS.md');
  const walk = (relative, test) => {
    const full = path.join(root, relative);
    if (!existsSync(full)) return;
    for (const entry of readdirSync(full, { withFileTypes: true })) {
      const child = `${relative}/${entry.name}`;
      if (entry.isDirectory()) {
        if (entry.name === 'node_modules' || entry.name.startsWith('.')) continue;
        walk(child, test);
      } else if (entry.isFile() && test(entry.name)) out.push(child);
    }
  };
  walk('sites/marketing', (name) => name.endsWith('.html') || name.endsWith('.txt'));
  walk('sites/docs/src/content/docs', (name) => /\.mdx?$/.test(name));
  return out.sort();
}

/** Where the public copy names 16 GB (any mention requires the C2 paths). */
export function sixteenGbMentions(root) {
  const mentions = [];
  for (const file of publicCopyFiles(root)) {
    readFileSync(path.join(root, file), 'utf8').split('\n').forEach((line, index) => {
      if (/\b16[\s-]?(?:GB|GiB)\b/i.test(line)) mentions.push(`${file}:${index + 1}`);
    });
  }
  return mentions;
}

function git(root, ...args) {
  return execFileSync('git', ['-C', root, ...args], { encoding: 'utf8', stdio: ['ignore', 'pipe', 'pipe'] }).trim();
}

/** What may change between the tested commit and the tag: evidence and the gate's own lists. */
export const GATE_KEYS_FILE = '.axocoatl/gate-keys';
export const OVERRIDE_ACTORS_FILE = '.axocoatl/gate-override-actors';
const evidenceOnly = (file) => file.startsWith(`${EVIDENCE_ROOT}/`) || file === GATE_KEYS_FILE || file === OVERRIDE_ACTORS_FILE;

/**
 * Whether a bundle for `commit` is evidence for `tagCommit`: the same commit,
 * or an ancestor whose tree differs from it only under release-evidence/ and
 * in .axocoatl/gate-keys.
 */
export function commitMatches(root, commit, tagCommit) {
  try {
    git(root, 'cat-file', '-e', `${commit}^{commit}`);
  } catch {
    return { ok: false, reason: `commit ${commit} is not in this repository` };
  }
  if (commit === tagCommit) return { ok: true };
  try {
    git(root, 'merge-base', '--is-ancestor', commit, tagCommit);
  } catch {
    return { ok: false, reason: `commit ${commit} is not an ancestor of ${tagCommit}` };
  }
  const changed = git(root, 'diff', '--name-only', '--no-renames', commit, tagCommit)
    .split('\n').filter(Boolean).filter((file) => !evidenceOnly(file));
  if (changed.length > 0) {
    return { ok: false, reason: `the tag changes ${changed.length} file(s) after ${commit.slice(0, 12)} outside ${EVIDENCE_ROOT}/, ${GATE_KEYS_FILE} and ${OVERRIDE_ACTORS_FILE}: ${changed.slice(0, 5).join(', ')}${changed.length > 5 ? ', ...' : ''}` };
  }
  return { ok: true };
}

/** The override phrase the founder must type to publish without evidence. */
export function overridePhrase(version) {
  return `publish v${version} without the new-user gate`;
}

const GITHUB_LOGIN = /^[A-Za-z0-9](?:[A-Za-z0-9-]{0,38})$/;

/**
 * The GitHub logins a gate-override-actors file lists (one per line, `#`
 * comments allowed), lowercased; a line that is not a login is an error.
 */
export function parseActors(text) {
  const actors = [];
  text.split('\n').forEach((line, index) => {
    const login = line.replace(/#.*$/, '').trim();
    if (!login) return;
    if (!GITHUB_LOGIN.test(login)) throw new Error(`line ${index + 1}: ${JSON.stringify(login.slice(0, 40))} is not a GitHub login`);
    actors.push(login.toLowerCase());
  });
  return actors;
}

/**
 * The paths a release needs: every `always` path, and the C2 paths when the
 * public copy names 16 GB, with the steps each must show. Returns
 * `{ needed, mentions, requiredSteps }`.
 */
export function neededPaths(root, requiredFile) {
  const markdown = readFileSync(requiredFile, 'utf8');
  const required = parseRequiredPaths(markdown);
  const requiredSteps = parseRequiredSteps(markdown);
  const mentions = sixteenGbMentions(root);
  return {
    needed: required.filter((entry) => entry.when === 'always' || mentions.length > 0).map((entry) => entry.name),
    mentions,
    requiredSteps,
  };
}

/**
 * One bundle judged as the release judges it: it must verify, and every
 * needed path must be in it and pass, with every step `requiredSteps` names
 * for it recorded and passed. Paths it holds that the release does not need
 * may fail; they are listed as `extraFailing`.
 */
export function bundleDecision(dir, { keys, version, commit, needed, requiredSteps = new Map() }) {
  const bundle = verifyBundle(dir, { keys, version, commit });
  for (const name of needed) {
    const entry = bundle.paths.get(name);
    if (!entry) continue;
    const problems = requiredStepProblems(name, entry.steps, requiredSteps.get(name) ?? []);
    if (problems.length > 0) bundle.paths.set(name, { ...entry, pass: false, problems: [...entry.problems, ...problems] });
  }
  const missing = needed.filter((name) => !bundle.paths.has(name));
  const failing = needed.filter((name) => bundle.paths.has(name) && !bundle.paths.get(name).pass);
  const extraFailing = [...bundle.paths.keys()].filter((name) => !needed.includes(name) && !bundle.paths.get(name).pass);
  return {
    ...bundle, missing, failing, extraFailing,
    ok: bundle.problems.length === 0 && missing.length === 0 && failing.length === 0,
  };
}

/**
 * The release decision for `version` at `tagCommit`: `{ ok, status, lines }`,
 * `status` one of `pass`, `fail`, `overridden`.
 */
export function decideRelease({
  root, version, tagCommit, keysFile, requiredFile, overrideActorsFile = path.join(root, OVERRIDE_ACTORS_FILE),
  override = '', event = '', actor = '', triggeringActor = '',
}) {
  const lines = [];
  const fail = (reason) => ({ ok: false, status: 'fail', lines: [...lines, reason] });
  if (!VERSION.test(version ?? '')) return fail(`invalid version ${JSON.stringify(version)}`);
  if (!COMMIT.test(tagCommit ?? '')) return fail(`invalid tag commit ${JSON.stringify(tagCommit)}`);
  const evaluate = () => {
    const { needed, mentions, requiredSteps } = neededPaths(root, requiredFile);
    if (mentions.length > 0) lines.push(`The public copy names 16 GB (${mentions.slice(0, 5).join(', ')}), so the C2 paths are required.`);
    const problems = [];
    let keys = [];
    if (!existsSync(keysFile)) problems.push(`${path.relative(root, keysFile)} is missing: no gate key is trusted`);
    else {
      try {
        keys = parseKeys(readFileSync(keysFile, 'utf8'));
      } catch (error) {
        problems.push(`${path.relative(root, keysFile)}: ${error.message}`);
      }
      if (keys.length === 0 && problems.length === 0) problems.push(`${path.relative(root, keysFile)} lists no key: no gate key is trusted`);
    }
    const versionDir = path.join(root, EVIDENCE_ROOT, version);
    const candidates = existsSync(versionDir)
      ? readdirSync(versionDir, { withFileTypes: true }).filter((entry) => entry.isDirectory() && COMMIT.test(entry.name)).map((entry) => entry.name).sort()
      : [];
    if (candidates.length === 0) problems.push(`no evidence bundle exists under ${EVIDENCE_ROOT}/${version}/`);
    let best = null;
    for (const commit of keys.length > 0 ? candidates : []) {
      const match = commitMatches(root, commit, tagCommit);
      if (!match.ok) {
        problems.push(`${EVIDENCE_ROOT}/${version}/${commit}: ${match.reason}`);
        continue;
      }
      const bundle = bundleDecision(path.join(versionDir, commit), { keys, version, commit, needed, requiredSteps });
      const { missing, failing } = bundle;
      const result = { commit, bundle, missing, failing };
      if (bundle.ok) {
        return { ok: true, result, needed, problems };
      }
      if (!best || missing.length + failing.length < best.missing.length + best.failing.length) best = result;
      problems.push(...bundle.problems.map((problem) => `${EVIDENCE_ROOT}/${version}/${commit}: ${problem}`));
    }
    return { ok: false, best, needed, problems };
  };
  let outcome;
  try {
    outcome = evaluate();
  } catch (error) {
    outcome = { ok: false, needed: [], problems: [error.message] };
  }
  if (outcome.ok) {
    const { result } = outcome;
    lines.push(`New-user gate: PASS for v${version} at ${tagCommit.slice(0, 12)}: bundle ${EVIDENCE_ROOT}/${version}/${result.commit}/ signed by gate key ${result.bundle.key.shortId}.`);
    for (const name of outcome.needed) lines.push(`  ${name}: pass`);
    return { ok: true, status: 'pass', lines };
  }
  const missing = outcome.best ? outcome.best.missing : outcome.needed;
  if (missing.length > 0) lines.push(`Missing evidence for: ${missing.join(', ')}`);
  for (const name of outcome.best?.failing ?? []) {
    lines.push(...outcome.best.bundle.paths.get(name).problems);
  }
  lines.push(...outcome.problems);
  if (override === '') {
    lines.unshift(`New-user gate: FAIL for v${version} at ${tagCommit.slice(0, 12)}. Release publication is refused (docs/RELEASE_GATE.md).`);
    return { ok: false, status: 'fail', lines };
  }
  if (event !== 'workflow_dispatch') {
    lines.unshift(`New-user gate: FAIL: an override is accepted only from a manual workflow_dispatch run, not ${JSON.stringify(event)}.`);
    return { ok: false, status: 'fail', lines };
  }
  if (override !== overridePhrase(version)) {
    lines.unshift('New-user gate: FAIL: the override phrase does not match the one docs/RELEASE_GATE.md gives for this version.');
    return { ok: false, status: 'fail', lines };
  }
  // Only a login listed in the tagged commit's override list may override:
  // the phrase is public, so typing it is not enough.
  const listName = path.relative(root, overrideActorsFile);
  let actors = [];
  try {
    actors = existsSync(overrideActorsFile) ? parseActors(readFileSync(overrideActorsFile, 'utf8')) : [];
  } catch (error) {
    lines.unshift(`New-user gate: FAIL: ${listName}: ${error.message}; no one may override.`);
    return { ok: false, status: 'fail', lines };
  }
  if (actors.length === 0) {
    lines.unshift(`New-user gate: FAIL: ${listName} lists no GitHub login, so no one may override the gate.`);
    return { ok: false, status: 'fail', lines };
  }
  for (const [role, login] of [['actor', actor], ['triggering actor', triggeringActor || actor]]) {
    if (!actors.includes(String(login).toLowerCase())) {
      lines.unshift(`New-user gate: FAIL: the run's ${role} ${JSON.stringify(login)} is not listed in ${listName}; only a listed login may override.`);
      return { ok: false, status: 'fail', lines };
    }
  }
  lines.unshift(`New-user gate: OVERRIDDEN for v${version} at ${tagCommit.slice(0, 12)} by ${actor}${triggeringActor && triggeringActor !== actor ? ` (run by ${triggeringActor})` : ''}, listed in ${listName}, who typed the override phrase. This release is published WITHOUT complete new-user evidence:`);
  return { ok: true, status: 'overridden', lines };
}

/** Total seconds and bytes of a path's steps. */
export function totals(evidence) {
  const steps = Array.isArray(evidence?.steps) ? evidence.steps : [];
  const sum = (key) => steps.reduce((total, step) => total + (Number.isFinite(step?.[key]) ? step[key] : 0), 0);
  return { seconds: sum('seconds'), bytes: sum('bytes_downloaded') };
}

/**
 * How a path's time and download compare with the previous release's: one
 * line each, with REGRESSION when either grew by more than 20%.
 */
export function compareTotals(previous, current) {
  const before = totals(previous);
  const after = totals(current);
  const line = (label, old, now, unit) => {
    if (!old) return `${label}: ${now.toFixed(unit === 's' ? 1 : 0)}${unit} (no previous figure)`;
    const change = (now - old) / old;
    const flag = change > 0.2 ? ' REGRESSION (over 20%)' : '';
    return `${label}: ${old.toFixed(unit === 's' ? 1 : 0)}${unit} -> ${now.toFixed(unit === 's' ? 1 : 0)}${unit} (${change >= 0 ? '+' : ''}${(change * 100).toFixed(0)}%)${flag}`;
  };
  return [
    `${current?.path ?? 'path'}: ${previous?.version ?? '?'} -> ${current?.version ?? '?'}`,
    line('  wall time', before.seconds, after.seconds, 's'),
    line('  downloaded', before.bytes, after.bytes, ' bytes'),
  ];
}

// ---------------------------------------------------------------------------
// Writing evidence (used by the gate scripts)

/** The manifest of a bundle directory: every file but the manifest pair. */
export function buildManifest(dir, { version, commit, createdAt = new Date().toISOString() }) {
  if (!VERSION.test(version ?? '')) throw new Error(`invalid version ${JSON.stringify(version)}`);
  if (!COMMIT.test(commit ?? '')) throw new Error(`invalid commit ${JSON.stringify(commit)}`);
  const { files, problems } = listFiles(dir);
  if (problems.length > 0) throw new Error(problems.join('; '));
  const listed = files.filter((file) => file !== MANIFEST_FILE && file !== ENVELOPE_FILE);
  const paths = {};
  for (const file of listed) {
    if (file.includes('/') || !file.endsWith('.json')) continue;
    const name = file.slice(0, -'.json'.length);
    if (!PATH_NAME.test(name)) continue;
    const evidence = JSON.parse(readFileSync(path.join(dir, file), 'utf8'));
    if (evidence?.schema !== PATH_SCHEMA) continue;
    if (evidence.path !== name) throw new Error(`${file} is evidence of ${JSON.stringify(evidence.path)}`);
    if (evidence.version !== version || evidence.commit !== commit) {
      throw new Error(`${file} is evidence of ${evidence.version} at ${evidence.commit}, not ${version} at ${commit}`);
    }
    paths[name] = { file, pass: evidence.pass === true };
  }
  return {
    schema: MANIFEST_SCHEMA,
    version,
    commit,
    created_at: createdAt,
    paths,
    files: listed.map((file) => {
      const bytes = readFileSync(path.join(dir, ...file.split('/')));
      return { path: file, sha256: sha256(bytes), bytes: bytes.length };
    }),
  };
}

function parseFlags(argv, { repeatable = [], booleans = [] } = {}) {
  const flags = { _: [] };
  for (let index = 0; index < argv.length; index += 1) {
    const arg = argv[index];
    if (!arg.startsWith('--')) {
      flags._.push(arg);
      continue;
    }
    const name = arg.slice(2);
    if (booleans.includes(name)) {
      flags[name] = true;
      continue;
    }
    const value = argv[index + 1];
    if (value === undefined) throw new Error(`${arg} needs a value`);
    index += 1;
    if (repeatable.includes(name)) (flags[name] ??= []).push(value);
    else flags[name] = value;
  }
  return flags;
}

function main(argv) {
  const [command, ...rest] = argv;
  switch (command) {
    case 'add-step': {
      const flags = parseFlags(rest, { booleans: ['optional'] });
      if (!flags.file || !flags.name || !STEP_STATUSES.has(flags.status)) {
        throw new Error('add-step needs --file, --name and --status pass|fail|skip');
      }
      const step = { name: flags.name, status: flags.status, required: !flags.optional };
      if (flags.exit !== undefined && flags.exit !== '') step.exit_code = Number(flags.exit);
      if (flags.seconds !== undefined && flags.seconds !== '') step.seconds = Number(flags.seconds);
      if (flags.bytes !== undefined && flags.bytes !== '') step.bytes_downloaded = Number(flags.bytes);
      if (flags.detail) step.detail = String(flags.detail).slice(0, 4000);
      appendFileSync(flags.file, `${JSON.stringify(step)}\n`);
      return 0;
    }
    case 'path-file': {
      const flags = parseFlags(rest, { repeatable: ['json', 'attach'] });
      for (const key of ['steps', 'out', 'path', 'tier', 'version', 'commit']) {
        if (!flags[key]) throw new Error(`path-file needs --${key}`);
      }
      if (!PATH_NAME.test(flags.path)) throw new Error(`invalid path name ${flags.path}`);
      const steps = existsSync(flags.steps)
        ? readFileSync(flags.steps, 'utf8').split('\n').filter(Boolean).map((line) => JSON.parse(line))
        : [];
      const evidence = {
        schema: PATH_SCHEMA,
        path: flags.path,
        tier: flags.tier,
        version: flags.version,
        commit: flags.commit,
        pass: stepsPass(steps),
        started_at: flags.started ?? null,
        finished_at: flags.finished ?? new Date().toISOString(),
        steps,
      };
      for (const entry of flags.json ?? []) {
        const at = entry.indexOf('=');
        if (at < 1) throw new Error(`--json takes KEY=FILE, not ${entry}`);
        const file = entry.slice(at + 1);
        evidence[entry.slice(0, at)] = existsSync(file) ? JSON.parse(readFileSync(file, 'utf8')) : null;
      }
      evidence.attachments = (flags.attach ?? []).filter(plainRelative).sort();
      writeFileSync(flags.out, `${JSON.stringify(evidence, null, 2)}\n`);
      console.log(`${flags.path}: ${evidence.pass ? 'PASS' : 'FAIL'} (${steps.filter((step) => step.status === 'pass').length}/${steps.length} steps passed)`);
      return evidence.pass ? 0 : 1;
    }
    case 'manifest': {
      const flags = parseFlags(rest);
      const [dir] = flags._;
      if (!dir) throw new Error('manifest needs a bundle directory');
      const manifest = buildManifest(dir, { version: flags.version, commit: flags.commit });
      writeFileSync(path.join(dir, MANIFEST_FILE), `${JSON.stringify(manifest, null, 2)}\n`);
      console.log(`wrote ${path.join(dir, MANIFEST_FILE)}: ${manifest.files.length} files, paths ${Object.keys(manifest.paths).join(', ') || '(none)'}`);
      return 0;
    }
    case 'compare': {
      const [previous, current] = parseFlags(rest)._;
      if (!previous || !current) throw new Error('compare needs the previous and the current path file');
      for (const line of compareTotals(JSON.parse(readFileSync(previous, 'utf8')), JSON.parse(readFileSync(current, 'utf8')))) console.log(line);
      return 0;
    }
    case 'verify-bundle': {
      const flags = parseFlags(rest);
      const [dir] = flags._;
      if (!dir || !flags.keys) throw new Error('verify-bundle needs a bundle directory and --keys');
      const keys = parseKeys(readFileSync(flags.keys, 'utf8'));
      if (flags.required) {
        // As release.yml decides: required paths must pass; others may not.
        const { needed, mentions, requiredSteps } = neededPaths(path.resolve(flags.root ?? '.'), flags.required);
        if (mentions.length > 0) console.log(`The public copy names 16 GB (${mentions.slice(0, 5).join(', ')}), so the C2 paths are required.`);
        const result = bundleDecision(dir, { keys, version: flags.version, commit: flags.commit, needed, requiredSteps });
        for (const problem of result.problems) console.log(`problem: ${problem}`);
        for (const [name, entry] of result.paths) {
          const required = needed.includes(name);
          console.log(`${name}: ${entry.pass ? 'pass' : required ? 'FAIL' : 'warning: failed (not a required path)'}`);
          for (const problem of entry.problems) console.log(`  ${problem}`);
        }
        if (result.missing.length > 0) console.log(`missing required paths: ${result.missing.join(', ')}`);
        if (result.problems.length > 0) console.log('bundle does NOT verify');
        else if (result.ok) console.log(`bundle verifies, signed by gate key ${result.key.shortId}, and every required path passes`);
        else console.log(`bundle verifies, signed by gate key ${result.key.shortId}, but the release needs: ${[...result.missing, ...result.failing].join(', ')}`);
        return result.ok ? 0 : 1;
      }
      const result = verifyBundle(dir, { keys, version: flags.version, commit: flags.commit });
      for (const problem of result.problems) console.log(`problem: ${problem}`);
      for (const [name, entry] of result.paths) {
        console.log(`${name}: ${entry.pass ? 'pass' : 'FAIL'}`);
        for (const problem of entry.problems) console.log(`  ${problem}`);
      }
      const ok = result.problems.length === 0 && [...result.paths.values()].every((entry) => entry.pass);
      console.log(ok ? `bundle verifies, signed by gate key ${result.key.shortId}` : 'bundle does NOT verify');
      return ok ? 0 : 1;
    }
    default:
      throw new Error('usage: evidence.mjs add-step|path-file|manifest|verify-bundle|compare ...');
  }
}

if (process.argv[1] && path.resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  try {
    process.exitCode = main(process.argv.slice(2));
  } catch (error) {
    console.error(`evidence: ${error.message}`);
    process.exitCode = 2;
  }
}
