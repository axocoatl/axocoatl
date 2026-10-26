#!/usr/bin/env bash
# Build and exercise the visible embedded browser product on this host.
#
# AXO_BROWSER_ENGINE=chromium (default) or webkit (the Safari engine) picks
# the Playwright engine every test runs on. AXO_BROWSER_PROFILE=debug
# (default) or release picks the build the tests drive; AXO_BROWSER_BINARY
# names another binary, such as a release archive's, and nothing is built.
#
# AXO_BROWSER_WITHOUT_PODMAN=1 is for a host that has no Podman, such as a
# hosted macOS runner, which cannot run a Podman machine: the gate refuses
# to run where `podman` is found, and leaves out the journey files below
# that need a Session runtime on Podman. Every other journey must pass, and
# the macOS ones (WebKit's own cookie store, a Mac without Podman) run there.
set -euo pipefail

# The journey files whose journeys start a Session runtime on Podman. Other
# files skip their few such journeys when the image they need is missing.
needs_podman=(
  tests/loadout-audit-run.test.mjs
  tests/loadout-fix-run.test.mjs
  tests/loadouts.test.mjs
  tests/session-team-route.test.mjs
  tests/skill-automation.test.mjs
  tests/workbench.test.mjs
)

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
[[ -n "${AXO_BROWSER_BINARY:-}" ]] || command -v cargo >/dev/null 2>&1 || {
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

# WebKit (the Safari engine, the default browser on a Mac) is opt-in until it
# has passed this gate: with the webkit engine the gate installs
# Playwright's WebKit and the journeys that run per engine must pass in it
# too (AXOCOATL_E2E_WEBKIT=1 makes a missing WebKit a failure).
if [[ "$engine" == webkit ]]; then
  export AXOCOATL_E2E_WEBKIT=1
fi

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

if [[ -n "${AXO_BROWSER_BINARY:-}" ]]; then
  :
elif [[ "$profile" == release ]]; then
  (cd "$repo_root" && cargo build --locked --release -p axocoatl-cli)
else
  (cd "$repo_root" && cargo build --locked -p axocoatl-cli)
fi
binary=${AXO_BROWSER_BINARY:-$repo_root/target/$profile/axocoatl}
[[ -x "$binary" ]] || {
  echo "product-browser-gate: built binary is missing or not executable: $binary" >&2
  exit 1
}
files=()
if [[ "${AXO_BROWSER_WITHOUT_PODMAN:-}" == 1 ]]; then
  if command -v podman >/dev/null 2>&1; then
    echo "product-browser-gate: AXO_BROWSER_WITHOUT_PODMAN=1, but this host has $(command -v podman)" >&2
    exit 2
  fi
  for file in "$test_root"/tests/*.test.mjs; do
    file=tests/${file##*/}
    case " ${needs_podman[*]} " in
      *" $file "*) echo "product-browser-gate: no Podman here, so $file is left out" ;;
      *) files+=("$file") ;;
    esac
  done
fi
(cd "$test_root" && AXO_BROWSER_ENGINE="$engine" AXOCOATL_E2E_BINARY="$binary" npm run test:no-build -- ${files[@]+"${files[@]}"})

# The configuration and loadout snippets a new user copies from the README, the
# sites and the docs pass this build's own `validate` and `loadouts validate`
# (the YAML parser is a devDependency of sites/docs).
(cd "$repo_root/sites/docs" && npm ci --ignore-scripts --no-audit --no-fund)
node "$repo_root/scripts/check-copy-paste.mjs" --axocoatl "$binary"

echo "Product browser gate: PASS ($engine, $profile build${files[0]+, ${#files[@]} journey files without Podman})"
