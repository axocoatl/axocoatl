import assert from "node:assert/strict";
import { describe, test } from "node:test";

import {
  applyDiscount,
  orderSubtotal,
  orderTotal,
} from "./orders.js";

const order = (over = {}) => ({
  id: "ORD-DEMO",
  customer: "Demo Customer",
  items: [{ sku: "DEMO", name: "Demo item", quantity: 1, unitPrice: 10 }],
  ...over,
});

describe("orderSubtotal", () => {
  test("sums quantity by unit price", () => {
    assert.equal(orderSubtotal(order({
      items: [
        { sku: "A", name: "Keyboard", quantity: 1, unitPrice: 149.99 },
        { sku: "B", name: "Cable", quantity: 2, unitPrice: 12.5 },
      ],
    })), 174.99);
  });
});

describe("applyDiscount", () => {
  test("takes a percentage off", () => {
    assert.equal(applyDiscount(200, { kind: "percent", value: 10 }), 180);
  });

  test("takes an ordinary fixed amount off", () => {
    assert.equal(applyDiscount(200, { kind: "fixed", value: 50 }), 150);
  });

  test("never returns a negative payable total", () => {
    assert.equal(applyDiscount(30, { kind: "fixed", value: 50 }), 0);
  });

  test("passes through a subtotal without a discount", () => {
    assert.equal(applyDiscount(42.5), 42.5);
  });
});

describe("orderTotal", () => {
  test("applies tax after the discount", () => {
    assert.equal(orderTotal(order({
      items: [{ sku: "A", name: "Monitor", quantity: 1, unitPrice: 100 }],
      discount: { kind: "percent", value: 50 },
    }), 0.1), 55);
  });
});
