import { spawn } from 'node:child_process';
import { createServer } from 'node:net';
import {
  access,
  mkdtemp,
  mkdir,
  readFile,
  realpath,
  rm,
  writeFile,
} from 'node:fs/promises';
import { tmpdir } from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const SUPPORT_DIR = path.dirname(fileURLToPath(import.meta.url));
export const TEST_ROOT = path.resolve(SUPPORT_DIR, '..');
export const REPOSITORY_ROOT = path.resolve(TEST_ROOT, '..', '..');

const delay = (milliseconds) => new Promise((resolve) => setTimeout(resolve, milliseconds));

async function freeLoopbackPort() {
  const server = createServer();
  await new Promise((resolve, reject) => {
    server.once('error', reject);
    server.listen(0, '127.0.0.1', resolve);
  });
  const address = server.address();
  const port = typeof address === 'object' && address ? address.port : 0;
  await new Promise((resolve, reject) => server.close((error) => error ? reject(error) : resolve()));
  if (!port) throw new Error('Could not reserve a loopback port for the browser test daemon.');
  return port;
}

function boundedLog(buffer, chunk) {
  const next = buffer + String(chunk);
  return next.length > 32_000 ? next.slice(-32_000) : next;
}

async function stopProcess(child) {
  if (!child || child.exitCode !== null || child.signalCode !== null) return;
  child.kill('SIGTERM');
  const exited = new Promise((resolve) => child.once('exit', resolve));
  await Promise.race([exited, delay(5_000)]);
  if (child.exitCode === null && child.signalCode === null) {
    child.kill('SIGKILL');
    await exited;
  }
}

async function waitForHealth(baseUrl, child, logs) {
  // macOS bootstrap may run three sequential, individually bounded 15-second
  // Podman readiness probes before HTTP binds. Keep a finite margin above
  // that 45-second backend contract; a process exit still fails immediately.
  const deadline = Date.now() + 75_000;
  let lastError = null;
  while (Date.now() < deadline) {
    if (child.exitCode !== null || child.signalCode !== null) {
      throw new Error(`Axocoatl exited before becoming healthy.\n${logs()}`);
    }
    try {
      const response = await fetch(`${baseUrl}/health/live`);
      if (response.ok) return;
      lastError = new Error(`health returned HTTP ${response.status}`);
    } catch (error) {
      lastError = error;
    }
    await delay(100);
  }
  throw new Error(`Axocoatl did not become healthy: ${lastError}\n${logs()}`);
}

async function api(baseUrl, method, pathname, body) {
  const response = await fetch(`${baseUrl}${pathname}`, {
    method,
    headers: body === undefined ? undefined : { 'content-type': 'application/json' },
    body: body === undefined ? undefined : JSON.stringify(body),
  });
  const text = await response.text();
  let payload = null;
  try { payload = text ? JSON.parse(text) : null; } catch { payload = text; }
  if (!response.ok) {
    throw new Error(`${method} ${pathname} returned HTTP ${response.status}: ${text}`);
  }
  return payload;
}

async function makeProject(root, name, { packageLock = true, devcontainer = null } = {}) {
  const directory = path.join(root, name);
  await mkdir(path.join(directory, 'src'), { recursive: true });
  await writeFile(path.join(directory, 'package.json'), JSON.stringify({
    name,
    version: '0.0.0',
    private: true,
  }, null, 2));
  if (packageLock) {
    // A lockfile deliberately makes setup a proposed `npm ci`, so Session
    // creation is side-effect free and remains AwaitingApproval.
    await writeFile(path.join(directory, 'package-lock.json'), JSON.stringify({
      name,
      version: '0.0.0',
      lockfileVersion: 3,
      requires: true,
      packages: { '': { name, version: '0.0.0' } },
    }, null, 2));
  }
  if (devcontainer) {
    await mkdir(path.join(directory, '.devcontainer'), { recursive: true });
    await writeFile(
      path.join(directory, '.devcontainer', 'devcontainer.json'),
      JSON.stringify(devcontainer, null, 2),
    );
  }
  await writeFile(path.join(directory, 'src', 'index.js'), `export const workspace = ${JSON.stringify(name)};\n`);
  return realpath(directory);
}

