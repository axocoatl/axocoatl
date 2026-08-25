#!/usr/bin/env node

import { existsSync, lstatSync, mkdirSync, readdirSync } from 'node:fs';
import { dirname, resolve } from 'node:path';
import {
  fail,
  filmsDir,
  findFilm,
  loadPortfolio,
  probePoster,
  probeVideo,
  readJson,
  repoRelative,
  resolveRepoPath,
  run,
  sha256Buffer,
  sha256File,
  sourceIdentity,
  validateTimeline,
  writeJsonAtomic,
} from './film-lib.mjs';

function usage() {
  console.error(`Usage: write-provenance.mjs <film-slug> --binary <axocoatl> --frames <dir> --evidence <evidence.json> [options]

Options:
  --frames <dir>         exact staged frame sequence used for the encode
  --capture <path>       capture record (default: films/source/<slug>/capture.json)
  --timeline <path>      timeline (default: films/source/<slug>/timeline.json)
  --stage-record <path>  stage record (default: films/source/<slug>/stage.json)
  --poster-frame <n>     encoded poster frame (default: stage record poster_frame)
  --output <path>        provenance record (default: manifest provenance path)
  --replace              replace an existing provenance record
`);
}

function requireObject(value, label) {
  if (!value || typeof value !== 'object' || Array.isArray(value)) fail(`${label} must be an object.`);
}

function requireString(value, label) {
  if (typeof value !== 'string' || !value.trim()) fail(`${label} must be a non-empty string.`);
}

function requireHash(value, label) {
  if (!/^[0-9a-f]{64}$/.test(value || '')) fail(`${label} must be a lowercase SHA-256 digest.`);
}

function assertRegularFile(path, label) {
  if (!existsSync(path) || !lstatSync(path).isFile() || lstatSync(path).isSymbolicLink()) {
    fail(`${label} must be a regular, non-symlink file: ${path}`);
  }
}

function validateCapture(portfolio, film, capture, capturePath, timelinePath) {
  requireObject(capture, 'capture');
  if (capture.schema_version !== 1 || capture.film !== film.slug) fail(`Capture record identity does not match ${film.slug}.`);
  requireString(capture.captured_at, 'capture.captured_at');
  if (new Date(capture.captured_at).toISOString() !== capture.captured_at) fail('capture.captured_at must be canonical ISO-8601 UTC.');
  requireString(capture.url, 'capture.url');
  requireString(capture.browser, 'capture.browser');
  requireObject(capture.viewport, 'capture.viewport');
  const contract = portfolio.media_contract;
  if (
    capture.viewport.width !== contract.width ||
    capture.viewport.height !== contract.height ||
    capture.viewport.device_scale_factor !== 1
  ) fail('Capture viewport must be 1280x720 at device scale factor 1.');
  if (!['light', 'dark'].includes(capture.theme)) fail('capture.theme must be light or dark.');
  if (!['reduce', 'no-preference'].includes(capture.reduced_motion)) fail('capture.reduced_motion is invalid.');
  if (capture.timeline !== repoRelative(timelinePath) || capture.timeline_sha256 !== sha256File(timelinePath)) {
    fail('Capture record timeline identity does not match the supplied timeline.');
  }
  if (!Array.isArray(capture.keyframes) || capture.keyframes.length !== film.beats.length) {
    fail(`Capture record must contain ${film.beats.length} keyframes.`);
  }
  const keyframeHashes = new Map();
  for (const [index, keyframe] of capture.keyframes.entries()) {
    requireObject(keyframe, `capture.keyframes[${index}]`);
    const beat = film.beats[index].id;
    if (keyframe.beat !== beat) fail(`capture.keyframes[${index}].beat must be ${beat}.`);
    const keyframePath = resolveRepoPath(keyframe.path);
    assertRegularFile(keyframePath, `capture keyframe ${beat}`);
    requireHash(keyframe.sha256, `capture.keyframes[${index}].sha256`);
    if (keyframe.sha256 !== sha256File(keyframePath)) fail(`Capture keyframe hash changed: ${keyframe.path}`);
    const previousBeat = keyframeHashes.get(keyframe.sha256);
    if (previousBeat) fail(`Capture beats ${previousBeat} and ${beat} use the same keyframe.`);
    keyframeHashes.set(keyframe.sha256, beat);
    const probe = probePoster(keyframePath);
    if (probe.codec !== contract.poster_codec || probe.width !== contract.width || probe.height !== contract.height) {
      fail(`Capture keyframe violates the media contract: ${keyframe.path}`);
    }
  }
  return {
    record: repoRelative(capturePath),
    record_sha256: sha256File(capturePath),
    url: capture.url,
    browser: capture.browser,
    viewport: capture.viewport,
    theme: capture.theme,
    reduced_motion: capture.reduced_motion,
    keyframes: capture.keyframes,
  };
}

