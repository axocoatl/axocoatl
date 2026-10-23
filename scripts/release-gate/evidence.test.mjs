// node --test scripts/release-gate/evidence.test.mjs
//
// The release decision against throwaway git repositories, bundles signed
// with throwaway Ed25519 keys, and one bundle signed by the real CLI
// (tests/fixtures/release-evidence/cli-signed). To regenerate that one: in a
// fresh HOME, write a bundle with `evidence.mjs add-step`/`path-file`, its
// Tier A path's `ci-run.json` and `evidence.mjs manifest`, run
// `axocoatl receipt key > gate-keys` and `axocoatl receipt sign-evidence
// manifest.json`, then replace the key line's comment.
import assert from 'node:assert/strict';
import { execFileSync, spawnSync } from 'node:child_process';
import { createHash, generateKeyPairSync, sign } from 'node:crypto';
import {
  mkdirSync, mkdtempSync, readFileSync, rmSync, symlinkSync, writeFileSync,
} from 'node:fs';
import { tmpdir } from 'node:os';
import path from 'node:path';
import { after, test } from 'node:test';
import { fileURLToPath } from 'node:url';

import {
  CI_RUN_SCHEMA, ENVELOPE_FILE, MANIFEST_FILE, PATH_SCHEMA, PAYLOAD_TYPE, buildManifest, bundleDecision,
  commitMatches, compareTotals, decideRelease, overridePhrase, pae, parseActors, parseKeys,
  parseRequiredPaths, sixteenGbMentions, stepsPass, verifyBundle,
} from './evidence.mjs';

const HERE = path.dirname(fileURLToPath(import.meta.url));
const REPO = path.resolve(HERE, '..', '..');
const VERSION = '1.5.0';
const scratch = mkdtempSync(path.join(tmpdir(), 'axocoatl-evidence-test-'));
after(() => rmSync(scratch, { recursive: true, force: true }));

const REQUIRED = `# Release gate

<!-- release-gate:required-paths:start -->
- \`A-x86\`: Tier A on x86_64
- \`C1-local\`: Tier C, local models
- \`C2-local\` (when the public copy names 16 GB): the small-memory Mac
<!-- release-gate:required-paths:end -->
`;

function makeKey() {
  const { publicKey, privateKey } = generateKeyPairSync('ed25519');
  const raw = Buffer.from(publicKey.export({ format: 'jwk' }).x, 'base64url');
  return { privateKey, line: `ed25519 ${raw.toString('base64')} gate@test`, keyId: `sha256:${digest(raw)}` };
}

const digest = (bytes) => createHash('sha256').update(bytes).digest('hex');

let counter = 0;
function git(root, ...args) {
  return execFileSync('git', ['-C', root, ...args], { encoding: 'utf8' }).trim();
}

/** A repository whose first commit is the product; returns its root and commit. */
function makeRepo({ readme = '# product\n' } = {}) {
  counter += 1;
  const root = path.join(scratch, `repo-${counter}`);
  mkdirSync(path.join(root, 'docs'), { recursive: true });
  mkdirSync(path.join(root, '.axocoatl'), { recursive: true });
  writeFileSync(path.join(root, 'README.md'), readme);
  writeFileSync(path.join(root, 'docs', 'RELEASE_GATE.md'), REQUIRED);
  git(scratch, 'init', '-q', '-b', 'main', root);
  git(root, 'config', 'user.name', 'Gate Test');
  git(root, 'config', 'user.email', 'gate@example.invalid');
  git(root, 'add', '.');
  git(root, 'commit', '-qm', 'product');
  return { root, commit: git(root, 'rev-parse', 'HEAD') };
}

function pathEvidence(name, commit, { pass = true, steps } = {}) {
  const recorded = steps ?? [{ name: 'run fix node-deps', status: pass ? 'pass' : 'fail', required: true, exit_code: pass ? 0 : 1 }];
  return { schema: PATH_SCHEMA, path: name, tier: name[0], version: VERSION, commit, pass: stepsPass(recorded), steps: recorded };
}

