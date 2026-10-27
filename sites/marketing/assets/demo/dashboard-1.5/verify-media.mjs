#!/usr/bin/env node
// Read-only: verifies actual screenshot bytes, encoded cuts and frozen run evidence.
import assert from 'node:assert/strict';
import {createHash} from 'node:crypto';
import {readFileSync, lstatSync} from 'node:fs';
import {dirname, resolve} from 'node:path';
import {fileURLToPath} from 'node:url';
import {execFileSync, spawnSync} from 'node:child_process';
import {gunzipSync} from 'node:zlib';

const directory = dirname(fileURLToPath(import.meta.url));
let repository = null, probe = false;
for (let i = 2; i < process.argv.length; i++) {
  if (process.argv[i] === '--repo' && process.argv[i + 1]) repository = resolve(process.argv[++i]);
  else if (process.argv[i] === '--probe') probe = true;
  else throw Error('Usage: node verify-media.mjs [--repo /path/to/axocoatl] [--probe]');
}
const hash = bytes => createHash('sha256').update(bytes).digest('hex');
const read = name => {
  assert(/^[a-z0-9][a-z0-9.-]*$/.test(name), 'asset must be a plain local filename');
  const path = resolve(directory, name);
  assert(lstatSync(path).isFile() && !lstatSync(path).isSymbolicLink(), `not a regular asset: ${name}`);
  return readFileSync(path);
};
const p = JSON.parse(read('provenance.json'));
assert.equal(p.schema, 'axocoatl.dashboard-media-provenance/1');
assert.equal(p.product.version, '1.5.0');
assert.equal(p.product.release_commit, '19e3c89ca7b4d02cd742bccd671f7274e68b95d7');
assert.equal(p.product.runtime_commit, 'b222302f32639c9211255ab4f818c3262bce856a');
assert.equal(p.product.binary_sha256, 'd25cd64f70da4d17eb781d223aeab553f4180d074dbb29af302b23492ce31d1b');
assert.equal(p.product.run_id, 'run-a2c57675-d55d-4abb-9993-d0c27d4a6fd3');
assert.equal(p.presentation.kind, 'sampled-ui-navigation');
assert.equal(p.presentation.live_execution, false);
assert.equal(p.presentation.new_inference, false);
assert.equal(p.presentation.run_started_from, 'CLI');
assert.equal(p.capture.human_operator, false);
assert.equal(p.capture.operator, 'Codex assistant using the actual browser UI');
const origin = new URL(p.capture.origin);
assert(['127.0.0.1', 'localhost'].includes(origin.hostname));
assert.equal(origin.protocol, 'http:');
assert.equal(origin.pathname, '/');
assert(!origin.search && !origin.hash && !origin.username && !origin.password);
assert(['light', 'dark'].includes(p.capture.theme));
assert(!/Bearer\s|[?&](?:token|code)=|sk-or-v1-/i.test(JSON.stringify(p)), 'authentication material in public metadata');
function jpegSize(bytes) {
  assert(bytes[0] === 0xff && bytes[1] === 0xd8, 'capture is not JPEG');
  let offset = 2;
  while (offset < bytes.length) {
    assert.equal(bytes[offset++], 0xff, 'invalid JPEG marker');
    while (bytes[offset] === 0xff) offset++;
    const marker = bytes[offset++];
    if (marker === 0xd9 || marker === 0xda) break;
    if (marker === 0x01 || (marker >= 0xd0 && marker <= 0xd7)) continue;
    const length = bytes.readUInt16BE(offset);
    assert(length >= 2 && offset + length <= bytes.length);
    if ([0xc0, 0xc1, 0xc2].includes(marker)) return [bytes.readUInt16BE(offset + 5), bytes.readUInt16BE(offset + 3)];
    offset += length;
  }
  throw Error('JPEG dimensions missing');
}
function regularTarMembers(compressed) {
  const bytes = gunzipSync(compressed, {maxOutputLength: 32 * 1024 * 1024});
  const members = new Map();
  for (let offset = 0; offset + 512 <= bytes.length;) {
    const header = bytes.subarray(offset, offset + 512);
    if (header.every(byte => byte === 0)) break;
    const name = header.subarray(0, 100).toString().replace(/\0.*$/s, '');
    assert(/^(?:frame-\d{4}\.jpg|capture\.json)$/.test(name), 'unexpected source archive member');
    assert(header[156] === 0 || header[156] === 48, 'source archive must contain only regular files');
    assert(!members.has(name), 'duplicate source frame');
    const size = Number.parseInt(header.subarray(124, 136).toString().replace(/\0.*$/s, '').trim(), 8);
    assert(Number.isFinite(size) && size > 0 && size < 2 * 1024 * 1024);
    assert(offset + 512 + size <= bytes.length);
    members.set(name, bytes.subarray(offset + 512, offset + 512 + size));
    offset += 512 + Math.ceil(size / 512) * 512;
  }
  return members;
}
const captureBytes = read(p.capture.source_metadata_file);
assert.equal(hash(captureBytes), p.capture.source_metadata_sha256);
const capture = JSON.parse(captureBytes);
assert.equal(capture.run_id, p.product.run_id);
assert.equal(capture.session_id, p.product.session_id);
assert.equal(capture.url, p.capture.session_url);
const ids = ['conversation', 'changes', 'outcome', 'receipt'];
assert.deepEqual(p.shots.map(shot => shot.id), ids);
let dimensions = null;
const captureTimes = new Map();
for (const shot of p.shots) {
  assert.equal(shot.file, `${shot.id}.jpg`);
  const bytes = read(shot.file);
  assert.equal(hash(bytes), shot.sha256, shot.file);
  assert.equal(bytes.length, shot.bytes, shot.file);
  const size = jpegSize(bytes);
  assert.deepEqual(size, [shot.width, shot.height], shot.file);
  assert(size.every(value => value > 0 && value <= 4096));
  if (dimensions) assert.deepEqual(size, dimensions, 'screenshot dimensions changed between panels');
  dimensions = size;
  const time = Date.parse(shot.captured_at);
  assert(Number.isFinite(time), 'invalid screenshot capture time');
  captureTimes.set(shot.id, time);
  const observation = capture.shots.find(source => source.file === shot.source_capture_file);
  assert(observation);
  assert.equal(observation.captured_at, shot.captured_at);
  assert.equal(observation.description, shot.observed);
  assert.equal(shot.source_capture_sha256, shot.sha256, 'published JPEG must retain original capture bytes');
  assert.equal(shot.pixel_edits, 'none');
}
assert.deepEqual(p.capture.capture_order, [...captureTimes].sort((a, b) => a[1] - b[1]).map(([id]) => id));
const v = p.video, video = read(v.file);
assert.equal(v.file, 'dashboard-tour.mp4');
assert.equal(hash(video), v.sha256);
assert.equal(video.length, v.bytes);
assert.equal(v.codec, 'h264');
assert.equal(v.pixel_format, 'yuv420p');
assert.equal(v.frames_per_second, 24);
assert.equal(v.audio, false);
assert.equal(v.editing, 'Original screenshot samples in captured chronological order and timing; repeated frames only for 24fps playback. No generated frames, overlays, interpolated motion or simulated execution.');
assert.deepEqual([v.width, v.height], dimensions);
const archive = read(v.source_archive_file);
assert.equal(hash(archive), v.source_archive_sha256);
assert.equal(archive.length, v.source_archive_bytes);
const members = regularTarMembers(archive);
const videoCaptureBytes = members.get('capture.json');
assert(videoCaptureBytes);
assert.equal(hash(videoCaptureBytes), v.source_metadata_sha256);
const videoCapture = JSON.parse(videoCaptureBytes);
assert.equal(videoCapture.kind, 'continuous screenshot sampling of actual UI navigation');
assert.deepEqual(videoCapture.errors, []);
assert.deepEqual(v.errors, []);
assert.equal(videoCapture.duration_ms / 1000, v.duration_seconds);
assert.equal(Date.parse(videoCapture.finished_at) - Date.parse(videoCapture.started_at), videoCapture.duration_ms);
assert.equal(v.captured_start, videoCapture.started_at);
assert.equal(v.captured_end, videoCapture.finished_at);
assert.deepEqual(v.actions, videoCapture.actions);
assert.deepEqual(v.frames.map(({file, elapsed_ms}) => ({file, elapsed_ms})), videoCapture.frames);
assert.equal(v.source_frame_count, v.frames.length);
assert.equal(members.size, v.frames.length + 1);
assert(v.frames.length > 20 && v.duration_seconds > 10 && v.duration_seconds < 60);
let previous = -1;
for (const [index, frame] of v.frames.entries()) {
  assert.equal(frame.file, `frame-${String(index).padStart(4, '0')}.jpg`);
  assert(frame.elapsed_ms > previous && frame.elapsed_ms < videoCapture.duration_ms);
  if (index === 0) assert.equal(frame.elapsed_ms, 0);
  else assert(frame.elapsed_ms - previous <= 1000, 'unreported capture gap');
  previous = frame.elapsed_ms;
  const bytes = members.get(frame.file); assert(bytes);
  assert.equal(hash(bytes), frame.sha256, frame.file);
  assert.equal(bytes.length, frame.bytes, frame.file);
  assert.deepEqual(jpegSize(bytes), dimensions);
}
if (repository) {
  const frozen = p.product.release_commit;
  const prefix = `release-evidence/1.5.0/${p.product.runtime_commit}/`;
  const frozenBytes = path => execFileSync('git', ['show', `${frozen}:${path}`], {cwd: repository, maxBuffer: 16 * 1024 * 1024});
  const manifest = JSON.parse(frozenBytes(`${prefix}manifest.json`));
  const inventory = new Map(manifest.files.map(file => [prefix + file.path, file]));
  const seen = new Set();
  for (const source of p.sources) {
    assert(source.path.startsWith(prefix) && !source.path.split('/').includes('..'));
    assert(!seen.has(source.path), 'duplicate source'); seen.add(source.path);
    const entry = inventory.get(source.path); assert(entry, `not in signed inventory: ${source.path}`);
    const bytes = frozenBytes(source.path);
    assert.equal(hash(bytes), source.sha256, source.path);
    assert.equal(bytes.length, source.bytes, source.path);
    assert.equal(entry.sha256, source.sha256, source.path);
    assert.equal(entry.bytes, source.bytes, source.path);
  }
  const assessmentPath = `${prefix}C1-local/acceptance/readme-node-deps/assessment.json`;
  assert(seen.has(assessmentPath));
  const assessment = JSON.parse(frozenBytes(assessmentPath));
  assert.equal(assessment.result, 'ACCEPTED');
  assert.equal(assessment.original_tests_unchanged, true);
  assert.equal(assessment.all_other_tracked_files_unchanged, true);
  assert.equal(assessment.host_test.exit_code, 0);
  assert.equal(assessment.verification.receipt_exit, 0);
  assert.equal(assessment.verification.record_exit, 0);
  assert.equal(assessment.negative_evidence.tool_errors.length, 2);
  assert.equal(assessment.negative_evidence.provider_retries.length, 0);
}
if (probe) {
  const videoPath = resolve(directory, v.file);
  const metadata = JSON.parse(execFileSync('ffprobe', ['-v', 'error', '-show_streams', '-show_format', '-of', 'json', videoPath], {encoding: 'utf8'}));
  assert.equal(metadata.streams.length, 1, 'unexpected audio/subtitle/data stream');
  const stream = metadata.streams[0];
  assert.equal(stream.codec_type, 'video'); assert.equal(stream.codec_name, v.codec);
  assert.equal(stream.pix_fmt, v.pixel_format);
  assert.deepEqual([stream.width, stream.height], dimensions);
  assert.equal(stream.r_frame_rate, '24/1');
  assert(Math.abs(Number(metadata.format.duration) - v.duration_seconds) <= 1 / 24 + 0.001);
  for (const [index, frame] of v.frames.entries()) {
    const end = v.frames[index + 1]?.elapsed_ms ?? videoCapture.duration_ms;
    const midpoint = (frame.elapsed_ms + end) / 2000;
    const result = spawnSync('ffmpeg', ['-hide_banner', '-ss', String(midpoint), '-i', videoPath, '-i', 'pipe:0', '-lavfi', '[0:v]trim=end_frame=1,setpts=PTS-STARTPTS[a];[1:v]setpts=PTS-STARTPTS[b];[a][b]ssim=shortest=1', '-frames:v', '1', '-f', 'null', '-'], {input: members.get(frame.file), encoding: 'utf8', maxBuffer: 2 * 1024 * 1024});
    assert.equal(result.status, 0, `cannot decode source frame ${frame.file}: ${result.stderr}`);
    const match = result.stderr.match(/All:([0-9.]+)/);
    assert(match && Number(match[1]) >= 0.99, `video differs from actual capture at ${midpoint}s: ${frame.file} ${match?.[1]}`);
  }
}
console.log(`Verified four original UI screenshots, all source capture frames and their completed-run walkthrough${repository ? ', frozen signed evidence sources' : ''}${probe ? ', video format and all encoded capture frames' : ''}.`);
