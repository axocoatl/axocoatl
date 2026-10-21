// What run-tests.mjs does with a run's failures: a test file whose every
// failure has a `# browser crash:` line under it (support/browser-crash.mjs)
// is run once more, and a file with any failure without one is not (README,
// "Browser crashes"). No browser runs here; tests/browser-crash.test.mjs
// checks these rules.

/** The prefix of the line printed under a test in which the browser crashed. */
export const CRASH_LINE = 'browser crash:';

/** Errors that a browser crash gives a journey, and that alone prove nothing. */
export const CRASH_ERRORS = /Target page, context or browser has been closed|Target crashed|Page crashed|page crashed|WebKit encountered an internal error|Browser has been closed|Browser closed/;

export const DEFAULT_RERUN_CAP = 3;

/** AXO_BROWSER_CRASH_RERUNS: how many files a suite may rerun (0 never reruns). */
export function rerunCap(value) {
  if (value === undefined || value === '') return DEFAULT_RERUN_CAP;
  if (!/^\d+$/.test(value) || Number(value) > 20) {
    throw new Error(`AXO_BROWSER_CRASH_RERUNS=${value} is not a whole number from 0 to 20`);
  }
  return Number(value);
}

/**
 * Sort a run (`code`, its exit status; `report`, what support/crash-report.mjs
 * wrote) into the files to run once more and the failures that fail the
 * suite. A test that failed only because a test inside it did is judged by
 * that test. `cap` bounds the files rerun; more than that fails the suite.
 */
export function judgeRun({ code, report }, cap) {
  const crashed = [];
  const failed = [];
  for (const [file, entry] of Object.entries(report?.files || {})) {
    if (entry.failures.length === 0) continue;
    const own = entry.failures.filter((failure) => !failure.derived);
    const judged = own.length ? own : entry.failures;
    const unexplained = judged.filter((failure) => failure.crash.length === 0);
    if (unexplained.length) {
      failed.push({ file, failures: unexplained, afterCrash: judged.filter((failure) => failure.crash.length > 0) });
    } else {
      crashed.push({ file, failures: own });
    }
  }
  const problems = [];
  if (code !== 0 && crashed.length === 0 && failed.length === 0) {
    problems.push(`node --test exited with ${code} without reporting a failed test`);
  }
  if (crashed.length > cap) {
    problems.push(`${crashed.length} files failed after a browser crash; a suite reruns at most ${cap} `
      + '(AXO_BROWSER_CRASH_RERUNS), so none was rerun');
  }
  return {
    rerun: crashed.length <= cap ? crashed : [],
    crashed,
    failed,
    problems,
  };
}

/** Whether a rerun of `files` passed entirely. */
export function rerunPassed({ code, report }, files) {
  if (code !== 0) return false;
  return files.every((file) => {
    const entry = report?.files?.[file];
    return Boolean(entry) && entry.failures.length === 0;
  });
}
