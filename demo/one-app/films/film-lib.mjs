import { createHash } from 'node:crypto';
import { existsSync, lstatSync, readFileSync, readlinkSync, renameSync, writeFileSync } from 'node:fs';
import { dirname, relative, resolve, sep } from 'node:path';
import { spawnSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';

export const filmsDir = dirname(fileURLToPath(import.meta.url));
export const repoRoot = resolve(filmsDir, '../../..');
export const portfolioPath = resolve(filmsDir, 'portfolio.json');

export function fail(message) {
  throw new Error(message);
}

export function readJson(path) {
  try {
    return JSON.parse(readFileSync(path, 'utf8'));
  } catch (error) {
    fail(`Could not read JSON ${path}: ${error.message}`);
  }
}

export function writeJsonAtomic(path, value) {
  const temporary = `${path}.${process.pid}.tmp`;
  writeFileSync(temporary, `${JSON.stringify(value, null, 2)}\n`, { flag: 'wx' });
  renameSync(temporary, path);
}

export function resolveRepoPath(path) {
  if (typeof path !== 'string' || !path || path.startsWith('/') || path.includes('\\')) {
    fail(`Repository path must be a non-empty POSIX-relative path: ${path}`);
  }
  const absolute = resolve(repoRoot, path);
  const back = relative(repoRoot, absolute);
  if (!back || back === '..' || back.startsWith(`..${sep}`)) {
    fail(`Repository path escapes the repository: ${path}`);
  }
  return absolute;
}

export function repoRelative(path) {
  const back = relative(repoRoot, resolve(path));
  if (!back || back === '..' || back.startsWith(`..${sep}`)) {
    fail(`Path must stay inside the repository: ${path}`);
  }
  return back.split(sep).join('/');
}

export function sha256Buffer(value) {
  return createHash('sha256').update(value).digest('hex');
}

export function sha256File(path) {
  return sha256Buffer(readFileSync(path));
}

export const provenanceSourceExcludes = Object.freeze([
  'demo/one-app/films/source/',
  'demo/one-app/films/staged/',
  'demo/one-app/films/provenance/',
  'sites/marketing/assets/films/',
]);

function excludedFromProvenance(path, excludes = provenanceSourceExcludes) {
  return excludes.some(prefix => path.startsWith(prefix));
}

/**
 * Hash the byte content at every Git-visible checkout path, independent of
 * whether that content currently comes from HEAD, an index entry, or an
 * untracked file. Paths, entry types, normalized executable state, and bytes
 * participate in the digest; Git-ignored files and the four recording-output
 * families do not.
 */
export function sourceContentDigest(root = repoRoot, excludes = provenanceSourceExcludes) {
  const listing = run(
    'git',
    ['ls-files', '--cached', '--others', '--exclude-standard', '-z', '--', '.'],
    { cwd: root, encoding: null },
  );
  const paths = [...new Set(String(listing).split('\0').filter(Boolean))]
    .filter(path => !excludedFromProvenance(path, excludes))
    .sort((left, right) => Buffer.compare(Buffer.from(left), Buffer.from(right)));
  const digest = createHash('sha256');
  digest.update('axocoatl-source-content-v1\0');

  for (const path of paths) {
    const absolute = resolve(root, path);
    const back = relative(root, absolute);
    if (!back || back === '..' || back.startsWith(`..${sep}`)) {
      fail(`Git-visible source path escapes the checkout: ${path}`);
    }
    let stat;
    try {
      stat = lstatSync(absolute);
    } catch (error) {
      // A cached path can be absent when the working tree records a deletion.
      // Omitting it makes the digest describe current content, not index state.
      if (error?.code === 'ENOENT') continue;
      throw error;
    }
    let type;
    let content;
    if (stat.isSymbolicLink()) {
      type = 'symlink';
      content = readlinkSync(absolute, { encoding: 'buffer' });
    } else if (stat.isFile()) {
      type = stat.mode & 0o111 ? 'file+x' : 'file';
      content = readFileSync(absolute);
    } else {
      fail(`Git-visible source path must be a regular file or symlink: ${path}`);
    }
    digest.update(`${type}\0${path}\0${sha256Buffer(content)}\n`);
  }
  return digest.digest('hex');
}

export function sourceIdentity() {
  const branch = String(run('git', ['rev-parse', '--abbrev-ref', 'HEAD'])).trim();
  const head = String(run('git', ['rev-parse', 'HEAD'])).trim();
  if (!/^[0-9a-f]{40}$/.test(head)) fail(`Git HEAD is not a full SHA-1: ${head}`);
  const pathspecExcludes = provenanceSourceExcludes.map(prefix => `:(exclude)${prefix}**`);
  const tracked = run('git', ['diff', '--binary', 'HEAD', '--', '.', ...pathspecExcludes], { encoding: null });
  const untracked = String(run('git', ['ls-files', '--others', '--exclude-standard', '-z']))
    .split('\0')
    .filter(Boolean)
    .filter(path => !excludedFromProvenance(path))
    .sort();
  const chunks = [Buffer.from('tracked\0'), tracked];
  for (const path of untracked) {
    chunks.push(Buffer.from(`\0untracked\0${path}\0${sha256File(resolve(repoRoot, path))}`));
  }
  const material = Buffer.concat(chunks);
  const dirty = tracked.length > 0 || untracked.length > 0;
  return {
    branch,
    head,
    dirty,
    patch_sha256: dirty ? sha256Buffer(material) : null,
    patch_excludes: [...provenanceSourceExcludes],
    content_sha256: sourceContentDigest(),
  };
}

export function run(command, args, options = {}) {
  const result = spawnSync(command, args, {
    cwd: options.cwd || repoRoot,
    encoding: options.encoding === null ? null : 'utf8',
    maxBuffer: 64 * 1024 * 1024,
    ...options,
  });
  if (result.error) fail(`${command} could not run: ${result.error.message}`);
  if (result.status !== 0) {
    const detail = String(result.stderr || result.stdout || '').trim();
    fail(`${command} ${args.join(' ')} failed (${result.status})${detail ? `: ${detail}` : ''}`);
  }
  return result.stdout;
}

function requireObject(value, label) {
  if (!value || typeof value !== 'object' || Array.isArray(value)) fail(`${label} must be an object.`);
}

function requireString(value, label) {
  if (typeof value !== 'string' || !value.trim()) fail(`${label} must be a non-empty string.`);
}

function requireInteger(value, label) {
  if (!Number.isInteger(value)) fail(`${label} must be an integer.`);
}

export function validatePortfolio(portfolio) {
  requireObject(portfolio, 'portfolio');
  if (portfolio.schema_version !== 1) fail('portfolio.schema_version must be 1.');
  requireObject(portfolio.media_contract, 'portfolio.media_contract');
  const contract = portfolio.media_contract;
  for (const key of ['width', 'height', 'input_fps', 'output_fps', 'poster_width', 'poster_height']) {
    requireInteger(contract[key], `media_contract.${key}`);
    if (contract[key] <= 0) fail(`media_contract.${key} must be positive.`);
  }
  for (const key of ['video_codec', 'pixel_format', 'poster_codec']) {
    requireString(contract[key], `media_contract.${key}`);
  }
  if (typeof contract.audio !== 'boolean' || typeof contract.fast_start !== 'boolean') {
    fail('media_contract.audio and media_contract.fast_start must be booleans.');
  }
  const releaseMediaContract = {
    width: 1280,
    height: 720,
    input_fps: 8,
    output_fps: 24,
    video_codec: 'h264',
    pixel_format: 'yuv420p',
    audio: false,
    fast_start: true,
    poster_codec: 'mjpeg',
    poster_width: 1280,
    poster_height: 720,
  };
  for (const [key, value] of Object.entries(releaseMediaContract)) {
    if (contract[key] !== value) fail(`media_contract.${key} must be ${JSON.stringify(value)} for schema v1.`);
  }
  if (!Array.isArray(portfolio.films) || portfolio.films.length !== 12) {
    fail(`portfolio.films must contain exactly 12 entries; found ${portfolio.films?.length ?? 0}.`);
  }

  const slugs = new Set();
  const placements = new Set();
  const placementPages = new Set([
    'sites/marketing/index.html',
    'sites/marketing/concepts/index.html',
    'sites/marketing/why/index.html',
    'sites/marketing/showcase/index.html',
  ]);
  for (const [index, film] of portfolio.films.entries()) {
    const base = `films[${index}]`;
    requireObject(film, base);
    requireString(film.slug, `${base}.slug`);
    if (!/^[a-z0-9]+(?:-[a-z0-9]+)*$/.test(film.slug)) fail(`${base}.slug is invalid: ${film.slug}`);
    if (slugs.has(film.slug)) fail(`Duplicate film slug: ${film.slug}`);
    slugs.add(film.slug);
    requireString(film.title, `${base}.title`);
    if (!['needs_recording', 'ready'].includes(film.status)) fail(`${base}.status must be needs_recording or ready.`);
    for (const key of ['scenario', 'fixture', 'shot_contract', 'provenance']) requireString(film[key], `${base}.${key}`);
    requireObject(film.media, `${base}.media`);
    const expectedMp4 = `sites/marketing/assets/films/${film.slug}.mp4`;
    const expectedPoster = `sites/marketing/assets/films/${film.slug}.jpg`;
    if (film.media.mp4 !== expectedMp4 || film.media.poster !== expectedPoster) {
      fail(`${base}.media must be the matching MP4/JPEG pair for ${film.slug}.`);
    }
    if (film.provenance !== `demo/one-app/films/provenance/${film.slug}.json`) {
      fail(`${base}.provenance must use the canonical per-film path.`);
    }
    if (film.shot_contract !== `demo/one-app/films/SHOT-MANIFEST.md#${film.slug}`) {
      fail(`${base}.shot_contract must use the matching SHOT-MANIFEST anchor.`);
    }
    if (!Array.isArray(film.placements) || film.placements.length === 0) fail(`${base}.placements must not be empty.`);
    for (const placement of film.placements) {
      requireObject(placement, `${base}.placement`);
      requireString(placement.page, `${base}.placement.page`);
      if (!placementPages.has(placement.page)) fail(`${base}.placement.page is not a release marketing page: ${placement.page}`);
      requireInteger(placement.order, `${base}.placement.order`);
      if (placement.order < 1) fail(`${base}.placement.order must be positive.`);
      const key = `${placement.page}#${placement.order}`;
      if (placements.has(key)) fail(`Duplicate film placement order: ${key}`);
      placements.add(key);
    }
    requireObject(film.duration_seconds, `${base}.duration_seconds`);
    const { min, max } = film.duration_seconds;
    if (!(Number.isFinite(min) && Number.isFinite(max) && min > 0 && max >= min)) {
      fail(`${base}.duration_seconds must have positive min/max bounds.`);
    }
    if (!Array.isArray(film.beats) || film.beats.length < 2) fail(`${base}.beats must contain at least two beats.`);
    const beatIds = new Set();
    for (const beat of film.beats) {
      requireObject(beat, `${base}.beat`);
      requireString(beat.id, `${base}.beat.id`);
      requireString(beat.label, `${base}.beat.label`);
      if (!/^[a-z0-9]+(?:-[a-z0-9]+)*$/.test(beat.id)) fail(`${base}.beat.id is invalid: ${beat.id}`);
      if (beatIds.has(beat.id)) fail(`${base} has duplicate beat ${beat.id}.`);
      beatIds.add(beat.id);
      if (!Array.isArray(beat.evidence) || beat.evidence.length === 0 || beat.evidence.some(item => typeof item !== 'string' || !item.trim())) {
        fail(`${base}.beat ${beat.id} must contain evidence strings.`);
      }
    }
    if (!beatIds.has(film.poster_beat)) fail(`${base}.poster_beat is not a declared beat.`);
    if (!Array.isArray(film.evidence) || film.evidence.length === 0 || film.evidence.some(item => typeof item !== 'string' || !item.trim())) {
      fail(`${base}.evidence must contain non-empty strings.`);
    }
  }
  const showcaseOrders = portfolio.films.map(film => {
    const showcase = film.placements.filter(placement => placement.page === 'sites/marketing/showcase/index.html');
    if (showcase.length !== 1) fail(`${film.slug} must have exactly one Showcase placement.`);
    return showcase[0].order;
  }).sort((left, right) => left - right);
  const expectedShowcaseOrders = Array.from({ length: portfolio.films.length }, (_, index) => index + 1);
  if (showcaseOrders.some((order, index) => order !== expectedShowcaseOrders[index])) {
    fail(`Showcase placement orders must be the complete sequence 1-${portfolio.films.length}.`);
  }
  return portfolio;
}

export function loadPortfolio() {
  return validatePortfolio(readJson(portfolioPath));
}

export function findFilm(portfolio, slug) {
  const film = portfolio.films.find(item => item.slug === slug);
  if (!film) fail(`Unknown film slug: ${slug}`);
  return film;
}

export function validateTimeline(portfolio, film, timeline) {
  requireObject(timeline, 'timeline');
  if (timeline.schema_version !== 1) fail('timeline.schema_version must be 1.');
  if (timeline.film !== film.slug) fail(`timeline.film must be ${film.slug}.`);
  if (timeline.input_fps !== portfolio.media_contract.input_fps) {
    fail(`timeline.input_fps must be ${portfolio.media_contract.input_fps}.`);
  }
  if (!Array.isArray(timeline.shots) || timeline.shots.length !== film.beats.length) {
    fail(`timeline.shots must contain exactly ${film.beats.length} entries.`);
  }
  let frameCount = 0;
  for (const [index, shot] of timeline.shots.entries()) {
    requireObject(shot, `timeline.shots[${index}]`);
    const expectedBeat = film.beats[index].id;
    if (shot.beat !== expectedBeat) {
      fail(`timeline.shots[${index}].beat must be ${expectedBeat}; beats must stay in manifest order.`);
    }
    const expectedSource = `shot-${expectedBeat}.jpg`;
    if (shot.source !== expectedSource) {
      fail(`timeline.shots[${index}].source must be ${expectedSource}.`);
    }
    requireInteger(shot.hold_frames, `timeline.shots[${index}].hold_frames`);
    if (shot.hold_frames <= 0) fail(`timeline.shots[${index}].hold_frames must be positive.`);
    frameCount += shot.hold_frames;
  }
  const durationSeconds = frameCount / timeline.input_fps;
  if (durationSeconds < film.duration_seconds.min || durationSeconds > film.duration_seconds.max) {
    fail(
      `Timeline duration ${durationSeconds.toFixed(3)}s is outside ${film.slug}'s ` +
      `${film.duration_seconds.min}-${film.duration_seconds.max}s contract.`,
    );
  }
  return { frameCount, durationSeconds };
}

export function parseMp4Atoms(path) {
  const bytes = readFileSync(path);
  const atoms = [];
  let offset = 0;
  while (offset + 8 <= bytes.length) {
    let size = bytes.readUInt32BE(offset);
    const type = bytes.subarray(offset + 4, offset + 8).toString('ascii');
    let header = 8;
    if (size === 1) {
      if (offset + 16 > bytes.length) fail(`Invalid large MP4 atom in ${path}.`);
      const large = bytes.readBigUInt64BE(offset + 8);
      if (large > BigInt(Number.MAX_SAFE_INTEGER)) fail(`Oversized MP4 atom in ${path}.`);
      size = Number(large);
      header = 16;
    } else if (size === 0) {
      size = bytes.length - offset;
    }
    if (size < header || offset + size > bytes.length) fail(`Invalid MP4 atom ${type} in ${path}.`);
    atoms.push({ type, offset, size });
    offset += size;
  }
  return atoms;
}

function ffprobe(path) {
  const output = run('ffprobe', [
    '-v', 'error', '-show_streams', '-show_format', '-of', 'json', path,
  ]);
  return JSON.parse(output);
}

export function probeVideo(path) {
  const data = ffprobe(path);
  const video = data.streams.find(stream => stream.codec_type === 'video');
  if (!video) fail(`No video stream in ${path}.`);
  const audioStreams = data.streams.filter(stream => stream.codec_type === 'audio').length;
  const atoms = parseMp4Atoms(path);
  const moov = atoms.find(atom => atom.type === 'moov');
  const mdat = atoms.find(atom => atom.type === 'mdat');
  return {
    duration_seconds: Number(data.format.duration),
    codec: video.codec_name,
    width: video.width,
    height: video.height,
    pixel_format: video.pix_fmt,
    fps: video.r_frame_rate,
    audio_streams: audioStreams,
    fast_start: Boolean(moov && mdat && moov.offset < mdat.offset),
  };
}

export function probePoster(path) {
  const data = ffprobe(path);
  const video = data.streams.find(stream => stream.codec_type === 'video');
  if (!video) fail(`No image stream in ${path}.`);
  return { codec: video.codec_name, width: video.width, height: video.height };
}

export function exists(path) {
  return existsSync(path);
}

export function parentDirectory(path) {
  return dirname(path);
}
