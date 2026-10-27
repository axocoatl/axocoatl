#!/usr/bin/env node
// The commands a new user copies, read from the published copy itself, so
// the new-user gate runs them exactly as written (docs/RELEASE_GATE.md).
//
//   readme-run <README.md>          the README's `axocoatl run fix` command
//   install <file>                  the first `curl … install.sh | sh` command
//   install-page <index.html>       the local install page linked from the home page
//   output-path <command> <flag> <repo> <evidence-dir>  resolve a run output without eval
//   commands <file.md|.mdx|.html>   every shell command in code blocks, in order
//   first <file> <prefix>           the first such command starting with prefix
//   has <file> <command>...         exit 0 when every command is shown exactly
//   yaml-block <file.md|.mdx> <text>  the first ```yaml block containing text
//
// A command is one logical line: backslash continuations are joined and a
// trailing `# comment` outside quotes is dropped. In HTML, only <pre> blocks
// and <ax-cli-snippet> elements are commands; markup is removed and entities
// decoded, and a <br> inside a block breaks the line as it does when copied.

import { readFileSync } from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const ENTITIES = { amp: '&', lt: '<', gt: '>', quot: '"', apos: "'", nbsp: ' ', hellip: '…', mdash: '—', ndash: '–' };

export function decodeEntities(text) {
  return text.replace(/&(#x[0-9a-f]+|#\d+|[a-z]+);/gi, (match, name) => {
    if (name[0] === '#') {
      const code = name[1] === 'x' || name[1] === 'X' ? parseInt(name.slice(2), 16) : parseInt(name.slice(1), 10);
      return Number.isFinite(code) ? String.fromCodePoint(code) : match;
    }
    return ENTITIES[name.toLowerCase()] ?? match;
  });
}

/** Drop a trailing `# comment` that is outside quotes. */
export function stripComment(line) {
  let quote = null;
  for (let index = 0; index < line.length; index += 1) {
    const char = line[index];
    if (quote) {
      if (char === '\\' && quote === '"') index += 1;
      else if (char === quote) quote = null;
    } else if (char === '"' || char === "'") {
      quote = char;
    } else if (char === '#' && (index === 0 || /\s/.test(line[index - 1]))) {
      return line.slice(0, index).trimEnd();
    }
  }
  return line.trimEnd();
}

/** Logical command lines of one code block's text. */
export function logicalLines(text) {
  const out = [];
  let pending = '';
  for (const raw of text.replace(/\r\n/g, '\n').split('\n')) {
    const stripped = stripComment(raw);
    const line = pending ? stripped.trimStart() : stripped;
    if (line.endsWith('\\')) {
      pending += `${line.slice(0, -1).trimEnd()} `;
      continue;
    }
    const joined = `${pending}${line}`.trim();
    pending = '';
    if (joined) out.push(joined);
  }
  if (pending.trim()) out.push(pending.trim());
  return out;
}

/** The code blocks of a Markdown or MDX file: `{ lang, text }`. */
export function markdownBlocks(source) {
  const blocks = [];
  const fence = /^([ \t]*)(```+|~~~+)[ \t]*([A-Za-z0-9_+-]*)[^\n]*\n([\s\S]*?)^\1\2[ \t]*$/gm;
  for (const match of source.matchAll(fence)) {
    blocks.push({ lang: match[3].toLowerCase(), text: match[4].replace(new RegExp(`^${match[1]}`, 'gm'), '') });
  }
  return blocks;
}

/** The command blocks of an HTML page: <pre> contents and <ax-cli-snippet> elements. */
export function htmlBlocks(source) {
  const blocks = [];
  const pattern = /<pre\b[^>]*>([\s\S]*?)<\/pre>|<ax-cli-snippet\b[^>]*>([\s\S]*?)<\/ax-cli-snippet>/gi;
  for (const match of source.matchAll(pattern)) {
    const inner = (match[1] ?? match[2])
      .replace(/<br\s*\/?>/gi, '\n')
      .replace(/<[^>]+>/g, '');
    blocks.push({ lang: match[2] !== undefined ? 'snippet' : 'pre', text: decodeEntities(inner) });
  }
  return blocks;
}

const SHELL_LANGS = new Set(['', 'bash', 'sh', 'shell', 'console', 'zsh', 'snippet', 'pre']);
const COMMAND_START = /^(axocoatl|curl|cd|brew|podman|ollama|export|mkdir|git|gh|sudo|claude|codex|wsl)\b/;

/** Every shell command of a file, in order. */
export function commandsOf(file, source = readFileSync(file, 'utf8')) {
  const blocks = /\.html?$/i.test(file) ? htmlBlocks(source) : markdownBlocks(source);
  return blocks
    .filter((block) => SHELL_LANGS.has(block.lang))
    .flatMap((block) => logicalLines(block.text))
    .map((line) => line.replace(/^\$\s+/, ''))
    .filter((line) => COMMAND_START.test(line));
}

export function firstCommand(file, prefix, source) {
  return commandsOf(file, source).find((command) => command === prefix || command.startsWith(`${prefix} `));
}

/** The README's `axocoatl run fix` command (not fix-claude-code or fix-codex). */
export function readmeRunCommand(file, source) {
  const command = firstCommand(file, 'axocoatl run fix', source);
  if (!command) throw new Error(`${file} shows no \`axocoatl run fix\` command in a shell code block`);
  return command;
}