async function seedWorkspace(baseUrl, projectPath, name, sessionNames) {
  const workspace = await api(baseUrl, 'POST', '/api/workspaces', { path: projectPath, name });
  const sessions = [];
  for (const sessionName of sessionNames) {
    const session = await api(
      baseUrl,
      'POST',
      `/api/workspaces/${encodeURIComponent(workspace.id)}/sessions`,
      {
        name: sessionName,
        mode: { kind: 'single_agent', agent_id: 'browser-test-coder' },
        enabled_skills: [],
        exposed_ports: [3000],
        setup_approved: false,
        setup_reviewed: false,
      },
    );
    if (session?.environment?.state !== 'awaiting_approval'
        || session?.environment?.setup_command !== 'npm ci') {
      throw new Error(`Expected ${sessionName} to await approval for npm ci; received ${JSON.stringify(session?.environment)}`);
    }
    sessions.push(session);
  }
  return { workspace, sessions };
}

export async function launchTestDaemon({
  allowPostCreateCommand = false,
  ollamaBaseUrl = 'http://127.0.0.1:9',
  skills = [],
} = {}) {
  const runRoot = await mkdtemp(path.join(tmpdir(), 'axocoatl-browser-e2e-'));
  const dataDirectory = path.join(runRoot, 'data');
  const projectsDirectory = path.join(runRoot, 'projects');
  await mkdir(dataDirectory, { recursive: true });
  await mkdir(projectsDirectory, { recursive: true });

  const port = Number(process.env.AXOCOATL_E2E_PORT) || await freeLoopbackPort();
  const baseUrl = `http://127.0.0.1:${port}`;
  const configPath = path.join(runRoot, 'axocoatl.e2e.yaml');
  const socketPath = path.join(runRoot, 'axocoatl.sock');
  const binary = process.env.AXOCOATL_E2E_BINARY
    ? path.resolve(process.env.AXOCOATL_E2E_BINARY)
    : path.join(REPOSITORY_ROOT, 'target', 'debug', 'axocoatl');
  const skillsConfig = skills.length ? `
skills:
${skills.map((skill) => `  - id: ${JSON.stringify(skill.id)}
    name: ${JSON.stringify(skill.name)}
    description: ${JSON.stringify(skill.description)}
    emits: ${JSON.stringify(skill.emits || [])}
    reacts_to: ${JSON.stringify(skill.reactsTo || [])}
    agents: ${JSON.stringify(skill.agents || [])}
    prompt: ${JSON.stringify(skill.prompt || '')}`).join('\n')}
` : '';
  const config = `
agents:
  - id: browser-test-coder
    name: Browser Test Coder
    provider: ollama
    model: browser-test-model
    system_prompt: Browser regression fixture. No turns are executed.
    depends_on: []

providers:
  ollama:
    base_url: ${JSON.stringify(ollamaBaseUrl)}

${skillsConfig}

server:
  host: 127.0.0.1
  port: ${port}

sandbox:
  backend: podman
  allow_untrusted_images: false
  allow_post_create_command: ${allowPostCreateCommand}
  network: none
  require_resource_limits: false

consolidation:
  enabled: false
`;
  await writeFile(configPath, config.trimStart());

  let stdout = '';
  let stderr = '';
  let child = null;
  let launchCount = 0;
  const startDaemon = async () => {
    launchCount += 1;
    if (launchCount > 1) stderr = boundedLog(stderr, `\n[restart ${launchCount - 1}]\n`);
    child = spawn(binary, ['serve', '--config', configPath], {
      cwd: REPOSITORY_ROOT,
      env: {
        ...process.env,
        AXOCOATL_DATA_DIR: dataDirectory,
        AXOCOATL_SOCKET_PATH: socketPath,
        RUST_LOG: process.env.AXOCOATL_E2E_RUST_LOG || 'warn',
      },
      stdio: ['ignore', 'pipe', 'pipe'],
    });
    child.stdout.on('data', (chunk) => { stdout = boundedLog(stdout, chunk); });
    child.stderr.on('data', (chunk) => { stderr = boundedLog(stderr, chunk); });
    await waitForHealth(baseUrl, child, logs);
  };
  const logs = () => `stdout:\n${stdout}\nstderr:\n${stderr}`;

  try {
    await startDaemon();
    const alphaPath = await makeProject(projectsDirectory, 'alpha-project');
    const betaPath = await makeProject(projectsDirectory, 'beta-project');
    const alpha = await seedWorkspace(baseUrl, alphaPath, 'Alpha Workspace', [
      'Alpha First Session',
      'Alpha Second Session',
    ]);
    const beta = await seedWorkspace(baseUrl, betaPath, 'Beta Workspace', [
      'Beta Only Session',
    ]);
    const fixtures = { alpha, beta };
    return {
      baseUrl,
      runRoot,
      fixtures,
      logs,
      async createProjectWorkspace(folderName, workspaceName, options = {}) {
        const projectPath = await makeProject(projectsDirectory, folderName, options);
        const workspace = await api(baseUrl, 'POST', '/api/workspaces', {
          path: projectPath,
          name: workspaceName,
        });
        return { projectPath, workspace };
      },
      async listSessions() {
        return api(baseUrl, 'GET', '/api/sessions');
      },
      async pathExists(target) {
        try { await access(target); return true; } catch { return false; }
      },
      async restartWithSessionPatches(patches) {
        await stopProcess(child);
        for (const { id, update } of patches) {
          const sessionPath = path.join(dataDirectory, 'sessions', `${id}.json`);
          const session = JSON.parse(await readFile(sessionPath, 'utf8'));
          const updated = typeof update === 'function' ? (update(session) || session) : session;
          await writeFile(sessionPath, JSON.stringify(updated, null, 2));
        }
        await startDaemon();
        const sessions = await api(baseUrl, 'GET', '/api/sessions');
        for (const group of Object.values(fixtures)) {
          for (const fixture of group.sessions || []) {
            const current = sessions.find((session) => session.id === fixture.id);
            if (current) Object.assign(fixture, current);
          }
        }
        return sessions;
      },
      async restartWithSessionTurnEvents(events) {
        await stopProcess(child);
        const historyDirectory = path.join(dataDirectory, 'session-history');
        await mkdir(historyDirectory, { recursive: true });
        const ledger = Array.from(events || [], (event) => JSON.stringify(event)).join('\n');
        await writeFile(
          path.join(historyDirectory, 'turns.v1.jsonl'),
          ledger ? `${ledger}\n` : '',
        );
        await startDaemon();
        return api(baseUrl, 'GET', '/api/sessions');
      },
      async restart() {
        await stopProcess(child);
        await startDaemon();
      },
      async stop() {
        await stopProcess(child);
        // Only the unique mkdtemp directory created above is eligible for
        // cleanup. The guard prevents a malformed path from widening scope.
        if (path.basename(runRoot).startsWith('axocoatl-browser-e2e-')) {
          await rm(runRoot, { recursive: true, force: true });
        }
      },
    };
  } catch (error) {
    await stopProcess(child);
    if (path.basename(runRoot).startsWith('axocoatl-browser-e2e-')) {
      await rm(runRoot, { recursive: true, force: true });
    }
    throw error;
  }
}

export async function resolveChromiumExecutable() {
  if (process.env.PLAYWRIGHT_CHROMIUM_EXECUTABLE) {
    return process.env.PLAYWRIGHT_CHROMIUM_EXECUTABLE;
  }
  const macChrome = '/Applications/Google Chrome.app/Contents/MacOS/Google Chrome';
  try {
    await access(macChrome);
    return macChrome;
  } catch {
    return undefined;
  }
}