function signManifest(dir, key) {
  const bytes = readFileSync(path.join(dir, MANIFEST_FILE));
  const envelope = {
    payloadType: PAYLOAD_TYPE,
    payload: bytes.toString('base64'),
    signatures: [{ keyid: key.keyId, sig: sign(null, pae(PAYLOAD_TYPE, bytes), key.privateKey).toString('base64') }],
  };
  writeFileSync(path.join(dir, ENVELOPE_FILE), `${JSON.stringify(envelope)}\n`);
}

/** The run record collect-evidence.sh writes for a path it took from CI. */
function ciRun(commit, overrides = {}) {
  return {
    schema: CI_RUN_SCHEMA, id: 111, run_attempt: 1, html_url: 'https://github.com/axocoatl/axocoatl/actions/runs/111',
    repository: 'axocoatl/axocoatl', head_repository: 'axocoatl/axocoatl', workflow_path: '.github/workflows/ci.yml',
    event: 'push', head_branch: 'main', head_sha: commit, status: 'completed', conclusion: 'success', ...overrides,
  };
}

/**
 * Write (and sign with `key`) a bundle for `commit` with `paths`. Each Tier A
 * path gets the record of a green CI run unless `runs` gives another (or
 * null for none).
 */
function writeBundle(root, commit, paths, key, { version = VERSION, extra = {}, runs = {} } = {}) {
  const dir = path.join(root, 'release-evidence', version, commit);
  mkdirSync(dir, { recursive: true });
  for (const [name, evidence] of Object.entries(paths)) {
    writeFileSync(path.join(dir, `${name}.json`), `${JSON.stringify(evidence, null, 2)}\n`);
    mkdirSync(path.join(dir, name), { recursive: true });
    writeFileSync(path.join(dir, name, 'transcript.txt'), `${name} transcript\n`);
    const run = name in runs ? runs[name] : evidence.tier === 'A' ? ciRun(commit) : null;
    if (run) writeFileSync(path.join(dir, name, 'ci-run.json'), `${JSON.stringify(run, null, 2)}\n`);
  }
  for (const [file, content] of Object.entries(extra)) writeFileSync(path.join(dir, file), content);
  const manifest = buildManifest(dir, { version, commit });
  writeFileSync(path.join(dir, MANIFEST_FILE), `${JSON.stringify(manifest, null, 2)}\n`);
  if (key) signManifest(dir, key);
  return dir;
}

function trust(root, ...keys) {
  writeFileSync(path.join(root, '.axocoatl', 'gate-keys'), `# gate account keys\n${keys.map((key) => key.line).join('\n')}\n`);
}

function allowOverride(root, ...logins) {
  writeFileSync(path.join(root, '.axocoatl', 'gate-override-actors'), `# who may override\n${logins.join('\n')}\n`);
}

function commitAll(root, message = 'evidence') {
  git(root, 'add', '-A');
  git(root, 'commit', '-qm', message);
  return git(root, 'rev-parse', 'HEAD');
}

function decide(root, tagCommit, extra = {}) {
  return decideRelease({
    root, version: VERSION, tagCommit,
    keysFile: path.join(root, '.axocoatl', 'gate-keys'),
    requiredFile: path.join(root, 'docs', 'RELEASE_GATE.md'),
    ...extra,
  });
}

const passingPaths = (commit) => ({
  'A-x86': pathEvidence('A-x86', commit),
  'C1-local': pathEvidence('C1-local', commit),
});

test('the required paths come from the block in RELEASE_GATE.md, the 16 GB ones conditionally', () => {
  assert.deepEqual(parseRequiredPaths(REQUIRED), [
    { name: 'A-x86', when: 'always' }, { name: 'C1-local', when: 'always' }, { name: 'C2-local', when: 'sixteen-gb' },
  ]);
  assert.throws(() => parseRequiredPaths('# nothing'), /required-paths block/);
  const real = parseRequiredPaths(readFileSync(path.join(REPO, 'docs', 'RELEASE_GATE.md'), 'utf8'));
  const always = real.filter((entry) => entry.when === 'always').map((entry) => entry.name);
  assert.deepEqual(always, ['A-x86', 'A-arm', 'C1-local', 'C1-claude-code', 'C1-codex', 'C1-openrouter', 'C1-workbench-webkit', 'D1']);
  assert.ok(real.some((entry) => entry.name.startsWith('C2-') && entry.when === 'sixteen-gb'));
});

