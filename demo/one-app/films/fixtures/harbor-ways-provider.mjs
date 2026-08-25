// Deterministic local Ollama-compatible fixture for the accepted Several Ways film.
// It proves Axocoatl's execution lifecycle, not model quality. Run with Node.js;
// the server binds only to 127.0.0.1:18110 and is stateless by message history.
import { createServer } from "node:http";

const host = "127.0.0.1";
const port = 18110;
const startedAt = Date.now();
let requestCount = 0;

const broadInvalidationUpsertOld = `    if (index === -1) items = [...items, incoming];
    else items = items.map((item, itemIndex) => itemIndex === index ? incoming : item);
    // Seeded defect: cached queries can now describe the old catalog.`;

const broadInvalidationUpsertNew = `    if (index === -1) items = [...items, incoming];
    else items = items.map((item, itemIndex) => itemIndex === index ? incoming : item);
    cache.clear();`;

const broadInvalidationRemoveOld = `    items = items.filter((item) => item.id !== id);
    // Seeded defect: cached queries can still return the removed item.`;

const broadInvalidationRemoveNew = `    items = items.filter((item) => item.id !== id);
    cache.clear();`;

const revisionedCatalog = `const normalize = (value) => String(value ?? "").trim().toLowerCase();

const copyItem = (item) => ({ ...item, tags: [...(item.tags ?? [])] });

/**
 * A tiny in-memory catalog with cached searches.
 *
 * Cache entries carry the catalog revision they describe. Mutations advance
 * the revision, so stale values are replaced lazily on the next matching read.
 */
export function createCatalog(seed = []) {
  let items = seed.map(copyItem);
  let revision = 0;
  const cache = new Map();

  function search(query) {
    const key = normalize(query);
    const cached = cache.get(key);
    if (cached?.revision === revision) return cached.result.map(copyItem);
    const result = items.filter((item) => {
      const haystack = [item.name, item.description, ...(item.tags ?? [])]
        .map(normalize)
        .join(" ");
      return haystack.includes(key);
    });
    cache.set(key, { revision, result: result.map(copyItem) });
    return result.map(copyItem);
  }

  function upsert(next) {
    const incoming = copyItem(next);
    const index = items.findIndex((item) => item.id === incoming.id);
    if (index === -1) items = [...items, incoming];
    else items = items.map((item, itemIndex) => itemIndex === index ? incoming : item);
    revision += 1;
  }

  function remove(id) {
    items = items.filter((item) => item.id !== id);
    revision += 1;
  }

  return {
    search,
    upsert,
    remove,
    all: () => items.map(copyItem),
  };
}
`;

const routes = {
  orientation: [
    { name: "read_file", arguments: { path: "package.json" } },
  ],
  minimal: [
    { name: "read_file", arguments: { path: "lib/catalog.js" } },
    {
      name: "edit_file",
      arguments: {
        path: "lib/catalog.js",
        old: broadInvalidationUpsertOld,
        new: broadInvalidationUpsertNew,
      },
    },
    {
      name: "edit_file",
      arguments: {
        path: "lib/catalog.js",
        old: broadInvalidationRemoveOld,
        new: broadInvalidationRemoveNew,
      },
    },
    { name: "bash", arguments: { command: "npm run check" } },
  ],
  defender: [
    { name: "grep", arguments: { pattern: "cache|upsert|remove", path: "lib/catalog.js" } },
    { name: "read_file", arguments: { path: "lib/catalog.test.js" } },
    { name: "read_file", arguments: { path: "lib/catalog.js" } },
    { name: "write_file", arguments: { path: "lib/catalog.js", content: revisionedCatalog } },
    { name: "bash", arguments: { command: "npm run check" } },
  ],
};

const finalAnswers = {
  orientation: "The npm scripts are check (runs node --test lib/*.test.js) and demo (runs node demo/server.mjs).",
  minimal: `Diagnosis
Cached search results survive every catalog mutation, so repeated queries describe the old item set.

Decision
Invalidate the search cache after each upsert or removal. This is the smallest contract-preserving repair and keeps caching for reads between mutations.

Evidence
The production-only patch changes lib/catalog.js. npm run check passes all 6 tests, including additions, updates, removals, copy isolation, case-insensitive matching, and ordering.

Tradeoff
Every mutation clears all cached queries. That favors simple, auditable coherence over retaining unrelated entries.`,
  defender: `Diagnosis
The cache has no dependency relationship to catalog mutations, so entries can remain valid-looking after their matching item is added, changed, or removed.

Decision
Version cached results against a catalog revision. Each mutation advances that revision, and searches lazily refresh a key whose cached result belongs to an older state.

Evidence
The route inspected the cache surface and all six contract tests, then changed only lib/catalog.js. npm run check passes 6/6 with no test edits.

Tradeoff
Revisioned entries make mutation work constant-time and keep cache storage bounded per query, but every previously cached query is recomputed once after a write.`,
};

