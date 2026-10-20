#!/usr/bin/env node
// Every command a new user copies from the README, llms.txt, the marketing site or the
// docs' first-run pages must run as written, and the claims around them must be ones
// the project can stand behind. This lint extracts the shell commands and YAML snippets
// from those pages and refuses:
//
// - a <br> inside <code>, <pre> or <ax-cli-snippet> (the copied text breaks the
//   command in two);
// - a copy-button command with a newline or a placeholder (`…`, `<name>`);
// - a shell line that starts with a flag, a trailing `\` with nothing after it, an
//   unbalanced quote, or `axocoatl <subcommand>` the CLI does not have;
// - `--keys FILE` when no earlier command on the page creates FILE;
// - YAML that does not parse, an Axocoatl configuration snippet whose Agents lack
//   an `id` or `name` (with --axocoatl BIN, `BIN validate` / `BIN loadouts validate`
//   must also accept it), and a GitHub workflow without `on:` or `jobs:`, which
//   GitHub refuses as a workflow file;
// - "stronger" within five lines of `gpt-oss:20b` outside a `measured:` block (only
//   the measured gpt-oss 120B reviewer was stronger; the 48 GB default is smaller than
//   its writer and unmeasured);
// - `gpt-oss:120b` without its "96 GB" memory note nearby;
// - claims withdrawn for 1.5.0 ("nothing leaves your machine", "keeps working if the
//   project stops", "from a shell or a CI job", "every finished run gets a receipt");
// - on the install surfaces, WSL2, Fedora, Debian 12 or Ubuntu 22.04 named without
//   "not yet verified" nearby.
//
// A snippet that is deliberately partial carries `copy-paste: excerpt` in a comment on
// one of the three lines above it (`{/* copy-paste: excerpt */}` in MDX,
// `<!-- copy-paste: excerpt -->` in HTML and Markdown).
//
// Usage: node scripts/check-copy-paste.mjs [--root DIR] [--axocoatl BIN] [--json]
//   --json prints every extracted block (file, line, kind, text, commands) and the
//   problems as one JSON document, for a gate that runs the commands verbatim.