test('a tag with no evidence is refused, naming every missing path', () => {
  const { root, commit } = makeRepo();
  trust(root, makeKey());
  const tag = commitAll(root, 'trust a key');
  const decision = decide(root, tag);
  assert.equal(decision.ok, false);
  assert.equal(decision.status, 'fail');
  assert.match(decision.lines[0], /FAIL for v1\.5\.0/);
  assert.ok(decision.lines.includes('Missing evidence for: A-x86, C1-local'), decision.lines.join('\n'));
  assert.ok(decision.lines.some((line) => line.includes('no evidence bundle exists under release-evidence/1.5.0/')));
  assert.notEqual(commit, tag);
});

test('a signed bundle that passes every required path lets the tag publish, from the evidence commit on top (which may also add the gate key)', () => {
  const { root, commit } = makeRepo();
  const key = makeKey();
  trust(root, key);
  writeBundle(root, commit, passingPaths(commit), key);
  const tag = commitAll(root);
  const decision = decide(root, tag);
  assert.equal(decision.ok, true, decision.lines.join('\n'));
  assert.equal(decision.status, 'pass');
  assert.match(decision.lines[0], /^New-user gate: PASS for v1\.5\.0/);
  assert.ok(decision.lines.includes('  A-x86: pass'));
  assert.equal(commitMatches(root, commit, tag).ok, true);
  // The same bundle is evidence for its own commit too.
  assert.equal(commitMatches(root, commit, commit).ok, true);
});

test('a product change after the tested commit makes its bundle no evidence for the tag', () => {
  const { root, commit } = makeRepo();
  const key = makeKey();
  trust(root, key);
  writeBundle(root, commit, passingPaths(commit), key);
  writeFileSync(path.join(root, 'README.md'), '# product, changed after the gate ran\n');
  const tag = commitAll(root);
  const decision = decide(root, tag);
  assert.equal(decision.ok, false);
  assert.ok(decision.lines.some((line) => /outside release-evidence\/, \.axocoatl\/gate-keys and \.axocoatl\/gate-override-actors: .*README\.md/.test(line)), decision.lines.join('\n'));
});

test('evidence for a commit that is not an ancestor of the tag is refused', () => {
  const { root, commit } = makeRepo();
  const key = makeKey();
  git(root, 'checkout', '-q', '-b', 'side');
  writeFileSync(path.join(root, 'side.txt'), 'side\n');
  const side = commitAll(root, 'side');
  git(root, 'checkout', '-q', 'main');
  trust(root, key);
  writeBundle(root, side, passingPaths(side), key);
  const tag = commitAll(root);
  const decision = decide(root, tag);
  assert.equal(decision.ok, false);
  assert.ok(decision.lines.some((line) => line.includes('is not an ancestor of')), decision.lines.join('\n'));
  assert.notEqual(side, commit);
});

test('a bundle signed by a key not in .axocoatl/gate-keys, or not signed, is refused', () => {
  const { root, commit } = makeRepo();
  const trusted = makeKey();
  const stranger = makeKey();
  trust(root, trusted);
  writeBundle(root, commit, passingPaths(commit), stranger);
  const tag = commitAll(root);
  const decision = decide(root, tag);
  assert.equal(decision.ok, false);
  assert.ok(decision.lines.some((line) => line.includes('no signature in manifest.dsse.json verifies with a key in the gate keys')), decision.lines.join('\n'));

  const unsigned = makeRepo();
  trust(unsigned.root, trusted);
  writeBundle(unsigned.root, unsigned.commit, passingPaths(unsigned.commit), null);
  const unsignedDecision = decide(unsigned.root, commitAll(unsigned.root));
  assert.ok(unsignedDecision.lines.some((line) => line.includes('manifest.dsse.json is missing')), unsignedDecision.lines.join('\n'));
});

test('without any gate key nothing verifies', () => {
  const { root, commit } = makeRepo();
  const key = makeKey();
  writeFileSync(path.join(root, '.axocoatl', 'gate-keys'), '# no key yet\n');
  writeBundle(root, commit, passingPaths(commit), key);
  const decision = decide(root, commitAll(root));
  assert.equal(decision.ok, false);
  assert.ok(decision.lines.some((line) => line.includes('lists no key: no gate key is trusted')), decision.lines.join('\n'));
  rmSync(path.join(root, '.axocoatl', 'gate-keys'));
  assert.ok(decide(root, git(root, 'rev-parse', 'HEAD')).lines.some((line) => line.includes('gate-keys is missing')));
});

