# Documentation-site instructions

These rules extend `sites/AGENTS.md`.

- Verify every command against the current CLI help and run important first-run commands
  when practical.
- Keep duplicated root README and docs-site material synchronized.
- Use `cargo build -p axocoatl-cli --release` for a source build of the binary.
- Do not publish volatile exact test counts or binary sizes unless they are generated and
  checked in the same change.
- Never expose sibling growth strategy, private memory, branch-held work, or internal
  security notes.

For site changes, install locked dependencies with `npm ci`, run `npm run build`, then
inspect the changed pages at desktop and mobile widths and verify links.
