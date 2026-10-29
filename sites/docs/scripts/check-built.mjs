import fs from 'node:fs';
import path from 'node:path';
import { createHash } from 'node:crypto';
import { fileURLToPath } from 'node:url';
import { isEmailOff } from './email-off.mjs';

const docsRoot = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const distRoot = path.join(docsRoot, 'dist');
const failures = [];

function walk(directory) {
  return fs.readdirSync(directory, { withFileTypes: true }).flatMap((entry) => {
    const target = path.join(directory, entry.name);
    return entry.isDirectory() ? walk(target) : [target];
  });
}

function targetExists(urlPath) {
  const clean = decodeURIComponent(urlPath.split(/[?#]/, 1)[0]);
  const relative = clean.replace(/^\/+/, '');
  const direct = path.join(distRoot, relative);
  return fs.existsSync(direct)
    || fs.existsSync(`${direct}.html`)
    || fs.existsSync(path.join(direct, 'index.html'));
}

if (!fs.existsSync(distRoot)) {
  console.error('dist/ is missing; run npm run build before npm run check:links');
  process.exit(1);
}

if (!fs.existsSync(path.join(distRoot, 'favicon.png'))) {
  failures.push('built favicon is missing: dist/favicon.png');
}

// Serve the recordings and tracks from the docs origin, retaining the exact
// already-verified canonical bytes. Frame decoding belongs to the media gate.
const recordings = ['dashboard-flow', 'review', 'receipt'].map((clip) => ({
  directory: 'dashboard-flow-1.5.3',
  label: clip,
  schema: 'axocoatl.dashboard-flow-provenance/1',
  files: [`${clip}.mp4`, `${clip}.jpg`, `${clip}.captions.vtt`, `${clip}.chapters.vtt`],
}));

function readMedia(file) {
  try {
    const stat = fs.lstatSync(file);
    if (!stat.isFile() || stat.isSymbolicLink()) throw new Error('expected a regular, non-symlink file');
    return fs.readFileSync(file);
  } catch (error) {
    failures.push(`${path.relative(docsRoot, file)}: ${error.message}`);
    return null;
  }
}

for (const recording of recordings) {
  const canonicalRoot = path.resolve(docsRoot, '../marketing/assets/demo', recording.directory);
  const builtRoot = path.join(distRoot, 'assets', recording.directory);
  const provenanceBytes = readMedia(path.join(canonicalRoot, 'provenance.json'));
  let provenance = null;
  if (provenanceBytes) {
    try {
      const parsed = JSON.parse(provenanceBytes.toString('utf8'));
      if (parsed.schema !== recording.schema || !Array.isArray(parsed.files)) {
        throw new Error('unexpected provenance schema or file inventory');
      }
      provenance = parsed;
    } catch (error) {
      failures.push(`canonical ${recording.label} provenance is invalid: ${error.message}`);
    }
  }

  for (const name of recording.files) {
    const source = readMedia(path.join(canonicalRoot, name));
    const built = readMedia(path.join(builtRoot, name));
    const entries = provenance?.files.filter((entry) => entry?.file === name) ?? [];
    const expected = entries[0];
    if (entries.length !== 1 || !/^[a-f0-9]{64}$/.test(expected?.sha256 ?? '')
        || !Number.isSafeInteger(expected?.bytes) || expected.bytes < 1) {
      failures.push(`canonical ${recording.label} provenance needs one valid entry for ${name}`);
      continue;
    }
    for (const [label, bytes] of [['canonical', source], ['built', built]]) {
      if (bytes && (bytes.length !== expected.bytes
          || createHash('sha256').update(bytes).digest('hex') !== expected.sha256)) {
        failures.push(`${label} ${recording.label} media differs from provenance: ${name}`);
      }
    }
    if (source && built && !source.equals(built)) {
      failures.push(`built ${recording.label} media differs from its canonical source: ${name}`);
    }
  }
}

const canonicalChapterModule = readMedia(path.resolve(docsRoot, '../marketing/components/ax-video-chapters.js'));
const builtChapterModule = readMedia(path.join(distRoot, 'assets/ax-video-chapters.js'));
if (canonicalChapterModule && builtChapterModule && !canonicalChapterModule.equals(builtChapterModule)) {
  failures.push('built chapter controls differ from the canonical marketing module: assets/ax-video-chapters.js');
}

const htmlFiles = walk(distRoot).filter((file) => file.endsWith('.html'));
for (const file of htmlFiles) {
  const html = fs.readFileSync(file, 'utf8');
  if (/FLOW_[A-Z_]+_PENDING/.test(html)) {
    failures.push(`${path.relative(docsRoot, file)} has unfinished dashboard recording integration`);
  }
  if (/<video\b[^>]*(?:dashboard-1\.5|dashboard-task-1\.5|required-review-1\.5)\//i.test(html)) {
    failures.push(`${path.relative(docsRoot, file)} embeds a retired dashboard recording`);
  }
  if (/<body\b/i.test(html) && !isEmailOff(html)) {
    failures.push(`${path.relative(docsRoot, file)} does not wrap its body in <!--email_off-->, so Cloudflare can rewrite text such as verify-receipt@v1.4.0 into "[email protected]"`);
  }
  const pagePath = `/${path.relative(distRoot, file).replace(/index\.html$/, '').replaceAll(path.sep, '/')}`;
  for (const match of html.matchAll(/\b(?:href|src|poster)=["']([^"']+)["']/g)) {
    const raw = match[1];
    if (!raw || /^(?:[a-z][a-z0-9+.-]*:|\/\/|#)/i.test(raw)) continue;
    const resolved = raw.startsWith('/') ? raw : new URL(raw, `https://docs.axocoatl.ai${pagePath}`).pathname;
    if (!targetExists(resolved)) {
      failures.push(`${path.relative(docsRoot, file)} references missing ${raw}`);
    }
  }
}

if (failures.length) {
  console.error([...new Set(failures)].map((failure) => `- ${failure}`).join('\n'));
  process.exit(1);
}

console.log(`Checked ${htmlFiles.length} built pages, the canonical favicon, and ${recordings.reduce((count, recording) => count + recording.files.length, 0)} canonical recording media files plus their shared chapter controls.`);