test('a file changed, added or replaced by a link after signing breaks the bundle', () => {
  const key = makeKey();
  for (const [tamper, expected] of [
    [(dir) => writeFileSync(path.join(dir, 'A-x86', 'transcript.txt'), 'edited\n'), 'A-x86/transcript.txt changed after the manifest was signed'],
    [(dir) => writeFileSync(path.join(dir, 'A-x86', 'extra.txt'), 'unlisted\n'), 'A-x86/extra.txt is in the bundle but not in the signed manifest'],
    [(dir) => rmSync(path.join(dir, 'C1-local', 'transcript.txt')), 'C1-local/transcript.txt is in the signed manifest but missing'],
    [(dir) => writeFileSync(path.join(dir, MANIFEST_FILE), '{"schema":"axocoatl.release-evidence/1"}\n'), 'manifest.json differs from the signed manifest'],
    [(dir) => { rmSync(path.join(dir, 'A-x86', 'transcript.txt')); symlinkSync('/etc/hosts', path.join(dir, 'A-x86', 'transcript.txt')); }, 'A-x86/transcript.txt is a symbolic link'],
  ]) {
    const { root, commit } = makeRepo();
    trust(root, key);
    const dir = writeBundle(root, commit, passingPaths(commit), key);
    tamper(dir);
    const decision = decide(root, commitAll(root));
    assert.equal(decision.ok, false, expected);
    assert.ok(decision.lines.some((line) => line.includes(expected)), `${expected}\n${decision.lines.join('\n')}`);
  }
});

test('a failed, skipped or self-contradicting path is refused by name', () => {
  const key = makeKey();
  const cases = [
    [(commit) => pathEvidence('C1-local', commit, { pass: false }), 'step "run fix node-deps" failed'],
    [(commit) => pathEvidence('C1-local', commit, { steps: [{ name: 'workbench in Safari', status: 'skip', required: true }] }), 'step "workbench in Safari" was skipped'],
    [(commit) => ({ ...pathEvidence('C1-local', commit, { pass: false }), pass: true }), 'says it passed, but required steps did not'],
    [(commit) => ({ ...pathEvidence('C1-local', commit), commit: 'f'.repeat(40) }), `commit "${'f'.repeat(40)}"`],
    [(commit) => ({ ...pathEvidence('C1-local', commit), path: 'C1-codex' }), 'the file is evidence of "C1-codex"'],
  ];
  for (const [make, expected] of cases) {
    const { root, commit } = makeRepo();
    trust(root, key);
    const dir = path.join(root, 'release-evidence', VERSION, commit);
    mkdirSync(dir, { recursive: true });
    const evidence = make(commit);
    writeFileSync(path.join(dir, 'A-x86.json'), JSON.stringify(pathEvidence('A-x86', commit)));
    writeFileSync(path.join(dir, 'C1-local.json'), JSON.stringify(evidence));
    const manifest = {
      schema: 'axocoatl.release-evidence/1', version: VERSION, commit, created_at: 'now',
      paths: { 'A-x86': { file: 'A-x86.json', pass: true }, 'C1-local': { file: 'C1-local.json', pass: evidence.pass } },
      files: ['A-x86.json', 'C1-local.json'].map((file) => ({ path: file, sha256: digest(readFileSync(path.join(dir, file))) })),
    };
    writeFileSync(path.join(dir, MANIFEST_FILE), JSON.stringify(manifest));
    signManifest(dir, key);
    const decision = decide(root, commitAll(root));
    assert.equal(decision.ok, false, expected);
    assert.ok(decision.lines.some((line) => line.includes(expected)), `${expected}\n${decision.lines.join('\n')}`);
  }
});

