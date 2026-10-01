import assert from 'node:assert/strict';
import { execFileSync } from 'node:child_process';
import { chmodSync, mkdtempSync, mkdirSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { dirname, join } from 'node:path';
import test from 'node:test';

import { provenanceSourceExcludes, sourceContentDigest } from './film-lib.mjs';

function git(root, ...args) {
  execFileSync('git', args, { cwd: root, stdio: 'pipe' });
}

function write(root, path, content) {
  const absolute = join(root, path);
  mkdirSync(dirname(absolute), { recursive: true });
  writeFileSync(absolute, content);
}

test('source content digest is representation-independent and preserves recording exclusions', (t) => {
  const root = mkdtempSync(join(tmpdir(), 'axocoatl-film-source-digest-'));
  t.after(() => rmSync(root, { recursive: true, force: true }));
  git(root, 'init', '--quiet');
  git(root, 'config', 'user.name', 'Film Test');
  git(root, 'config', 'user.email', 'film-test@example.invalid');

  write(root, '.gitignore', 'ignored.txt\n');
  write(root, 'tracked.txt', 'tracked\n');
  git(root, 'add', '.gitignore', 'tracked.txt');
  git(root, 'commit', '--quiet', '-m', 'initial');

  const initial = sourceContentDigest(root);
  write(root, 'candidate.txt', 'same candidate bytes\n');
  const dirty = sourceContentDigest(root);
  assert.notEqual(dirty, initial, 'untracked source content must participate');
  git(root, 'add', 'candidate.txt');
  assert.equal(sourceContentDigest(root), dirty, 'staging identical checkout content must not change the digest');
  git(root, 'commit', '--quiet', '-m', 'record candidate');
  assert.equal(sourceContentDigest(root), dirty, 'committing identical checkout content must not change the digest');

  chmodSync(join(root, 'candidate.txt'), 0o755);
  assert.notEqual(sourceContentDigest(root), dirty, 'normalized executable state must participate');
  chmodSync(join(root, 'candidate.txt'), 0o644);
  assert.equal(sourceContentDigest(root), dirty, 'restoring executable state must restore the digest');

  for (const [index, prefix] of provenanceSourceExcludes.entries()) {
    write(root, `${prefix}excluded-${index}.txt`, `excluded change ${index}\n`);
  }
  assert.equal(sourceContentDigest(root), dirty, 'recording outputs must stay excluded');

  write(root, 'ignored.txt', 'ignored output\n');
  assert.equal(sourceContentDigest(root), dirty, 'Git-ignored files must stay outside the source contract');

  write(root, 'candidate.txt', 'changed candidate bytes\n');
  assert.notEqual(sourceContentDigest(root), dirty, 'source byte changes must change the digest');
});

test('versioned recordings require matching canonical media and provenance without replacing historical paths', async () => {
  const {loadPortfolio, validatePortfolio, filmSourceDirectory} = await import('./film-lib.mjs');
  const portfolio = loadPortfolio();
  assert.equal(portfolio.recording_version, 'v1.1.1');
  assert.match(filmSourceDirectory(portfolio, portfolio.films[0].slug), /\/source\/v1\.1\.1\/session-workbench$/);
  for (const version of ['../v1.1.0', 'v1.1.0/extra', '1.1.0', 'v01.1.0']) {
    assert.throws(() => validatePortfolio({...portfolio, recording_version:version}), /recording_version/);
  }
  const crossed = structuredClone(portfolio);
  crossed.films[0].media.mp4 = 'sites/marketing/assets/films/session-workbench.mp4';
  assert.throws(() => validatePortfolio(crossed), /media must/);
  const old = structuredClone(portfolio); delete old.recording_version;
  for (const film of old.films) {
    film.media.mp4 = `sites/marketing/assets/films/${film.slug}.mp4`;
    film.media.poster = `sites/marketing/assets/films/${film.slug}.jpg`;
    film.provenance = `demo/one-app/films/provenance/${film.slug}.json`;
  }
  assert.doesNotThrow(() => validatePortfolio(old));
  assert.match(filmSourceDirectory(old, old.films[0].slug), /\/source\/session-workbench$/);
  const wrongProvenance = structuredClone(portfolio);
  wrongProvenance.films[0].provenance = 'demo/one-app/films/provenance/v1.0.0/session-workbench.json';
  assert.throws(() => validatePortfolio(wrongProvenance), /canonical per-film path/);
});
