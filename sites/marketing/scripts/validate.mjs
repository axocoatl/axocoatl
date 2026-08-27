import { spawnSync } from 'node:child_process';
import { createHash } from 'node:crypto';
import { existsSync, readFileSync, readdirSync } from 'node:fs';
import { join, resolve } from 'node:path';

const sourceRoot = resolve(import.meta.dirname, '..');
const repositoryRoot = resolve(sourceRoot, '../..');
const portfolioPath = resolve(repositoryRoot, 'demo/one-app/films/portfolio.json');
const arguments_ = process.argv.slice(2);
const strictFilms = arguments_.includes('--strict-films');
const positionalArguments = arguments_.filter((argument) => !argument.startsWith('--'));
const unknownFlags = arguments_.filter((argument) => argument.startsWith('--') && argument !== '--strict-films');

if (unknownFlags.length || positionalArguments.length > 1) {
  console.error('Usage: node sites/marketing/scripts/validate.mjs [marketing-root] [--strict-films]');
  process.exit(64);
}

const root = resolve(positionalArguments[0] || sourceRoot);
const pages = [
  'index.html', '404.html', 'changelog/index.html', 'concepts/index.html',
  'install/index.html', 'integrations/openrouter/index.html', 'pricing/index.html',
  'showcase/index.html', 'why/index.html',
];
const scripts = [
  'components/ax-site-nav.js', 'components/ax-footer.js',
  'components/ax-theme-toggle.js', 'components/ax-cli-snippet.js',
  'components/ax-comparison-row.js', 'components/ax-product-film.js',
];
if (root === sourceRoot) scripts.push('scripts/build.mjs', 'scripts/validate.mjs');

