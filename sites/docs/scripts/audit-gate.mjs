// Dependency audit gate for the documentation site.
//
// Runs `npm audit --json` and fails on every high or critical advisory that no
// entry in audit-exceptions.json covers. An exception covers an advisory only
// while all of these hold:
//   - its `advisory` id is the advisory's GitHub id and its `package` is the
//     advisory's package;
//   - today (UTC) is on or before `expires`;
//   - the package's published `latest` version is still inside the recorded
//     `vulnerable` range and inside the range npm reports for the advisory, so
//     a published fix voids the exception;
//   - read from package-lock.json, aliased installs included: every package
//     that depends directly on an installed copy of the vulnerable package is
//     one of the listed `dependents`; every dependency path to such a copy,
//     from this package and from its workspaces and linked folders, goes
//     through one of them; and every installed copy can be traced to one of
//     those starting points, so a copy the lockfile does not connect fails.
//
// Usage: node scripts/audit-gate.mjs [--package-lock-only]
// No dependencies; Node 22.
import { spawnSync } from 'node:child_process';
import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';

export const GATED_SEVERITIES = new Set(['high', 'critical']);
export const KNOWN_SEVERITIES = new Set(['info', 'low', 'moderate', 'high', 'critical']);
export const MAX_EXCEPTION_DAYS = 90;
export const EXCEPTION_SCOPES = new Set(['build']);
export const EXCEPTIONS_FILE = 'audit-exceptions.json';
const EXCEPTION_KEYS = [
  'advisory',
  'package',
  'vulnerable',
  'dependents',
  'scope',
  'reason',
  'reviewed',
  'expires',
];
const GHSA_ID = /^GHSA(?:-[0-9a-z]{4}){3}$/;
const ADVISORY_URL = /^https:\/\/github\.com\/advisories\/(GHSA(?:-[0-9a-z]{4}){3})$/i;
const PACKAGE_NAME = /^(?:@[a-z0-9][a-z0-9._~-]*\/)?[a-z0-9][a-z0-9._~-]*$/;
const ISO_DATE = /^(\d{4})-(\d{2})-(\d{2})$/;
const DAY_MS = 24 * 60 * 60 * 1000;

export class GateError extends Error {}

// ---------------------------------------------------------------------------
// Versions and ranges. Enough of semver for advisory ranges: full versions,
// comparators (<, <=, >, >=, =), "*", whitespace or comma for AND, "||" for
// OR. Anything else is refused so the gate fails closed.

