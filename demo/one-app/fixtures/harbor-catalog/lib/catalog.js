const normalize = (value) => String(value ?? "").trim().toLowerCase();

const copyItem = (item) => ({ ...item, tags: [...(item.tags ?? [])] });

/**
 * A tiny in-memory catalog with cached searches.
 *
 * The seeded mutation path contains the cache-coherency defect used in the
 * Several Ways demonstration. The public API is intentionally neutral about
 * which invalidation strategy should repair it.
 */
export function createCatalog(seed = []) {
  let items = seed.map(copyItem);
  const cache = new Map();

  function search(query) {
    const key = normalize(query);
    if (cache.has(key)) return cache.get(key).map(copyItem);
    const result = items.filter((item) => {
      const haystack = [item.name, item.description, ...(item.tags ?? [])]
        .map(normalize)
        .join(" ");
      return haystack.includes(key);
    });
    cache.set(key, result.map(copyItem));
    return result.map(copyItem);
  }

  function upsert(next) {
    const incoming = copyItem(next);
    const index = items.findIndex((item) => item.id === incoming.id);
    if (index === -1) items = [...items, incoming];
    else items = items.map((item, itemIndex) => itemIndex === index ? incoming : item);
    // Seeded defect: cached queries can now describe the old catalog.
  }

  function remove(id) {
    items = items.filter((item) => item.id !== id);
    // Seeded defect: cached queries can still return the removed item.
  }

  return {
    search,
    upsert,
    remove,
    all: () => items.map(copyItem),
  };
}
