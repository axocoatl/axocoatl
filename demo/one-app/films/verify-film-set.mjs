#!/usr/bin/env node

import { existsSync, lstatSync, readFileSync } from 'node:fs';
import { dirname, resolve } from 'node:path';
import {
  fail,
  loadPortfolio,
  probePoster,
  probeVideo,
  readJson,
  repoRoot,
  resolveRepoPath,
  run,
  sha256Buffer,
  sha256File,
  sourceContentDigest,
  validateTimeline,
} from './film-lib.mjs';
import { verifyReleaseCompatibility } from './release-compatibility.mjs';

function usage() {
  console.error(`Usage: verify-film-set.mjs [--manifest-only | --portable | --source-bound | --allow-needs-recording]
       verify-film-set.mjs --release-compatibility <attestation> --release-root <frozen-checkout>

With no flag, verification is release-strict: all 12 films must be ready, match
the technical contract and duration, have complete recorded provenance, and match
the local binary to the first-committed declared hash/version and source content.

--manifest-only verifies the authoritative portfolio, scenario references, and
12 shot-contract anchors without inspecting media.

--portable verifies every recorded source frame, capture record, evidence record,
timeline, staged-sequence digest, and final media file. It validates the recorded
binary declaration without requiring that platform-specific binary in the checkout;
the binary bytes themselves are not preserved by this portfolio.

--source-bound performs portable verification and additionally requires the
recorded source-content digest to match this checkout. It does not require the
platform-specific capture binary. It remains the strict contract for a new capture
or a checkout that exactly materializes the first-committed source declaration.

--allow-needs-recording performs the same structural verification, warns for
films explicitly marked needs_recording, and strictly verifies any ready film.

--release-compatibility performs portable verification in the explicit frozen
release checkout, audits its historical source/binary-only provenance rewrite
against the restored first-committed declarations in the control checkout, and
binds the complete release delta. It does not claim capture with the patch binary
and never changes or falls back from the ordinary source-bound gate.
`);
}

function assert(condition, message) {
  if (!condition) fail(message);
}

function assertRegular(path, label) {
  assert(existsSync(path), `${label} does not exist: ${path}`);
  const stat = lstatSync(path);
  assert(stat.isFile() && !stat.isSymbolicLink(), `${label} must be a regular non-symlink file: ${path}`);
}

function assertHash(value, label) {
  assert(/^[0-9a-f]{64}$/.test(value || ''), `${label} must be a lowercase SHA-256 digest.`);
}

function canonicalInstant(value, label) {
  assert(typeof value === 'string' && value, `${label} must be a string.`);
  const instant = new Date(value);
  assert(!Number.isNaN(instant.valueOf()) && instant.toISOString() === value, `${label} must be canonical ISO-8601 UTC.`);
}

function jsonEqual(left, right) {
  return JSON.stringify(left) === JSON.stringify(right);
}

function verifyMedia(portfolio, film, activeRepoRoot) {
  const contract = portfolio.media_contract;
  const mp4Path = resolveRepoPath(film.media.mp4, activeRepoRoot);
  const posterPath = resolveRepoPath(film.media.poster, activeRepoRoot);
  assertRegular(mp4Path, `${film.slug} MP4`);
  assertRegular(posterPath, `${film.slug} poster`);
  const video = probeVideo(mp4Path);
  const poster = probePoster(posterPath);
  assert(video.codec === contract.video_codec, `${film.slug}: video codec ${video.codec} != ${contract.video_codec}.`);
  assert(video.width === contract.width && video.height === contract.height, `${film.slug}: video dimensions are not ${contract.width}x${contract.height}.`);
  assert(video.pixel_format === contract.pixel_format, `${film.slug}: pixel format ${video.pixel_format} != ${contract.pixel_format}.`);
  assert(video.fps === `${contract.output_fps}/1`, `${film.slug}: frame rate ${video.fps} != ${contract.output_fps}/1.`);
  assert(video.audio_streams === (contract.audio ? 1 : 0), `${film.slug}: audio stream count violates the contract.`);
  assert(video.fast_start === contract.fast_start, `${film.slug}: MP4 fast-start contract failed.`);
  assert(
    video.duration_seconds >= film.duration_seconds.min && video.duration_seconds <= film.duration_seconds.max,
    `${film.slug}: duration ${video.duration_seconds}s is outside ${film.duration_seconds.min}-${film.duration_seconds.max}s.`,
  );
  assert(poster.codec === contract.poster_codec, `${film.slug}: poster codec ${poster.codec} != ${contract.poster_codec}.`);
  assert(
    poster.width === contract.poster_width && poster.height === contract.poster_height,
    `${film.slug}: poster dimensions are not ${contract.poster_width}x${contract.poster_height}.`,
  );
  return { mp4Path, posterPath, video, poster };
}