const errors = [];
const warnings = [];
const expectedMediaContract = {
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

function fail(file, message) { errors.push(`${file}: ${message}`); }
function warn(file, message) { warnings.push(`${file}: ${message}`); }
function isRecord(value) { return value !== null && typeof value === 'object' && !Array.isArray(value); }
function isNonEmptyString(value) { return typeof value === 'string' && value.trim().length > 0; }
function isSha256(value) { return typeof value === 'string' && /^[a-f0-9]{64}$/.test(value); }
function isGitHead(value) { return typeof value === 'string' && /^[a-f0-9]{40}$/.test(value); }
function validateLlms(source, label) {
  for (const [contract, marker] of [
    ['1.0 product category', '# Axocoatl — the local-first workbench for coding agents'],
    ['durable Session spine', 'durable, folder-anchored Session'],
    ['separate repository truth', 'Source Control separately shows the repository as it exists now'],
    ['honest network boundary', 'Local-first does not mean every configured path is offline'],
    ['user-level onboarding', 'Configure Axocoatl once for this OS user'],
    ['canonical product reference', 'docs/PRODUCT.md'],
  ]) {
    if (!source.includes(marker)) fail(label, `missing ${contract}`);
  }
  for (const retired of [
    'Rust-Native Agentic AI Runtime',
    'building self-coordinating multi-agent systems',
    'stigmergic pheromone',
    'no central orchestrator',
    'provider and starter project',
    'axocoatl onboard                 Interactive setup',
  ]) {
    if (source.toLowerCase().includes(retired.toLowerCase())) {
      fail(label, `retired pre-1.0 narrative: ${retired}`);
    }
  }
}
function isSafeRepositoryPath(value) {
  return isNonEmptyString(value)
    && !value.startsWith('/')
    && !value.includes('\\')
    && !value.split('/').includes('..');
}

function readRequired(path, label, encoding) {
  if (!existsSync(path)) {
    fail(label, 'missing required file');
    return null;
  }
  try {
    return readFileSync(path, encoding);
  } catch (error) {
    fail(label, `could not be read: ${error.message}`);
    return null;
  }
}

function parseJson(path, label) {
  const source = readRequired(path, label, 'utf8');
  if (source === null) return null;
  try {
    return JSON.parse(source);
  } catch (error) {
    fail(label, `invalid JSON: ${error.message}`);
    return null;
  }
}

function repositoryPath(path, label) {
  if (!isNonEmptyString(path) || path.startsWith('/') || path.split('/').includes('..')) {
    fail(label, 'must be a safe repository-relative path');
    return null;
  }
  const absolute = resolve(repositoryRoot, path);
  if (absolute !== repositoryRoot && !absolute.startsWith(`${repositoryRoot}/`)) {
    fail(label, 'must remain inside the repository');
    return null;
  }
  return absolute;
}

function marketingRelativePath(path, label) {
  if (!isNonEmptyString(path) || !path.startsWith('sites/marketing/') || path.split('/').includes('..')) {
    fail(label, 'must be a repository-relative path below sites/marketing/');
    return null;
  }
  return path.slice('sites/marketing/'.length);
}

function destinationExists(url) {
  const clean = url.split(/[?#]/)[0];
  if (!clean.startsWith('/')) return true;
  const relative = clean.slice(1);
  if (!relative) return existsSync(join(root, 'index.html'));
  const exact = join(root, relative);
  return existsSync(exact) || existsSync(join(exact, 'index.html'));
}

function markdownAnchors(markdown) {
  const anchors = new Set();
  const counts = new Map();
  for (const line of markdown.split(/\r?\n/)) {
    const heading = line.match(/^#{1,6}\s+(.+?)\s*#*\s*$/)?.[1];
    if (!heading) continue;
    const base = heading
      .replace(/<[^>]+>/g, '')
      .replace(/`/g, '')
      .toLowerCase()
      .trim()
      .replace(/[^\p{Letter}\p{Number}\s-]/gu, '')
      .replace(/\s+/g, '-');
    const count = counts.get(base) || 0;
    counts.set(base, count + 1);
    anchors.add(count ? `${base}-${count}` : base);
  }
  return anchors;
}

function inspectMp4Atoms(buffer) {
  const atoms = [];
  let offset = 0;
  while (offset + 8 <= buffer.length) {
    let size = buffer.readUInt32BE(offset);
    const type = buffer.subarray(offset + 4, offset + 8).toString('ascii');
    let headerSize = 8;
    if (size === 1) {
      if (offset + 16 > buffer.length) break;
      const extendedSize = buffer.readBigUInt64BE(offset + 8);
      if (extendedSize > BigInt(Number.MAX_SAFE_INTEGER)) break;
      size = Number(extendedSize);
      headerSize = 16;
    } else if (size === 0) {
      size = buffer.length - offset;
    }
    if (size < headerSize || offset + size > buffer.length) break;
    atoms.push(type);
    offset += size;
  }
  return atoms;
}

function probeMedia(path) {
  const result = spawnSync('ffprobe', [
    '-v', 'error', '-show_streams', '-show_format', '-of', 'json', path,
  ], { encoding: 'utf8', maxBuffer: 8 * 1024 * 1024 });
  if (result.error) return { error: result.error };
  if (result.status !== 0) return { error: new Error(result.stderr.trim() || `ffprobe exited ${result.status}`) };
  try {
    return { value: JSON.parse(result.stdout) };
  } catch (error) {
    return { error };
  }
}

function rateAsNumber(rate) {
  if (typeof rate !== 'string') return Number.NaN;
  const [numerator, denominator = '1'] = rate.split('/').map(Number);
  if (!Number.isFinite(numerator) || !Number.isFinite(denominator) || denominator === 0) return Number.NaN;
  return numerator / denominator;
}

function sha256(buffer) {
  return createHash('sha256').update(buffer).digest('hex');
}

const portfolio = parseJson(portfolioPath, 'demo/one-app/films/portfolio.json');
if (!portfolio) {
  console.error(`Marketing validation failed (${errors.length})\n${errors.map((error) => `- ${error}`).join('\n')}`);
  process.exit(1);
}

if (portfolio.schema_version !== 1) fail('demo/one-app/films/portfolio.json', 'schema_version must be 1');
if (!isRecord(portfolio.media_contract)) {
  fail('demo/one-app/films/portfolio.json', 'media_contract must be an object');
} else {
  const expectedKeys = Object.keys(expectedMediaContract).sort();
  const actualKeys = Object.keys(portfolio.media_contract).sort();
  if (actualKeys.join('\n') !== expectedKeys.join('\n')) {
    fail('demo/one-app/films/portfolio.json', `media_contract must contain exactly: ${expectedKeys.join(', ')}`);
  }
  for (const [key, expected] of Object.entries(expectedMediaContract)) {
    if (portfolio.media_contract[key] !== expected) {
      fail('demo/one-app/films/portfolio.json', `media_contract.${key} must be ${JSON.stringify(expected)}`);
    }
  }
}

const films = Array.isArray(portfolio.films) ? portfolio.films : [];
if (!Array.isArray(portfolio.films)) fail('demo/one-app/films/portfolio.json', 'films must be an array');
if (films.length !== 12) fail('demo/one-app/films/portfolio.json', `expected exactly 12 films, found ${films.length}`);

const filmBySlug = new Map();
const expectedFilmsByPage = new Map();
const mediaPaths = new Set();

function filmIssue(film, file, message) {
  if (strictFilms || film.status === 'ready') fail(file, message);
  else warn(file, message);
}

for (const [filmIndex, film] of films.entries()) {
  const location = `demo/one-app/films/portfolio.json films[${filmIndex}]`;
  if (!isRecord(film)) {
    fail(location, 'film must be an object');
    continue;
  }

  if (!isNonEmptyString(film.slug) || !/^[a-z0-9]+(?:-[a-z0-9]+)*$/.test(film.slug)) {
    fail(location, 'slug must use lowercase words separated by single hyphens');
    continue;
  }
  if (filmBySlug.has(film.slug)) fail(location, `duplicate film slug ${film.slug}`);
  else filmBySlug.set(film.slug, film);

  if (!isNonEmptyString(film.title)) fail(location, 'title must be a non-empty string');
  if (!isNonEmptyString(film.fixture)) fail(location, 'fixture must be a non-empty string');
  if (!['needs_recording', 'ready'].includes(film.status)) fail(location, 'status must be needs_recording or ready');
  else if (film.status === 'needs_recording') filmIssue(film, film.slug, 'film is marked needs_recording');

  const scenario = repositoryPath(film.scenario, `${film.slug} scenario`);
  if (scenario && !film.scenario.startsWith('demo/one-app/scenarios/')) {
    fail(film.slug, 'scenario must be below demo/one-app/scenarios/');
  } else if (scenario && !existsSync(scenario)) {
    filmIssue(film, film.scenario, 'referenced scenario does not exist');
  }

  const expectedShotContract = `demo/one-app/films/SHOT-MANIFEST.md#${film.slug}`;
  if (film.shot_contract !== expectedShotContract) {
    fail(film.slug, `shot_contract must be ${expectedShotContract}`);
  }
  if (!isNonEmptyString(film.shot_contract) || !film.shot_contract.includes('#')) {
    fail(film.slug, 'shot_contract must be a repository path with a heading anchor');
  } else {
    const separator = film.shot_contract.lastIndexOf('#');
    const shotPath = film.shot_contract.slice(0, separator);
    const shotAnchor = film.shot_contract.slice(separator + 1);
    const absoluteShotPath = repositoryPath(shotPath, `${film.slug} shot_contract`);
    if (!shotAnchor) {
      fail(film.slug, 'shot_contract anchor must not be empty');
    } else if (absoluteShotPath && !existsSync(absoluteShotPath)) {
      filmIssue(film, shotPath, 'referenced shot contract does not exist');
    } else if (absoluteShotPath) {
      const anchors = markdownAnchors(readFileSync(absoluteShotPath, 'utf8'));
      if (!anchors.has(shotAnchor)) filmIssue(film, film.shot_contract, 'referenced shot-contract heading does not exist');
    }
  }

  const expectedProvenance = `demo/one-app/films/provenance/${film.slug}.json`;
  if (film.provenance !== expectedProvenance) {
    fail(film.slug, `provenance must be ${expectedProvenance}`);
  }

  if (!isRecord(film.duration_seconds)
      || !Number.isFinite(film.duration_seconds.min)
      || !Number.isFinite(film.duration_seconds.max)
      || film.duration_seconds.min <= 0
      || film.duration_seconds.max <= film.duration_seconds.min) {
    fail(film.slug, 'duration_seconds must contain positive min/max numbers with max greater than min');
  }

  const beatIds = new Set();
  if (!Array.isArray(film.beats) || film.beats.length === 0) {
    fail(film.slug, 'beats must be a non-empty array');
  } else {
    for (const [beatIndex, beat] of film.beats.entries()) {
      if (!isRecord(beat) || !isNonEmptyString(beat.id) || !/^[a-z0-9]+(?:-[a-z0-9]+)*$/.test(beat.id)) {
        fail(film.slug, `beats[${beatIndex}].id must be a lowercase hyphenated identifier`);
        continue;
      }
      if (beatIds.has(beat.id)) fail(film.slug, `duplicate beat id ${beat.id}`);
      beatIds.add(beat.id);
      if (!isNonEmptyString(beat.label)) fail(film.slug, `beat ${beat.id} needs a label`);
      if (!Array.isArray(beat.evidence) || beat.evidence.length === 0 || beat.evidence.some((item) => !isNonEmptyString(item))) {
        fail(film.slug, `beat ${beat.id} needs non-empty evidence strings`);
      }
    }
  }
  if (!isNonEmptyString(film.poster_beat) || !beatIds.has(film.poster_beat)) {
    fail(film.slug, 'poster_beat must name one declared beat');
  }
  if (!Array.isArray(film.evidence) || film.evidence.length === 0 || film.evidence.some((item) => !isNonEmptyString(item))) {
    fail(film.slug, 'evidence must be a non-empty array of strings');
  }

  if (!isRecord(film.media)) {
    fail(film.slug, 'media must be an object');
  } else {
    const expectedMp4 = `sites/marketing/assets/films/${film.slug}.mp4`;
    const expectedPoster = `sites/marketing/assets/films/${film.slug}.jpg`;
    if (film.media.mp4 !== expectedMp4) fail(film.slug, `media.mp4 must be ${expectedMp4}`);
    if (film.media.poster !== expectedPoster) fail(film.slug, `media.poster must be ${expectedPoster}`);
    for (const mediaPath of [film.media.mp4, film.media.poster]) {
      if (!isNonEmptyString(mediaPath)) continue;
      if (mediaPaths.has(mediaPath)) fail(film.slug, `duplicate portfolio media path ${mediaPath}`);
      mediaPaths.add(mediaPath);
    }
  }

  if (!Array.isArray(film.placements) || film.placements.length === 0) {
    fail(film.slug, 'placements must be a non-empty array');
  } else {
    const placedPages = new Set();
    for (const [placementIndex, placement] of film.placements.entries()) {
      if (!isRecord(placement)) {
        fail(film.slug, `placements[${placementIndex}] must be an object`);
        continue;
      }
      const page = marketingRelativePath(placement.page, `${film.slug} placement page`);
      if (!page || !pages.includes(page)) {
        if (page) fail(film.slug, `placement page is not a shipped marketing page: ${placement.page}`);
        continue;
      }
      if (placedPages.has(page)) fail(film.slug, `duplicate placement on ${placement.page}`);
      placedPages.add(page);
      if (!Number.isInteger(placement.order) || placement.order < 1) {
        fail(film.slug, `placement order on ${placement.page} must be a positive integer`);
        continue;
      }
      const placements = expectedFilmsByPage.get(page) || [];
      placements.push({ slug: film.slug, order: placement.order });
      expectedFilmsByPage.set(page, placements);
    }
  }
}

if (filmBySlug.size !== 12) fail('demo/one-app/films/portfolio.json', `expected 12 unique film slugs, found ${filmBySlug.size}`);
if (mediaPaths.size !== 24) fail('demo/one-app/films/portfolio.json', `expected 24 unique media paths, found ${mediaPaths.size}`);

const filmAssetDirectory = join(root, 'assets/films');
if (!existsSync(filmAssetDirectory)) {
  fail('assets/films', 'missing film asset directory');
} else {
  const shippedMedia = readdirSync(filmAssetDirectory)
    .filter((name) => /\.(?:mp4|jpg)$/i.test(name))
    .sort();
  const declaredMedia = [...mediaPaths]
    .map((path) => path.slice('sites/marketing/assets/films/'.length))
    .sort();
  if (shippedMedia.join('\n') !== declaredMedia.join('\n')) {
    const undeclared = shippedMedia.filter((name) => !declaredMedia.includes(name));
    const missing = declaredMedia.filter((name) => !shippedMedia.includes(name));
    if (undeclared.length) fail('assets/films', `undeclared film media: ${undeclared.join(', ')}`);
    if (missing.length) fail('assets/films', `declared film media missing from the directory: ${missing.join(', ')}`);
  }
  const mp4Count = shippedMedia.filter((name) => name.endsWith('.mp4')).length;
  const posterCount = shippedMedia.filter((name) => name.endsWith('.jpg')).length;
  if (mp4Count !== 12 || posterCount !== 12) fail('assets/films', `expected exactly 12 MP4s and 12 JPEG posters; found ${mp4Count} MP4s and ${posterCount} posters`);
}

for (const [page, placements] of expectedFilmsByPage) {
  placements.sort((left, right) => left.order - right.order);
  for (const [index, placement] of placements.entries()) {
    if (placement.order !== index + 1) fail(page, `film placement orders must be contiguous from 1; found ${placement.order} at position ${index + 1}`);
    if (index > 0 && placement.order === placements[index - 1].order) fail(page, `duplicate film placement order ${placement.order}`);
  }
}

const showcasePlacements = expectedFilmsByPage.get('showcase/index.html') || [];
if (showcasePlacements.length !== 12 || new Set(showcasePlacements.map((placement) => placement.slug)).size !== 12) {
  fail('showcase/index.html', 'the authoritative portfolio must place every one of the 12 films on Showcase exactly once');
}

if (root !== sourceRoot) {
  const builtPortfolioPath = join(root, 'assets/films/portfolio.json');
  const builtPortfolio = readRequired(builtPortfolioPath, 'assets/films/portfolio.json');
  const authoritativePortfolio = readRequired(portfolioPath, 'demo/one-app/films/portfolio.json');
  if (builtPortfolio && authoritativePortfolio && !builtPortfolio.equals(authoritativePortfolio)) {
    fail('assets/films/portfolio.json', 'built portfolio differs from the authoritative demo/one-app portfolio');
  }
}

for (const page of pages) {
  const path = join(root, page);
  const html = readRequired(path, page, 'utf8');
  if (html === null) continue;
  const h1s = html.match(/<h1\b/gi) || [];
  if (h1s.length !== 1) fail(page, `expected one h1, found ${h1s.length}`);
  if (!/<html\s+lang="en"/i.test(html)) fail(page, 'missing html language');
  if (!/<meta\s+name="description"\s+content="[^"]+"/i.test(html)) fail(page, 'missing description');
  if (page !== '404.html' && !/<link\s+rel="canonical"\s+href="https:\/\/axocoatl\.ai\//i.test(html)) fail(page, 'missing canonical URL');
  if (!/<main\s+id="main-content"/i.test(html)) fail(page, 'main must expose the skip-link target');

  for (const match of html.matchAll(/<img\b([^>]*)>/gi)) {
    if (!/\balt="[^"]*"/i.test(match[1])) fail(page, 'image is missing alt text');
  }
  for (const match of html.matchAll(/\b(?:href|src|poster)="([^"]+)"/gi)) {
    const url = match[1];
    if (/^(?:https?:|mailto:|tel:|#)/.test(url)) continue;
    if (!destinationExists(url)) fail(page, `broken local reference ${url}`);
  }
  for (const match of html.matchAll(/<a\b([^>]*)>/gi)) {
    if (/target="_blank"/i.test(match[1]) && !/rel="[^"]*noopener/i.test(match[1])) fail(page, 'target=_blank requires rel=noopener');
  }

  const seenFilms = [];
  for (const match of html.matchAll(/<ax-product-film\b([^>]*)>/gi)) {
    const attributes = match[1];
    for (const required of ['film', 'src', 'poster', 'label', 'caption']) {
      if (!new RegExp(`\\b${required}="[^"]+"`, 'i').test(attributes)) {
        fail(page, `product film is missing ${required}`);
      }
    }
    const film = attributes.match(/\bfilm="([^"]+)"/i)?.[1];
    const src = attributes.match(/\bsrc="([^"]+)"/i)?.[1];
    const poster = attributes.match(/\bposter="([^"]+)"/i)?.[1];
    if (film) {
      seenFilms.push(film);
      if (!filmBySlug.has(film)) fail(page, `product film ${film} is not declared in the authoritative portfolio`);
      if (src !== `/assets/films/${film}.mp4`) fail(page, `product film ${film} must use its matching MP4`);
      if (poster !== `/assets/films/${film}.jpg`) fail(page, `product film ${film} must use its matching JPEG poster`);
    }
  }

  const expectedFilms = (expectedFilmsByPage.get(page) || []).map((placement) => placement.slug);
  for (const film of expectedFilms) {
    const count = seenFilms.filter((candidate) => candidate === film).length;
    if (count !== 1) fail(page, `expected product film ${film} exactly once, found ${count}`);
  }
  for (const film of seenFilms) {
    if (!expectedFilms.includes(film)) fail(page, `unexpected product film placement ${film}`);
  }
  if (seenFilms.join('\n') !== expectedFilms.join('\n')) {
    fail(page, `product films must appear in portfolio order: ${expectedFilms.join(', ')}`);
  }

  if (page !== 'changelog/index.html') {
    const visible = html.replace(/<script[\s\S]*?<\/script>/gi, '').replace(/<style[\s\S]*?<\/style>/gi, '').replace(/<[^>]+>/g, ' ');
    const forbidden = ['unleash', 'supercharge', 'revolutionize', 'reimagine', 'lightning-fast', 'blazing-fast', 'next-generation', 'next-gen', 'AI-powered', 'AI-native', 'seamless', 'frictionless'];
    for (const word of forbidden) if (new RegExp(`\\b${word.replace('-', '[- ]')}\\b`, 'i').test(visible)) fail(page, `forbidden marketing phrase: ${word}`);
    if (/\bv0\.1\.4\b/i.test(visible)) fail(page, 'stale release version in current product copy');
    if (/\b(?:Activity pane|Sessions cockpit|Studio lattice|Browser pane)\b/i.test(visible)) fail(page, 'retired product vocabulary in current copy');
    if (page === 'integrations/openrouter/index.html') {
      if (!/API-key prompt is masked/i.test(visible)) {
        fail(page, 'must explain that normal onboarding masks the OpenRouter key prompt');
      }
      if (!/owner-only Axocoatl configuration/i.test(visible)) {
        fail(page, 'must explain that normal onboarding uses the owner-only user configuration');
      }
      if (!/does not load dotenv files automatically/i.test(visible)) {
        fail(page, 'must explain dotenv behavior for advanced environment injection');
      }
      if (/\.env\.example/i.test(visible)) {
        fail(page, 'must not describe the retired onboarding .env.example flow');
      }
    }
  }
}

for (const script of scripts) {
  const source = readRequired(join(root, script), script, 'utf8');
  if (source !== null && !source.trim()) fail(script, 'empty JavaScript file');
}

const llmsLabel = 'llms.txt';
const llmsPath = root === sourceRoot
  ? join(repositoryRoot, llmsLabel)
  : join(root, llmsLabel);
const llms = readRequired(llmsPath, llmsLabel, 'utf8');
if (llms) validateLlms(llms, llmsLabel);

if (root === sourceRoot) {
  const readmeLabel = 'README.md';
  const readme = readRequired(join(repositoryRoot, readmeLabel), readmeLabel, 'utf8');
  if (readme) {
    for (const [contract, marker] of [
      ['1.0 product category', 'The open-source, local-first workbench for coding agents.'],
      ['current HTTP reference', 'https://docs.axocoatl.ai/reference/http-api/'],
    ]) {
      if (!readme.includes(marker)) fail(readmeLabel, `missing ${contract}`);
    }
    if (readme.includes('https://docs.axocoatl.ai/api/http/')) {
      fail(readmeLabel, 'retired pre-1.0 HTTP reference URL');
    }
  }

  const workflowLabel = '.github/workflows/marketing-deploy.yml';
  const workflow = readRequired(join(repositoryRoot, workflowLabel), workflowLabel, 'utf8');
  if (workflow) {
    for (const [contract, marker] of [
      ['AI-readable narrative trigger', "'llms.txt'"],
      ['authoritative portfolio trigger', "'demo/one-app/films/portfolio.json'"],
      ['source-bound film provenance gate', 'node demo/one-app/films/verify-film-set.mjs --source-bound'],
      ['source strict-film gate', 'node sites/marketing/scripts/validate.mjs --strict-films'],
      ['built strict-film gate', 'node sites/marketing/scripts/validate.mjs /tmp/axocoatl-marketing --strict-films'],
      ['validated build deployment', 'pages deploy /tmp/axocoatl-marketing'],
    ]) {
      if (!workflow.includes(marker)) fail(workflowLabel, `missing ${contract}`);
    }
  }
}

const socialCard = readRequired(join(root, 'assets/og-home.png'), 'assets/og-home.png');
if (socialCard) {
  const pngSignature = Buffer.from([137, 80, 78, 71, 13, 10, 26, 10]);
  if (!socialCard.subarray(0, pngSignature.length).equals(pngSignature)) {
    fail('assets/og-home.png', 'social card must be encoded as PNG');
  } else if (socialCard.readUInt32BE(16) !== 1200 || socialCard.readUInt32BE(20) !== 630) {
    fail('assets/og-home.png', 'social card must be 1200×630');
  }
}

const cliSnippet = readRequired(join(root, 'components/ax-cli-snippet.js'), 'components/ax-cli-snippet.js', 'utf8');
if (cliSnippet) {
  for (const [capability, marker] of [
    ['button semantics', "setAttribute('role', 'button')"],
    ['keyboard focus', 'this.tabIndex = 0'],
    ['keyboard activation', "addEventListener('keydown'"],
    ['Enter-key activation', "event.key !== 'Enter'"],
    ['Space-key activation', "event.key !== ' '"],
    ['copy status announcement', "setAttribute('aria-live', 'polite')"],
  ]) {
    if (!cliSnippet.includes(marker)) fail('components/ax-cli-snippet.js', `missing ${capability}`);
  }
}

const productFilm = readRequired(join(root, 'components/ax-product-film.js'), 'components/ax-product-film.js', 'utf8');
if (productFilm) {
  for (const [capability, marker] of [
    ['stable film identity', "this.getAttribute('film')"],
    ['matching MP4/JPEG pair', '`/assets/films/${film}.mp4`'],
    ['muted playback', 'video.muted = true'],
    ['inline playback', 'video.playsInline = true'],
    ['explicit Play control', "control.addEventListener('click'"],
    ['replay control', "this._setControl('Replay film'"],
    ['offscreen pause', "'IntersectionObserver' in window"],
    ['reduced-motion handling', "matchMedia('(prefers-reduced-motion: reduce)')"],
    ['reduced-motion autoplay guard', "else if (this.hasAttribute('autoplay') && !this._motionQuery.matches)"],
    ['reduced-motion resume guard', 'if (this._motionQuery.matches || this._manualPause || this._video.ended) return'],
    ['reduced-motion preference pause', 'if (event.matches && !video.paused)'],
    ['discoverable open-film control', "openControl.textContent = 'Open film'"],
    ['fullscreen support', 'frame.requestFullscreen'],
    ['fullscreen fallback', "window.open(this._src, '_blank', 'noopener,noreferrer')"],
  ]) {
    if (!productFilm.includes(marker)) fail('components/ax-product-film.js', `missing ${capability}`);
  }
}

const baseCss = readRequired(join(root, 'styles/base.css'), 'styles/base.css', 'utf8');
if (baseCss) {
  for (const [capability, marker] of [
    ['contained film rendering', 'object-fit: contain'],
    ['grouped film controls', '.product-film-actions'],
    ['fullscreen film layout', '.product-film-frame:fullscreen'],
    ['narrow layout', '@media (max-width: 560px)'],
  ]) {
    if (!baseCss.includes(marker)) fail('styles/base.css', `missing ${capability}`);
  }
}

const ffprobeCheck = spawnSync('ffprobe', ['-version'], { encoding: 'utf8' });
const ffprobeAvailable = !ffprobeCheck.error && ffprobeCheck.status === 0;
if (!ffprobeAvailable) {
  const requiresProbe = strictFilms || films.some((film) => film.status === 'ready');
  (requiresProbe ? fail : warn)('film portfolio', 'ffprobe is required to verify the exact film media contract');
}

function validateMedia(film) {
  const facts = {};
  if (!isRecord(film.media)) return facts;
  const mp4Relative = marketingRelativePath(film.media.mp4, `${film.slug} media.mp4`);
  const posterRelative = marketingRelativePath(film.media.poster, `${film.slug} media.poster`);
  const mp4Path = mp4Relative ? join(root, mp4Relative) : null;
  const posterPath = posterRelative ? join(root, posterRelative) : null;

  if (mp4Path && !existsSync(mp4Path)) {
    filmIssue(film, film.media.mp4, 'product film MP4 does not exist');
  } else if (mp4Path) {
    const video = readFileSync(mp4Path);
    facts.mp4Sha256 = sha256(video);
    if (video.length < 12 || video.subarray(4, 8).toString('ascii') !== 'ftyp') {
      filmIssue(film, film.media.mp4, 'product film must be an MP4 file');
    }
    const atoms = inspectMp4Atoms(video);
    facts.fastStart = atoms.includes('moov') && atoms.includes('mdat') && atoms.indexOf('moov') < atoms.indexOf('mdat');
    if (!facts.fastStart) filmIssue(film, film.media.mp4, 'MP4 must place moov before mdat for fast-start delivery');

    if (ffprobeAvailable) {
      const probe = probeMedia(mp4Path);
      if (probe.error) {
        filmIssue(film, film.media.mp4, `ffprobe failed: ${probe.error.message}`);
      } else {
        const streams = Array.isArray(probe.value.streams) ? probe.value.streams : [];
        const videoStreams = streams.filter((stream) => stream.codec_type === 'video');
        const audioStreams = streams.filter((stream) => stream.codec_type === 'audio');
        if (videoStreams.length !== 1) filmIssue(film, film.media.mp4, `expected exactly one video stream, found ${videoStreams.length}`);
        if (audioStreams.length !== 0) filmIssue(film, film.media.mp4, `expected no audio streams, found ${audioStreams.length}`);
        const stream = videoStreams[0] || {};
        facts.videoCodec = stream.codec_name;
        facts.width = stream.width;
        facts.height = stream.height;
        facts.pixelFormat = stream.pix_fmt;
        facts.fps = rateAsNumber(stream.r_frame_rate || stream.avg_frame_rate);
        facts.audioStreams = audioStreams.length;
        facts.duration = Number(probe.value.format?.duration ?? stream.duration);
        if (facts.videoCodec !== expectedMediaContract.video_codec) filmIssue(film, film.media.mp4, `codec must be ${expectedMediaContract.video_codec}; found ${facts.videoCodec || 'unknown'}`);
        if (facts.width !== expectedMediaContract.width || facts.height !== expectedMediaContract.height) filmIssue(film, film.media.mp4, `dimensions must be ${expectedMediaContract.width}×${expectedMediaContract.height}; found ${facts.width || '?'}×${facts.height || '?'}`);
        if (facts.pixelFormat !== expectedMediaContract.pixel_format) filmIssue(film, film.media.mp4, `pixel format must be ${expectedMediaContract.pixel_format}; found ${facts.pixelFormat || 'unknown'}`);
        if (!Number.isFinite(facts.fps) || Math.abs(facts.fps - expectedMediaContract.output_fps) > 0.001) filmIssue(film, film.media.mp4, `frame rate must be ${expectedMediaContract.output_fps} fps; found ${Number.isFinite(facts.fps) ? facts.fps : 'unknown'}`);
        if (!Number.isFinite(facts.duration)) {
          filmIssue(film, film.media.mp4, 'duration could not be measured');
        } else if (isRecord(film.duration_seconds)
          && (facts.duration < film.duration_seconds.min - 0.05 || facts.duration > film.duration_seconds.max + 0.05)) {
          filmIssue(film, film.media.mp4, `duration ${facts.duration.toFixed(3)}s is outside declared ${film.duration_seconds.min}–${film.duration_seconds.max}s`);
        }
      }
    }
  }

  if (posterPath && !existsSync(posterPath)) {
    filmIssue(film, film.media.poster, 'product film poster does not exist');
  } else if (posterPath) {
    const poster = readFileSync(posterPath);
    facts.posterSha256 = sha256(poster);
    const validStart = poster.length >= 4 && poster[0] === 0xff && poster[1] === 0xd8;
    const validEnd = poster.length >= 4 && poster.at(-2) === 0xff && poster.at(-1) === 0xd9;
    if (!validStart || !validEnd) filmIssue(film, film.media.poster, 'product film poster must be a JPEG file');

    if (ffprobeAvailable) {
      const probe = probeMedia(posterPath);
      if (probe.error) {
        filmIssue(film, film.media.poster, `ffprobe failed: ${probe.error.message}`);
      } else {
        const stream = probe.value.streams?.find((candidate) => candidate.codec_type === 'video') || {};
        facts.posterCodec = stream.codec_name;
        facts.posterWidth = stream.width;
        facts.posterHeight = stream.height;
        if (facts.posterCodec !== expectedMediaContract.poster_codec) filmIssue(film, film.media.poster, `codec must be ${expectedMediaContract.poster_codec}; found ${facts.posterCodec || 'unknown'}`);
        if (facts.posterWidth !== expectedMediaContract.poster_width || facts.posterHeight !== expectedMediaContract.poster_height) filmIssue(film, film.media.poster, `dimensions must be ${expectedMediaContract.poster_width}×${expectedMediaContract.poster_height}; found ${facts.posterWidth || '?'}×${facts.posterHeight || '?'}`);
      }
    }
  }
  return facts;
}

function validateProvenance(film, facts) {
  const provenancePath = repositoryPath(film.provenance, `${film.slug} provenance`);
  if (!provenancePath) return;
  if (!existsSync(provenancePath)) {
    filmIssue(film, film.provenance, 'recording provenance does not exist');
    return;
  }

  let provenance;
  try {
    provenance = JSON.parse(readFileSync(provenancePath, 'utf8'));
  } catch (error) {
    filmIssue(film, film.provenance, `invalid provenance JSON: ${error.message}`);
    return;
  }
  const issue = (message) => filmIssue(film, film.provenance, message);
  const requireRecord = (value, name) => {
    if (!isRecord(value)) {
      issue(`${name} must be an object`);
      return {};
    }
    return value;
  };
  const requireString = (value, name) => {
    if (!isNonEmptyString(value)) issue(`${name} must be a non-empty string`);
  };
  const requireHash = (value, name) => {
    if (!isSha256(value)) issue(`${name} must be a lowercase SHA-256 hash`);
  };
  const requirePath = (value, name) => {
    if (!isSafeRepositoryPath(value)) issue(`${name} must be a safe repository-relative path`);
  };

  if (!isRecord(provenance)) {
    issue('provenance root must be an object');
    return;
  }
  if (![1, 2].includes(provenance.schema_version)) issue('schema_version must be 1 or 2');
  if (provenance.film !== film.slug) issue(`film must be ${film.slug}`);
  if (!isNonEmptyString(provenance.recorded_at) || Number.isNaN(Date.parse(provenance.recorded_at))) {
    issue('recorded_at must be a valid timestamp');
  } else if (new Date(provenance.recorded_at).toISOString() !== provenance.recorded_at) {
    issue('recorded_at must use canonical ISO-8601 UTC form');
  }

  const source = requireRecord(provenance.source, 'source');
  requireString(source.branch, 'source.branch');
  if (!isGitHead(source.head)) issue('source.head must be a lowercase 40-character Git commit hash');
  if (typeof source.dirty !== 'boolean') issue('source.dirty must be a boolean');
  if (source.dirty === false && source.patch_sha256 !== null) issue('source.patch_sha256 must be null for a clean source');
  if (source.dirty === true && !isSha256(source.patch_sha256)) issue('source.patch_sha256 must be a lowercase SHA-256 hash for a dirty source');
  if (provenance.schema_version >= 2 || source.content_sha256 !== undefined) {
    requireHash(source.content_sha256, 'source.content_sha256');
  }
  const expectedSourceExcludes = [
    'demo/one-app/films/source/',
    'demo/one-app/films/staged/',
    'demo/one-app/films/provenance/',
    'sites/marketing/assets/films/',
  ];
  if (JSON.stringify(source.patch_excludes) !== JSON.stringify(expectedSourceExcludes)) {
    issue(`source.patch_excludes must contain exactly: ${expectedSourceExcludes.join(', ')}`);
  }

  const binary = requireRecord(provenance.binary, 'binary');
  requirePath(binary.path, 'binary.path');
  requireString(binary.version, 'binary.version');
  requireHash(binary.sha256, 'binary.sha256');

  const capture = requireRecord(provenance.capture, 'capture');
  requirePath(capture.record, 'capture.record');
  requireHash(capture.record_sha256, 'capture.record_sha256');
  requireString(capture.url, 'capture.url');
  requireString(capture.browser, 'capture.browser');
  const viewport = requireRecord(capture.viewport, 'capture.viewport');
  if (viewport.width !== expectedMediaContract.width || viewport.height !== expectedMediaContract.height) issue(`capture.viewport must be ${expectedMediaContract.width}×${expectedMediaContract.height}`);
  if (viewport.device_scale_factor !== 1) issue('capture.viewport.device_scale_factor must be 1');
  if (!['light', 'dark'].includes(capture.theme)) issue('capture.theme must be light or dark');
  if (!['reduce', 'no-preference'].includes(capture.reduced_motion)) issue('capture.reduced_motion must be reduce or no-preference');
  if (!Array.isArray(capture.keyframes) || capture.keyframes.length !== film.beats.length) {
    issue(`capture.keyframes must contain exactly ${film.beats.length} entries`);
  } else {
    const expectedBeats = film.beats.map((beat) => beat.id);
    const actualBeats = [];
    const keyframeHashes = new Map();
    for (const [index, keyframe] of capture.keyframes.entries()) {
      if (!isRecord(keyframe)) {
        issue(`capture.keyframes[${index}] must be an object`);
        continue;
      }
      actualBeats.push(keyframe.beat);
      requirePath(keyframe.path, `capture.keyframes[${index}].path`);
      requireHash(keyframe.sha256, `capture.keyframes[${index}].sha256`);
      if (isSha256(keyframe.sha256)) {
        const previousBeat = keyframeHashes.get(keyframe.sha256);
        if (previousBeat) issue(`capture beats ${previousBeat} and ${keyframe.beat || index} use the same keyframe`);
        keyframeHashes.set(keyframe.sha256, keyframe.beat || String(index));
      }
    }
    if (actualBeats.join('\n') !== expectedBeats.join('\n')) issue('capture.keyframes must match manifest beats in order');
  }

  const edit = requireRecord(provenance.edit, 'edit');
  requirePath(edit.timeline, 'edit.timeline');
  requireHash(edit.timeline_sha256, 'edit.timeline_sha256');
  requirePath(edit.stage_record, 'edit.stage_record');
  requireHash(edit.stage_record_sha256, 'edit.stage_record_sha256');
  if (!Number.isInteger(edit.poster_frame) || edit.poster_frame <= 0) issue('edit.poster_frame must be a positive integer');
  if (edit.poster_beat !== film.poster_beat) issue(`edit.poster_beat must be ${film.poster_beat}`);
  requireHash(edit.poster_source_sha256, 'edit.poster_source_sha256');
  if (facts.posterSha256 && edit.poster_source_sha256 !== facts.posterSha256) {
    issue('edit.poster_source_sha256 does not match the shipped poster');
  }
  if (!Number.isInteger(edit.frame_count) || edit.frame_count <= 0) issue('edit.frame_count must be a positive integer');
  if (Number.isInteger(edit.frame_count) && Number.isInteger(edit.poster_frame) && edit.poster_frame > edit.frame_count) {
    issue('edit.poster_frame must be inside edit.frame_count');
  }
  requireHash(edit.sequence_sha256, 'edit.sequence_sha256');

  const media = requireRecord(provenance.media, 'media');
  const mp4 = requireRecord(media.mp4, 'media.mp4');
  if (mp4.path !== film.media.mp4) issue(`media.mp4.path must be ${film.media.mp4}`);
  requireHash(mp4.sha256, 'media.mp4.sha256');
  if (facts.mp4Sha256 && mp4.sha256 !== facts.mp4Sha256) issue('media.mp4.sha256 does not match the shipped MP4');
  if (!Number.isFinite(mp4.duration_seconds)
      || mp4.duration_seconds < film.duration_seconds.min - 0.05
      || mp4.duration_seconds > film.duration_seconds.max + 0.05) issue('media.mp4.duration_seconds is outside the manifest bounds');
  if (Number.isFinite(facts.duration) && Math.abs(mp4.duration_seconds - facts.duration) > 0.05) issue('media.mp4.duration_seconds does not match the shipped MP4');
  if (mp4.codec !== expectedMediaContract.video_codec) issue(`media.mp4.codec must be ${expectedMediaContract.video_codec}`);
  if (mp4.width !== expectedMediaContract.width || mp4.height !== expectedMediaContract.height) issue(`media.mp4 dimensions must be ${expectedMediaContract.width}×${expectedMediaContract.height}`);
  if (mp4.pixel_format !== expectedMediaContract.pixel_format) issue(`media.mp4.pixel_format must be ${expectedMediaContract.pixel_format}`);
  if (mp4.fps !== `${expectedMediaContract.output_fps}/1`) issue(`media.mp4.fps must be ${expectedMediaContract.output_fps}/1`);
  if (mp4.audio_streams !== 0) issue('media.mp4.audio_streams must be 0');
  if (mp4.fast_start !== true) issue('media.mp4.fast_start must be true');

  const poster = requireRecord(media.poster, 'media.poster');
  if (poster.path !== film.media.poster) issue(`media.poster.path must be ${film.media.poster}`);
  requireHash(poster.sha256, 'media.poster.sha256');
  if (facts.posterSha256 && poster.sha256 !== facts.posterSha256) issue('media.poster.sha256 does not match the shipped poster');
  if (poster.codec !== expectedMediaContract.poster_codec) issue(`media.poster.codec must be ${expectedMediaContract.poster_codec}`);
  if (poster.width !== expectedMediaContract.poster_width || poster.height !== expectedMediaContract.poster_height) issue(`media.poster dimensions must be ${expectedMediaContract.poster_width}×${expectedMediaContract.poster_height}`);

  const evidence = requireRecord(provenance.evidence, 'evidence');
  requirePath(evidence.record, 'evidence.record');
  requireHash(evidence.record_sha256, 'evidence.record_sha256');
  if (!Array.isArray(evidence.checks) || evidence.checks.length !== film.beats.length) {
    issue(`evidence.checks must contain exactly ${film.beats.length} entries`);
  } else {
    const checkIds = new Set();
    for (const [index, check] of evidence.checks.entries()) {
      if (!isRecord(check)) {
        issue(`evidence.checks[${index}] must be an object`);
        continue;
      }
      if (check.id !== film.beats[index].id) issue(`evidence.checks[${index}].id must be ${film.beats[index].id}`);
      if (checkIds.has(check.id)) issue(`duplicate evidence check id ${check.id}`);
      checkIds.add(check.id);
      if (check.status !== 'passed') issue(`evidence check ${check.id || index} must have status passed`);
      requireString(check.detail, `evidence.checks[${index}].detail`);
    }
  }
  if (!isRecord(evidence.identities) || Object.keys(evidence.identities).length === 0) {
    issue('evidence.identities must be a non-empty object');
  } else {
    for (const [key, value] of Object.entries(evidence.identities)) {
      if (!isNonEmptyString(key)) issue('evidence identity keys must be non-empty');
      const validValue = isNonEmptyString(value)
        || (Array.isArray(value) && value.length > 0 && value.every((item) => isNonEmptyString(item)));
      if (!validValue) issue(`evidence.identities.${key} must be a string or non-empty string array`);
    }
  }
}

for (const film of films) {
  if (!isRecord(film) || !isNonEmptyString(film.slug)) continue;
  const facts = validateMedia(film);
  validateProvenance(film, facts);
}

if (warnings.length) {
  console.warn(`Marketing film warnings (${warnings.length})\n${warnings.map((warning) => `- ${warning}`).join('\n')}`);
}
if (errors.length) {
  console.error(`Marketing validation failed (${errors.length})\n${errors.map((error) => `- ${error}`).join('\n')}`);
  process.exit(1);
}
console.log(`Marketing validation passed: ${pages.length} pages, ${scripts.length} scripts, 12 manifest films${strictFilms ? ', strict film contract' : ''}.`);
