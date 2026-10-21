// A node:test reporter that run-tests.mjs adds beside the usual one: at the
// end of a run it writes, as JSON, every file's test counts and failures,
// each failure with the `# browser crash:` lines printed under it
// (support/browser-crash.mjs). node:test reports a test's diagnostics right
// after the test, at the same nesting, and runs one file at a time here.
import path from 'node:path';

import { CRASH_LINE } from './crash-rerun.mjs';

function fileEntry(files, file) {
  let entry = files[file];
  if (!entry) {
    entry = { tests: 0, passed: 0, failed: 0, skipped: 0, failures: [], crashLinesOnPassedTests: 0 };
    files[file] = entry;
  }
  return entry;
}

function errorLine(error) {
  const messages = [];
  for (let current = error, depth = 0; current && depth < 4; current = current.cause, depth += 1) {
    if (current.message && !messages.includes(current.message)) messages.push(String(current.message));
  }
  const lines = (messages.find((message) => message !== 'test failed') || messages[0] || '')
    .split('\n').map((line) => line.trim()).filter(Boolean);
  // "Expected values to be strictly equal:" says more with the line after it.
  const shown = lines[0]?.endsWith(':') && lines[1] ? `${lines[0]} ${lines[1]}` : lines[0] || '';
  return shown.slice(0, 300);
}

export default async function* crashReport(source) {
  const files = {};
  // The last test that ended at each nesting, in each file.
  const last = new Map();
  for await (const { type, data } of source) {
    // A file's output (test:stdout, test:stderr) names it as given on the
    // command line, its tests by absolute path; only these events count.
    if (!data?.file || !['test:pass', 'test:fail', 'test:diagnostic', 'test:summary'].includes(type)) continue;
    const entry = fileEntry(files, path.resolve(data.file));
    if (type === 'test:pass' || type === 'test:fail') {
      const suite = data.details?.type === 'suite';
      const record = { name: data.name, nesting: data.nesting, failed: type === 'test:fail', crash: [] };
      last.set(`${path.resolve(data.file)}\0${data.nesting}`, record);
      if (!suite) {
        entry.tests += 1;
        if (type === 'test:fail') entry.failed += 1;
        else if (data.skip !== undefined || data.todo !== undefined) entry.skipped += 1;
        else entry.passed += 1;
      }
      if (type === 'test:fail') {
        const error = data.details?.error;
        Object.assign(record, {
          failureType: error?.failureType || null,
          error: errorLine(error),
          // A test (or suite) that failed only because a test inside it did.
          derived: error?.failureType === 'subtestsFailed',
        });
        entry.failures.push(record);
      }
    } else if (type === 'test:diagnostic' && typeof data.message === 'string'
        && data.message.startsWith(CRASH_LINE)) {
      const record = last.get(`${path.resolve(data.file)}\0${data.nesting}`);
      if (record?.failed) record.crash.push(data.message);
      else entry.crashLinesOnPassedTests += 1;
    }
  }
  for (const entry of Object.values(files)) {
    for (const failure of entry.failures) delete failure.failed;
  }
  yield `${JSON.stringify({ files })}\n`;
}
