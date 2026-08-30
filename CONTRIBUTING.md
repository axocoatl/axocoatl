# Contributing to Axocoatl

Thanks for your interest. Axocoatl is a Rust-native, local-first coding workbench
backed by an agent runtime. The app, runtime, and CLI live in one workspace.

## TL;DR

```bash
git clone https://github.com/axocoatl/axocoatl
cd axocoatl
cargo build -p axocoatl-cli --release  # binary: target/release/axocoatl
cargo test --workspace
./target/release/axocoatl doctor
```

If `doctor` is green, you're ready to develop. Open a PR against `main`.

## What we look for

Fixes, features, docs, examples — all welcome. Before opening a PR:

- Open an issue first for anything non-trivial (refactor, new top-level surface, new agent
  primitive, breaking API change) so we can align on direction.
- Small fixes (typos, doc tweaks, single-bug patches) can go straight to PR.

## The quality gate

Every PR has to pass the repository preflight. CI runs the same underlying
gates; run this command locally before pushing so GitHub is not the first place
that finds a deterministic failure.

```bash
./scripts/preflight.sh
```

The preflight deliberately checks the runtimes CI pins: Node 22, Python 3.13.7,
Go 1.26.2, active Rust 1.95.0, installed Rust 1.88.0, `cargo-audit` 0.22.2,
and `cargo-about` 0.9.1. It also requires `ffmpeg`/`ffprobe`, npm, Ruby, `jq`,
and Playwright's pinned Chromium download. Install the Rust prerequisites with:

```bash
rustup toolchain install 1.88.0
rustup toolchain install 1.95.0 --component clippy,rustfmt
cargo install cargo-audit --locked --version 0.22.2
cargo install cargo-about --locked --version '=0.9.1' --features cli
```

On Linux, the command performs the aarch64 link gate and therefore also needs
the `aarch64-unknown-linux-gnu` Rust target and `aarch64-linux-gnu-gcc`. On
macOS, it passes only when those cross-build inputs are unchanged from the
comparison commit; if they changed, run the full preflight in the pinned Linux
environment before opening the PR.

The focused Cargo commands remain useful while iterating, but they are not the
complete pre-PR gate.

The native portion deliberately runs Cargo with one job. This removes
host- and core-count-dependent rustdoc jobserver stalls so a local green result
has the same deterministic meaning as CI.

If you touch the browser app (`axocoatl-server/static/index.html` or
`axocoatl-server/static/ui/*`):

1. Run `cargo build -p axocoatl-cli` and restart the daemon.
2. Open `http://localhost:8080` and exercise the affected session journey through
   the one app. Do not validate a retired feature tab or a stale standalone page.
3. Check session restore, modules, Settings, the bottom terminal, and the browser
   console for errors where relevant.
4. Toggle light/dark/system and check responsive widths. Every affected surface
   must remain readable and keyboard reachable.

## Code style

- **Rust 2021**, MSRV `1.88`. Match the surrounding file's style.
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
- [`docs/PRODUCT.md`](docs/PRODUCT.md) — the one-app product model and terminology.
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
