#!/usr/bin/env node
// The release workflow's new-user gate: refuse to publish a version unless
// a signed evidence bundle for the tagged commit passes every required path
// that docs/RELEASE_GATE.md lists. The only way past a failing gate is a
// manual workflow_dispatch run, started by a login listed in
// .axocoatl/gate-override-actors, in which the override phrase is typed;
// that run is logged as a warning and in the job summary.
//
// Usage:
//   node scripts/release-gate/check-evidence.mjs --version X.Y.Z --tag-commit SHA
//     [--root DIR] [--keys FILE] [--required FILE] [--override-actors FILE]
//     [--override PHRASE --event workflow_dispatch --actor LOGIN
//      --triggering-actor LOGIN]
// Exit: 0 pass (or a logged override), 1 refused, 2 usage.

import { appendFileSync } from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

import { decideRelease } from './evidence.mjs';

const HERE = path.dirname(fileURLToPath(import.meta.url));

function parse(argv) {
  const flags = {};
  for (let index = 0; index < argv.length; index += 1) {
    const flag = argv[index];
    if (!flag.startsWith('--') || argv[index + 1] === undefined) throw new Error(`unexpected argument ${flag}`);
    flags[flag.slice(2)] = argv[index + 1];
    index += 1;
  }
  for (const known of Object.keys(flags)) {
    if (!['version', 'tag-commit', 'root', 'keys', 'required', 'override-actors', 'override', 'event', 'actor', 'triggering-actor'].includes(known)) {
      throw new Error(`unknown flag --${known}`);
    }
  }
  if (!flags.version || !flags['tag-commit']) throw new Error('--version and --tag-commit are required');
  return flags;
}

let flags;
try {
  flags = parse(process.argv.slice(2));
} catch (error) {
  console.error(`check-evidence: ${error.message}`);
  process.exit(2);
}
const root = path.resolve(flags.root ?? path.join(HERE, '..', '..'));
const decision = decideRelease({
  root,
  version: flags.version.replace(/^v/, ''),
  tagCommit: flags['tag-commit'],
  keysFile: path.resolve(root, flags.keys ?? '.axocoatl/gate-keys'),
  requiredFile: path.resolve(root, flags.required ?? 'docs/RELEASE_GATE.md'),
  overrideActorsFile: path.resolve(root, flags['override-actors'] ?? '.axocoatl/gate-override-actors'),
  override: flags.override ?? '',
  event: flags.event ?? '',
  actor: flags.actor ?? '',
  triggeringActor: flags['triggering-actor'] ?? '',
});

for (const line of decision.lines) console.log(line);
if (process.env.GITHUB_ACTIONS === 'true') {
  if (decision.status === 'overridden') {
    console.log(`::warning title=New-user gate overridden::${decision.lines[0]}`);
  } else if (decision.status === 'fail') {
    console.log(`::error title=New-user gate::${decision.lines[0]}`);
  }
}
if (process.env.GITHUB_STEP_SUMMARY) {
  const heading = { pass: 'New-user gate: pass', fail: 'New-user gate: FAIL', overridden: 'New-user gate: OVERRIDDEN' }[decision.status];
  const body = decision.lines.map((line) => `    ${line}`).join('\n');
  try {
    appendFileSync(process.env.GITHUB_STEP_SUMMARY, `## ${heading}\n\n${body}\n\n`);
  } catch {
    // the summary is a convenience; the log above is the record
  }
}
process.exit(decision.ok ? 0 : 1);
