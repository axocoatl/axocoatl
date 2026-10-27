const test = require("node:test");
const assert = require("node:assert/strict");
const { paginate } = require("../src/paging.js");

test("splits items into full pages", () => {
  assert.deepEqual(paginate([1, 2, 3, 4], 2), [
    [1, 2],
    [3, 4],
  ]);
});

test("keeps the last item on a page of its own", () => {
  assert.deepEqual(paginate([1, 2, 3, 4, 5], 2), [[1, 2], [3, 4], [5]]);
});

test("keeps items when there are fewer than a full page", () => {
  assert.deepEqual(paginate([1, 2], 3), [[1, 2]]);
});

test("returns no pages for empty input", () => {
  assert.deepEqual(paginate([], 2), []);
});

test("refuses a page size below one", () => {
  assert.throws(() => paginate([1], 0), RangeError);
});
