import { spawn } from 'node:child_process';
import { existsSync } from 'node:fs';
import { mkdtemp, readdir, readFile, rm } from 'node:fs/promises';
import { homedir, tmpdir } from 'node:os';
import path from 'node:path';
import { pathToFileURL } from 'node:url';

import { judgeRun, rerunCap, rerunPassed } from './crash-rerun.mjs';
import { REPOSITORY_ROOT, TEST_ROOT } from './daemon.mjs';

function start(command, arguments_, cwd) {
  return new Promise((resolve, reject) => {
    const child = spawn(command, arguments_, { cwd, env: process.env, stdio: 'inherit' });
    child.once('error', reject);
    child.once('exit', (code, signal) => resolve(signal ? `signal ${signal}` : code));
  });
}

async function run(command, arguments_, cwd) {
  const status = await start(command, arguments_, cwd);
  if (status !== 0) throw new Error(`${command} exited with ${typeof status === 'number' ? `status ${status}` : status}`);
}

const cargoFromHome = path.join(homedir(), '.cargo', 'bin', 'cargo');
const cargo = process.env.CARGO || (existsSync(cargoFromHome) ? cargoFromHome : 'cargo');
const arguments_ = process.argv.slice(2);
const skipBuild = arguments_.includes('--no-build');
// Test files to run instead of all of them (tests/<name>.test.mjs).
const chosenFiles = arguments_.filter((argument) => argument !== '--no-build');
const unknown = chosenFiles.filter((argument) => argument.startsWith('-')
  || !/^tests\/[^/]+\.test\.mjs$/.test(argument) || !existsSync(path.join(TEST_ROOT, argument)));
if (unknown.length > 0) {
  console.error(`Unknown browser test runner argument: ${unknown.join(' ')} `
    + '(expected --no-build and test files named tests/<name>.test.mjs)');
  process.exit(2);
}

// AXO_BROWSER_ENGINE=webkit runs every file on WebKit (support/browser-engine.mjs).
const engine = process.env.AXO_BROWSER_ENGINE || 'chromium';
if (!['chromium', 'webkit'].includes(engine)) {
  console.error(`AXO_BROWSER_ENGINE=${engine} is not chromium or webkit`);
  process.exit(2);
}
let cap;
try {
  cap = rerunCap(process.env.AXO_BROWSER_CRASH_RERUNS);
} catch (error) {
  console.error(error.message);
  process.exit(2);
}
if (engine !== 'chromium') {
  const hook = pathToFileURL(path.join(TEST_ROOT, 'support', 'browser-engine.mjs')).href;
  process.env.NODE_OPTIONS = [process.env.NODE_OPTIONS, `--import=${hook}`].filter(Boolean).join(' ');
  console.log(`Browser regressions on ${engine}`);
}

// Besides the usual reporter, support/crash-report.mjs writes each file's
// failures and the `# browser crash:` lines under them to a file.
const reporter = pathToFileURL(path.join(TEST_ROOT, 'support', 'crash-report.mjs')).href;
const humanReporter = process.stdout.isTTY ? 'spec' : 'tap';
let reportDirectory = null;

async function runFiles(files, name) {
  const reportFile = path.join(reportDirectory, `${name}.json`);
  // --test-force-exit: a file ends when its tests have, even if a browser or
  // daemon a journey launched is still running, so one leak cannot keep the
  // suite waiting until CI cancels it. Playwright closes the browsers it
  // launched when the process exits; support/daemon.mjs kills a daemon left
  // running and fails that file.
  const code = await start(process.execPath, [
    '--test', '--test-force-exit', '--test-concurrency=1',
    `--test-reporter=${humanReporter}`, '--test-reporter-destination=stdout',
    `--test-reporter=${reporter}`, `--test-reporter-destination=${reportFile}`,
    ...files,
  ], TEST_ROOT);
  let report = null;
  try {
    report = JSON.parse(await readFile(reportFile, 'utf8'));
  } catch (error) {
    throw new Error(`node --test exited with ${code} and left no report (${error.message})`);
  }
  // The report names files by absolute path; the runner by tests/<name>.
  const files_ = {};
  for (const [file, entry] of Object.entries(report.files || {})) {
    const name = path.relative(TEST_ROOT, file);
    if (files_[name]) throw new Error(`the run's report names ${name} twice`);
    files_[name] = entry;
  }
  return { code, report: { files: files_ } };
}

function plural(count, noun) {
  return `${count} ${noun}${count === 1 ? '' : 's'}`;
}

function counts(report) {
  const total = { files: 0, tests: 0, passed: 0, failed: 0, skipped: 0 };
  for (const entry of Object.values(report.files)) {
    total.files += 1;
    for (const key of ['tests', 'passed', 'failed', 'skipped']) total[key] += entry[key];
  }
  return `${plural(total.files, 'file')}, ${plural(total.tests, 'test')}: ${total.passed} passed, `
    + `${total.failed} failed, ${total.skipped} skipped`;
}

