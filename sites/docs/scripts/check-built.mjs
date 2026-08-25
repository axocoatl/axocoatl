import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

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

const htmlFiles = walk(distRoot).filter((file) => file.endsWith('.html'));
for (const file of htmlFiles) {
  const html = fs.readFileSync(file, 'utf8');
  const pagePath = `/${path.relative(distRoot, file).replace(/index\.html$/, '').replaceAll(path.sep, '/')}`;
  for (const match of html.matchAll(/\b(?:href|src)=["']([^"']+)["']/g)) {
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

console.log(`Checked ${htmlFiles.length} built pages and the canonical favicon.`);
