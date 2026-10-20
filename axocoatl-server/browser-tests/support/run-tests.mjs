import { spawn } from 'node:child_process';
import { existsSync } from 'node:fs';
import { mkdtemp, readdir, rm } from 'node:fs/promises';
import { homedir, tmpdir } from 'node:os';
import path from 'node:path';
import { pathToFileURL } from 'node:url';

import { REPOSITORY_ROOT, TEST_ROOT } from './daemon.mjs';

function run(command, arguments_, cwd) {
  return new Promise((resolve, reject) => {
    const child = spawn(command, arguments_, { cwd, env: process.env, stdio: 'inherit' });
    child.once('error', reject);
    child.once('exit', (code, signal) => {
      if (code === 0) resolve();
      else reject(new Error(`${command} exited with ${signal || `status ${code}`}`));
    });
  });
}

const cargoFromHome = path.join(homedir(), '.cargo', 'bin', 'cargo');
const cargo = process.env.CARGO || (existsSync(cargoFromHome) ? cargoFromHome : 'cargo');
const arguments_ = process.argv.slice(2);
const skipBuild = arguments_.length === 1 && arguments_[0] === '--no-build';

if (arguments_.length > 0 && !skipBuild) {
  console.error(`Unknown browser test runner argument: ${arguments_.join(' ')}`);
  process.exit(2);
}

// AXO_BROWSER_ENGINE=webkit runs every file on WebKit (support/browser-engine.mjs).
const engine = process.env.AXO_BROWSER_ENGINE || 'chromium';
if (!['chromium', 'webkit'].includes(engine)) {
  console.error(`AXO_BROWSER_ENGINE=${engine} is not chromium or webkit`);
  process.exit(2);
}
if (engine !== 'chromium') {
  const hook = pathToFileURL(path.join(TEST_ROOT, 'support', 'browser-engine.mjs')).href;
  process.env.NODE_OPTIONS = [process.env.NODE_OPTIONS, `--import=${hook}`].filter(Boolean).join(' ');
  console.log(`Browser regressions on ${engine}`);
}

let ownedModelCache = null;
try {
  const entries = await readdir(path.join(TEST_ROOT, 'tests'), { withFileTypes: true });
  const testFiles = entries
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
  // --test-force-exit: a file ends when its tests have, even if a browser or
  // daemon a journey launched is still running, so one leak cannot keep the
  // suite waiting until CI cancels it. Playwright closes the browsers it
  // launched when the process exits; support/daemon.mjs kills a daemon left
  // running and fails that file.
  await run(
    process.execPath,
    ['--test', '--test-force-exit', '--test-concurrency=1', ...testFiles],
    TEST_ROOT,
  );
} catch (error) {
  console.error(error.message || error);
  process.exitCode = 1;
} finally {
  if (ownedModelCache) await rm(ownedModelCache, {recursive:true,force:true});
}