function verifyProvenance(portfolio, film, media, { verifyLocalBinary, currentSourceContentSha256, activeRepoRoot }) {
  const path = resolveRepoPath(film.provenance, activeRepoRoot);
  assertRegular(path, `${film.slug} provenance`);
  const provenance = readJson(path);
  assert([1, 2].includes(provenance.schema_version), `${film.slug}: provenance.schema_version must be 1 or 2.`);
  assert(provenance.film === film.slug, `${film.slug}: provenance film identity mismatch.`);
  canonicalInstant(provenance.recorded_at, `${film.slug}.recorded_at`);

  assert(provenance.source && typeof provenance.source === 'object', `${film.slug}: missing source provenance.`);
  assert(/^[0-9a-f]{40}$/.test(provenance.source.head || ''), `${film.slug}: source.head must be a full Git SHA-1.`);
  assert(typeof provenance.source.branch === 'string' && provenance.source.branch, `${film.slug}: source.branch is missing.`);
  assert(typeof provenance.source.dirty === 'boolean', `${film.slug}: source.dirty must be boolean.`);
  if (provenance.source.dirty) assertHash(provenance.source.patch_sha256, `${film.slug}.source.patch_sha256`);
  else assert(provenance.source.patch_sha256 === null, `${film.slug}: clean source must use a null patch_sha256.`);
  if (provenance.schema_version >= 2 || provenance.source.content_sha256 !== undefined) {
    assertHash(provenance.source.content_sha256, `${film.slug}.source.content_sha256`);
  }
  if (currentSourceContentSha256) {
    assert(
      provenance.source.content_sha256 !== undefined,
      'source.content_sha256 is missing; verification against the current checkout requires provenance schema v2. ' +
        'Recapture the film with demo/one-app/films/write-provenance.mjs.',
    );
    assert(
      provenance.schema_version >= 2,
      'source-bound verification requires provenance schema v2. Recapture the film with ' +
        'demo/one-app/films/write-provenance.mjs.',
    );
    assert(
      provenance.source.content_sha256 === currentSourceContentSha256,
      'source content differs from the recorded checkout. Recapture the film with the exact release source.',
    );
  }
  if (verifyLocalBinary) run('git', ['cat-file', '-e', `${provenance.source.head}^{commit}`], { cwd: activeRepoRoot });
  assert(
    jsonEqual(provenance.source.patch_excludes, [
      'demo/one-app/films/source/',
      'demo/one-app/films/staged/',
      'demo/one-app/films/provenance/',
      'sites/marketing/assets/films/',
    ]),
    `${film.slug}: source patch exclusions do not match the recording contract.`,
  );

  assert(provenance.binary && typeof provenance.binary === 'object', `${film.slug}: missing binary provenance.`);
  const binaryPath = resolveRepoPath(provenance.binary.path, activeRepoRoot);
  assertHash(provenance.binary.sha256, `${film.slug}.binary.sha256`);
  assert(typeof provenance.binary.version === 'string' && provenance.binary.version, `${film.slug}: binary.version is missing.`);
  if (verifyLocalBinary) {
    assertRegular(binaryPath, `${film.slug} release binary`);
    assert(provenance.binary.sha256 === sha256File(binaryPath), `${film.slug}: release binary hash changed.`);
    const binaryVersion = String(run(binaryPath, ['--version'])).trim();
    assert(provenance.binary.version === binaryVersion && binaryVersion, `${film.slug}: release binary version changed.`);
  }

  assert(provenance.capture && typeof provenance.capture === 'object', `${film.slug}: missing capture provenance.`);
  const capturePath = resolveRepoPath(provenance.capture.record, activeRepoRoot);
  assertRegular(capturePath, `${film.slug} capture record`);
  assertHash(provenance.capture.record_sha256, `${film.slug}.capture.record_sha256`);
  assert(provenance.capture.record_sha256 === sha256File(capturePath), `${film.slug}: capture record hash changed.`);
  const capture = readJson(capturePath);
  assert(capture.schema_version === 1 && capture.film === film.slug, `${film.slug}: capture record identity mismatch.`);
  canonicalInstant(capture.captured_at, `${film.slug} capture instant`);
  assert(capture.captured_at === provenance.recorded_at, `${film.slug}: recorded_at differs from capture time.`);
  assert(jsonEqual(capture.viewport, provenance.capture.viewport), `${film.slug}: capture viewport provenance mismatch.`);
  assert(
    capture.viewport?.width === portfolio.media_contract.width &&
      capture.viewport?.height === portfolio.media_contract.height &&
      capture.viewport?.device_scale_factor === 1,
    `${film.slug}: capture viewport must be 1280x720 at device scale factor 1.`,
  );
  assert(capture.theme === provenance.capture.theme && ['light', 'dark'].includes(capture.theme), `${film.slug}: capture theme mismatch.`);
  assert(
    capture.reduced_motion === provenance.capture.reduced_motion && ['reduce', 'no-preference'].includes(capture.reduced_motion),
    `${film.slug}: reduced-motion provenance mismatch.`,
  );
  assert(capture.url === provenance.capture.url && capture.browser === provenance.capture.browser, `${film.slug}: capture client provenance mismatch.`);
  assert(jsonEqual(capture.keyframes, provenance.capture.keyframes), `${film.slug}: capture keyframe provenance mismatch.`);
  assert(Array.isArray(capture.keyframes) && capture.keyframes.length === film.beats.length, `${film.slug}: capture keyframe count mismatch.`);
  const keyframeHashes = new Map();
  for (const [index, keyframe] of capture.keyframes.entries()) {
    assert(keyframe.beat === film.beats[index].id, `${film.slug}: capture beat order mismatch.`);
    const keyframePath = resolveRepoPath(keyframe.path, activeRepoRoot);
    assertRegular(keyframePath, `${film.slug} ${keyframe.beat} keyframe`);
    assertHash(keyframe.sha256, `${film.slug} ${keyframe.beat} keyframe hash`);
    assert(keyframe.sha256 === sha256File(keyframePath), `${film.slug}: keyframe hash changed for ${keyframe.beat}.`);
    const previousBeat = keyframeHashes.get(keyframe.sha256);
    assert(!previousBeat, `${film.slug}: capture beats ${previousBeat} and ${keyframe.beat} use the same keyframe.`);
    keyframeHashes.set(keyframe.sha256, keyframe.beat);
    const probe = probePoster(keyframePath);
    assert(
      probe.codec === portfolio.media_contract.poster_codec &&
        probe.width === portfolio.media_contract.width &&
        probe.height === portfolio.media_contract.height,
      `${film.slug}: keyframe ${keyframe.beat} violates the source-frame contract.`,
    );
  }

  assert(provenance.edit && typeof provenance.edit === 'object', `${film.slug}: missing edit provenance.`);
  const timelinePath = resolveRepoPath(provenance.edit.timeline, activeRepoRoot);
  const stagePath = resolveRepoPath(provenance.edit.stage_record, activeRepoRoot);
  assertRegular(timelinePath, `${film.slug} timeline`);
  assertRegular(stagePath, `${film.slug} stage record`);
  assertHash(provenance.edit.timeline_sha256, `${film.slug}.edit.timeline_sha256`);
  assertHash(provenance.edit.stage_record_sha256, `${film.slug}.edit.stage_record_sha256`);
  assert(provenance.edit.timeline_sha256 === sha256File(timelinePath), `${film.slug}: timeline hash changed.`);
  assert(provenance.edit.stage_record_sha256 === sha256File(stagePath), `${film.slug}: stage record hash changed.`);
  assert(capture.timeline === provenance.edit.timeline, `${film.slug}: capture and edit timeline paths differ.`);
  assert(capture.timeline_sha256 === provenance.edit.timeline_sha256, `${film.slug}: capture and edit timeline hashes differ.`);
  const timeline = readJson(timelinePath);
  const calculated = validateTimeline(portfolio, film, timeline);
  const stage = readJson(stagePath);
  assert(stage.schema_version === 1 && stage.film === film.slug, `${film.slug}: stage identity mismatch.`);
  assert(stage.timeline === provenance.edit.timeline && stage.timeline_sha256 === provenance.edit.timeline_sha256, `${film.slug}: stage timeline mismatch.`);
  assert(stage.input_fps === portfolio.media_contract.input_fps, `${film.slug}: stage input fps mismatch.`);
  assert(stage.frame_count === calculated.frameCount && stage.duration_seconds === calculated.durationSeconds, `${film.slug}: stage timing mismatch.`);
  assertHash(stage.sequence_sha256, `${film.slug}.stage.sequence_sha256`);
  assert(Array.isArray(stage.shots) && stage.shots.length === film.beats.length, `${film.slug}: stage shot count mismatch.`);
  const sequenceParts = [];
  let sequenceFrame = 1;
  for (const [index, shot] of stage.shots.entries()) {
    const declared = timeline.shots[index];
    assert(
      shot.beat === declared.beat &&
        shot.source === declared.source &&
        shot.hold_frames === declared.hold_frames &&
        shot.first_frame === sequenceFrame &&
        shot.last_frame === sequenceFrame + declared.hold_frames - 1,
      `${film.slug}: stage shot ${index} does not match timeline expansion.`,
    );
    assertHash(shot.source_sha256, `${film.slug}.${shot.beat}.source_sha256`);
    const sourceFramePath = resolve(dirname(timelinePath), declared.source);
    assertRegular(sourceFramePath, `${film.slug} ${shot.beat} stage source`);
    assert(shot.source_sha256 === sha256File(sourceFramePath), `${film.slug}: stage source hash changed for ${shot.beat}.`);
    for (let held = 0; held < shot.hold_frames; held += 1) {
      sequenceParts.push(`frame-${String(sequenceFrame).padStart(4, '0')}.jpg\0${shot.source_sha256}\n`);
      sequenceFrame += 1;
    }
  }
  assert(sequenceFrame - 1 === stage.frame_count, `${film.slug}: stage shot holds do not cover the frame count.`);
  assert(sha256Buffer(sequenceParts.join('')) === stage.sequence_sha256, `${film.slug}: stage sequence digest mismatch.`);
  const posterShot = stage.shots.find(
    shot => provenance.edit.poster_frame >= shot.first_frame && provenance.edit.poster_frame <= shot.last_frame,
  );
  assert(stage.poster_beat === film.poster_beat && provenance.edit.poster_beat === film.poster_beat, `${film.slug}: poster beat mismatch.`);
  assert(stage.poster_frame === provenance.edit.poster_frame, `${film.slug}: poster frame mismatch.`);
  assertHash(provenance.edit.poster_source_sha256, `${film.slug}.edit.poster_source_sha256`);
  assert(provenance.edit.poster_source_sha256 === posterShot?.source_sha256, `${film.slug}: poster source hash mismatch.`);
  assert(provenance.edit.frame_count === stage.frame_count, `${film.slug}: provenance frame count mismatch.`);
  assert(provenance.edit.sequence_sha256 === stage.sequence_sha256, `${film.slug}: provenance sequence digest mismatch.`);
  assert(posterShot?.beat === film.poster_beat, `${film.slug}: poster frame is outside the declared poster beat.`);

  assert(provenance.media && typeof provenance.media === 'object', `${film.slug}: missing media provenance.`);
  assert(provenance.media.mp4?.path === film.media.mp4, `${film.slug}: MP4 provenance path mismatch.`);
  assert(provenance.media.poster?.path === film.media.poster, `${film.slug}: poster provenance path mismatch.`);
  assertHash(provenance.media.mp4.sha256, `${film.slug}.media.mp4.sha256`);
  assertHash(provenance.media.poster.sha256, `${film.slug}.media.poster.sha256`);
  assert(provenance.media.mp4.sha256 === sha256File(media.mp4Path), `${film.slug}: MP4 hash changed.`);
  assert(provenance.media.poster.sha256 === sha256File(media.posterPath), `${film.slug}: poster hash changed.`);
  assert(provenance.media.poster.sha256 === provenance.edit.poster_source_sha256, `${film.slug}: poster is not the exact declared beat keyframe.`);
  assert(jsonEqual(provenance.media.mp4, { path: film.media.mp4, sha256: sha256File(media.mp4Path), ...media.video }), `${film.slug}: MP4 probe provenance mismatch.`);
  assert(jsonEqual(provenance.media.poster, { path: film.media.poster, sha256: sha256File(media.posterPath), ...media.poster }), `${film.slug}: poster probe provenance mismatch.`);

  assert(provenance.evidence && typeof provenance.evidence === 'object', `${film.slug}: missing durable evidence provenance.`);
  const evidencePath = resolveRepoPath(provenance.evidence.record, activeRepoRoot);
  assertRegular(evidencePath, `${film.slug} evidence record`);
  assertHash(provenance.evidence.record_sha256, `${film.slug}.evidence.record_sha256`);
  assert(provenance.evidence.record_sha256 === sha256File(evidencePath), `${film.slug}: evidence record hash changed.`);
  const evidence = readJson(evidencePath);
  assert(evidence.schema_version === 1 && evidence.film === film.slug, `${film.slug}: evidence identity mismatch.`);
  assert(jsonEqual(evidence.checks, provenance.evidence.checks), `${film.slug}: evidence check provenance mismatch.`);
  assert(jsonEqual(evidence.identities, provenance.evidence.identities), `${film.slug}: evidence identity values changed.`);
  assert(Array.isArray(evidence.checks) && evidence.checks.length === film.beats.length, `${film.slug}: evidence must cover every beat.`);
  for (const [index, check] of evidence.checks.entries()) {
    assert(check.id === film.beats[index].id, `${film.slug}: evidence check order mismatch.`);
    assert(check.status === 'passed', `${film.slug}: evidence check ${check.id} did not pass.`);
    assert(typeof check.detail === 'string' && check.detail.trim(), `${film.slug}: evidence check ${check.id} lacks detail.`);
  }
  assert(
    evidence.identities && typeof evidence.identities === 'object' && !Array.isArray(evidence.identities) && Object.keys(evidence.identities).length,
    `${film.slug}: durable evidence identities are missing.`,
  );
}

