#!/usr/bin/env node

import {
  copyFileSync,
  existsSync,
  lstatSync,
  mkdirSync,
  readdirSync,
  rmSync,
  statSync,
} from 'node:fs';
import { resolve } from 'node:path';
import {
  fail,
  filmsDir,
  findFilm,
  loadPortfolio,
  probePoster,
  readJson,
  repoRelative,
  sha256File,
  validateTimeline,
  writeJsonAtomic,
} from './film-lib.mjs';

function usage() {
  console.error(`Usage: record-capture.mjs <film-slug> <capture-dir> [options]

Required capture-dir contents:
  shot-<beat>.jpg   one 1280x720 JPEG for every manifest beat
  timeline.json     ordered beat holds expressed as 8 fps hold_frames

Options:
  --output <dir>       canonical source directory (default: films/source/<slug>)
  --captured-at <iso>  capture instant (default: SOURCE_DATE_EPOCH or newest input mtime)
  --url <url>          captured product URL (default: http://127.0.0.1:18080/)
  --browser <name>     capture client identity (default: Codex in-app browser)
  --theme <name>       light or dark (default: dark)
  --reduced-motion     capture with reduced motion enabled
  --replace            replace this slug's known source files
`);
}

function optionValue(args, index, flag) {
  if (index + 1 >= args.length) fail(`${flag} requires a value.`);
  return args[index + 1];
}

const args = process.argv.slice(2);
if (args.length < 2 || args.includes('--help')) {
  usage();
  process.exit(args.includes('--help') ? 0 : 64);
}

const slug = args[0];
const inputDirectory = resolve(args[1]);
let outputDirectory;
let capturedAt;
let url = 'http://127.0.0.1:18080/';
let browser = 'Codex in-app browser';
let theme = 'dark';
let reducedMotion = false;
let replace = false;

for (let index = 2; index < args.length; index += 1) {
  const argument = args[index];
  if (argument === '--output') {
    outputDirectory = resolve(optionValue(args, index, argument));
    index += 1;
  } else if (argument === '--captured-at') {
    capturedAt = optionValue(args, index, argument);
    index += 1;
  } else if (argument === '--url') {
    url = optionValue(args, index, argument);
    index += 1;
  } else if (argument === '--browser') {
    browser = optionValue(args, index, argument);
    index += 1;
  } else if (argument === '--theme') {
    theme = optionValue(args, index, argument);
    index += 1;
  } else if (argument === '--reduced-motion') {
    reducedMotion = true;
  } else if (argument === '--replace') {
    replace = true;
  } else {
    fail(`Unknown option: ${argument}`);
  }
}

const portfolio = loadPortfolio();
const film = findFilm(portfolio, slug);
outputDirectory ||= resolve(filmsDir, 'source', slug);
repoRelative(outputDirectory);

if (!existsSync(inputDirectory) || !lstatSync(inputDirectory).isDirectory()) {
  fail(`Capture directory does not exist: ${inputDirectory}`);
}
if (lstatSync(inputDirectory).isSymbolicLink()) fail(`Capture directory may not be a symlink: ${inputDirectory}`);
if (existsSync(outputDirectory) && lstatSync(outputDirectory).isSymbolicLink()) {
  fail(`Output directory may not be a symlink: ${outputDirectory}`);
}

const timelineInput = resolve(inputDirectory, 'timeline.json');
if (!existsSync(timelineInput) || !lstatSync(timelineInput).isFile() || lstatSync(timelineInput).isSymbolicLink()) {
  fail(`Missing regular timeline.json in ${inputDirectory}.`);
}
const timeline = readJson(timelineInput);
validateTimeline(portfolio, film, timeline);

const expectedShots = film.beats.map(beat => `shot-${beat.id}.jpg`);
const expectedSet = new Set(expectedShots);
const unexpectedShots = readdirSync(inputDirectory)
  .filter(name => /^shot-.*\.jpg$/.test(name) && !expectedSet.has(name));
if (unexpectedShots.length) fail(`Unexpected capture shots: ${unexpectedShots.join(', ')}`);