function validateStage(portfolio, film, stage, stagePath, timelinePath, timeline, framesDirectory, posterFrameOverride) {
  requireObject(stage, 'stage record');
  if (stage.schema_version !== 1 || stage.film !== film.slug) fail(`Stage record identity does not match ${film.slug}.`);
  const calculated = validateTimeline(portfolio, film, timeline);
  if (stage.timeline !== repoRelative(timelinePath) || stage.timeline_sha256 !== sha256File(timelinePath)) {
    fail('Stage record timeline identity does not match the supplied timeline.');
  }
  if (
    stage.input_fps !== portfolio.media_contract.input_fps ||
    stage.frame_count !== calculated.frameCount ||
    stage.duration_seconds !== calculated.durationSeconds
  ) fail('Stage record timing does not match timeline.json.');
  requireHash(stage.sequence_sha256, 'stage.sequence_sha256');
  if (!Array.isArray(stage.shots) || stage.shots.length !== film.beats.length) fail('Stage record shot count is invalid.');
  let expectedFrame = 1;
  for (const [index, shot] of stage.shots.entries()) {
    const source = timeline.shots[index];
    requireObject(shot, `stage.shots[${index}]`);
    if (
      shot.beat !== source.beat ||
      shot.source !== source.source ||
      shot.hold_frames !== source.hold_frames ||
      shot.first_frame !== expectedFrame ||
      shot.last_frame !== expectedFrame + source.hold_frames - 1
    ) fail(`Stage shot ${index} does not match the deterministic timeline expansion.`);
    requireHash(shot.source_sha256, `stage.shots[${index}].source_sha256`);
    const sourcePath = resolve(dirname(timelinePath), source.source);
    if (shot.source_sha256 !== sha256File(sourcePath)) fail(`Stage source hash changed: ${source.source}`);
    expectedFrame = shot.last_frame + 1;
  }
  if (!existsSync(framesDirectory) || !lstatSync(framesDirectory).isDirectory() || lstatSync(framesDirectory).isSymbolicLink()) {
    fail(`Staged frames must be a regular directory: ${framesDirectory}`);
  }
  const names = readdirSync(framesDirectory).filter(name => /^frame-.*\.jpg$/.test(name)).sort();
  if (names.length !== stage.frame_count) fail(`Staged frame count changed: expected ${stage.frame_count}, found ${names.length}.`);
  const sequenceParts = [];
  for (let frame = 1; frame <= stage.frame_count; frame += 1) {
    const name = `frame-${String(frame).padStart(4, '0')}.jpg`;
    if (names[frame - 1] !== name) fail(`Staged sequence is not contiguous at ${name}.`);
    const path = resolve(framesDirectory, name);
    assertRegularFile(path, `Staged frame ${name}`);
    const shot = stage.shots.find(item => frame >= item.first_frame && frame <= item.last_frame);
    const hash = sha256File(path);
    if (!shot || hash !== shot.source_sha256) fail(`Staged frame content changed: ${name}`);
    sequenceParts.push(`${name}\0${hash}\n`);
  }
  if (sha256Buffer(sequenceParts.join('')) !== stage.sequence_sha256) {
    fail('Staged sequence digest does not match the stage record.');
  }
  if (stage.poster_beat !== film.poster_beat) fail(`stage.poster_beat must be ${film.poster_beat}.`);
  const posterFrame = posterFrameOverride ?? stage.poster_frame;
  if (!Number.isInteger(posterFrame) || posterFrame < 1 || posterFrame > stage.frame_count) {
    fail('Poster frame must be inside the staged sequence.');
  }
  const posterShot = stage.shots.find(shot => posterFrame >= shot.first_frame && posterFrame <= shot.last_frame);
  if (!posterShot || posterShot.beat !== film.poster_beat) {
    fail(`Poster frame ${posterFrame} must come from the ${film.poster_beat} beat.`);
  }
  return {
    timeline: repoRelative(timelinePath),
    timeline_sha256: sha256File(timelinePath),
    stage_record: repoRelative(stagePath),
    stage_record_sha256: sha256File(stagePath),
    poster_frame: posterFrame,
    poster_beat: film.poster_beat,
    poster_source_sha256: posterShot.source_sha256,
    frame_count: stage.frame_count,
    sequence_sha256: stage.sequence_sha256,
  };
}

