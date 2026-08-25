# Signal Desk

A dependency-free incident-correlation service for the Axocoatl multi-agent
Session demonstration.

The production sample shows one deployment emitting several signals. The seeded
correlator incorrectly turns those signals into separate incidents, which pages
the same on-call engineer more than once. The repository includes the production
sample, a runbook, a small browser preview, and executable checks.

```bash
npm run check  # intentionally red before the demo
npm run demo   # preview on http://127.0.0.1:8765
```
