#!/usr/bin/env bash
# Deterministic native preflight shared by PR CI and the release workflow.
set -euo pipefail

[[ $# -eq 0 ]] || {
  echo 'Usage: verify-native-gate.sh' >&2
  exit 2
}

script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
repo_root=$(CDPATH= cd -- "$script_dir/.." && pwd)
cd "$repo_root"

if ! command -v cargo >/dev/null 2>&1; then
  cargo_bin=${CARGO_HOME:-${HOME:-}/.cargo}/bin
  [[ -x "$cargo_bin/cargo" ]] && export PATH="$cargo_bin:$PATH"
fi
command -v cargo >/dev/null 2>&1 || {
  echo 'verify-native-gate: cargo is required' >&2
  exit 1
}

cargo audit -D warnings
cargo fetch --locked --manifest-path axocoatl-cli/Cargo.toml
./scripts/sync-server-embedded-assets.sh --check
./scripts/check-third-party-licenses.sh
./scripts/test-install.sh
./scripts/test-release-artifact.sh
./scripts/test-release-plan.sh
./scripts/test-release-order.sh
./scripts/test-previous-public-release.sh
./scripts/test-publish-crate-resilient.sh
./scripts/test-prove-crate-index.sh
./scripts/test-prove-release-crates.sh
./scripts/test-public-release.sh
./scripts/test-cross-linux-gate.sh
cargo +1.88.0 check --locked --workspace --all-targets --all-features --jobs 1
cargo fmt --all -- --check
cargo clippy --locked --workspace --all-targets --all-features --jobs 1 -- -D warnings
cargo test --locked --workspace --jobs 1
cargo test --locked --doc --workspace --jobs 1
cargo build --locked --release -p axocoatl-cli --jobs 1
./scripts/test-server-package.sh

echo 'Native release gate: PASS'