function annotate(level, title, message) {
  if (process.env.GITHUB_ACTIONS === 'true') {
    console.log(`::${level} title=${title}::${message.replace(/%/g, '%25').replace(/\r/g, '%0D').replace(/\n/g, '%0A')}`);
  }
}

let ownedModelCache = null;
try {
  const entries = await readdir(path.join(TEST_ROOT, 'tests'), { withFileTypes: true });
  const testFiles = chosenFiles.length > 0 ? [...new Set(chosenFiles)].sort() : entries
    .filter((entry) => entry.isFile() && entry.name.endsWith('.test.mjs'))
    .map((entry) => path.join('tests', entry.name))
    .sort();
  if (testFiles.length === 0) {
    throw new Error('No browser regression files matched tests/*.test.mjs.');
  }

  if (!skipBuild) {
    await run(cargo, ['build', '--locked', '-p', 'axocoatl-cli'], REPOSITORY_ROOT);
  }
  if (!process.env.AXOCOATL_E2E_MODEL_CACHE) {
    ownedModelCache = await mkdtemp(path.join(tmpdir(), 'axocoatl-browser-model-cache-'));
    process.env.AXOCOATL_E2E_MODEL_CACHE = ownedModelCache;
  }
  // One genuinely cold daemon downloads and verifies the pinned embedding
  // model. Subsequent isolated fixtures copy artifacts, never control state.
  process.env.AXOCOATL_E2E_STARTUP_TIMEOUT_MS ||= '300000';
  reportDirectory = await mkdtemp(path.join(tmpdir(), 'axocoatl-browser-report-'));

  const first = await runFiles(testFiles, 'run');
  const judged = judgeRun(first, cap);
  const summary = [`Browser regressions (${engine}): ${counts(first.report)}.`];
  const crashedPasses = Object.entries(first.report.files)
    .filter(([, entry]) => entry.crashLinesOnPassedTests > 0)
    .map(([file, entry]) => `${file} (${entry.crashLinesOnPassedTests})`);
  if (crashedPasses.length > 0) {
    summary.push(`  "# browser crash:" lines under tests that passed anyway: ${crashedPasses.join(', ')}`);
  }
  let passed = first.code === 0;
  let rerunNote = '';

  if (first.code !== 0) {
    for (const { file, failures, afterCrash } of judged.failed) {
      for (const failure of failures) {
        summary.push(`  ${file}: "${failure.name}" failed without a detected browser crash, so the file is not rerun: ${failure.error}`);
      }
      for (const failure of afterCrash) {
        summary.push(`  ${file}: "${failure.name}" failed after a detected browser crash: ${failure.error}`);
        for (const line of failure.crash) summary.push(`      # ${line}`);
      }
    }
    for (const { file, failures } of judged.crashed) {
      summary.push(`  ${file}: ${plural(failures.length, 'failed test')}, each after a detected browser crash:`);
      for (const failure of failures) {
        summary.push(`    "${failure.name}": ${failure.error}`);
        for (const line of failure.crash) summary.push(`      # ${line}`);
      }
    }
    summary.push(...judged.problems.map((problem) => `  ${problem}`));

    if (judged.rerun.length > 0) {
      const files = judged.rerun.map(({ file }) => file);
      console.log(`\nRunning ${plural(files.length, 'file')} once more, because every failure in `
        + `${files.length === 1 ? 'it' : 'them'} followed a detected browser crash (a suite reruns at most `
        + `${plural(cap, 'file')}): ${files.join(', ')}\n`);
      const rerun = await runFiles(files, 'rerun');
      const rerunOk = rerunPassed(rerun, files);
      summary.push(`Rerun after a browser crash: ${counts(rerun.report)}; ${rerunOk ? 'passed' : 'FAILED'}.`);
      for (const file of files) {
        const entry = rerun.report.files[file];
        summary.push(`  ${file}: ${entry ? `${entry.passed} passed, ${entry.failed} failed, ${entry.skipped} skipped` : 'did not run'} on its rerun`);
        for (const failure of entry?.failures || []) {
          if (!failure.derived) summary.push(`    "${failure.name}" failed again: ${failure.error}`);
        }
        annotate(rerunOk ? 'warning' : 'error', 'Browser crash',
          `${file} failed after a browser crash and was rerun once; the rerun ${rerunOk ? 'passed' : 'failed'}.`);
      }
      rerunNote = `${plural(files.length, 'file')} rerun once after a browser crash`;
      passed = judged.failed.length === 0 && judged.problems.length === 0 && rerunOk;
    }
  }

  summary.push(passed
    ? `Browser regressions: PASS (${rerunNote ? `${rerunNote}, and passed` : 'no file rerun'})`
    : `Browser regressions: FAIL${rerunNote ? ` (${rerunNote})` : ''}`);
  console.log(`\n${summary.join('\n')}`);
  if (!passed) process.exitCode = 1;
} catch (error) {
  console.error(error.message || error);
  process.exitCode = 1;
} finally {
  if (ownedModelCache) await rm(ownedModelCache, {recursive:true,force:true});
  if (reportDirectory) await rm(reportDirectory, {recursive:true,force:true});
}
