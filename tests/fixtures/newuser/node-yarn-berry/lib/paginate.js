'use strict';

/**
 * Split `values` into pages of `size` values. Every page but the last is full;
 * the last page holds whatever is left.
 */
function paginate(values, size) {
  if (!Number.isInteger(size) || size <= 0) {
    throw new RangeError('size must be a positive integer');
  }
  const count = Math.ceil(values.length / size);
  const pages = [];
  for (let page = 0; page < count; page += 1) {
    const start = page * size;
    const length = page < count - 1 ? size : values.length % size;
    pages.push(values.slice(start, start + length));
  }
  return pages;
}

module.exports = { paginate };
