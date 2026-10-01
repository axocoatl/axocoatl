// Shared by build.mjs and validate.mjs: decide whether this version's films are
// pending, render a page without its films, and resolve optional demo slots.
import { existsSync, lstatSync, readFileSync } from 'node:fs';
import { resolve } from 'node:path';

export const marketingRoot = resolve(import.meta.dirname, '..');
export const repositoryRoot = resolve(marketingRoot, '../..');
export const pendingPath = 'demo/one-app/films/PENDING';

// The CLI product version, read the way scripts/verify-film-gate.sh reads it.
export function cliVersion(root = repositoryRoot) {
  const manifest = readFileSync(resolve(root, 'axocoatl-cli/Cargo.toml'), 'utf8');
  let inPackage = false;
  for (const line of manifest.split(/\r?\n/)) {
    if (line === '[package]') {
      inPackage = true;
      continue;
    }
    if (inPackage && line.startsWith('[')) break;
    const match = inPackage && line.match(/^\s*version\s*=\s*"([^"]*)"/);
    if (match && match[1]) return match[1];
  }
  throw new Error('could not read the axocoatl-cli product version');
}

// The version demo/one-app/films/PENDING declares, or null. Mirrors the shell
// gate: a regular non-symlink file whose content, without whitespace, is not empty.
export function pendingVersion(root = repositoryRoot) {
  const path = resolve(root, pendingPath);
  if (!existsSync(path)) return null;
  const stat = lstatSync(path);
  if (!stat.isFile() || stat.isSymbolicLink()) return null;
  const content = readFileSync(path, 'utf8').replace(/\s+/g, '');
  return content || null;
}

// Films are pending when PENDING names the current CLI version.
export function filmMode(root = repositoryRoot) {
  const version = cliVersion(root);
  return { version, pending: pendingVersion(root) === version };
}

function escapeHtml(value) {
  return value
    .replaceAll('&', '&amp;')
    .replaceAll('<', '&lt;')
    .replaceAll('>', '&gt;')
    .replaceAll('"', '&quot;');
}

export function filmAttributes(source) {
  const attributes = {};
  for (const match of source.matchAll(/([a-z][a-z0-9-]*)(?:="([^"]*)")?/gi)) {
    attributes[match[1].toLowerCase()] = match[2] ?? '';
  }
  return attributes;
}

export const filmElementPattern = /<ax-product-film\b([^>]*)>\s*<\/ax-product-film>/gi;

// Replace every product film with a static note that names what the film will
// show, or remove it when the element says pending="omit". The note carries no
// media and no link.
export function renderPendingFilms(html, version) {
  const notes = [];
  const omitted = [];
  const rendered = html.replace(filmElementPattern, (_element, source) => {
    const attributes = filmAttributes(source);
    const slug = attributes.film || '';
    if (attributes.pending === 'omit') {
      omitted.push(slug);
      return '';
    }
    notes.push(slug);
    const label = escapeHtml(attributes.label || '');
    const caption = escapeHtml(attributes.caption || '');
    return [
      `<aside class="film-pending" data-film="${escapeHtml(slug)}" aria-label="Film not yet recorded: ${label}">`,
      `<span class="film-pending-kicker">Film · ${escapeHtml(version)} recording pending</span>`,
      `<p class="film-pending-title">${label}</p>`,
      `<p class="film-pending-caption">${caption}</p>`,
      '</aside>',
    ].join('');
  });
  return { html: rendered, notes, omitted };
}

// An optional demo slot renders only when every file it names exists:
//   <!-- optional-demo: assets/demo/a.mp4 assets/demo/a.jpg -->
//   ...markup...
//   <!-- /optional-demo -->
// When all files exist the markup stays (markers removed) and the files are
// returned for the build to copy; when none exist the slot is removed. A slot
// with only some of its files is an error.
const optionalDemoPattern = /<!--\s*optional-demo:([^>]*?)-->([\s\S]*?)<!--\s*\/optional-demo\s*-->/g;

export function resolveOptionalDemos(html, root, label = 'page') {
  const files = [];
  const errors = [];
  let slots = 0;
  let renderedSlots = 0;
  const rendered = html.replace(optionalDemoPattern, (_slot, declared, markup) => {
    slots += 1;
    const paths = declared.trim().split(/\s+/).filter(Boolean);
    if (!paths.length || paths.some((path) => path.startsWith('/') || path.split('/').includes('..'))) {
      errors.push(`${label}: optional demo slot must name safe marketing-relative files`);
      return '';
    }
    const present = paths.filter((path) => existsSync(resolve(root, path)));
    if (present.length === 0) return '';
    if (present.length !== paths.length) {
      const missing = paths.filter((path) => !present.includes(path));
      errors.push(`${label}: optional demo slot has ${present.join(', ')} but is missing ${missing.join(', ')}`);
      return '';
    }
    renderedSlots += 1;
    files.push(...paths);
    return markup;
  });
  if (/<!--\s*\/?optional-demo\b/.test(rendered)) errors.push(`${label}: unbalanced optional demo slot markers`);
  return { html: rendered, files, errors, slots, renderedSlots };
}