const args = process.argv.slice(2);
if (args.includes('--help')) {
  usage();
  process.exit(0);
}
const releaseCompatibility =
  args.length === 4 && args[0] === '--release-compatibility' && args[2] === '--release-root';
const ordinaryMode =
  args.length <= 1 && args.every(argument => ['--manifest-only', '--portable', '--source-bound', '--allow-needs-recording'].includes(argument));
if (!releaseCompatibility && !ordinaryMode) {
  usage();
  process.exit(64);
}
const compatibilityPath = releaseCompatibility ? resolve(args[1]) : null;
const activeRepoRoot = releaseCompatibility ? resolve(args[3]) : repoRoot;
const manifestOnly = args[0] === '--manifest-only';
const portable = args[0] === '--portable' || releaseCompatibility;
const sourceBound = args[0] === '--source-bound';
const allowNeedsRecording = args[0] === '--allow-needs-recording';
const releaseStrict = args.length === 0;

const failures = [];
let portfolio;
let currentSourceContentSha256 = null;
try {
  portfolio = loadPortfolio(activeRepoRoot);
  if (releaseStrict || sourceBound) currentSourceContentSha256 = sourceContentDigest(activeRepoRoot);
} catch (error) {
  console.error(`FAIL portfolio: ${error.message}`);
  process.exit(1);
}