function validateEvidence(film, evidence, evidencePath) {
  requireObject(evidence, 'evidence');
  if (evidence.schema_version !== 1 || evidence.film !== film.slug) fail(`Evidence identity does not match ${film.slug}.`);
  if (!Array.isArray(evidence.checks) || evidence.checks.length !== film.beats.length) {
    fail(`Evidence must contain exactly one check for every ${film.slug} beat.`);
  }
  for (const [index, check] of evidence.checks.entries()) {
    requireObject(check, `evidence.checks[${index}]`);
    if (check.id !== film.beats[index].id) fail(`evidence.checks[${index}].id must be ${film.beats[index].id}.`);
    if (check.status !== 'passed') fail(`Evidence check ${check.id} must be passed.`);
    requireString(check.detail, `evidence check ${check.id} detail`);
  }
  requireObject(evidence.identities, 'evidence.identities');
  if (Object.keys(evidence.identities).length === 0) fail('evidence.identities must contain durable product identifiers.');
  for (const [key, value] of Object.entries(evidence.identities)) {
    requireString(key, 'evidence identity key');
    if (typeof value !== 'string' && !(Array.isArray(value) && value.length && value.every(item => typeof item === 'string' && item))) {
      fail(`evidence.identities.${key} must be a string or non-empty string array.`);
    }
  }
  return {
    record: repoRelative(evidencePath),
    record_sha256: sha256File(evidencePath),
    checks: evidence.checks,
    identities: evidence.identities,
  };
}

const args = process.argv.slice(2);
if (args.length < 1 || args.includes('--help')) {
  usage();
  process.exit(args.includes('--help') ? 0 : 64);
}
const slug = args[0];
let binaryPath;
let evidencePath;
let framesDirectory;
let capturePath;
let timelinePath;
let stagePath;
let outputPath;
let posterFrame;
let replace = false;

for (let index = 1; index < args.length; index += 1) {
  const argument = args[index];
  if (['--binary', '--evidence', '--frames', '--capture', '--timeline', '--stage-record', '--output', '--poster-frame'].includes(argument)) {
    if (index + 1 >= args.length) fail(`${argument} requires a value.`);
    const value = args[index + 1];
    index += 1;
    if (argument === '--binary') binaryPath = resolve(value);
    else if (argument === '--evidence') evidencePath = resolve(value);
    else if (argument === '--frames') framesDirectory = resolve(value);
    else if (argument === '--capture') capturePath = resolve(value);
    else if (argument === '--timeline') timelinePath = resolve(value);
    else if (argument === '--stage-record') stagePath = resolve(value);
    else if (argument === '--output') outputPath = resolve(value);
    else {
      posterFrame = Number(value);
      if (!Number.isInteger(posterFrame) || posterFrame <= 0) fail('--poster-frame must be a positive integer.');
    }
  } else if (argument === '--replace') {
    replace = true;
  } else {
    fail(`Unknown option: ${argument}`);
  }
}
if (!binaryPath || !evidencePath || !framesDirectory) {
  usage();
  fail('--binary, --evidence, and --frames are required.');
}