/** Follow the home page's local Install link, never an external or inferred page. */
export function installPage(file, source = readFileSync(file, 'utf8')) {
  const visible = source.replace(/<!--[\s\S]*?-->/g, '')
    .replace(/<(script|style|template)\b[^>]*>[\s\S]*?<\/\1\s*>/gi, '');
  const linked = [...visible.matchAll(/<a\b([^>]*)>([\s\S]*?)<\/a\s*>/gi)].some(([, attributes, label]) => {
    const href = attributes.match(/(?:^|\s)href\s*=\s*(["'])(.*?)\1/i)?.[2];
    return href && /^\/install(?:\/index\.html|\/)?(?:[?#].*)?$/.test(decodeEntities(href))
      && decodeEntities(label.replace(/<[^>]+>/g, '')).trim();
  });
  if (!linked) throw new Error(`${file} has no local link to /install`);
  const page = path.join(path.dirname(path.resolve(file)), 'install', 'index.html');
  // Do not count a link whose target was left out of the staged site.
  readFileSync(page, 'utf8');
  return page;
}

/** The published install command: the first `curl … install.sh | sh`. */
export function installCommand(file, source) {
  const command = commandsOf(file, source).find((line) => /^curl\b[^|]*\binstall\.sh\b[^|]*\|\s*(?:sh|bash)$/.test(line));
  if (!command) throw new Error(`${file} shows no \`curl … install.sh | sh\` command in a code block`);
  return command;
}

/** Resolve one published output path; only the documented AXO_EVIDENCE variable is allowed. */
export function outputPath(command, flag, repo, evidenceDir) {
  if (!['--junit', '--record', '--receipt'].includes(flag)) throw new Error(`not an output flag: ${flag}`);
  const values = [...command.matchAll(new RegExp(`(?:^|\\s)${flag}\\s+("[^"\\n]*"|'[^'\\n]*'|[^\\s]+)`, 'g'))];
  if (values.length !== 1) throw new Error(`expected exactly one ${flag} output`);
  const word = values[0][1];
  let value = /^["']/.test(word) ? word.slice(1, -1) : word;
  const variable = /^(?:\$AXO_EVIDENCE|\$\{AXO_EVIDENCE\})(?=\/|$)/;
  const remainder = value.replace(variable, '');
  if (!value || /[$`\\]/.test(remainder) || (word.startsWith("'") && variable.test(value))) {
    throw new Error(`unsupported shell expression in ${flag} output`);
  }
  value = value.replace(variable, () => path.resolve(evidenceDir));
  return path.resolve(repo, value);
}

/** Which of `wanted` the file does not show as a command, exactly. */
export function missingCommands(file, wanted, source) {
  const shown = new Set(commandsOf(file, source));
  return wanted.filter((command) => !shown.has(command));
}

export function yamlBlock(file, contains, source = readFileSync(file, 'utf8')) {
  const block = markdownBlocks(source).find((candidate) => ['yaml', 'yml'].includes(candidate.lang) && candidate.text.includes(contains));
  if (!block) throw new Error(`${file} has no yaml block containing ${JSON.stringify(contains)}`);
  return block.text;
}

function main([command, file, ...rest]) {
  const [extra] = rest;
  switch (command) {
    case 'readme-run':
      console.log(readmeRunCommand(file));
      return 0;
    case 'output-path':
      console.log(outputPath(file, extra, rest[1], rest[2]));
      return 0;
    case 'install-page':
      console.log(installPage(file));
      return 0;
    case 'install':
      console.log(installCommand(file));
      return 0;
    case 'commands':
      for (const line of commandsOf(file)) console.log(line);
      return 0;
    case 'has': {
      const missing = missingCommands(file, rest);
      for (const command of missing) console.error(`published-commands: ${file} does not show \`${command}\``);
      return missing.length === 0 ? 0 : 1;
    }
    case 'first': {
      const found = firstCommand(file, extra);
      if (!found) {
        console.error(`published-commands: ${file} shows no command starting with ${JSON.stringify(extra)}`);
        return 1;
      }
      console.log(found);
      return 0;
    }
    case 'yaml-block':
      process.stdout.write(yamlBlock(file, extra));
      return 0;
    default:
      console.error('usage: published-commands.mjs readme-run|install|install-page|output-path|commands|first|has|yaml-block FILE [ARG...]');
      return 2;
  }
}

if (process.argv[1] && path.resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  try {
    process.exitCode = main(process.argv.slice(2));
  } catch (error) {
    console.error(`published-commands: ${error.message}`);
    process.exitCode = 1;
  }
}
