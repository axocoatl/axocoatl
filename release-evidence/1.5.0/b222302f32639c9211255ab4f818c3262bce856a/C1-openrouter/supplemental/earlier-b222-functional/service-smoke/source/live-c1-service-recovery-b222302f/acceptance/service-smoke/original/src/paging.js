// Split `items` into pages of `size` items each.
function paginate(items, size) {
  if (!Number.isInteger(size) || size < 1) {
    throw new RangeError("size must be a positive integer");
  }
  const pages = [];
  const count = Math.floor(items.length / size);
  for (let page = 0; page < count; page += 1) {
    pages.push(items.slice(page * size, (page + 1) * size));
  }
  return pages;
}

module.exports = { paginate };
