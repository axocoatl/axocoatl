import assert from "node:assert/strict";
import { describe, test } from "node:test";

import { correlateSignals } from "./correlate.js";

const signal = (id, over = {}) => ({
  id,
  service: "checkout",
  deployment: "deploy-42",
  kind: "error-rate",
  severity: "ticket",
  observed_at: "2026-08-17T17:00:00Z",
  ...over,
});

describe("correlateSignals", () => {
  test("correlates evidence from one deployment into one incident", () => {
    const incidents = correlateSignals([
      signal("sig-a", { severity: "page" }),
      signal("sig-b", { kind: "latency", observed_at: "2026-08-17T17:03:00Z" }),
      signal("sig-c", { kind: "timeout", observed_at: "2026-08-17T17:05:00Z" }),
    ]);
    assert.equal(incidents.length, 1);
    assert.deepEqual(incidents[0].signal_ids, ["sig-a", "sig-b", "sig-c"]);
  });

  test("retains the strongest severity across correlated signals", () => {
    const [incident] = correlateSignals([
      signal("sig-a", { severity: "ticket" }),
      signal("sig-b", { severity: "page", observed_at: "2026-08-17T17:02:00Z" }),
    ]);
    assert.equal(incident.severity, "page");
  });

  test("starts a new incident outside the correlation window", () => {
    const incidents = correlateSignals([
      signal("sig-a"),
      signal("sig-b", { observed_at: "2026-08-17T17:11:00Z" }),
    ]);
    assert.equal(incidents.length, 2);
  });

  test("never correlates different services", () => {
    const incidents = correlateSignals([
      signal("sig-a"),
      signal("sig-b", { service: "search", observed_at: "2026-08-17T17:01:00Z" }),
    ]);
    assert.equal(incidents.length, 2);
  });

  test("never correlates different deployments", () => {
    const incidents = correlateSignals([
      signal("sig-a"),
      signal("sig-b", { deployment: "deploy-43", observed_at: "2026-08-17T17:01:00Z" }),
    ]);
    assert.equal(incidents.length, 2);
  });
});
