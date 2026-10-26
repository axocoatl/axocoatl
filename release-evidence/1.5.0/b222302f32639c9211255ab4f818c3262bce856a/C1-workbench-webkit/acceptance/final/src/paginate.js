'use strict';

/**
 * Split `items` into pages of `size` items. Every page but the last is full;
 * the last page holds whatever is left.
 */
function paginate(items, size) {
  if (!Number.isInteger(size) || size <= 0) {
    throw new RangeError('size must be a positive integer');
  }
  const count = Math.ceil(items.length / size);
  const pages = [];
  for (let page = 0; page < count; page += 1) {
    const start = page * size;
    const length = page < count - 1 ? size : (items.length % size || size);
    pages.push(items.slice(start, start + length));
  }
  return pages;
}

module.exports = { paginate };