import { spawnSync } from 'node:child_process';
import { existsSync, mkdtempSync, readFileSync, readdirSync, realpathSync, rmSync, writeFileSync } from 'node:fs';
import { createRequire } from 'node:module';
import { tmpdir } from 'node:os';
import { dirname, join, relative, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const scriptDirectory = dirname(fileURLToPath(import.meta.url));
const defaultRoot = resolve(scriptDirectory, '..');

// The pages a new user copies first-run commands from, relative to the root.
export const COMMAND_SURFACES = [
  'README.md',
  'llms.txt',
  'sites/docs/src/content/docs/index.mdx',
  'sites/docs/src/content/docs/start/*.mdx',
  'sites/docs/src/content/docs/configure/external-agents.mdx',
  'sites/docs/src/content/docs/configure/cross-model-review.mdx',
  'sites/docs/src/content/docs/configure/loadouts.mdx',
  'sites/docs/src/content/docs/workbench/receipts.mdx',
  'sites/docs/src/content/docs/workbench/keep-as-pr.mdx',
  'sites/docs/src/content/docs/workbench/fix-qa-audit.mdx',
  'sites/docs/src/content/docs/operate/uninstall.mdx',
  'sites/marketing/**/*.html',
];

// Pages whose platform claims a new user reads before installing.
export const PLATFORM_SURFACES = [
  'README.md',
  'llms.txt',
  'sites/marketing/index.html',
  'sites/marketing/install/index.html',
  'sites/docs/src/content/docs/start/install.mdx',
];

const SKIPPED_DIRECTORIES = new Set(['node_modules', 'assets', 'changelog', '_brand', 'scripts', 'components', 'styles']);

const SHELL_LANGUAGES = new Set(['sh', 'bash', 'shell', 'zsh', 'console']);
const YAML_LANGUAGES = new Set(['yaml', 'yml']);

// First words that make an HTML <pre> a shell block.
const SHELL_COMMANDS = new Set([
  'axocoatl', 'cd', 'curl', 'podman', 'git', 'gh', 'ollama', 'mkdir', 'export', 'brew',
  'cargo', 'sudo', 'wsl', 'sh', 'bash', 'npm', 'node', 'cp', 'rm', 'ls', 'cat', 'echo',
  'command', 'tail', 'launchctl', 'systemctl', 'journalctl', 'xargs', 'case', 'esac',
]);

const WITHDRAWN = [
  [/nothing (?:ever )?leaves (?:your|the) (?:machine|computer)/i, '"nothing leaves your machine": the first daemon start downloads an embedding model, and hosted writers and reviewers send code out'],
  [/\bfully offline\b/i, '"fully offline": hosted providers and downloads use the network'],
  [/stopped tomorrow|keep(?:s)? (?:on )?working if the project stops/i, 'the "keeps working if the project stops" claim: a new major Ollama release needs a newer Axocoatl'],
  [/from a shell or (?:a )?CI(?: job)?\b/i, '"from a shell or a CI job": no hosted CI runner has run axocoatl run; say "from your shell"'],
  [/\bevery finished run\b/i, '"every finished run": only axocoatl run gets a receipt, workbench turns do not'],
];

const UNVERIFIED_PLATFORMS = [
  [/\bWSL ?2\b/, 'WSL2'],
  [/\bFedora\b/, 'Fedora'],
  [/\bDebian 12\b/, 'Debian 12'],
  [/\bUbuntu 22\.04\b/, 'Ubuntu 22.04'],
];

// ---------------------------------------------------------------------------
// Files

function globToRegExp(pattern) {
  let source = '';
  for (let index = 0; index < pattern.length; index += 1) {
    const character = pattern[index];
    if (character === '*' && pattern[index + 1] === '*') {
      source += '(?:.*/)?';
      index += pattern[index + 2] === '/' ? 2 : 1;
    } else if (character === '*') {
      source += '[^/]*';
    } else {
      source += character.replace(/[.+?^${}()|[\]\\]/g, '\\$&');
    }
  }
  return new RegExp(`^${source}$`);
}

function walk(root, directory = root) {
  if (!existsSync(directory)) return [];
  return readdirSync(directory, { withFileTypes: true }).flatMap((entry) => {
    const target = join(directory, entry.name);
    if (entry.isDirectory()) return SKIPPED_DIRECTORIES.has(entry.name) ? [] : walk(root, target);
    return [relative(root, target).split('\\').join('/')];
  });
}

export function expandSurfaces(root, patterns) {
  const files = new Set();
  const candidates = [
    ...walk(root, join(root, 'sites/marketing')),
    ...walk(root, join(root, 'sites/docs/src/content/docs')),
    ...['README.md', 'llms.txt'].filter((file) => existsSync(join(root, file))),
  ];
  for (const pattern of patterns) {
    const matcher = globToRegExp(pattern);
    for (const candidate of candidates) if (matcher.test(candidate)) files.add(candidate);
  }
  return [...files].sort();
}

// ---------------------------------------------------------------------------
// Extraction

const ENTITIES = { amp: '&', lt: '<', gt: '>', quot: '"', apos: "'", nbsp: ' ', rarr: '→', larr: '←', hellip: '…', middot: '·' };

export function htmlText(html) {
  return html
    .replace(/<!--[\s\S]*?-->/g, '')
    .replace(/<[^>]+>/g, '')
    .replace(/&(#x?[0-9a-f]+|[a-z]+);/gi, (match, entity) => {
      if (entity[0] === '#') {
        const code = entity[1].toLowerCase() === 'x' ? parseInt(entity.slice(2), 16) : parseInt(entity.slice(1), 10);
        return Number.isFinite(code) ? String.fromCodePoint(code) : match;
      }
      return ENTITIES[entity.toLowerCase()] ?? match;
    });
}

function lineAt(source, index) {
  let line = 1;
  for (let cursor = 0; cursor < index; cursor += 1) if (source.charCodeAt(cursor) === 10) line += 1;
  return line;
}

function hasExcerptMarker(lines, fenceLineIndex) {
  for (let index = fenceLineIndex - 1, seen = 0; index >= 0 && seen < 3; index -= 1) {
    if (!lines[index].trim()) continue;
    seen += 1;
    if (/copy-paste:\s*excerpt/.test(lines[index])) return true;
  }
  return false;
}

// Fenced code blocks of a Markdown or MDX file, with the line their text starts on.
export function markdownBlocks(file, source) {
  const lines = source.split(/\r?\n/);
  const blocks = [];
  for (let index = 0; index < lines.length; index += 1) {
    const open = lines[index].match(/^(\s*)(`{3,}|~{3,})\s*([^\s`]*)(.*)$/);
    if (!open) continue;
    const [, indent, fence, language] = open;
    const body = [];
    let close = index + 1;
    for (; close < lines.length; close += 1) {
      const candidate = lines[close];
      if (new RegExp(`^\\s*${fence[0] === '`' ? '`' : '~'}{${fence.length},}\\s*$`).test(candidate)) break;
      body.push(candidate.startsWith(indent) ? candidate.slice(indent.length) : candidate.trimStart());
    }
    const lang = language.toLowerCase();
    const kind = SHELL_LANGUAGES.has(lang) ? 'shell' : YAML_LANGUAGES.has(lang) ? 'yaml' : 'text';
    blocks.push({ file, line: index + 2, kind, text: body.join('\n'), excerpt: hasExcerptMarker(lines, index), raw: body.join('\n') });
    index = close;
  }
  return blocks;
}

