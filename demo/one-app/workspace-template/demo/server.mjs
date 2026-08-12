import { createServer } from "node:http";
import { readFile } from "node:fs/promises";
import { extname, join } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const root = join(fileURLToPath(new URL("..", import.meta.url)));
const publicRoot = join(root, "public");
const port = 8765;

const orders = [
  {
    id: "ORD-2048",
    customer: "Ada Lovelace",
    label: "Keyboard launch kit",
    subtotal: 174.99,
    discount: { kind: "percent", value: 10 },
  },
  {
    id: "ORD-2051",
    customer: "Grace Hopper",
    label: "Conference cable pack",
    subtotal: 30,
    discount: { kind: "fixed", value: 50 },
  },
  {
    id: "ORD-2059",
    customer: "Katherine Johnson",
    label: "Monitor arm",
    subtotal: 89,
    discount: { kind: "fixed", value: 9 },
  },
];

const contentTypes = {
  ".css": "text/css; charset=utf-8",
  ".html": "text/html; charset=utf-8",
  ".js": "text/javascript; charset=utf-8",
};

async function currentOrders() {
  const moduleUrl = pathToFileURL(join(root, "lib/orders.js"));
  moduleUrl.searchParams.set("demo-refresh", Date.now().toString());
  const { applyDiscount } = await import(moduleUrl.href);
  return orders.map((order) => ({
    ...order,
    payable: applyDiscount(order.subtotal, order.discount),
  }));
}

const server = createServer(async (request, response) => {
  try {
    const requestUrl = new URL(request.url ?? "/", "http://127.0.0.1");
    if (requestUrl.pathname === "/api/orders") {
      response.writeHead(200, { "content-type": "application/json; charset=utf-8" });
      response.end(JSON.stringify(await currentOrders()));
      return;
    }

    const requested = requestUrl.pathname === "/" ? "/index.html" : requestUrl.pathname;
    if (!requested || requested.includes("..")) {
      response.writeHead(400);
      response.end("invalid path");
      return;
    }
    const path = join(publicRoot, requested);
    const body = await readFile(path);
    response.writeHead(200, {
      "content-type": contentTypes[extname(path)] ?? "application/octet-stream",
    });
    response.end(body);
  } catch (error) {
    response.writeHead(500, { "content-type": "text/plain; charset=utf-8" });
    response.end(`preview error: ${error.message}`);
  }
});

server.listen(port, "0.0.0.0", () => {
  console.log(`Northstar Supply preview: http://127.0.0.1:${port}`);
});
