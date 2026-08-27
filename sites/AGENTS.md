# Public site instructions

These rules extend the repository-root instructions for `sites/`.

Read `../docs/PRODUCT.md` and `../BRAND.md` before changing public copy. Product facts
come from current code and `docs/PRODUCT.md`; `BRAND.md` governs voice and visuals.

## Tell one product story

The first-run journey is `install -> onboard -> doctor -> dev -> open / -> create or
resume a workspace session -> work through chat`. Do not send a user to retired peer
destinations such as Studio, Sessions, Agents, Skills, or MCP tabs. Configuration lives
in Settings. Runtime concepts remain important, but they explain how the workbench is
trustworthy rather than replacing the user journey.

`axocoatl onboard` configures the product for the current OS user. It creates no
project or repository folder. The person authorizes repositories through **Open
workspace…** after the app starts; project-local configuration is an explicit advanced
override.

Lead with the session loop: one chat spine, optional parallel attempts, checks,
comparison, keep, and git review. Use the product vocabulary defined in the root
instructions.

## Claim discipline

- Verify a capability in both backend and reachable UI before calling it shipped.
- Do not call a mock or screenshot `1:1`, exact, or faithful unless it is compared to
  the current app in the same change.
- Keep install commands aligned with the actual package. A source build of the binary is
  `cargo build -p axocoatl-cli --release`, not a root `cargo build`.
- Recheck platform matrix, provider list, binary size, test count, privacy language, and
  outbound network behavior before publishing exact claims.
- When code and copy differ, correct all affected docs and marketing pages together.

Use a dry, direct, technically specific voice. Avoid hype, fabricated social proof,
competitor callouts, and claims based only on planned work.