function looksLikeShell(text) {
  let commands = 0;
  for (const line of logicalLines(text)) {
    const trimmed = line.text.trim();
    if (!trimmed || trimmed.startsWith('#')) continue;
    const first = trimmed.replace(/^(?:[A-Z_][A-Z0-9_]*=\S*\s+)+/, '').split(/\s+/)[0];
    if (!SHELL_COMMANDS.has(first) && !/^[A-Z_][A-Z0-9_]*=/.test(trimmed)) return false;
    commands += 1;
  }
  return commands > 0;
}

// Copy buttons, <pre> blocks and inline <code> of an HTML page.
export function htmlBlocks(file, source) {
  const blocks = [];
  for (const match of source.matchAll(/<ax-cli-snippet\b[^>]*>([\s\S]*?)<\/ax-cli-snippet>/gi)) {
    blocks.push({ file, line: lineAt(source, match.index), kind: 'snippet', text: htmlText(match[1]).trim(), raw: match[1], excerpt: false });
  }
  for (const match of source.matchAll(/<pre\b[^>]*>([\s\S]*?)<\/pre>/gi)) {
    const before = source.slice(Math.max(0, match.index - 700), match.index);
    const head = [...before.matchAll(/<div class="code-pane-head">([\s\S]*?)<\/div>/g)].pop()?.[1] || '';
    const headText = htmlText(head);
    const text = htmlText(match[1]).replace(/^\n/, '');
    const excerpt = /copy-paste:\s*excerpt/.test(before.slice(-300)) || /\bexcerpt\b/i.test(headText);
    let kind = 'text';
    if (/\.ya?ml\b/i.test(headText)) kind = 'yaml';
    else if (looksLikeShell(text)) kind = 'shell';
    blocks.push({ file, line: lineAt(source, match.index), kind, text, raw: match[1], excerpt, head: headText });
  }
  for (const match of source.matchAll(/<code\b[^>]*>([\s\S]*?)<\/code>/gi)) {
    blocks.push({ file, line: lineAt(source, match.index), kind: 'inline', text: htmlText(match[1]), raw: match[1], excerpt: false });
  }
  return blocks;
}

// ---------------------------------------------------------------------------
// Shell

// Lines joined across a trailing backslash, each with the line it starts on.
export function logicalLines(text) {
  const physical = text.split('\n');
  const result = [];
  for (let index = 0; index < physical.length; index += 1) {
    let current = physical[index];
    const start = index;
    let dangling = false;
    while (/\\$/.test(current)) {
      if (index + 1 >= physical.length || !physical[index + 1].trim()) {
        dangling = true;
        break;
      }
      index += 1;
      current = `${current.slice(0, -1).trimEnd()} ${physical[index].trim()}`;
    }
    result.push({ text: current, offset: start, dangling, trailingSpace: /\\[ \t]+$/.test(physical[start]) });
  }
  return result;
}

