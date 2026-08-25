import { spawn } from 'node:child_process';
import { existsSync } from 'node:fs';
import { readdir } from 'node:fs/promises';
import { homedir } from 'node:os';
import path from 'node:path';

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
  await run(
    process.execPath,
    ['--test', '--test-concurrency=1', ...testFiles],
    TEST_ROOT,
  );
} catch (error) {
  console.error(error.message || error);
  process.exitCode = 1;
}
