import assert from "node:assert/strict";
import { describe, test } from "node:test";

import { createCatalog } from "./catalog.js";

const seed = [
  { id: "harbor-chair", name: "Harbor Chair", description: "Oak desk chair", tags: ["office"] },
  { id: "signal-lamp", name: "Signal Lamp", description: "Green task light", tags: ["lighting"] },
  { id: "dock-shelf", name: "Dock Shelf", description: "Low steel shelf", tags: ["storage"] },
];

describe("catalog search", () => {
  test("matches names, descriptions, and tags without case sensitivity", () => {
    const catalog = createCatalog(seed);
    assert.deepEqual(catalog.search("OFFICE").map((item) => item.id), ["harbor-chair"]);
    assert.deepEqual(catalog.search("green").map((item) => item.id), ["signal-lamp"]);
  });

  test("returns copies rather than exposing mutable catalog state", () => {
    const catalog = createCatalog(seed);
    const [first] = catalog.search("chair");
    first.name = "mutated outside";
    assert.equal(catalog.search("chair")[0].name, "Harbor Chair");
  });

  test("reflects a newly added match after a query has been cached", () => {
    const catalog = createCatalog(seed);
    assert.deepEqual(catalog.search("linen"), []);
    catalog.upsert({
      id: "linen-shade",
      name: "Linen Shade",
      description: "Warm replacement lamp shade",
      tags: ["linen", "lighting"],
    });
    assert.deepEqual(catalog.search("linen").map((item) => item.id), ["linen-shade"]);
  });

  test("reflects an updated item after a query has been cached", () => {
    const catalog = createCatalog(seed);
    assert.deepEqual(catalog.search("blue"), []);
    catalog.upsert({
      id: "signal-lamp",
      name: "Signal Lamp",
      description: "Blue task light",
      tags: ["lighting"] ,
    });
    assert.deepEqual(catalog.search("blue").map((item) => item.id), ["signal-lamp"]);
  });

  test("does not return a removed item from a cached result", () => {
    const catalog = createCatalog(seed);
    assert.deepEqual(catalog.search("storage").map((item) => item.id), ["dock-shelf"]);
    catalog.remove("dock-shelf");
    assert.deepEqual(catalog.search("storage"), []);
  });

  test("preserves first-seen ordering", () => {
    const catalog = createCatalog(seed);
    assert.deepEqual(catalog.search("l").map((item) => item.id), [
      "signal-lamp",
      "dock-shelf",
    ]);
  });
});