const portfolio = loadPortfolio();
const film = findFilm(portfolio, slug);
const sourceDirectory = resolve(filmsDir, 'source', slug);
capturePath ||= resolve(sourceDirectory, 'capture.json');
timelinePath ||= resolve(sourceDirectory, 'timeline.json');
stagePath ||= resolve(sourceDirectory, 'stage.json');
outputPath ||= resolveRepoPath(film.provenance);

for (const [path, label] of [
  [binaryPath, 'Release binary'],
  [evidencePath, 'Evidence record'],
  [capturePath, 'Capture record'],
  [timelinePath, 'Timeline'],
  [stagePath, 'Stage record'],
]) assertRegularFile(path, label);
for (const path of [binaryPath, evidencePath, capturePath, timelinePath, stagePath, outputPath]) repoRelative(path);
if (existsSync(outputPath) && !replace) fail(`Provenance exists; pass --replace: ${outputPath}`);

const timeline = readJson(timelinePath);
validateTimeline(portfolio, film, timeline);
const capture = validateCapture(portfolio, film, readJson(capturePath), capturePath, timelinePath);
const edit = validateStage(
  portfolio,
  film,
  readJson(stagePath),
  stagePath,
  timelinePath,
  timeline,
  framesDirectory,
  posterFrame,
);
const evidence = validateEvidence(film, readJson(evidencePath), evidencePath);

const binaryVersion = String(run(binaryPath, ['--version'])).trim();
requireString(binaryVersion, 'binary version');
const binary = {
  path: repoRelative(binaryPath),
  version: binaryVersion,
  sha256: sha256File(binaryPath),
};

const mp4Path = resolveRepoPath(film.media.mp4);
const posterPath = resolveRepoPath(film.media.poster);
assertRegularFile(mp4Path, 'Film MP4');
assertRegularFile(posterPath, 'Film poster');
const video = probeVideo(mp4Path);
const poster = probePoster(posterPath);
const contract = portfolio.media_contract;
const expectedRate = `${contract.output_fps}/1`;
if (
  video.codec !== contract.video_codec ||
  video.width !== contract.width ||
  video.height !== contract.height ||
  video.pixel_format !== contract.pixel_format ||
  video.fps !== expectedRate ||
  video.audio_streams !== (contract.audio ? 1 : 0) ||
  video.fast_start !== contract.fast_start
) fail(`Encoded MP4 violates the portfolio media contract: ${film.media.mp4}`);
if (video.duration_seconds < film.duration_seconds.min || video.duration_seconds > film.duration_seconds.max) {
  fail(`Encoded MP4 duration ${video.duration_seconds}s is outside ${film.duration_seconds.min}-${film.duration_seconds.max}s.`);
}
if (poster.codec !== contract.poster_codec || poster.width !== contract.poster_width || poster.height !== contract.poster_height) {
  fail(`Encoded poster violates the portfolio media contract: ${film.media.poster}`);
}
if (sha256File(posterPath) !== edit.poster_source_sha256) {
  fail(`Encoded poster is not the exact staged ${film.poster_beat} keyframe.`);
}

const provenance = {
  schema_version: 2,
  film: film.slug,
  recorded_at: readJson(capturePath).captured_at,
  source: sourceIdentity(),
  binary,
  capture,
  edit,
  media: {
    mp4: {
      path: film.media.mp4,
      sha256: sha256File(mp4Path),
      ...video,
    },
    poster: {
      path: film.media.poster,
      sha256: sha256File(posterPath),
      ...poster,
    },
  },
  evidence,
};

mkdirSync(dirname(outputPath), { recursive: true });
writeJsonAtomic(outputPath, provenance);
console.log(`Wrote ${film.slug} provenance`);
console.log(`Binary     ${binary.path} (${binary.version})`);
console.log(`Source     ${provenance.source.head}${provenance.source.dirty ? ' + recorded patch' : ''}`);
console.log(`Content    ${provenance.source.content_sha256}`);
console.log(`Provenance ${repoRelative(outputPath)}`);
