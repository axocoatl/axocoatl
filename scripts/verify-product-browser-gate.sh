#!/usr/bin/env bash
# Build and exercise the visible embedded browser product on this host.
set -euo pipefail

[[ $# -eq 0 ]] || {
  echo 'Usage: verify-product-browser-gate.sh' >&2
  exit 2
}

script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
repo_root=$(CDPATH= cd -- "$script_dir/.." && pwd)
test_root="$repo_root/axocoatl-server/browser-tests"

if ! command -v cargo >/dev/null 2>&1; then
  cargo_bin=${CARGO_HOME:-${HOME:-}/.cargo}/bin
  [[ -x "$cargo_bin/cargo" ]] && export PATH="$cargo_bin:$PATH"
fi
command -v cargo >/dev/null 2>&1 || {
  echo 'product-browser-gate: cargo is required' >&2
  exit 1
}
command -v npm >/dev/null 2>&1 || {
  echo 'product-browser-gate: npm is required' >&2
  exit 1
}

git -C "$repo_root" ls-files --error-unmatch \
  axocoatl-server/browser-tests/package-lock.json \
  axocoatl-server/browser-tests/support/run-tests.mjs \
  axocoatl-server/browser-tests/tests/workbench.test.mjs >/dev/null

(
  cd "$test_root"
  npm ci
  npm audit --audit-level=high
  if [[ "${AXO_BROWSER_INSTALL_WITH_DEPS:-false}" == true ]]; then
    npx --no-install playwright install --with-deps chromium
  else
    npx --no-install playwright install chromium
  fi
)

(cd "$repo_root" && cargo build --locked -p axocoatl-cli)
binary=${AXO_BROWSER_BINARY:-$repo_root/target/debug/axocoatl}
[[ -x "$binary" ]] || {
  echo "product-browser-gate: built binary is missing or not executable: $binary" >&2
  exit 1
}
(cd "$test_root" && AXOCOATL_E2E_BINARY="$binary" npm run test:no-build)

echo 'Product browser gate: PASS'
