import { cpSync, existsSync, mkdirSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { dirname, resolve } from 'node:path';
import { loadPortfolio } from '../../../demo/one-app/films/film-lib.mjs';
import { filmMode, pendingPath, renderPendingFilms, resolveOptionalDemos } from './site-mode.mjs';

const marketingRoot = resolve(import.meta.dirname, '..');
const repositoryRoot = resolve(marketingRoot, '../..');
const portfolioSource = resolve(repositoryRoot, 'demo/one-app/films/portfolio.json');
const portfolioDestination = 'assets/films/portfolio.json';
const llmsSource = resolve(repositoryRoot, 'llms.txt');
const output = resolve(process.argv[2] || resolve(marketingRoot, '.dist'));

if (output === marketingRoot || !output) throw new Error('Refusing to replace the marketing source directory.');
if (!existsSync(portfolioSource)) throw new Error('Missing authoritative film portfolio: demo/one-app/films/portfolio.json');
if (!existsSync(llmsSource)) throw new Error('Missing public AI-readable product narrative: llms.txt');

// When demo/one-app/films/PENDING names the CLI version, this version's films are
// not recorded. The site is then built without them: each film placement becomes a
// static note (or is omitted), and no film media or portfolio ships.
const { version, pending: filmsPending } = filmMode(repositoryRoot);

let portfolio;
try {
  portfolio = loadPortfolio();
} catch (error) {
  throw new Error(`Could not validate the authoritative film portfolio: ${error.message}`);
}
if (!Array.isArray(portfolio.films) || portfolio.films.length !== 12) {
  throw new Error(`The authoritative film portfolio must contain exactly 12 films; found ${portfolio.films?.length ?? 'none'}.`);
}

function marketingRelativePath(repositoryPath, label) {
  const prefix = 'sites/marketing/';
  if (typeof repositoryPath !== 'string' || !repositoryPath.startsWith(prefix)) {
    throw new Error(`${label} must be a repository-relative path below sites/marketing/.`);
  }
  const relativePath = repositoryPath.slice(prefix.length);
  if (!relativePath || relativePath.split('/').includes('..')) {
    throw new Error(`${label} is not a safe marketing path: ${repositoryPath}`);
  }
  return relativePath;
}

const filmFiles = portfolio.films.flatMap((film) => {
  if (!film || typeof film !== 'object' || !film.media || typeof film.media !== 'object') {
    throw new Error('Every portfolio film must declare a media object.');
  }
  return [
    marketingRelativePath(film.media.mp4, `${film.slug || 'film'} media.mp4`),
    marketingRelativePath(film.media.poster, `${film.slug || 'film'} media.poster`),
  ];
});
if (new Set(filmFiles).size !== 24) {
  throw new Error('The 12-film portfolio must resolve to 24 unique MP4/poster build inputs.');
}

const pages = [
  'index.html', '404.html',
  'changelog/index.html', 'concepts/index.html', 'install/index.html',
  'integrations/openrouter/index.html', 'pricing/index.html',
  'showcase/index.html', 'why/index.html',
];
const files = [
  'robots.txt', 'sitemap.xml',
  'assets/colors.json', 'assets/favicon.png', 'assets/og-home.png',
  ...(filmsPending ? [] : filmFiles),
  'components/ax-site-nav.js', 'components/ax-footer.js',
  'components/ax-theme-toggle.js', 'components/ax-cli-snippet.js',
  'components/ax-comparison-row.js', 'components/ax-product-film.js',
  'styles/tokens.css', 'styles/base.css', 'styles/finder.css',
];

rmSync(output, { recursive: true, force: true });
mkdirSync(output, { recursive: true });

function copyInput(file) {
  const source = resolve(marketingRoot, file);
  if (!existsSync(source)) throw new Error(`Missing build input: ${file}`);
  const destination = resolve(output, file);
  mkdirSync(dirname(destination), { recursive: true });
  cpSync(source, destination);
}

const demoFiles = new Set();
const pendingNotes = [];
const pendingOmitted = [];
for (const page of pages) {
  const source = resolve(marketingRoot, page);
  if (!existsSync(source)) throw new Error(`Missing build input: ${page}`);
  const demos = resolveOptionalDemos(readFileSync(source, 'utf8'), marketingRoot, page);
  if (demos.errors.length) throw new Error(demos.errors.join('\n'));
  for (const file of demos.files) demoFiles.add(file);
  let html = demos.html;
  if (filmsPending) {
    const rendered = renderPendingFilms(html, version);
    html = rendered.html;
    pendingNotes.push(...rendered.notes.map((slug) => `${page}#${slug}`));
    pendingOmitted.push(...rendered.omitted.map((slug) => `${page}#${slug}`));
  }
  const destination = resolve(output, page);
  mkdirSync(dirname(destination), { recursive: true });
  writeFileSync(destination, html);
}
for (const file of [...files, ...demoFiles]) copyInput(file);

if (!filmsPending) {
  const builtPortfolio = resolve(output, portfolioDestination);
  mkdirSync(dirname(builtPortfolio), { recursive: true });
  cpSync(portfolioSource, builtPortfolio);
}
cpSync(llmsSource, resolve(output, 'llms.txt'));

const total = pages.length + files.length + demoFiles.size;
const demoSummary = demoFiles.size ? `, ${demoFiles.size} optional demo file(s)` : ', no optional demo media';
if (filmsPending) {
  console.log(
    `Built ${total} marketing files and llms.txt in ${output}${demoSummary}. ` +
    `Films for ${version} are pending (${pendingPath}): ${pendingNotes.length} film placement(s) shown as static notes, ` +
    `${pendingOmitted.length} omitted; no film media or portfolio shipped.`,
  );
} else {
  console.log(`Built ${total} marketing files, llms.txt, and the authoritative 12-film portfolio in ${output}${demoSummary}`);
}
