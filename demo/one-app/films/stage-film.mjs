#!/usr/bin/env node

import {
  copyFileSync,
  existsSync,
  lstatSync,
  mkdirSync,
  readdirSync,
  rmSync,
} from 'node:fs';
import { basename, dirname, resolve } from 'node:path';
import {
  fail,
  filmsDir,
  findFilm,
  loadPortfolio,
  probePoster,
  readJson,
  repoRelative,
  sha256Buffer,
  sha256File,
  validateTimeline,
  writeJsonAtomic,
} from './film-lib.mjs';

function usage() {
  console.error(`Usage: stage-film.mjs <film-slug> <timeline.json> <frames-dir> [options]

Options:
  --record <path>  stage record (default: films/source/<slug>/stage.json)
  --replace        replace only frame-NNNN.jpg and the stage record
`);
}

const args = process.argv.slice(2);
if (args.length < 3 || args.includes('--help')) {
  usage();
  process.exit(args.includes('--help') ? 0 : 64);
}

const slug = args[0];
const timelinePath = resolve(args[1]);
const framesDirectory = resolve(args[2]);
let recordPath;
let replace = false;
for (let index = 3; index < args.length; index += 1) {
  if (args[index] === '--record') {
    if (index + 1 >= args.length) fail('--record requires a path.');
    recordPath = resolve(args[index + 1]);
    index += 1;
  } else if (args[index] === '--replace') {
    replace = true;
  } else {
    fail(`Unknown option: ${args[index]}`);
  }
}

const portfolio = loadPortfolio();
const film = findFilm(portfolio, slug);
recordPath ||= resolve(filmsDir, 'source', slug, 'stage.json');
repoRelative(timelinePath);
repoRelative(recordPath);

if (!existsSync(timelinePath) || !lstatSync(timelinePath).isFile() || lstatSync(timelinePath).isSymbolicLink()) {
  fail(`Timeline must be a regular repository file: ${timelinePath}`);
}
if (existsSync(framesDirectory) && lstatSync(framesDirectory).isSymbolicLink()) {
  fail(`Frames directory may not be a symlink: ${framesDirectory}`);
}
if (existsSync(recordPath) && !replace) fail(`Stage record exists; pass --replace: ${recordPath}`);

const timeline = readJson(timelinePath);
const { frameCount, durationSeconds } = validateTimeline(portfolio, film, timeline);
const timelineDirectory = dirname(timelinePath);
const sources = [];
for (const shot of timeline.shots) {
  const source = resolve(timelineDirectory, shot.source);
  if (dirname(source) !== timelineDirectory) fail(`Shot source must stay beside timeline.json: ${shot.source}`);
  if (!existsSync(source) || !lstatSync(source).isFile() || lstatSync(source).isSymbolicLink()) {
    fail(`Shot source must be a regular file: ${source}`);
  }
  const probe = probePoster(source);
  const contract = portfolio.media_contract;
  if (probe.codec !== contract.poster_codec || probe.width !== contract.width || probe.height !== contract.height) {
    fail(`${shot.source} violates the ${contract.poster_codec} ${contract.width}x${contract.height} source contract.`);
  }
  sources.push({ ...shot, path: source, sha256: sha256File(source) });
}

mkdirSync(framesDirectory, { recursive: true });
const frameLike = readdirSync(framesDirectory).filter(name => name.startsWith('frame-') && name.endsWith('.jpg'));
const malformedFrames = frameLike.filter(name => !/^frame-\d{4}\.jpg$/.test(name));
if (malformedFrames.length) fail(`Frames directory contains malformed frame names: ${malformedFrames.join(', ')}`);
const oldFrames = frameLike;
if (oldFrames.length && !replace) fail(`Frames directory already contains staged frames; pass --replace: ${framesDirectory}`);
if (replace) {
  for (const name of oldFrames) rmSync(resolve(framesDirectory, name));
  rmSync(recordPath, { force: true });
}

let frame = 1;
const shots = [];
const sequenceHashParts = [];
for (const source of sources) {
  const firstFrame = frame;
  for (let held = 0; held < source.hold_frames; held += 1) {
    const name = `frame-${String(frame).padStart(4, '0')}.jpg`;
    copyFileSync(source.path, resolve(framesDirectory, name));
    sequenceHashParts.push(`${name}\0${source.sha256}\n`);
    frame += 1;
  }
  shots.push({
    beat: source.beat,
    source: basename(source.path),
    source_sha256: source.sha256,
    hold_frames: source.hold_frames,
    first_frame: firstFrame,
    last_frame: frame - 1,
  });
}
if (frame - 1 !== frameCount) fail(`Internal staging error: expected ${frameCount} frames, wrote ${frame - 1}.`);

mkdirSync(dirname(recordPath), { recursive: true });
const record = {
  schema_version: 1,
  film: film.slug,
  timeline: repoRelative(timelinePath),
  timeline_sha256: sha256File(timelinePath),
  input_fps: portfolio.media_contract.input_fps,
  frame_count: frameCount,
  duration_seconds: durationSeconds,
  sequence_sha256: sha256Buffer(sequenceHashParts.join('')),
  shots,
};
const posterShot = shots.find(shot => shot.beat === film.poster_beat);
record.poster_beat = film.poster_beat;
record.poster_frame = Math.floor((posterShot.first_frame + posterShot.last_frame) / 2);
writeJsonAtomic(recordPath, record);

console.log(`Staged ${film.slug}`);
console.log(`Frames    ${framesDirectory}`);
console.log(`Count     ${frameCount} (${durationSeconds.toFixed(3)}s at ${record.input_fps} fps)`);
console.log(`Poster    frame-${String(record.poster_frame).padStart(4, '0')}.jpg (${record.poster_beat})`);
console.log(`Record    ${repoRelative(recordPath)}`);
