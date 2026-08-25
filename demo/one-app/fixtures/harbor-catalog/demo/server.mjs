import { createServer } from "node:http";
import { readFile } from "node:fs/promises";
import { extname, join } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const root = join(fileURLToPath(new URL("..", import.meta.url)));
const publicRoot = join(root, "public");
const port = 8765;

const seed = [
  { id: "harbor-chair", name: "Harbor Chair", description: "Oak desk chair", tags: ["office"] },
  { id: "signal-lamp", name: "Signal Lamp", description: "Green task light", tags: ["lighting"] },
  { id: "dock-shelf", name: "Dock Shelf", description: "Low steel shelf", tags: ["storage"] },
];

async function createFreshCatalog() {
  const moduleUrl = pathToFileURL(join(root, "lib/catalog.js"));
  moduleUrl.searchParams.set("demo-refresh", Date.now().toString());
  const { createCatalog } = await import(moduleUrl.href);
  return createCatalog(seed);
}

const contentTypes = {
  ".css": "text/css; charset=utf-8",
  ".html": "text/html; charset=utf-8",
  ".js": "text/javascript; charset=utf-8",
};

const server = createServer(async (request, response) => {
  try {
    const requestUrl = new URL(request.url ?? "/", "http://127.0.0.1");
    if (requestUrl.pathname === "/api/reproduction") {
      const catalog = await createFreshCatalog();
      const before = catalog.search("linen");
      catalog.upsert({
        id: "linen-shade",
        name: "Linen Shade",
        description: "Warm replacement lamp shade",
        tags: ["linen", "lighting"],
      });
      const after = catalog.search("linen");
      response.writeHead(200, { "content-type": "application/json; charset=utf-8" });
      response.end(JSON.stringify({ before, after, stale: after.length === 0 }));
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
  console.log(`Harbor Catalog preview: http://127.0.0.1:${port}`);
});