function bodyText(body) {
  return (body.messages ?? []).map((message) => String(message.content ?? "")).join("\n");
}

function systemText(body) {
  return (body.messages ?? [])
    .filter((message) => message.role === "system")
    .map((message) => String(message.content ?? ""))
    .join("\n");
}

function toolRound(body) {
  return (body.messages ?? []).filter(
    (message) => message.role === "assistant" && Array.isArray(message.tool_calls) && message.tool_calls.length,
  ).length;
}

function completion(content, model, finishReason = "stop", toolCalls = undefined) {
  const message = { role: "assistant", content };
  if (toolCalls) message.tool_calls = toolCalls;
  return {
    id: `chatcmpl-fixture-${requestCount}`,
    object: "chat.completion",
    created: Math.floor(Date.now() / 1000),
    model,
    choices: [{ index: 0, message, finish_reason: finishReason }],
    usage: { prompt_tokens: 180, completion_tokens: 72, total_tokens: 252 },
  };
}

function toolCall(name, args, id) {
  return {
    id,
    type: "function",
    function: { name, arguments: JSON.stringify(args) },
  };
}

function nonStreamingResponse(body) {
  const text = bodyText(body);
  const model = body.model ?? "harbor-ways-fixture";

  if (text.includes("Read the file README.md. Call the tool")) {
    return completion("", model, "tool_calls", [
      toolCall("read_file", { path: "README.md" }, `probe-${requestCount}`),
    ]);
  }

  if (text.includes("Name the files you must read to write a precise plan")) {
    return completion(JSON.stringify({ files: ["lib/catalog.js", "lib/catalog.test.js", "AXOCOATL.md"] }), model);
  }

  if (text.includes("Write an implementation plan for this task")) {
    return completion(JSON.stringify({
      summary: "Restore search-cache coherence across catalog additions, updates, and removals without changing the public API or tests.",
      steps: [{
        path: "lib/catalog.js",
        change: "Keep cached search reads, but invalidate every affected cached query after upsert and remove so the next search is derived from current items.",
      }],
      constraints: [
        "Preserve createCatalog, search, upsert, remove, and all public return shapes.",
        "Do not add, modify, or delete tests.",
        "Keep copy isolation and first-seen ordering intact.",
      ],
      acceptance: [
        "npm run check passes all six existing tests.",
        "Both independent implementations change production code and retain caching between mutations.",
      ],
    }), model);
  }

  if (text.includes("Rank every candidate (rank 1 = best)")) {
    const indices = [...text.matchAll(/^## Candidate (\d+)$/gm)].map((match) => Number(match[1]));
    const [first = 0, second = 1] = [...new Set(indices)];
    return completion(JSON.stringify({
      winner: first,
      reasoning: "Both candidates pass the same protected checks. The broad invalidation patch wins because it is the smallest complete coherence boundary; the revisioned strategy makes invalidation lazy but introduces extra cache-entry state.",
      candidates: [
        {
          index: first,
          rank: 1,
          approach: "Clear the search cache after each catalog mutation.",
          tradeoffs: "Gains minimal, obvious correctness; gives up unrelated warm cache entries after a write.",
        },
        {
          index: second,
          rank: 2,
          approach: "Attach a catalog revision to each cached query result and refresh stale entries lazily.",
          tradeoffs: "Gains constant-time mutation and bounded per-query storage; gives up simplicity and recomputes each used key after a write.",
        },
      ],
    }), model);
  }

  return completion(JSON.stringify({ ok: true }), model);
}

function streamingStep(body) {
  const text = bodyText(body);
  // Plan first and Judge now use the same streaming provider path as normal
  // Turns. Keep their structured JSON responses out of the repository-tool
  // route; otherwise the fixture would emit a coding tool call to a scope that
  // intentionally has no repository tools.
  if (text.includes("Name the files you must read to write a precise plan")
    || text.includes("Write an implementation plan for this task")
    || text.includes("Rank every candidate (rank 1 = best)")) {
    const response = nonStreamingResponse(body);
    return {
      routeName: "control",
      round: 0,
      content: response.choices[0].message.content,
    };
  }
  const system = systemText(body);
  const routeName = text.includes("Read package.json and report its npm scripts in one sentence")
    ? "orientation"
    : system.includes("defensive JavaScript engineer")
      ? "defender"
      : "minimal";
  const round = toolRound(body);
  const route = routes[routeName];
  if (round < route.length) {
    return { routeName, round, call: route[round] };
  }
  return { routeName, round, content: finalAnswers[routeName] };
}

function sendJson(response, status, value) {
  const data = JSON.stringify(value);
  response.writeHead(status, {
    "content-type": "application/json; charset=utf-8",
    "content-length": Buffer.byteLength(data),
  });
  response.end(data);
}

function sendSse(response, body, step) {
  const model = body.model ?? "harbor-ways-fixture";
  const base = {
    id: `chatcmpl-fixture-${requestCount}`,
    object: "chat.completion.chunk",
    created: Math.floor(Date.now() / 1000),
    model,
  };
  response.writeHead(200, {
    "content-type": "text/event-stream; charset=utf-8",
    "cache-control": "no-cache",
    connection: "keep-alive",
  });

  setTimeout(() => {
    if (step.call) {
      const call = toolCall(step.call.name, step.call.arguments, `${step.routeName}-${step.round}`);
      response.write(`data: ${JSON.stringify({
        ...base,
        choices: [{
          index: 0,
          delta: { role: "assistant", tool_calls: [{ index: 0, ...call }] },
          finish_reason: null,
        }],
      })}\n\n`);
      response.write(`data: ${JSON.stringify({
        ...base,
        choices: [{ index: 0, delta: {}, finish_reason: "tool_calls" }],
        usage: { prompt_tokens: 180 + step.round * 40, completion_tokens: 18, total_tokens: 198 + step.round * 40 },
      })}\n\n`);
    } else {
      response.write(`data: ${JSON.stringify({
        ...base,
        choices: [{ index: 0, delta: { role: "assistant", content: step.content }, finish_reason: null }],
      })}\n\n`);
      response.write(`data: ${JSON.stringify({
        ...base,
        choices: [{ index: 0, delta: {}, finish_reason: "stop" }],
        usage: { prompt_tokens: 420, completion_tokens: 140, total_tokens: 560 },
      })}\n\n`);
    }
    response.end("data: [DONE]\n\n");
  }, 900);
}

const server = createServer((request, response) => {
  requestCount += 1;
  const url = new URL(request.url ?? "/", `http://${host}:${port}`);

  if (request.method === "GET" && url.pathname === "/api/tags") {
    return sendJson(response, 200, {
      models: [{
        name: "harbor-ways-fixture",
        model: "harbor-ways-fixture",
        modified_at: "2026-08-20T00:00:00Z",
        size: 1,
        digest: "sha256:axocoatl-harbor-deterministic",
      }],
    });
  }

  if (request.method === "GET" && url.pathname === "/fixture/state") {
    return sendJson(response, 200, {
      ok: true,
      fixture: "axocoatl-harbor-ways-v1",
      uptime_ms: Date.now() - startedAt,
      requests: requestCount,
      routes: Object.fromEntries(Object.entries(routes).map(([name, steps]) => [name, steps.map((step) => step.name)])),
    });
  }

  if (request.method !== "POST" || url.pathname !== "/v1/chat/completions") {
    return sendJson(response, 404, { error: { message: "fixture endpoint not found" } });
  }

  let raw = "";
  request.setEncoding("utf8");
  request.on("data", (chunk) => { raw += chunk; });
  request.on("end", () => {
    let body;
    try {
      body = JSON.parse(raw);
    } catch {
      return sendJson(response, 400, { error: { message: "invalid JSON" } });
    }
    if (body.stream) return sendSse(response, body, streamingStep(body));
    return sendJson(response, 200, nonStreamingResponse(body));
  });
});

server.listen(port, host, () => {
  process.stdout.write(`Axocoatl Harbor deterministic provider: http://${host}:${port}\n`);
});

for (const signal of ["SIGINT", "SIGTERM"]) {
  process.on(signal, () => server.close(() => process.exit(0)));
}
