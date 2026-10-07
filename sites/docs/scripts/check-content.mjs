import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const docsRoot = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const repoRoot = path.resolve(docsRoot, '../..');
const contentRoot = path.join(docsRoot, 'src/content/docs');

function walk(directory) {
  return fs.readdirSync(directory, { withFileTypes: true }).flatMap((entry) => {
    const target = path.join(directory, entry.name);
    return entry.isDirectory() ? walk(target) : [target];
  });
}

const contentFiles = walk(contentRoot).filter((file) => /\.mdx?$/.test(file));
const allContent = contentFiles.map((file) => fs.readFileSync(file, 'utf8')).join('\n');
const failures = [];

for (const asset of ['favicon.png', 'mark.png', 'wordmark.png', 'colors.json']) {
  const canonical = path.join(repoRoot, 'branding', asset);
  const mirrored = path.join(docsRoot, 'public', asset);
  if (!fs.existsSync(canonical)) failures.push(`canonical brand asset is missing: branding/${asset}`);
  if (!fs.existsSync(mirrored)) failures.push(`prebuild did not mirror public asset: public/${asset}`);
}

const benchmarkSourceRelative = 'benches/resource_footprint.rs';
const benchmarkSource = path.join(repoRoot, benchmarkSourceRelative);
const resourceGuide = fs.readFileSync(path.join(contentRoot, 'operate/resources.mdx'), 'utf8');

if (!fs.existsSync(benchmarkSource)) failures.push(`benchmark source is missing: ${benchmarkSourceRelative}`);
if (!resourceGuide.includes(`\`${benchmarkSourceRelative}\``)) {
  failures.push(`resource guide must cite ${benchmarkSourceRelative} as code text`);
}
for (const invocation of [
  'cargo bench --bench resource_footprint --',
  '--output /tmp/axocoatl-resource-footprint.json',
  '--validate /tmp/axocoatl-resource-footprint.json',
]) {
  if (!resourceGuide.includes(invocation)) failures.push(`resource guide is missing benchmark invocation: ${invocation}`);
}
for (const staleClaim of [
  'benchmark-results/',
  '6,640 KiB',
  '1,200 KiB',
  '12.0 KiB per actor',
]) {
  if (resourceGuide.includes(staleClaim)) failures.push(`resource guide still publishes stale host-specific evidence: ${staleClaim}`);
}

for (const stale of [
  'Starlight Starter Kit',
  'Seasoned astronaut',
  'Activity module',
  'Browser preview',
  'right-side Attempts',
  'Attempts dock',
]) {
  if (allContent.includes(stale)) failures.push(`stale public copy remains: ${stale}`);
}

// Retired positioning. A changelog page may keep its history; no other page may
// use these terms.
const retiredPositioning = [
  ['stigmergy', /stigmerg/i],
  ['pheromones', /pheromone/i],
  ['signal field', /signal[- ]field/i],
  ['swarm', /\bswarm/i],
  ['without a manager', /without a manager/i],
  ['no central orchestrator', /no central orchestrator/i],
  ['secure sandbox', /secure sandbox/i],
  ['egress control', /egress control/i],
  ['zero trust', /zero[- ]trust/i],
  ['go-to', /\bgo-to\b/i],
];
for (const file of contentFiles) {
  const relative = path.relative(contentRoot, file);
  if (/(^|\/)changelog(\/|\.mdx?$)/.test(relative)) continue;
  const source = fs.readFileSync(file, 'utf8');
  for (const [term, pattern] of retiredPositioning) {
    if (pattern.test(source)) failures.push(`${relative} uses retired positioning term: ${term}`);
  }
}

const cliSource = fs.readFileSync(path.join(repoRoot, 'axocoatl-cli/src/main.rs'), 'utf8');
const cliReference = fs.readFileSync(path.join(contentRoot, 'reference/cli.mdx'), 'utf8');

function enumBodyIn(source, name, file) {
  const match = source.match(new RegExp(`enum ${name} \\{([\\s\\S]*?)\\n\\}`));
  if (!match) throw new Error(`could not find ${name} in ${file}`);
  return match[1];
}

