# Example instructions

These rules extend the repository-root instructions for `examples/`.

Examples demonstrate public, reachable behavior. They are not scratchpads and they do not
make an otherwise unwired capability shipped.

- Use the public API and the same runtime path a user would use.
- Mock only the provider boundary when an offline example needs deterministic LLM output.
- Make the output prove the advertised capability rather than only exit successfully.
- Keep the example README, config, command line, and code synchronized.
- Build and run the changed example. Do not rely only on a workspace compile.
- Verify names and flags against the current CLI. Remove deprecated examples instead of
  teaching a dead path.
- Do not use example-only helpers as evidence for a public runtime claim.

If an example changes what Axocoatl publicly supports, update the root README, docs site,
and changelog in the same change.
