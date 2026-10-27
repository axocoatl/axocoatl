'use strict';

const { test } = require('uvu');
const assert = require('uvu/assert');
const { paginate } = require('../src/paginate');

const numbers = (count) => Array.from({ length: count }, (_, index) => index + 1);

test('a partial last page keeps the items left over', () => {
  assert.equal(paginate(numbers(11), 5), [[1, 2, 3, 4, 5], [6, 7, 8, 9, 10], [11]]);
});

test('the last page is full when the total is a multiple of the page size', () => {
  assert.equal(paginate(numbers(10), 5), [[1, 2, 3, 4, 5], [6, 7, 8, 9, 10]]);
});

test('no items make no pages', () => {
  assert.equal(paginate([], 5), []);
});

test('a page size of zero is refused', () => {
  assert.throws(() => paginate(numbers(3), 0), RangeError);
});

test('paging skips the last item when the total is a multiple of the page size - specific test', () => {
  // This test specifically checks that when total=10 and page size=5,
  // all 10 items are included (no skipping)
  const result = paginate(numbers(10), 5);
  const allItems = result.flat();
  assert.equal(allItems, numbers(10));
  assert.equal(result, [[1, 2, 3, 4, 5], [6, 7, 8, 9, 10]]);
});

test.run();