function enumBody(name) {
  return enumBodyIn(cliSource, name, 'axocoatl-cli/src/main.rs');
}

function variantNames(body) {
  // Struct (`Name {`), unit (`Name,`) and tuple (`Name(`) variants.
  return [...body.matchAll(/^    ([A-Z][A-Za-z0-9_]*)\s*(?:\{|,|\()/gm)]
    .map((match) => match[1].replace(/([a-z0-9])([A-Z])/g, '$1-$2').toLowerCase());
}

function variants(name) {
  return variantNames(enumBody(name));
}

for (const command of variants('Commands')) {
  if (!cliReference.includes(`axocoatl ${command}`)) {
    failures.push(`CLI reference is missing top-level command: axocoatl ${command}`);
  }
}

for (const [group, name] of [
  ['service', 'ServiceCommands'],
  ['session', 'SessionCommands'],
  ['agents', 'AgentCommands'],
  ['skills', 'SkillCommands'],
  ['mcp', 'McpCommands'],
  ['workflow', 'WorkflowCommands'],
  ['tokens', 'TokenCommands'],
  ['browser', 'BrowserCommands'],
  ['network', 'NetworkCommands'],
]) {
  for (const command of variants(name)) {
    if (!cliReference.includes(`axocoatl ${group} ${command}`)) {
      failures.push(`CLI reference is missing subcommand: axocoatl ${group} ${command}`);
    }
  }
}

// Subcommands defined outside main.rs: loadout runs, records, secrets, recipes.
for (const [group, file, name] of [
  ['loadouts', 'axocoatl-cli/src/run_cmd.rs', 'LoadoutCommands'],
  ['record', 'axocoatl-cli/src/run_cmd.rs', 'RecordCommands'],
  ['secret', 'axocoatl-cli/src/secret_cmd.rs', 'SecretCommands'],
  ['recipe', 'axocoatl-cli/src/recipe_cmd.rs', 'RecipeCommands'],
]) {
  const source = fs.readFileSync(path.join(repoRoot, file), 'utf8');
  for (const command of variantNames(enumBodyIn(source, name, file))) {
    if (!cliReference.includes(`axocoatl ${group} ${command}`)) {
      failures.push(`CLI reference is missing subcommand: axocoatl ${group} ${command}`);
    }
  }
}

const routerSource = fs.readFileSync(path.join(repoRoot, 'axocoatl-server/src/lib.rs'), 'utf8');
const httpReference = fs.readFileSync(path.join(contentRoot, 'reference/http-api.mdx'), 'utf8');
const websocketReference = fs.readFileSync(path.join(contentRoot, 'reference/websocket.mdx'), 'utf8');
const routeReference = `${httpReference}\n${websocketReference}`;
const routes = [...routerSource.matchAll(/\.route\(\s*"([^"]+)"/g)].map((match) => match[1]);

for (const route of [...new Set(routes)]) {
  if (!routeReference.includes(route)) failures.push(`HTTP reference is missing route: ${route}`);
}

const configSource = fs.readFileSync(path.join(repoRoot, 'crates/axocoatl-config/src/types.rs'), 'utf8');
const configReference = fs.readFileSync(path.join(contentRoot, 'reference/config.mdx'), 'utf8');
const rootConfig = configSource.match(/pub struct AxocoatlConfig \{([\s\S]*?)\n\}/)?.[1] || '';
const rootKeys = [...rootConfig.matchAll(/^    pub ([a-z_]+):/gm)].map((match) => match[1]);

for (const key of rootKeys) {
  if (!configReference.includes(`\`${key}\``)) failures.push(`config reference is missing root key: ${key}`);
}

const astroConfig = fs.readFileSync(path.join(docsRoot, 'astro.config.mjs'), 'utf8');
for (const section of ['Start', 'Use the workbench', 'Configure', 'Operate', 'Understand', 'Reference']) {
  if (!astroConfig.includes(`label: '${section}'`)) failures.push(`sidebar is missing section: ${section}`);
}

// Every page is reachable from the sidebar (the splash page and 404 aside).
for (const file of contentFiles) {
  const slug = path.relative(contentRoot, file).replace(/\.mdx?$/, '').split(path.sep).join('/');
  if (slug === 'index' || slug === '404') continue;
  if (!astroConfig.includes(`slug: '${slug}'`)) failures.push(`page is not in the sidebar: ${slug}`);
}

// Claims. Public copy states only measured results (BRAND.md); docs/CLAIMS.md maps
// each one to its evidence. These checks cover the public surfaces: the README,
// llms.txt, the product and architecture documents, every docs page, and the
// marketing pages other than the changelog.
function walkPublicHtml(directory) {
  return fs.readdirSync(directory, { withFileTypes: true }).flatMap((entry) => {
    const target = path.join(directory, entry.name);
    if (entry.isDirectory()) {
      if (['node_modules', 'assets', 'changelog', '_brand', 'scripts', 'components', 'styles'].includes(entry.name)) return [];
      return walkPublicHtml(target);
    }
    return entry.name.endsWith('.html') ? [target] : [];
  });
}
const publicSurfaces = [
  ...['README.md', 'llms.txt', 'docs/PRODUCT.md', 'docs/ARCHITECTURE.md']
    .map((relative) => path.join(repoRoot, relative)),
  ...contentFiles,
  ...walkPublicHtml(path.join(repoRoot, 'sites/marketing')),
].filter((file) => fs.existsSync(file));
const claimsLedgerPath = path.join(repoRoot, 'docs/CLAIMS.md');
const claimsLedger = fs.existsSync(claimsLedgerPath) ? fs.readFileSync(claimsLedgerPath, 'utf8') : '';
if (!claimsLedger) failures.push('docs/CLAIMS.md, the claims ledger, is missing');

// Claims that were withdrawn because nothing measured them.
const withdrawnClaims = [
  ['small local models as a first-class target', /first-class target/i],
];
// A measured number labeled with the harness it ran in.
// Prose wraps, so words may be separated by any whitespace.
const opusLabel = /Claude\s+Code\s+subagents/;
const notAxocoatl = /not\s+(?:through\s+)?Axocoatl|not\s+Axocoatl\s+results|outside\s+Axocoatl/i;
const unmeasured = String.raw`(?:not\s+(?:been\s+)?measured|have\s+not\s+measured)`;
const openRouterUnmeasured = new RegExp(
  String.raw`OpenRouter[\s\S]{0,400}?${unmeasured}|${unmeasured}[\s\S]{0,400}?OpenRouter`, 'i');

for (const file of publicSurfaces) {
  const relative = path.relative(repoRoot, file);
  const source = fs.readFileSync(file, 'utf8');
  for (const [claim, pattern] of withdrawnClaims) {
    if (pattern.test(source)) failures.push(`${relative} repeats a withdrawn claim: ${claim}`);
  }
  if (/\bOpus\b/.test(source) && !(opusLabel.test(source) && notAxocoatl.test(source))) {
    failures.push(`${relative} cites Claude Opus results without labeling them Claude Code subagents, not Axocoatl`);
  }
  const citesMeasurement = /measured:/.test(source) || /what-we-measured/.test(source);
  if (/OpenRouter/.test(source) && /\breview/i.test(source) && citesMeasurement
      && !openRouterUnmeasured.test(source)) {
    failures.push(`${relative} states measured review results next to OpenRouter without saying OpenRouter reviewers were not measured`);
  }
  for (const match of source.matchAll(/(?:<!--|\{\/\*)\s*measured:\s*(.+?)\s*(?:-->|\*\/\})/g)) {
    if (!claimsLedger.includes(match[1])) {
      failures.push(`${relative}: measured block "${match[1]}" has no entry in docs/CLAIMS.md`);
    }
  }
}

if (failures.length) {
  console.error(failures.map((failure) => `- ${failure}`).join('\n'));
  process.exit(1);
}

console.log(`Checked ${contentFiles.length} content files, ${routes.length} routes, ${rootKeys.length} root config keys, and claims on ${publicSurfaces.length} public files.`);