const shotManifestPath = resolve(activeRepoRoot, 'demo/one-app/films/SHOT-MANIFEST.md');
assertRegular(shotManifestPath, 'Shot manifest');
const shotManifest = readFileSync(shotManifestPath, 'utf8');
for (const film of portfolio.films) {
  try {
    const scenarioPath = resolveRepoPath(film.scenario, activeRepoRoot);
    assertRegular(scenarioPath, `${film.slug} scenario`);
    const scenario = readFileSync(scenarioPath, 'utf8');
    assert(scenario.includes('## Recording beats'), `${film.slug}: scenario must declare Recording beats.`);
    assert(scenario.includes('## Durable') || scenario.includes('## API evidence'), `${film.slug}: scenario must declare durable evidence.`);
    assert(shotManifest.includes(`## \`${film.slug}\``), `${film.slug}: SHOT-MANIFEST section is missing.`);
  } catch (error) {
    failures.push(`${film.slug}: ${error.message}`);
  }
}

if (!manifestOnly) {
  for (const film of portfolio.films) {
    if (film.status === 'needs_recording' && allowNeedsRecording) {
      console.warn(`WARN ${film.slug}: needs_recording; media and provenance intentionally not accepted.`);
      continue;
    }
    if (film.status !== 'ready') {
      failures.push(`${film.slug}: status is ${film.status}; release-strict verification requires ready.`);
      continue;
    }
    try {
      const media = verifyMedia(portfolio, film, activeRepoRoot);
      verifyProvenance(portfolio, film, media, {
        verifyLocalBinary: !portable && !sourceBound,
        currentSourceContentSha256,
        activeRepoRoot,
      });
    } catch (error) {
      failures.push(`${film.slug}: ${error.message}`);
    }
  }
}

