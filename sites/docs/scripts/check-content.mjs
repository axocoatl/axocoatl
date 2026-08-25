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

const cliSource = fs.readFileSync(path.join(repoRoot, 'axocoatl-cli/src/main.rs'), 'utf8');
const cliReference = fs.readFileSync(path.join(contentRoot, 'reference/cli.mdx'), 'utf8');

function enumBody(name) {
  const match = cliSource.match(new RegExp(`enum ${name} \\{([\\s\\S]*?)\\n\\}`));
  if (!match) throw new Error(`could not find ${name} in CLI source`);
  return match[1];
}

function variants(name) {
  return [...enumBody(name).matchAll(/^    ([A-Z][A-Za-z0-9_]*)\s*(?:\{|,)/gm)]
    .map((match) => match[1].replace(/([a-z0-9])([A-Z])/g, '$1-$2').toLowerCase());
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
]) {
  for (const command of variants(name)) {
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

for (const section of ['Start', 'Use the workbench', 'Configure', 'Operate', 'Understand', 'Reference']) {
  const config = fs.readFileSync(path.join(docsRoot, 'astro.config.mjs'), 'utf8');
  if (!config.includes(`label: '${section}'`)) failures.push(`sidebar is missing section: ${section}`);
}

if (failures.length) {
  console.error(failures.map((failure) => `- ${failure}`).join('\n'));
  process.exit(1);
}

console.log(`Checked ${contentFiles.length} content files, ${routes.length} routes, and ${rootKeys.length} root config keys.`);
