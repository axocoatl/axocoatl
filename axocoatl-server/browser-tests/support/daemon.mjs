import { spawn } from 'node:child_process';
import { createHash } from 'node:crypto';
import { createServer } from 'node:net';
import {
  access,
  copyFile,
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
  const configuredTimeout = Number(process.env.AXOCOATL_E2E_STARTUP_TIMEOUT_MS || 75_000);
  if (!Number.isSafeInteger(configuredTimeout) || configuredTimeout < 1 || configuredTimeout > 300_000) {
    throw new Error('AXOCOATL_E2E_STARTUP_TIMEOUT_MS must be an integer from 1 to 300000.');
  }
  const deadline = Date.now() + configuredTimeout;
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

// Each daemon requires its per-daemon local API token (`<data>/local-api-token`).
// Running daemons are registered by origin so Node-side `fetch` calls and
// browser contexts in this process can authenticate without per-call wiring.
const LOCAL_TOKEN_FILE = 'local-api-token';
const authorizedDaemons = new Map();
const unauthenticatedFetch = globalThis.fetch;

function daemonForUrl(url) {
  try {
    return authorizedDaemons.get(new URL(url).origin) || null;
  } catch {
    return null;
  }
}

function hasCredential(headers) {
  return headers.has('authorization') || headers.has('x-api-key') || headers.has('cookie');
}

// Adds the bearer token to requests for a registered daemon unless the caller
// chose a credential itself. The workbench redirects `/` from a loopback IP to
// `localhost`; fetch would drop Authorization on that cross-origin hop, so
// redirects between registered origins are followed here with the header kept.
async function authorizedFetch(input, init = undefined) {
  if (typeof input !== 'string' && !(input instanceof URL)) return unauthenticatedFetch(input, init);
  let url = String(input);
  const daemon = daemonForUrl(url);
  if (!daemon) return unauthenticatedFetch(input, init);
  const headers = new Headers(init?.headers);
  if (!hasCredential(headers)) headers.set('authorization', `Bearer ${daemon.token}`);
  const redirect = init?.redirect || 'follow';
  let method = (init?.method || 'GET').toUpperCase();
  let response = await unauthenticatedFetch(url, { ...init, headers, redirect: 'manual' });
  for (let hop = 0; redirect === 'follow' && hop < 5; hop += 1) {
    const location = response.headers.get('location');
    if (![301, 302, 303, 307, 308].includes(response.status) || !location) break;
    url = new URL(location, url).href;
    if (response.status === 303) method = 'GET';
    if (!daemonForUrl(url) || !['GET', 'HEAD'].includes(method)) {
      return unauthenticatedFetch(url, { ...init, method });
    }
    response = await unauthenticatedFetch(url, { ...init, method, headers, redirect: 'manual' });
  }
  return response;
}

function registerDaemon(port, token) {
  const entry = { port, token };
  for (const host of ['127.0.0.1', 'localhost']) authorizedDaemons.set(`http://${host}:${port}`, entry);
  globalThis.fetch = authorizedFetch;
}

function unregisterDaemon(port) {
  for (const host of ['127.0.0.1', 'localhost']) authorizedDaemons.delete(`http://${host}:${port}`);
  if (authorizedDaemons.size === 0) globalThis.fetch = unauthenticatedFetch;
}

// Give a Playwright context the sign-in cookie of every daemon running in this
// process, exactly as `/?token=` would set it. Component-mode runtimes
// (AXOCOATL_COMPONENT_BASE_URL) register nothing, so this is then a no-op.
export async function authorizeContext(context) {
  const cookies = [];
  for (const { port, token } of new Set(authorizedDaemons.values())) {
    for (const host of ['localhost', '127.0.0.1']) {
      cookies.push({
        name: `axocoatl-token-${port}`,
        value: token,
        url: `http://${host}:${port}/`,
        httpOnly: true,
        sameSite: 'Strict',
      });
    }
  }
  if (cookies.length) await context.addCookies(cookies);
  return context;
}

// `browser.newContext(options)` with the sign-in cookies already in place.
export async function newAuthorizedContext(browser, options) {
  return authorizeContext(await browser.newContext(options));
}

async function api(baseUrl, method, pathname, body) {
  const headers = new Headers();
  if (body !== undefined) headers.set('content-type', 'application/json');
  const daemon = daemonForUrl(baseUrl);
  if (daemon) headers.set('authorization', `Bearer ${daemon.token}`);
  const response = await unauthenticatedFetch(`${baseUrl}${pathname}`, {
    method,
    headers,
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

// Match the immutable artifacts required by axocoatl-memory/src/neural.rs.
const EMBEDDING_MODEL_FILES = [
  ['config.json', 612, '953f9c0d463486b10a6871cc2fd59f223b2c70184f49815e7efbcab5d8908b41'],
  ['tokenizer.json', 466247, 'be50c3628f2bf5bb5e3a7f17b1f74611b2561a3a27eeab05e5aa30f411572037'],
  ['model.safetensors', 90868376, '53aa51172d142c89d9012cce15ae4d6cc0ca6895895114379cacb4fab128d9db'],
];
async function completeModelCache(directory) {
  if (!directory) return false;
  try { await Promise.all(EMBEDDING_MODEL_FILES.map(([name]) => access(path.join(directory, name)))); return true; }
  catch { return false; }
}
async function retainVerifiedFixtureModel(dataDirectory, cache) {
  if (!cache || await completeModelCache(cache)) return;
  const model = path.join(dataDirectory, 'models', 'all-MiniLM-L6-v2');
  if (!await completeModelCache(model)) return;
  const verified = [];
  for (const [name, size, digest] of EMBEDDING_MODEL_FILES) {
    const bytes = await readFile(path.join(model, name));
    if (bytes.length !== size || createHash('sha256').update(bytes).digest('hex') !== digest) return;
    verified.push([name, bytes]);
  }
  await mkdir(cache, {recursive:true});
  for (const [name, bytes] of verified) {
    try { await writeFile(path.join(cache, name), bytes, {flag:'wx'}); }
    catch (error) { if (error.code !== 'EEXIST') throw error; }
  }
}

export async function launchTestDaemon({
  nativeDataRoot = false,
  allowPostCreateCommand = false,
  ollamaBaseUrl = 'http://127.0.0.1:9',
  skills = [],
  agentTools = [],
  agentWrites,
  helpers = [],
  extraAgents = [],
  workflows = [],
  port: requestedPort = Number(process.env.AXOCOATL_E2E_PORT) || 0,
} = {}) {
  const runRoot = await mkdtemp(path.join(tmpdir(), 'axocoatl-browser-e2e-'));
  const dataDirectory = path.join(runRoot, 'data');
  const projectsDirectory = path.join(runRoot, 'projects');
  if (!nativeDataRoot) {
    await mkdir(dataDirectory, { recursive: true });
    // Isolated legacy fixtures may reuse the immutable model artifacts. The
    // daemon still verifies every byte; no Session or authority state is copied.
    // Native first-install fixtures must keep their data root genuinely absent.
    const modelCache = process.env.AXOCOATL_E2E_MODEL_CACHE;
    if (await completeModelCache(modelCache)) {
      const destination = path.join(dataDirectory, 'models', 'all-MiniLM-L6-v2');
      await mkdir(destination, { recursive: true });
      for (const [name] of EMBEDDING_MODEL_FILES) {
        await copyFile(path.join(modelCache, name), path.join(destination, name));
      }
    }
  }
  await mkdir(projectsDirectory, { recursive: true });

  const port = requestedPort || await freeLoopbackPort();
  const baseUrl = `http://127.0.0.1:${port}`;
  const configPath = path.join(runRoot, 'axocoatl.e2e.yaml');
  // The daemon runs in runRoot with a relative socket path, so the Unix
  // socket stays below SUN_LEN however long TMPDIR is.
  const socketPath = './axocoatl.sock';
  const binary = process.env.AXOCOATL_E2E_BINARY
    ? path.resolve(process.env.AXOCOATL_E2E_BINARY)
    : path.join(REPOSITORY_ROOT, 'target', 'debug', 'axocoatl');
  const skillsConfig = skills.length ? `
skills:
${skills.map((skill) => `  - id: ${JSON.stringify(skill.id)}
    name: ${JSON.stringify(skill.name)}
    description: ${JSON.stringify(skill.description)}
    emits: ${JSON.stringify(skill.emits || [])}`).join('\n')}
` : '';
  const config = `
agents:
  - id: browser-test-coder
    name: Browser Test Coder
    provider: ollama
    model: browser-test-model
    system_prompt: Browser regression fixture. No turns are executed.
    depends_on: []
    tools: ${JSON.stringify(agentTools)}${agentWrites === undefined ? '' : `
    writes: ${JSON.stringify(agentWrites)}`}${helpers.map((helper) => `
  - id: ${JSON.stringify(helper.id)}
    name: ${JSON.stringify(helper.name)}
    provider: ollama
    model: browser-test-model
    role: worker
    tools: ${JSON.stringify(helper.tools || [])}${helper.writes === undefined ? '' : `
    writes: ${JSON.stringify(helper.writes)}`}`).join('')}
${extraAgents.map((agent) => `  - id: ${JSON.stringify(agent.id)}
    name: ${JSON.stringify(agent.name || agent.id)}
    role: ${JSON.stringify(agent.role || 'autonomous')}
    provider: ollama
    model: browser-test-model
    system_prompt: Browser regression fixture. No turns are executed.
    depends_on: ${JSON.stringify(agent.dependsOn || [])}
    tools: ${JSON.stringify(agent.tools || [])}${agent.writes === undefined ? '' : `
    writes: ${JSON.stringify(agent.writes)}`}`).join('\n')}
${workflows.length ? `workflows:
${workflows.map((workflow) => `  - id: ${JSON.stringify(workflow.id)}
    name: ${JSON.stringify(workflow.name || workflow.id)}
    entry_point: ${JSON.stringify(workflow.entryPoint)}
    agents: ${JSON.stringify(workflow.agents)}`).join('\n')}` : ''}

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
  let token = null;
  const startDaemon = async () => {
    launchCount += 1;
    if (launchCount > 1) stderr = boundedLog(stderr, `\n[restart ${launchCount - 1}]\n`);
    child = spawn(binary, ['serve', '--config', configPath], {
      cwd: runRoot,
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
    // The daemon creates the token on first start and reuses it on restart.
    token = (await readFile(path.join(dataDirectory, LOCAL_TOKEN_FILE), 'utf8')).trim();
    registerDaemon(port, token);
    await retainVerifiedFixtureModel(dataDirectory, process.env.AXOCOATL_E2E_MODEL_CACHE);
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
      get token() { return token; },
      cookieName: `axocoatl-token-${port}`,
      get signInUrl() { return `http://localhost:${port}/?token=${token}`; },
      // A plain fetch that sends no Axocoatl credential.
      fetchWithoutToken(pathname, init) {
        return unauthenticatedFetch(`${baseUrl}${pathname}`, init);
      },
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
      async listAutomations() {
        return api(baseUrl, 'GET', '/api/automations');
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
        unregisterDaemon(port);
        // Only the unique mkdtemp directory created above is eligible for
        // cleanup. The guard prevents a malformed path from widening scope.
        if (path.basename(runRoot).startsWith('axocoatl-browser-e2e-')) {
          await rm(runRoot, { recursive: true, force: true });
        }
      },
    };
  } catch (error) {
    await stopProcess(child);
    unregisterDaemon(port);
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