const inputPaths = [timelineInput];
const keyframes = [];
const keyframeHashes = new Map();
for (const [index, beat] of film.beats.entries()) {
  const name = expectedShots[index];
  const source = resolve(inputDirectory, name);
  if (!existsSync(source) || !lstatSync(source).isFile() || lstatSync(source).isSymbolicLink()) {
    fail(`Missing regular capture keyframe: ${source}`);
  }
  const probe = probePoster(source);
  const contract = portfolio.media_contract;
  if (probe.codec !== contract.poster_codec || probe.width !== contract.width || probe.height !== contract.height) {
    fail(
      `${name} must be ${contract.poster_codec} ${contract.width}x${contract.height}; ` +
      `found ${probe.codec} ${probe.width}x${probe.height}.`,
    );
  }
  inputPaths.push(source);
  const sha256 = sha256File(source);
  const previousBeat = keyframeHashes.get(sha256);
  if (previousBeat) {
    fail(`Capture beats ${previousBeat} and ${beat.id} use the same keyframe; every product beat must show a distinct state.`);
  }
  keyframeHashes.set(sha256, beat.id);
  keyframes.push({ beat: beat.id, name, sha256 });
}

if (capturedAt === undefined) {
  if (process.env.SOURCE_DATE_EPOCH) {
    const epoch = Number(process.env.SOURCE_DATE_EPOCH);
    if (!Number.isInteger(epoch) || epoch < 0) fail('SOURCE_DATE_EPOCH must be a non-negative integer.');
    capturedAt = new Date(epoch * 1000).toISOString();
  } else {
    capturedAt = new Date(Math.max(...inputPaths.map(path => statSync(path).mtimeMs))).toISOString();
  }
}
const instant = new Date(capturedAt);
if (Number.isNaN(instant.valueOf()) || instant.toISOString() !== capturedAt) {
  fail('--captured-at must be a canonical ISO-8601 UTC instant, for example 2026-08-20T19:30:00.000Z.');
}
if (!['light', 'dark'].includes(theme)) fail('--theme must be light or dark.');
try {
  const parsedUrl = new URL(url);
  if (!['http:', 'https:'].includes(parsedUrl.protocol)) fail('--url must use http or https.');
} catch (error) {
  if (error.message.startsWith('--url')) throw error;
  fail(`Invalid --url: ${url}`);
}
if (!browser.trim()) fail('--browser must not be empty.');

mkdirSync(outputDirectory, { recursive: true });
const knownOutputs = [...expectedShots, 'timeline.json', 'capture.json'];
const staleOutputShots = readdirSync(outputDirectory)
  .filter(name => /^shot-.*\.jpg$/.test(name) && !expectedSet.has(name));
if (staleOutputShots.length) {
  fail(`Output contains stale or unknown shots; remove them deliberately: ${staleOutputShots.join(', ')}`);
}
const existingOutputs = knownOutputs.filter(name => existsSync(resolve(outputDirectory, name)));
const sameDirectory = inputDirectory === outputDirectory;
if (!replace && existingOutputs.some(name => !(sameDirectory && (expectedSet.has(name) || name === 'timeline.json')))) {
  fail(`Source output already exists (${existingOutputs.join(', ')}); pass --replace intentionally.`);
}
if (replace) {
  for (const name of knownOutputs) {
    const target = resolve(outputDirectory, name);
    if (target !== timelineInput && !expectedShots.some(shot => resolve(inputDirectory, shot) === target)) {
      rmSync(target, { force: true });
    }
  }
}

if (!sameDirectory) {
  for (const name of expectedShots) copyFileSync(resolve(inputDirectory, name), resolve(outputDirectory, name));
  copyFileSync(timelineInput, resolve(outputDirectory, 'timeline.json'));
}

const timelineOutput = resolve(outputDirectory, 'timeline.json');
const captureOutput = resolve(outputDirectory, 'capture.json');
const capture = {
  schema_version: 1,
  film: film.slug,
  captured_at: capturedAt,
  url,
  browser,
  viewport: {
    width: portfolio.media_contract.width,
    height: portfolio.media_contract.height,
    device_scale_factor: 1,
  },
  theme,
  reduced_motion: reducedMotion ? 'reduce' : 'no-preference',
  timeline: repoRelative(timelineOutput),
  timeline_sha256: sha256File(timelineOutput),
  keyframes: keyframes.map(keyframe => ({
    beat: keyframe.beat,
    path: repoRelative(resolve(outputDirectory, keyframe.name)),
    sha256: sha256File(resolve(outputDirectory, keyframe.name)),
  })),
};
if (existsSync(captureOutput) && !replace) fail(`Capture record already exists: ${captureOutput}`);
writeJsonAtomic(captureOutput, capture);

console.log(`Recorded ${film.slug} capture source`);
console.log(`Source    ${repoRelative(outputDirectory)}`);
console.log(`Timeline  ${capture.timeline}`);
console.log(`Record    ${repoRelative(captureOutput)}`);
