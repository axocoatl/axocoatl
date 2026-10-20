#!/usr/bin/env bash
# Build and exercise the visible embedded browser product on this host.
#
# AXO_BROWSER_ENGINE=chromium (default) or webkit (the Safari engine) picks
# the Playwright engine every test runs on. AXO_BROWSER_PROFILE=debug
# (default) or release picks the build the tests drive; AXO_BROWSER_BINARY
# names another binary.
set -euo pipefail

[[ $# -eq 0 ]] || {
  echo 'Usage: verify-product-browser-gate.sh' >&2
  exit 2
}

engine=${AXO_BROWSER_ENGINE:-chromium}
case "$engine" in
  chromium | webkit) ;;
  *)
    echo "product-browser-gate: AXO_BROWSER_ENGINE must be chromium or webkit, not $engine" >&2
    exit 2
    ;;
esac
profile=${AXO_BROWSER_PROFILE:-debug}
case "$profile" in
  debug | release) ;;
  *)
    echo "product-browser-gate: AXO_BROWSER_PROFILE must be debug or release, not $profile" >&2
    exit 2
    ;;
esac

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
    npx --no-install playwright install --with-deps "$engine"
  else
    npx --no-install playwright install "$engine"
  fi
)

if [[ "$profile" == release ]]; then
  (cd "$repo_root" && cargo build --locked --release -p axocoatl-cli)
else
  (cd "$repo_root" && cargo build --locked -p axocoatl-cli)
fi
binary=${AXO_BROWSER_BINARY:-$repo_root/target/$profile/axocoatl}
[[ -x "$binary" ]] || {
  echo "product-browser-gate: built binary is missing or not executable: $binary" >&2
  exit 1
}
(cd "$test_root" && AXO_BROWSER_ENGINE="$engine" AXOCOATL_E2E_BINARY="$binary" npm run test:no-build)

echo "Product browser gate: PASS ($engine, $profile build)"
