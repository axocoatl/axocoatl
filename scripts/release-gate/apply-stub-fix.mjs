#!/usr/bin/env node
// Apply the gate stub's fix for one fixture to a copy of it, exactly as the
// stub's edit_file call would: replace the one occurrence of `find` in
// `probe` with `replace`.
//
// Usage: node apply-stub-fix.mjs <fixture> <directory> [--fixes FILE]
import { readFileSync, writeFileSync } from 'node:fs';
import path from 'node:path';

import { DEFAULT_FIXES, loadFixes } from './stub-model-server.mjs';

const [fixture, directory, flag, fixesFile] = process.argv.slice(2);
if (!fixture || !directory || (flag !== undefined && (flag !== '--fixes' || !fixesFile))) {
  console.error('usage: apply-stub-fix.mjs <fixture> <directory> [--fixes FILE]');
  process.exit(2);
}
const fix = loadFixes(fixesFile ?? DEFAULT_FIXES).find((entry) => entry.fixture === fixture);
if (!fix) {
  console.error(`apply-stub-fix: no stub fix for ${fixture}`);
  process.exit(1);
}
const file = path.join(directory, fix.probe);
const before = readFileSync(file, 'utf8');
const occurrences = before.split(fix.find).length - 1;
if (occurrences !== 1) {
  console.error(`apply-stub-fix: ${fixture}: "${fix.find}" appears ${occurrences} times in ${fix.probe}, not once`);
  process.exit(1);
}
writeFileSync(file, before.replace(fix.find, fix.replace));
console.log(`apply-stub-fix: ${fixture}: ${fix.probe} fixed`);
