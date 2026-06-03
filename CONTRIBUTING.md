# Contributing to Axocoatl

Thanks for your interest. This is a Rust-native agentic AI framework — runtime,
dashboard, and CLI live in one workspace.

## TL;DR

```bash
git clone https://github.com/axocoatl/axocoatl
cd axocoatl
cargo build --release          # ~25 MB binary in target/release/axocoatl
cargo test --workspace         # 340+ tests
./target/release/axocoatl doctor
```

If `doctor` is green, you're ready to develop. Open a PR against `main`.

## What we look for

Fixes, features, docs, examples — all welcome. Before opening a PR:

- Open an issue first for anything non-trivial (refactor, new tab, new agent
  primitive, breaking API change) so we can align on direction.
- Small fixes (typos, doc tweaks, single-bug patches) can go straight to PR.

## The quality gate

Every PR has to pass these three checks. CI runs them; please run them locally
first to keep iteration tight.

```bash
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --workspace
```

If you touch the dashboard (`axocoatl-server/static/index.html`):
1. Run `cargo build -p axocoatl-cli` and restart the daemon.
2. Open `http://localhost:8080`, walk through every visible tab, watch the
   browser console for errors.
3. Toggle the theme (light/dark/system) — every surface must stay readable.

## Code style

- **Rust 2021**, MSRV `1.82`. Match the surrounding file's style.
- **No `unwrap()` / `expect()` in production paths.** They're fine in tests.
- **Errors**: use `thiserror` for per-crate error types, `anyhow` for
  application-layer glue. Don't construct ad-hoc `String` errors.
- **Comments explain WHY, not WHAT.** Don't restate what the code does;
  document hidden constraints, why a workaround exists, or a subtle invariant.
- **Don't add features behind feature flags unless they need to be optional.**
  The default build is what every user gets.

## Tests

- Crate-local tests live next to the code in `#[cfg(test)] mod tests`.
- Integration tests go in `crates/<crate>/tests/`.
- Benches live in `benches/`. Use `cargo bench` to run.
- New code without a test will be asked for one unless it's a one-line UI
  tweak.

## Commit & PR shape

- Subject line: imperative, ≤72 chars. "Add X", "Fix Y", "Refactor Z".
- Body: explain motivation, link the related issue, note any user-visible
  behavior change.
- One logical change per PR. If you're tempted to split, split.
- The PR description should answer: what changed, why, how was it tested.

## Project orientation

- [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md) — the mental model
  (lattice, actors, memory tiers, isolation).
- [`docs/LOCAL_TESTING_GUIDE.md`](docs/LOCAL_TESTING_GUIDE.md) — end-to-end
  walkthrough with Ollama.
- [`docs/TROUBLESHOOTING.md`](docs/TROUBLESHOOTING.md) — common runtime
  problems.

## Reporting bugs / security issues

- Bugs: open an issue using the bug template. Include `axocoatl doctor` output.
- Security: do **not** open a public issue. See
  [`SECURITY.md`](SECURITY.md) for the disclosure channel.

## License

Axocoatl is Apache-2.0. By contributing, you agree your changes are licensed
under the same terms.
