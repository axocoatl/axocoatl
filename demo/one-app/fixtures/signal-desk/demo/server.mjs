import { createServer } from "node:http";
import { readFile } from "node:fs/promises";
import { extname, join } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const root = join(fileURLToPath(new URL("..", import.meta.url)));
const publicRoot = join(root, "public");
const port = 8765;

const contentTypes = {
  ".css": "text/css; charset=utf-8",
  ".html": "text/html; charset=utf-8",
  ".js": "text/javascript; charset=utf-8",
};

async function currentIncidents() {
  const moduleUrl = pathToFileURL(join(root, "lib/correlate.js"));
  moduleUrl.searchParams.set("demo-refresh", Date.now().toString());
  const { correlateSignals } = await import(moduleUrl.href);
  const signals = JSON.parse(await readFile(join(root, "data/production-signals.json"), "utf8"));
  return correlateSignals(signals);
}

const server = createServer(async (request, response) => {
  try {
    const requestUrl = new URL(request.url ?? "/", "http://127.0.0.1");
    if (requestUrl.pathname === "/api/incidents") {
      response.writeHead(200, { "content-type": "application/json; charset=utf-8" });
      response.end(JSON.stringify(await currentIncidents()));
      return;
    }
    const requested = requestUrl.pathname === "/" ? "/index.html" : requestUrl.pathname;
    if (!requested || requested.includes("..")) {
      response.writeHead(400);
      response.end("invalid path");
      return;
    }
    const path = join(publicRoot, requested);
    response.writeHead(200, { "content-type": contentTypes[extname(path)] ?? "application/octet-stream" });
    response.end(await readFile(path));
  } catch (error) {
    response.writeHead(500, { "content-type": "text/plain; charset=utf-8" });
    response.end(`preview error: ${error.message}`);
  }
});

server.listen(port, "0.0.0.0", () => {
  console.log(`Signal Desk preview: http://127.0.0.1:${port}`);
});