const VERSION = /^v?(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)(?:-([0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*))?(?:\+[0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*)?$/;

export function parseVersion(text) {
  const match = VERSION.exec(String(text).trim());
  if (!match) return null;
  return {
    core: [Number(match[1]), Number(match[2]), Number(match[3])],
    pre: match[4] ? match[4].split('.') : [],
  };
}

function comparePre(a, b) {
  if (a.length === 0 || b.length === 0) return (a.length === 0) - (b.length === 0);
  for (let i = 0; i < Math.max(a.length, b.length); i += 1) {
    if (a[i] === undefined) return -1;
    if (b[i] === undefined) return 1;
    const an = /^\d+$/.test(a[i]);
    const bn = /^\d+$/.test(b[i]);
    if (an && bn) {
      const diff = Number(a[i]) - Number(b[i]);
      if (diff !== 0) return Math.sign(diff);
    } else if (an !== bn) {
      return an ? -1 : 1;
    } else if (a[i] !== b[i]) {
      return a[i] < b[i] ? -1 : 1;
    }
  }
  return 0;
}

export function compareVersions(a, b) {
  for (let i = 0; i < 3; i += 1) {
    if (a.core[i] !== b.core[i]) return Math.sign(a.core[i] - b.core[i]);
  }
  return comparePre(a.pre, b.pre);
}

export function parseRange(text) {
  if (typeof text !== 'string' || text.trim() === '') return null;
  const sets = [];
  for (const part of text.split('||')) {
    const tokens = part.trim().replace(/(<=|>=|<|>|=)\s+/g, '$1').split(/[\s,]+/).filter(Boolean);
    if (tokens.length === 0) return null;
    const comparators = [];
    for (const token of tokens) {
      if (token === '*') continue;
      const match = /^(<=|>=|<|>|=)?(.+)$/.exec(token);
      const version = parseVersion(match[2]);
      if (!version) return null;
      comparators.push({ op: match[1] || '=', version });
    }
    sets.push(comparators);
  }
  return sets;
}

export function satisfies(versionText, rangeText) {
  const version = parseVersion(versionText);
  const range = parseRange(rangeText);
  if (!version || !range) return null;
  return range.some((comparators) => comparators.every(({ op, version: bound }) => {
    const order = compareVersions(version, bound);
    switch (op) {
      case '<': return order < 0;
      case '<=': return order <= 0;
      case '>': return order > 0;
      case '>=': return order >= 0;
      default: return order === 0;
    }
  }));
}

// ---------------------------------------------------------------------------
// Dates.

export function utcToday(now = new Date()) {
  return now.toISOString().slice(0, 10);
}

function parseDate(text) {
  const match = ISO_DATE.exec(String(text));
  if (!match) return null;
  const time = Date.UTC(Number(match[1]), Number(match[2]) - 1, Number(match[3]));
  return utcToday(new Date(time)) === text ? time : null;
}

// ---------------------------------------------------------------------------
// Exceptions file.

export function validateExceptions(value, today) {
  const errors = [];
  if (!Array.isArray(value)) {
    return { exceptions: [], errors: [`${EXCEPTIONS_FILE} must hold a JSON array`] };
  }
  const todayTime = parseDate(today);
  const seen = new Set();
  value.forEach((entry, index) => {
    const label = `${EXCEPTIONS_FILE}[${index}]`;
    if (!entry || typeof entry !== 'object' || Array.isArray(entry)) {
      errors.push(`${label}: must be an object`);
      return;
    }
    const before = errors.length;
    for (const key of Object.keys(entry)) {
      if (!EXCEPTION_KEYS.includes(key)) errors.push(`${label}: unknown field "${key}"`);
    }
    for (const key of EXCEPTION_KEYS) {
      if (!(key in entry)) errors.push(`${label}: missing field "${key}"`);
    }
    if (errors.length > before) return;
    const { advisory, package: pkg, vulnerable, dependents, scope, reason, reviewed, expires } = entry;
    if (typeof advisory !== 'string' || !GHSA_ID.test(advisory)) {
      errors.push(`${label}: advisory must be a lowercase GitHub advisory id such as GHSA-xxxx-xxxx-xxxx`);
    } else if (seen.has(`${advisory} ${pkg}`)) {
      errors.push(`${label}: advisory ${advisory} is listed more than once for ${pkg}`);
    } else {
      seen.add(`${advisory} ${pkg}`);
    }
    if (typeof pkg !== 'string' || !PACKAGE_NAME.test(pkg)) {
      errors.push(`${label}: package must be an npm package name`);
    }
    if (typeof vulnerable !== 'string' || parseRange(vulnerable) === null) {
      errors.push(`${label}: vulnerable must be a range of full versions, such as "<=4.2.0"`);
    }
    if (!Array.isArray(dependents) || dependents.length === 0
      || dependents.some((name) => typeof name !== 'string' || !PACKAGE_NAME.test(name) || name === pkg)
      || new Set(dependents).size !== dependents.length) {
      errors.push(`${label}: dependents must be a non-empty list of distinct package names other than the package`);
    }
    if (!EXCEPTION_SCOPES.has(scope)) {
      errors.push(`${label}: scope must be one of ${[...EXCEPTION_SCOPES].join(', ')}`);
    }
    if (typeof reason !== 'string' || reason.trim().length < 20) {
      errors.push(`${label}: reason must explain why the advisory does not apply (at least 20 characters)`);
    }
    const reviewedTime = parseDate(reviewed);
    const expiresTime = parseDate(expires);
    if (reviewedTime === null) errors.push(`${label}: reviewed must be a date (YYYY-MM-DD)`);
    if (expiresTime === null) errors.push(`${label}: expires must be a date (YYYY-MM-DD)`);
    if (reviewedTime !== null && expiresTime !== null) {
      if (expiresTime < reviewedTime) {
        errors.push(`${label}: expires (${expires}) is before reviewed (${reviewed})`);
      } else if ((expiresTime - reviewedTime) / DAY_MS > MAX_EXCEPTION_DAYS) {
        errors.push(`${label}: expires (${expires}) is more than ${MAX_EXCEPTION_DAYS} days after reviewed (${reviewed})`);
      }
    }
    if (reviewedTime !== null && todayTime !== null && reviewedTime > todayTime) {
      errors.push(`${label}: reviewed (${reviewed}) is after today (${today}, UTC)`);
    }
  });
  return { exceptions: errors.length === 0 ? value : [], errors };
}

// ---------------------------------------------------------------------------
// npm audit report.

export function advisoryIdFromUrl(url) {
  const match = ADVISORY_URL.exec(String(url ?? ''));
  return match ? match[1].toLowerCase().replace(/^ghsa/, 'GHSA') : null;
}

// Returns every advisory in the report, deduplicated by advisory and package.
// Throws GateError when the report is an npm error or has an unknown shape, and
// when a high or critical entry cannot be traced to an advisory.
export function collectAdvisories(report) {
  if (!report || typeof report !== 'object' || Array.isArray(report)) {
    throw new GateError('npm audit did not return a JSON object');
  }
  if ('error' in report) {
    const detail = [report.message, report.error?.code, report.error?.summary, report.error?.detail]
      .filter((text) => typeof text === 'string' && text.trim() !== '')
      .join(': ');
    throw new GateError(`npm audit failed${detail ? `: ${detail}` : ''}`);
  }
  if (report.auditReportVersion !== 2 || !report.vulnerabilities
    || typeof report.vulnerabilities !== 'object') {
    throw new GateError('npm audit returned a report this gate does not understand (expected auditReportVersion 2)');
  }
  const entries = report.vulnerabilities;
  const advisories = new Map();
  for (const [name, entry] of Object.entries(entries)) {
    for (const via of entry.via ?? []) {
      if (typeof via === 'string') continue;
      const pkg = via.name ?? name;
      // The entry's nodes are the installed copies of the advisory's package
      // only when the entry is that package's own entry.
      const nodes = pkg === name ? entry.nodes ?? [] : [];
      const key = `${via.url}\u0000${pkg}`;
      const known = advisories.get(key);
      if (known) {
        for (const node of nodes) known.nodes.add(node);
        continue;
      }
      advisories.set(key, {
        id: advisoryIdFromUrl(via.url),
        url: via.url,
        package: pkg,
        severity: via.severity,
        range: via.range,
        title: via.title,
        nodes: new Set(nodes),
      });
    }
  }
  const reachable = (name, seen = new Set()) => {
    if (seen.has(name)) return [];
    seen.add(name);
    const entry = entries[name];
    if (!entry) return [];
    return (entry.via ?? []).flatMap((via) => (typeof via === 'string'
      ? reachable(via, seen)
      : [via]));
  };
  for (const [name, entry] of Object.entries(entries)) {
    if (!GATED_SEVERITIES.has(entry.severity) && KNOWN_SEVERITIES.has(entry.severity)) continue;
    const sources = reachable(name);
    if (!sources.some((via) => GATED_SEVERITIES.has(via.severity) || !KNOWN_SEVERITIES.has(via.severity))) {
      throw new GateError(`npm audit reports ${name} as ${entry.severity}, but no high or critical advisory explains it`);
    }
  }
  return [...advisories.values()];
}

// ---------------------------------------------------------------------------
// Dependency paths from package-lock.json (lockfileVersion 2 or 3).

function installedName(location) {
  const index = location.lastIndexOf('node_modules/');
  return index < 0 ? location : location.slice(index + 'node_modules/'.length);
}

function parentLocation(location) {
  const index = location.lastIndexOf('node_modules/');
  return index <= 0 ? '' : location.slice(0, index - 1);
}

// npm leaves out the name of a folder whose package name matches the folder
// (`tools/tool` is `tool`, `packages/@acme/tool` is `@acme/tool`).
function nameFromFolder(location) {
  const base = path.posix.basename(location);
  const parent = path.posix.basename(path.posix.dirname(location));
  return parent.startsWith('@') ? `${parent}/${base}` : base;
}

export function lockGraph(lock) {
  const packages = lock?.packages;
  if (!packages || typeof packages !== 'object' || !packages['']) {
    throw new GateError('package-lock.json has no "packages" map; lockfileVersion 2 or 3 is required');
  }
  // A link entry's `resolved` is the linked folder, relative to the lockfile.
  const follow = (location) => {
    const entry = packages[location];
    if (!entry?.link) return location;
    const target = typeof entry.resolved === 'string' ? path.posix.normalize(entry.resolved) : null;
    if (target === null || !Object.hasOwn(packages, target)) {
      throw new GateError(`package-lock.json links ${location} to a folder it does not list`);
    }
    return target;
  };
  const resolve = (from, name) => {
    let base = from;
    for (;;) {
      const candidate = base === '' ? `node_modules/${name}` : `${base}/node_modules/${name}`;
      if (Object.hasOwn(packages, candidate)) return follow(candidate);
      if (base === '') return null;
      base = parentLocation(base);
    }
  };
  const isFolder = (location) => !location.includes('node_modules/');
  const nameOf = (location) => {
    if (location === '') return packages[''].name ?? '(root)';
    const name = packages[location]?.name;
    if (typeof name === 'string') return name;
    return isFolder(location) ? nameFromFolder(location) : installedName(location);
  };
  const edges = (location) => {
    const entry = packages[location] ?? {};
    const fields = ['dependencies', 'optionalDependencies', 'peerDependencies'];
    if (isFolder(location)) fields.push('devDependencies');
    const names = new Set(fields.flatMap((field) => Object.keys(entry[field] ?? {})));
    return [...names].map((name) => resolve(location, name)).filter((target) => target !== null);
  };
  // Aliased installs show the folder name and the package behind it.
  const labelOf = (location) => {
    const name = nameOf(location);
    const installed = installedName(location);
    return location.includes('node_modules/') && installed !== name ? `${installed} (npm:${name})` : name;
  };
  const locationsOf = (pkg) => Object.keys(packages)
    .filter((location) => location !== '' && !packages[location].link && nameOf(location) === pkg);
  // Where a search starts: the root, then the folders outside node_modules/
  // (workspaces and linked folders). npm installs workspaces although the
  // root lists them under `workspaces`, not as dependencies.
  const starts = ['', ...Object.keys(packages)
    .filter((location) => location !== '' && isFolder(location) && !packages[location].link)];
  return { packages, nameOf, labelOf, edges, locationsOf, starts };
}

// Breadth-first search from the root, then from each start the search has not
// reached yet. Locations whose package is in `blocked` are not entered, except
// `targets`. Returns a map from each reached location to the location it was
// reached from (null for a start), in the order they were reached.
function reach(graph, targets = new Set(), blocked = new Set()) {
  const enters = (location) => targets.has(location) || !blocked.has(graph.nameOf(location));
  const parent = new Map();
  for (const start of graph.starts) {
    if (parent.has(start) || (start !== '' && !enters(start))) continue;
    parent.set(start, null);
    const queue = [start];
    while (queue.length > 0) {
      const location = queue.shift();
      for (const next of graph.edges(location)) {
        if (parent.has(next) || !enters(next)) continue;
        parent.set(next, location);
        queue.push(next);
      }
    }
  }
  return parent;
}

// Finds a dependency path from the root, a workspace or a linked folder to
// one of `targets` that passes through none of `dependents`. Returns the path
// as package names, or null when no such path exists. A target that no path
// reaches at all also gives null; untraced() finds those.
export function pathAvoiding(graph, targets, dependents) {
  const parent = reach(graph, targets, new Set(dependents));
  const hit = [...parent.keys()].find((location) => targets.has(location));
  if (hit === undefined) return null;
  const trail = [];
  for (let at = hit; at !== null; at = parent.get(at)) trail.push(at);
  return trail.reverse().map((step) => graph.labelOf(step));
}

// Returns the targets that no dependency path from the root, a workspace or a
// linked folder reaches, such as extraneous copies.
export function untraced(graph, targets) {
  const parent = reach(graph);
  return [...targets].filter((location) => !parent.has(location));
}

// Returns the lock locations that depend directly on one of `targets`, other
// than the targets themselves.
export function directDependents(graph, targets) {
  return Object.keys(graph.packages).filter((location) => !targets.has(location)
    && !graph.packages[location].link
    && graph.edges(location).some((next) => targets.has(next)));
}

// ---------------------------------------------------------------------------
// npm.

export function runNpm(args, cwd) {
  const result = spawnSync('npm', args, {
    cwd,
    encoding: 'utf8',
    maxBuffer: 64 * 1024 * 1024,
    shell: process.platform === 'win32',
  });
  if (result.error) throw new GateError(`could not run npm ${args.join(' ')}: ${result.error.message}`);
  return { status: result.status, stdout: result.stdout, stderr: result.stderr };
}

function parseJson(text, what) {
  try {
    return JSON.parse(text);
  } catch {
    throw new GateError(`could not parse JSON from ${what}`);
  }
}

// ---------------------------------------------------------------------------
// The gate.

export function runGate({
  dir,
  packageLockOnly = false,
  today = utcToday(),
  npm = (args) => runNpm(args, dir),
  read = (name) => fs.readFileSync(path.join(dir, name), 'utf8'),
  log = (line) => console.log(line),
} = {}) {
  const failures = [];
  const warnings = [];
  const applied = [];
  const say = (line) => log(`audit-gate: ${line}`);
  const finish = () => {
    for (const line of warnings) say(`warning: ${line}`);
    for (const line of failures) say(`FAIL ${line}`);
    say(failures.length === 0 ? 'PASS' : `FAIL (${failures.length} problem${failures.length === 1 ? '' : 's'})`);
    return { ok: failures.length === 0, failures, warnings, applied };
  };

  let exceptions = [];
  try {
    const checked = validateExceptions(parseJson(read(EXCEPTIONS_FILE), EXCEPTIONS_FILE), today);
    failures.push(...checked.errors);
    exceptions = checked.exceptions;
  } catch (error) {
    failures.push(error instanceof GateError ? error.message : `cannot read ${EXCEPTIONS_FILE}: ${error.message}`);
  }

  const auditArgs = ['audit', '--json', ...(packageLockOnly ? ['--package-lock-only'] : [])];
  let advisories;
  try {
    const result = npm(auditArgs);
    if (result.status !== 0 && result.status !== 1) {
      throw new GateError(`npm ${auditArgs.join(' ')} exited with status ${result.status}: ${String(result.stderr ?? '').trim()}`);
    }
    advisories = collectAdvisories(parseJson(result.stdout, `npm ${auditArgs.join(' ')}`));
  } catch (error) {
    if (!(error instanceof GateError)) throw error;
    failures.push(error.message);
    return finish();
  }

  const gated = advisories.filter((advisory) => GATED_SEVERITIES.has(advisory.severity)
    || !KNOWN_SEVERITIES.has(advisory.severity));
  const below = advisories.filter((advisory) => !gated.includes(advisory));
  say(`npm audit${packageLockOnly ? ' (package-lock only)' : ''}: ${gated.length} high or critical advisor${gated.length === 1 ? 'y' : 'ies'}, ${below.length} below the gate level`);
  for (const advisory of below) {
    say(`not gated: ${advisory.id ?? advisory.url} ${advisory.package} (${advisory.severity})`);
  }

  let graph = null;
  const lockGraphOnce = () => {
    if (graph === null) {
      let text;
      try {
        text = read('package-lock.json');
      } catch (error) {
        throw new GateError(`cannot read package-lock.json: ${error.message}`);
      }
      graph = lockGraph(parseJson(text, 'package-lock.json'));
    }
    return graph;
  };
  const latestCache = new Map();
  const latestOf = (pkg) => {
    if (!latestCache.has(pkg)) {
      const args = ['view', pkg, 'dist-tags', '--json'];
      const result = npm(args);
      let latest = null;
      if (result.status === 0) {
        try {
          latest = parseJson(result.stdout, `npm ${args.join(' ')}`)?.latest ?? null;
        } catch {
          latest = null;
        }
      }
      latestCache.set(pkg, typeof latest === 'string' ? latest : null);
    }
    return latestCache.get(pkg);
  };

  const used = new Set();
  for (const advisory of gated) {
    const name = `${advisory.id ?? advisory.url} ${advisory.package} (${advisory.severity})`;
    const sameAdvisory = advisory.id
      ? exceptions.filter((candidate) => candidate.advisory === advisory.id)
      : [];
    const exception = sameAdvisory.find((candidate) => candidate.package === advisory.package);
    if (!exception) {
      const other = sameAdvisory.map((candidate) => candidate.package).join(', ');
      failures.push(other
        ? `${name}: the exception for ${advisory.id} names ${other}, not ${advisory.package}`
        : `${name}: no exception in ${EXCEPTIONS_FILE}${advisory.title ? ` (${advisory.title})` : ''}`);
      for (const candidate of sameAdvisory) used.add(candidate);
      continue;
    }
    used.add(exception);
    const reasons = [];
    if (today > exception.expires) {
      reasons.push(`the exception expired on ${exception.expires}`);
    }
    if (reasons.length === 0) {
      const latest = latestOf(advisory.package);
      if (latest === null) {
        reasons.push(`could not read the latest published version of ${advisory.package} (npm view ${advisory.package} dist-tags)`);
      } else {
        const inRecorded = satisfies(latest, exception.vulnerable);
        const inAdvisory = satisfies(latest, advisory.range);
        if (inRecorded === null || inAdvisory === null) {
          reasons.push(`cannot compare latest ${latest} with the ranges "${exception.vulnerable}" and "${advisory.range}"`);
        } else if (!inAdvisory) {
          reasons.push(`a fixed version exists: latest ${advisory.package} is ${latest}, outside the advisory range "${advisory.range}"; upgrade instead`);
        } else if (!inRecorded) {
          reasons.push(`latest ${advisory.package} is ${latest}, outside the reviewed range "${exception.vulnerable}"; review the advisory again`);
        }
      }
      try {
        const lock = lockGraphOnce();
        const targets = new Set(lock.locationsOf(advisory.package));
        for (const node of advisory.nodes) {
          if (!Object.hasOwn(lock.packages, node)) {
            reasons.push(`npm audit reports ${node}, which package-lock.json does not list`);
          }
          targets.add(node);
        }
        if (targets.size === 0) {
          reasons.push(`package-lock.json has no installed copy of ${advisory.package}`);
        } else {
          const trail = pathAvoiding(lock, targets, exception.dependents);
          if (trail) {
            reasons.push(`dependency path ${trail.join(' > ')} does not go through ${exception.dependents.join(' or ')}`);
          }
          const unlisted = new Set(directDependents(lock, targets)
            .filter((location) => !exception.dependents.includes(lock.nameOf(location)))
            .map((location) => lock.labelOf(location)));
          for (const label of unlisted) {
            reasons.push(`${label} depends on ${advisory.package} directly but is not in dependents`);
          }
          for (const location of untraced(lock, targets)) {
            reasons.push(`cannot trace ${location} to the root, a workspace or a linked folder in package-lock.json`);
          }
        }
      } catch (error) {
        if (!(error instanceof GateError)) throw error;
        reasons.push(error.message);
      }
    }
    if (reasons.length > 0) {
      for (const reason of reasons) failures.push(`${name}: ${reason}`);
      continue;
    }
    applied.push(exception);
    say(`exception applied: ${exception.advisory} ${exception.package} ${exception.vulnerable} via ${exception.dependents.join(', ')} (scope ${exception.scope}, reviewed ${exception.reviewed}, expires ${exception.expires})`);
  }
  for (const exception of exceptions) {
    if (!used.has(exception)) {
      warnings.push(`exception ${exception.advisory} ${exception.package} matched no high or critical advisory; remove it from ${EXCEPTIONS_FILE}`);
    }
  }
  return finish();
}

// ---------------------------------------------------------------------------
// CLI.

export function main(argv = process.argv.slice(2)) {
  let packageLockOnly = false;
  for (const arg of argv) {
    if (arg === '--package-lock-only') {
      packageLockOnly = true;
    } else {
      console.error('Usage: node scripts/audit-gate.mjs [--package-lock-only]');
      return 2;
    }
  }
  const dir = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
  try {
    return runGate({ dir, packageLockOnly }).ok ? 0 : 1;
  } catch (error) {
    console.error(`audit-gate: FAIL ${error.message}`);
    return 1;
  }
}

if (process.argv[1] && import.meta.url === pathToFileURL(fs.realpathSync(process.argv[1])).href) {
  process.exitCode = main();
}
