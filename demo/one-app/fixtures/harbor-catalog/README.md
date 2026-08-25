# Harbor Catalog

A small dependency-free product catalog for the Axocoatl **Several Ways** demo.

The seeded repository has a real cache-coherency defect: a search result can stay
stale after a catalog mutation. More than one production-quality repair is
reasonable—broad invalidation, targeted invalidation, or revisioned keys—so two
Agents can take meaningfully different routes while being judged by the same
contract.

```bash
npm run check  # intentionally red before the demo
npm run demo   # preview on http://127.0.0.1:8765
```

The fixture has no package dependencies.