if (failures.length) {
  for (const failure of failures) console.error(`FAIL ${failure}`);
  console.error(`Film verification failed with ${failures.length} error(s).`);
  process.exit(1);
}

let compatibilityResult = null;
if (releaseCompatibility) {
  try {
    compatibilityResult = verifyReleaseCompatibility(compatibilityPath, {
      releaseRoot: activeRepoRoot,
      controlRoot: repoRoot,
      portfolio,
    });
  } catch (error) {
    console.error(`FAIL release compatibility: ${error.message}`);
    process.exit(1);
  }
}

if (manifestOnly) console.log('Film portfolio manifest: PASS (12 scenarios, placements, and shot contracts)');
else if (allowNeedsRecording) console.log('Film portfolio structure: PASS (ready films strict; needs_recording films warned)');
else if (releaseCompatibility) {
  console.log(
    `Film portfolio release compatibility: PASS (${compatibilityResult.tag} at ${compatibilityResult.commit}; ` +
    `${compatibilityResult.films} restored first-committed provenance records, ` +
    `${compatibilityResult.changedPaths} exact changed paths: ` +
    `${compatibilityResult.nonRecordingPaths} non-recording + ` +
    `${compatibilityResult.provenanceRewrites} audited source/binary-only rewrites; ` +
    `not captured with the patch binary)`,
  );
}
else if (portable) console.log('Film portfolio portable contract: PASS (12 ready films with restored first-committed provenance declarations)');
else if (sourceBound) console.log('Film portfolio source-bound contract: PASS (12 ready films with recorded provenance matching this checkout)');
else console.log('Film portfolio release contract: PASS (12 ready films, exact source content, and local binary matching the first-committed declaration)');