test('a Tier A path counts only with the record of a green ci.yml push run of the commit', () => {
  const key = makeKey();
  const cases = [
    [null, /A-x86: Tier A evidence comes only from a CI run, and A-x86\/ci-run\.json is missing/],
    [{ conclusion: 'failure' }, /A-x86: CI run 111 concluded "failure", not success/],
    [{ status: 'in_progress', conclusion: null }, /concluded "in_progress", not success/],
    [{ head_sha: 'f'.repeat(40) }, /ran commit "f{40}"/],
    [{ event: 'pull_request' }, /is a "pull_request" run, not a push run/],
    [{ workflow_path: '.github/workflows/release.yml' }, /not \.github\/workflows\/ci\.yml/],
    [{ head_repository: 'someone/axocoatl' }, /ran "someone\/axocoatl"'s code in "axocoatl\/axocoatl"/],
    [{ schema: 'something/1' }, /A-x86\/ci-run\.json: schema is not/],
  ];
  for (const [change, expected] of cases) {
    const { root, commit } = makeRepo();
    trust(root, key);
    writeBundle(root, commit, passingPaths(commit), key, { runs: { 'A-x86': change && ciRun(commit, change) } });
    const result = decide(root, commitAll(root));
    assert.equal(result.ok, false, JSON.stringify(change));
    assert.match(result.lines.join('\n'), expected);
  }
  // The same bundle with the green run passes; a Tier C path needs no run.
  const { root, commit } = makeRepo();
  trust(root, key);
  const dir = writeBundle(root, commit, passingPaths(commit), key);
  assert.equal(decide(root, commitAll(root)).ok, true);
  assert.deepEqual(verifyBundle(dir, { keys: parseKeys(key.line), commit }).paths.get('C1-local').problems, []);
});

test('optional steps may fail without failing the path', () => {
  assert.equal(stepsPass([{ name: 'a', status: 'pass' }, { name: 'timing', status: 'fail', required: false }]), true);
  assert.equal(stepsPass([{ name: 'a', status: 'skip' }]), false);
  assert.equal(stepsPass([]), false);
});

test('naming 16 GB anywhere in the public copy makes the C2 paths required', () => {
  const { root, commit } = makeRepo({ readme: '# product\n\nRuns on a 16 GB Mac.\n' });
  assert.deepEqual(sixteenGbMentions(root), ['README.md:3']);
  const key = makeKey();
  trust(root, key);
  writeBundle(root, commit, passingPaths(commit), key);
  const tag = commitAll(root);
  const without = decide(root, tag);
  assert.equal(without.ok, false);
  assert.ok(without.lines.some((line) => line.includes('names 16 GB (README.md:3)')), without.lines.join('\n'));
  assert.ok(without.lines.includes('Missing evidence for: C2-local'), without.lines.join('\n'));
  rmSync(path.join(root, 'release-evidence'), { recursive: true });
  writeBundle(root, commit, { ...passingPaths(commit), 'C2-local': pathEvidence('C2-local', commit) }, key);
  const withC2 = decide(root, commitAll(root, 'C2 evidence'));
  assert.equal(withC2.ok, true, withC2.lines.join('\n'));
});

test('the override passes a failing gate only from workflow_dispatch with the exact phrase, and says so loudly', () => {
  const { root } = makeRepo();
  trust(root, makeKey());
  allowOverride(root, 'Founder');
  const tag = commitAll(root);
  assert.equal(overridePhrase(VERSION), 'publish v1.5.0 without the new-user gate');
  const pushed = decide(root, tag, { override: overridePhrase(VERSION), event: 'push' });
  assert.equal(pushed.ok, false);
  assert.match(pushed.lines[0], /only from a manual workflow_dispatch run/);
  const wrong = decide(root, tag, { override: 'publish anyway', event: 'workflow_dispatch' });
  assert.equal(wrong.ok, false);
  assert.match(wrong.lines[0], /does not match/);
  const otherVersion = decide(root, tag, { override: overridePhrase('1.4.9'), event: 'workflow_dispatch' });
  assert.equal(otherVersion.ok, false);
  const overridden = decide(root, tag, { override: overridePhrase(VERSION), event: 'workflow_dispatch', actor: 'founder' });
  assert.equal(overridden.ok, true);
  assert.equal(overridden.status, 'overridden');
  assert.match(overridden.lines[0], /OVERRIDDEN for v1\.5\.0 .* by founder, listed in \.axocoatl\/gate-override-actors, who typed the override phrase/);
  assert.ok(overridden.lines.includes('Missing evidence for: A-x86, C1-local'));
});

test('only a login listed in .axocoatl/gate-override-actors may override, as actor and as the one who ran it', () => {
  const { root } = makeRepo();
  trust(root, makeKey());
  const tag = commitAll(root);
  const phrase = { override: overridePhrase(VERSION), event: 'workflow_dispatch' };
  const unlisted = decide(root, tag, { ...phrase, actor: 'founder' });
  assert.equal(unlisted.ok, false);
  assert.match(unlisted.lines[0], /gate-override-actors lists no GitHub login, so no one may override/);
  allowOverride(root, 'founder  # the founder', '', '# a comment');
  const stranger = decide(root, tag, { ...phrase, actor: 'someone-else' });
  assert.equal(stranger.ok, false);
  assert.match(stranger.lines[0], /actor "someone-else" is not listed/);
  const rerun = decide(root, tag, { ...phrase, actor: 'founder', triggeringActor: 'someone-else' });
  assert.equal(rerun.ok, false);
  assert.match(rerun.lines[0], /triggering actor "someone-else" is not listed/);
  const listed = decide(root, tag, { ...phrase, actor: 'FOUNDER', triggeringActor: 'founder' });
  assert.equal(listed.status, 'overridden', listed.lines.join('\n'));
  // Without the phrase, a listed login changes nothing.
  assert.equal(decide(root, tag, { event: 'workflow_dispatch', actor: 'founder' }).ok, false);
  allowOverride(root, 'not a login!');
  assert.match(decide(root, tag, { ...phrase, actor: 'founder' }).lines[0], /is not a GitHub login; no one may override/);
  assert.deepEqual(parseActors('# x\nA-b\n\n  c  # d\n'), ['a-b', 'c']);
  assert.throws(() => parseActors('-bad'), /not a GitHub login/);
});

test('a bundle is judged on the required paths; another path in it may fail, and the override list may change after the tested commit', () => {
  const { root, commit } = makeRepo();
  const key = makeKey();
  trust(root, key);
  const dir = writeBundle(root, commit, { ...passingPaths(commit), 'C1-real-repo': pathEvidence('C1-real-repo', commit, { pass: false }) }, key);
  allowOverride(root, 'founder');
  const tag = commitAll(root);
  const judged = bundleDecision(dir, { keys: parseKeys(key.line), version: VERSION, commit, needed: ['A-x86', 'C1-local'] });
  assert.equal(judged.ok, true);
  assert.deepEqual(judged.extraFailing, ['C1-real-repo']);
  assert.equal(decide(root, tag).ok, true);
  const short = bundleDecision(dir, { keys: parseKeys(key.line), version: VERSION, commit, needed: ['A-x86', 'C1-local', 'D1', 'C1-real-repo'] });
  assert.equal(short.ok, false);
  assert.deepEqual(short.missing, ['D1']);
  assert.deepEqual(short.failing, ['C1-real-repo']);
  // verify-bundle --required decides the same way (collect-evidence.sh).
  writeFileSync(path.join(scratch, `keys-${counter}`), `${key.line}\n`);
  const cli = (...args) => spawnSync(process.execPath, [path.join(HERE, 'evidence.mjs'), 'verify-bundle', dir, '--keys', path.join(scratch, `keys-${counter}`), ...args], { encoding: 'utf8' });
  const required = cli('--required', path.join(root, 'docs', 'RELEASE_GATE.md'), '--root', root);
  assert.equal(required.status, 0, required.stdout);
  assert.match(required.stdout, /C1-real-repo: warning: failed \(not a required path\)/);
  assert.match(required.stdout, /every required path passes/);
  assert.equal(cli().status, 1, 'without --required every path must pass');
  writeFileSync(path.join(root, 'docs', 'RELEASE_GATE.md'), REQUIRED.replace('- `C1-local`', '- `D1`: Tier D\n- `C1-local`'));
  const missing = cli('--required', path.join(root, 'docs', 'RELEASE_GATE.md'), '--root', root);
  assert.equal(missing.status, 1);
  assert.match(missing.stdout, /missing required paths: D1/);
});

test('check-evidence.mjs exits 0, 1 or 2 and writes the job summary', () => {
  const { root, commit } = makeRepo();
  const key = makeKey();
  trust(root, key);
  writeBundle(root, commit, passingPaths(commit), key);
  const tag = commitAll(root);
  const summary = path.join(scratch, `summary-${counter}.md`);
  const run = (...args) => spawnSync(process.execPath, [path.join(HERE, 'check-evidence.mjs'), '--root', root, ...args], {
    encoding: 'utf8', env: { ...process.env, GITHUB_ACTIONS: 'true', GITHUB_STEP_SUMMARY: summary },
  });
  const pass = run('--version', 'v1.5.0', '--tag-commit', tag);
  assert.equal(pass.status, 0, pass.stdout + pass.stderr);
  assert.match(readFileSync(summary, 'utf8'), /## New-user gate: pass/);
  const refused = run('--version', '1.5.1', '--tag-commit', tag);
  assert.equal(refused.status, 1, refused.stdout);
  assert.match(refused.stdout, /::error title=New-user gate::New-user gate: FAIL for v1\.5\.1/);
  const notListed = run('--version', '1.5.1', '--tag-commit', tag, '--override', overridePhrase('1.5.1'), '--event', 'workflow_dispatch', '--actor', 'founder');
  assert.equal(notListed.status, 1, notListed.stdout);
  allowOverride(root, 'founder');
  const overridden = run('--version', '1.5.1', '--tag-commit', tag, '--override', overridePhrase('1.5.1'), '--event', 'workflow_dispatch', '--actor', 'founder', '--triggering-actor', 'founder');
  assert.equal(overridden.status, 0, overridden.stdout);
  assert.match(overridden.stdout, /::warning title=New-user gate overridden::/);
  assert.match(readFileSync(summary, 'utf8'), /## New-user gate: OVERRIDDEN/);
  assert.equal(run('--version', '1.5.0').status, 2);
  assert.equal(run('--tag-commit', tag, '--version', '1.5.0', '--bogus', 'x').status, 2);
});

test('a bundle signed by the real `axocoatl receipt sign-evidence` verifies here', () => {
  const fixture = path.join(REPO, 'tests', 'fixtures', 'release-evidence', 'cli-signed');
  const keys = parseKeys(readFileSync(path.join(fixture, 'gate-keys'), 'utf8'));
  const result = verifyBundle(path.join(fixture, 'bundle'), { keys, version: VERSION, commit: '0123456789abcdef0123456789abcdef01234567' });
  assert.deepEqual(result.problems, []);
  assert.equal(result.paths.get('A-x86').pass, true);
  assert.equal(result.key.shortId, 'f9b090a441fbe4d5');
  // Another key does not verify it.
  const other = verifyBundle(path.join(fixture, 'bundle'), { keys: parseKeys(makeKey().line) });
  assert.ok(other.problems[0].includes('no signature'));
});

test('buildManifest refuses evidence of another version or commit', () => {
  const dir = path.join(scratch, 'mismatch');
  mkdirSync(dir, { recursive: true });
  writeFileSync(path.join(dir, 'A-x86.json'), JSON.stringify(pathEvidence('A-x86', 'a'.repeat(40))));
  assert.throws(() => buildManifest(dir, { version: VERSION, commit: 'b'.repeat(40) }), /not 1\.5\.0 at b{40}/);
  assert.throws(() => buildManifest(dir, { version: 'v1', commit: 'b'.repeat(40) }), /invalid version/);
});

test('time and download are compared with the previous release, flagging growth over 20%', () => {
  const run = (version, seconds, bytes) => ({ path: 'C1-local', version, steps: [{ name: 'a', status: 'pass', seconds, bytes_downloaded: bytes }, { name: 'b', status: 'pass' }] });
  const lines = compareTotals(run('1.4.0', 100, 1000), run('1.5.0', 130, 1100));
  assert.equal(lines[0], 'C1-local: 1.4.0 -> 1.5.0');
  assert.match(lines[1], /100\.0s -> 130\.0s \(\+30%\) REGRESSION/);
  assert.match(lines[2], /1000 bytes -> 1100 bytes \(\+10%\)$/);
  assert.match(compareTotals({}, run('1.5.0', 5, 0))[1], /no previous figure/);
});