// Words of one shell line (quotes removed), or an error for unbalanced quotes.
export function shellWords(line) {
  const words = [];
  let word = '';
  let inWord = false;
  let state = 'none';
  for (let index = 0; index < line.length; index += 1) {
    const character = line[index];
    if (state === 'single') {
      if (character === "'") state = 'none';
      else word += character;
      continue;
    }
    if (state === 'double') {
      if (character === '\\' && index + 1 < line.length) {
        word += line[index + 1];
        index += 1;
      } else if (character === '"') state = 'none';
      else word += character;
      continue;
    }
    if (character === '#' && !inWord) break;
    if (/\s/.test(character)) {
      if (inWord) words.push(word);
      word = '';
      inWord = false;
      continue;
    }
    inWord = true;
    if (character === "'") state = 'single';
    else if (character === '"') state = 'double';
    else if (character === '\\' && index + 1 < line.length) {
      word += line[index + 1];
      index += 1;
    } else word += character;
  }
  if (state !== 'none') return { words, error: `unbalanced ${state} quote` };
  if (inWord) words.push(word);
  return { words };
}

export function axocoatlSubcommands(root) {
  const path = join(root, 'axocoatl-cli/src/main.rs');
  if (!existsSync(path)) return null;
  const body = readFileSync(path, 'utf8').match(/enum Commands \{([\s\S]*?)\n\}/)?.[1];
  if (!body) return null;
  return new Set([...body.matchAll(/^    ([A-Z][A-Za-z0-9_]*)\s*(?:\{|,|\()/gm)]
    .map((match) => match[1].replace(/([a-z0-9])([A-Z])/g, '$1-$2').toLowerCase()));
}

function createdFiles(words) {
  const created = [];
  for (let index = 0; index < words.length; index += 1) {
    const word = words[index];
    if ((word === '>' || word === '>>' || word === '-o' || word === '--output') && words[index + 1]) created.push(words[index + 1]);
    else if (/^>>?./.test(word)) created.push(word.replace(/^>>?/, ''));
    if (word === 'tee') {
      for (let next = index + 1; next < words.length && !['|', '&&', ';'].includes(words[next]); next += 1) {
        if (!words[next].startsWith('-')) created.push(words[next]);
      }
    }
    if (word === 'cp' && words.length > index + 2) {
      let last = index + 1;
      while (last + 1 < words.length && !['|', '&&', ';'].includes(words[last + 1])) last += 1;
      created.push(words[last]);
    }
  }
  return created;
}

function isPlaceholder(path) {
  return /[<>…]/.test(path) || /^[A-Z][A-Z_]*$/.test(path) || path.startsWith('$') || path === '-';
}

// Problems in the shell commands of one file, in reading order.
export function checkShell(blocks, subcommands) {
  const problems = [];
  const created = new Set();
  for (const block of blocks) {
    if (block.kind !== 'shell' && block.kind !== 'snippet') continue;
    const at = (offset) => `${block.file}:${block.line + offset}`;
    if (block.kind === 'snippet') {
      if (block.text.includes('\n')) problems.push(`${at(0)}: a copy button holds more than one line; it copies a newline into the command`);
      if (/…|<[^>]+>/.test(block.text)) problems.push(`${at(0)}: a copy button holds a placeholder; it must run as written: ${block.text}`);
    }
    const commands = [];
    for (const line of logicalLines(block.text)) {
      const trimmed = line.text.trim();
      if (line.trailingSpace) problems.push(`${at(line.offset)}: a backslash followed by spaces does not continue the line`);
      if (line.dangling) problems.push(`${at(line.offset)}: a line ends in a backslash with no command line after it`);
      if (!trimmed || trimmed.startsWith('#')) continue;
      if (block.file.endsWith('.mdx') || block.file.endsWith('.md') || block.file.endsWith('.txt')) {
        if (/^\$ /.test(trimmed)) continue; // a transcript, not a command
      }
      commands.push(trimmed);
      if (/^-/.test(trimmed)) {
        problems.push(`${at(line.offset)}: this line starts with a flag, so a pasted copy runs it as its own command: ${trimmed}`);
        continue;
      }
      const { words, error } = shellWords(trimmed);
      if (error) {
        problems.push(`${at(line.offset)}: ${error}: ${trimmed}`);
        continue;
      }
      for (let index = 0; index < words.length; index += 1) {
        if (words[index] !== 'axocoatl') continue;
        const previous = words[index - 1];
        if (index > 0 && !['|', '&&', '||', ';', 'then', 'sudo', 'exec', 'time', '$(', 'xargs'].includes(previous) && !/^[A-Z_][A-Z0-9_]*=/.test(previous)) continue;
        const subcommand = words[index + 1];
        if (subcommands && subcommand && !subcommand.startsWith('-') && !subcommands.has(subcommand)) {
          problems.push(`${at(line.offset)}: axocoatl has no subcommand '${subcommand}': ${trimmed}`);
        }
      }
      for (let index = 0; index < words.length; index += 1) {
        if (words[index] !== '--keys') continue;
        const path = words[index + 1];
        if (!path || isPlaceholder(path) || created.has(path)) continue;
        problems.push(`${at(line.offset)}: --keys ${path}, but no earlier command on this page creates ${path} (use --key "$(axocoatl receipt key)", or create the file first)`);
      }
      for (const path of createdFiles(words)) created.add(path);
    }
    block.commands = commands;
  }
  return problems;
}

// ---------------------------------------------------------------------------
// HTML <br> in code

export function checkBreaks(blocks) {
  const problems = [];
  for (const block of blocks) {
    if (!block.raw || !['snippet', 'shell', 'yaml', 'text', 'inline'].includes(block.kind)) continue;
    if (/<br\b/i.test(block.raw)) {
      problems.push(`${block.file}:${block.line}: <br> inside a code element or copy button; copying it splits the command`);
    }
  }
  return problems;
}

// ---------------------------------------------------------------------------
// YAML

function loadYamlParser() {
  for (const base of [join(scriptDirectory, '../sites/docs/package.json'), join(scriptDirectory, 'package.json')]) {
    try {
      const require = createRequire(base);
      return require('js-yaml');
    } catch {
      // try the next location
    }
  }
  return null;
}

export function classifyYaml(document) {
  if (!document || typeof document !== 'object' || Array.isArray(document)) return 'other';
  if (typeof document.schema === 'string' && document.schema.startsWith('axocoatl.loadout/')) return 'loadout';
  if ('jobs' in document || 'on' in document || true in document) return 'workflow';
  if (['params', 'checks', 'review', 'budgets', 'prompt', 'environment', 'kind'].some((key) => key in document)) return 'loadout-fragment';
  if (['agents', 'providers', 'server', 'sandbox', 'run', 'credentials', 'mcp_servers', 'workflows'].some((key) => key in document)) return 'config';
  return 'other';
}

export function checkYaml(blocks, { yaml, binary }) {
  const problems = [];
  for (const block of blocks) {
    if (block.kind !== 'yaml') continue;
    const at = `${block.file}:${block.line}`;
    if (!yaml) {
      problems.push(`${at}: cannot check YAML: js-yaml is not installed (run npm ci in sites/docs)`);
      continue;
    }
    let document;
    try {
      document = yaml.load(block.text);
    } catch (error) {
      problems.push(`${at}: YAML does not parse: ${String(error.message).split('\n')[0]}`);
      continue;
    }
    const kind = classifyYaml(document);
    block.yamlKind = kind;
    if (kind === 'config' && !block.excerpt) {
      for (const [index, agent] of (Array.isArray(document.agents) ? document.agents : []).entries()) {
        for (const field of ['id', 'name']) {
          if (!agent || typeof agent !== 'object' || !(field in agent)) {
            problems.push(`${at}: agents[${index}] has no ${field}, so axocoatl validate refuses it; add it or mark the block copy-paste: excerpt`);
          }
        }
      }
    }
    if (kind === 'loadout') {
      for (const field of ['id', 'version', 'kind', 'agents']) {
        if (!(field in document)) problems.push(`${at}: a loadout without ${field}`);
      }
    }
    if (kind === 'workflow' && !block.excerpt) {
      // js-yaml reads `on` as a string key; a YAML 1.1 reader would make it true.
      const missing = [['on', 'on' in document || true in document], ['jobs', 'jobs' in document]]
        .filter(([, present]) => !present).map(([field]) => `${field}:`);
      if (missing.length) {
        problems.push(`${at}: a GitHub workflow without ${missing.join(' or ')}, which GitHub refuses as a workflow file; show the whole file or mark the block copy-paste: excerpt`);
      }
    }
    if (binary && !block.excerpt && (kind === 'config' || kind === 'loadout')) {
      const problem = validateWithBinary(binary, kind, block.text);
      if (problem) problems.push(`${at}: ${problem}`);
    }
  }
  return problems;
}

function validateWithBinary(binary, kind, text) {
  const directory = mkdtempSync(join(tmpdir(), 'axocoatl-copy-paste-'));
  try {
    const file = join(directory, kind === 'loadout' ? 'loadout.yaml' : 'config.yaml');
    writeFileSync(file, `${text}\n`);
    const home = join(directory, 'home');
    const args = kind === 'loadout' ? ['loadouts', 'validate', file] : ['validate', file];
    const result = spawnSync(binary, args, {
      encoding: 'utf8',
      cwd: directory,
      env: { PATH: process.env.PATH, HOME: home, XDG_CONFIG_HOME: join(home, '.config'), XDG_DATA_HOME: join(home, '.local/share'), NO_COLOR: '1' },
      timeout: 60_000,
    });
    if (result.error) return `could not run ${binary}: ${result.error.message}`;
    if (result.status !== 0) {
      return `axocoatl ${args.slice(0, -1).join(' ')} refuses it (exit ${result.status}): ${`${result.stderr}${result.stdout}`.trim().split('\n').slice(-3).join(' | ')}`;
    }
    return null;
  } finally {
    rmSync(directory, { recursive: true, force: true });
  }
}

// ---------------------------------------------------------------------------
// Claims

function measuredRanges(lines) {
  const inside = new Array(lines.length).fill(false);
  let open = false;
  for (const [index, line] of lines.entries()) {
    if (/(?:<!--|\{\/\*)\s*measured:/.test(line)) open = true;
    inside[index] = open;
    if (/(?:<!--|\{\/\*)\s*\/measured\b/.test(line)) open = false;
  }
  return inside;
}

// For each line of a Markdown file, the index of the heading that starts its
// section (headings inside code fences do not count); -1 before the first one.
function sectionStarts(lines) {
  const starts = [];
  let current = -1;
  let fence = null;
  for (const [index, line] of lines.entries()) {
    const marker = line.match(/^\s*(`{3,}|~{3,})/)?.[1];
    if (marker && (!fence || marker[0] === fence[0])) fence = fence ? null : marker;
    else if (!fence && /^#{1,6}\s/.test(line)) current = index;
    starts.push(current);
  }
  return starts;
}

function inLabelledSection(lines, starts, index, pattern) {
  for (let cursor = 0; cursor < lines.length; cursor += 1) {
    if (starts[cursor] === starts[index] && pattern.test(lines[cursor])) return true;
  }
  return false;
}

function near(lines, index, radius, pattern) {
  for (let cursor = Math.max(0, index - radius); cursor <= Math.min(lines.length - 1, index + radius); cursor += 1) {
    if (pattern.test(lines[cursor])) return true;
  }
  return false;
}

export function checkClaims(file, source, { platform = false } = {}) {
  const problems = [];
  const lines = source.split(/\r?\n/);
  const measured = measuredRanges(lines);
  const markdown = !file.endsWith('.html');
  const starts = markdown ? sectionStarts(lines) : null;
  for (const [index, line] of lines.entries()) {
    const at = `${file}:${index + 1}`;
    if (/\bstronger\b/i.test(line) && !measured[index] && near(lines, index, 5, /gpt-oss:20b/)) {
      problems.push(`${at}: "stronger" within five lines of gpt-oss:20b; the 48 GB reviewer is smaller than its writer and unmeasured, so say "a different model"`);
    }
    if (/gpt-oss:120b(?![-\w])/.test(line) && !near(lines, index, 3, /96\s*GB/)) {
      problems.push(`${at}: gpt-oss:120b without its memory note (about 96 GB or more) within three lines`);
    }
    for (const [pattern, why] of WITHDRAWN) {
      if (pattern.test(line)) problems.push(`${at}: ${why}`);
    }
    if (platform) {
      for (const [pattern, name] of UNVERIFIED_PLATFORMS) {
        const labelled = near(lines, index, 4, /not yet verified/i)
          || (markdown && starts[index] >= 0 && inLabelledSection(lines, starts, index, /not yet verified/i));
        if (pattern.test(line) && !labelled) {
          problems.push(`${at}: ${name} named without "not yet verified" nearby; only macOS on Apple silicon and Ubuntu 24.04 in CI have been verified`);
        }
      }
    }
  }
  return problems;
}

// ---------------------------------------------------------------------------
// Driver

export function lintRepository(root, { binary = null, yaml = loadYamlParser() } = {}) {
  const subcommands = axocoatlSubcommands(root);
  const problems = [];
  if (!subcommands) problems.push('axocoatl-cli/src/main.rs: could not read the Commands enum to check subcommands');
  const allBlocks = [];
  const commandFiles = expandSurfaces(root, COMMAND_SURFACES);
  const platformFiles = new Set(expandSurfaces(root, PLATFORM_SURFACES));
  for (const file of commandFiles) {
    const source = readFileSync(join(root, file), 'utf8');
    const blocks = file.endsWith('.html') ? htmlBlocks(file, source) : markdownBlocks(file, source);
    problems.push(...checkBreaks(blocks));
    problems.push(...checkShell(blocks, subcommands));
    problems.push(...checkYaml(blocks, { yaml, binary }));
    allBlocks.push(...blocks.filter((block) => block.kind !== 'inline'));
  }
  // Claims cover every public page, not only the ones with first-run commands.
  const claimFiles = new Set([...commandFiles, ...expandSurfaces(root, ['sites/docs/src/content/docs/**/*.mdx'])]);
  for (const file of [...claimFiles].sort()) {
    problems.push(...checkClaims(file, readFileSync(join(root, file), 'utf8'), { platform: platformFiles.has(file) }));
  }
  return { files: commandFiles, blocks: allBlocks, problems };
}

function parseArguments(argv) {
  const options = { root: defaultRoot, binary: process.env.AXOCOATL_COPY_PASTE_BIN || null, json: false };
  for (let index = 0; index < argv.length; index += 1) {
    const argument = argv[index];
    if (argument === '--json') options.json = true;
    else if (argument === '--root' && argv[index + 1]) options.root = resolve(argv[(index += 1)]);
    else if (argument === '--axocoatl' && argv[index + 1]) options.binary = resolve(argv[(index += 1)]);
    else {
      console.error('Usage: node scripts/check-copy-paste.mjs [--root DIR] [--axocoatl BIN] [--json]');
      process.exit(64);
    }
  }
  return options;
}

// Run as a program, also when invoked through a symlinked path (on macOS /tmp
// is a link to /private/tmp): import.meta.url is resolved through links and
// process.argv[1] is not.
function invokedAsProgram() {
  try {
    return Boolean(process.argv[1]) && realpathSync(fileURLToPath(import.meta.url)) === realpathSync(process.argv[1]);
  } catch {
    return false;
  }
}

if (invokedAsProgram()) {
  const options = parseArguments(process.argv.slice(2));
  const result = lintRepository(options.root, { binary: options.binary });
  if (options.json) {
    const blocks = result.blocks.map(({ file, line, kind, text, commands, excerpt, yamlKind }) => ({ file, line, kind, yamlKind, excerpt, text, commands }));
    console.log(JSON.stringify({ files: result.files, blocks, problems: result.problems }, null, 2));
  } else if (result.problems.length) {
    console.error(`Copy-paste check failed (${result.problems.length})`);
    for (const problem of result.problems) console.error(`- ${problem}`);
  } else {
    const shell = result.blocks.filter((block) => block.kind === 'shell' || block.kind === 'snippet').length;
    const yamlBlocks = result.blocks.filter((block) => block.kind === 'yaml').length;
    console.log(`Copy-paste check passed: ${result.files.length} files, ${shell} shell blocks and copy buttons, ${yamlBlocks} YAML snippets${options.binary ? `, validated with ${options.binary}` : ''}.`);
  }
  process.exit(result.problems.length ? 1 : 0);
}
