#!/usr/bin/env bash
# Exercise the browser gate's file selection without builds or browsers.
set -euo pipefail

script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
repo_root=$(CDPATH= cd -- "$script_dir/.." && pwd)
work_dir=$(mktemp -d "${TMPDIR:-/tmp}/axocoatl-browser-gate-test.XXXXXX")
trap 'rm -rf -- "$work_dir"' EXIT HUP INT TERM
fixture="$work_dir/repo"
bin="$work_dir/bin"
mkdir -p "$fixture/scripts" "$fixture/sites/docs" \
  "$fixture/axocoatl-server/browser-tests/tests" "$bin" "$work_dir/with-podman"
cp "$script_dir/verify-product-browser-gate.sh" "$fixture/scripts/"
for file in "$repo_root/axocoatl-server/browser-tests/tests/"*.test.mjs; do
  : > "$fixture/axocoatl-server/browser-tests/tests/${file##*/}"
done

fail() {
  echo "test-product-browser-gate: $*" >&2
  exit 1
}

# Only these commands are visible to the gate, so the host's real Podman,
# npm and Cargo cannot affect the selection or start work.
ln -s "$(command -v dirname)" "$bin/dirname"
for command in git npx node cargo; do
  printf '%s\n' '#!/bin/bash' 'exit 0' > "$bin/$command"
done
printf '%s\n' '#!/bin/bash' 'exit 0' > "$fixture/axocoatl"
printf '%s\n' '#!/bin/bash' 'exit 0' > "$work_dir/with-podman/podman"
cat > "$bin/npm" <<'SH'
#!/bin/bash
set -eu
if [[ "${1:-}" == run && "${2:-}" == test:no-build && "${3:-}" == -- ]]; then
  shift 3
  : > "$AXO_BROWSER_SELECTION_LOG"
  if [[ $# -gt 0 ]]; then printf '%s\n' "$@" > "$AXO_BROWSER_SELECTION_LOG"; fi
fi
SH
for command in git npx node cargo npm; do chmod +x "$bin/$command"; done
chmod +x "$fixture/axocoatl" "$work_dir/with-podman/podman"

run_gate() {
  local name=$1 engine=$2 without_podman=$3 podman_path=${4:-}
  PATH="$bin${podman_path:+:$podman_path}" \
    AXO_BROWSER_ENGINE="$engine" AXO_BROWSER_PROFILE=release \
    AXO_BROWSER_BINARY="$fixture/axocoatl" \
    AXO_BROWSER_WITHOUT_PODMAN="$without_podman" \
    AXO_BROWSER_SELECTION_LOG="$work_dir/$name.files" \
    /bin/bash "$fixture/scripts/verify-product-browser-gate.sh" > "$work_dir/$name.log" 2>&1
}

run_gate hosted-mac webkit 1
[[ -s "$work_dir/hosted-mac.files" ]] || fail 'hosted Mac did not select journeys'
for file in "$fixture/axocoatl-server/browser-tests/tests/"*.test.mjs; do
  file=tests/${file##*/}
  case "$file" in
    tests/loadout-audit-run.test.mjs | tests/loadout-fix-run.test.mjs | \
    tests/loadouts.test.mjs | tests/session-team-route.test.mjs | \
    tests/skill-automation.test.mjs | tests/workbench.test.mjs)
      if grep -Fxq "$file" "$work_dir/hosted-mac.files"; then
        fail "hosted Mac selected Podman-dependent $file"
      fi
      ;;
    *) grep -Fxq "$file" "$work_dir/hosted-mac.files" \
      || fail "hosted Mac lost independent $file" ;;
  esac
done

# Both Linux engines (including Chromium on arm64) and actual-Mac C1 use
# the runner's complete default suite, including skill-automation.
for engine in chromium webkit; do
  run_gate "full-$engine" "$engine" '' "$work_dir/with-podman"
  [[ -f "$work_dir/full-$engine.files" && ! -s "$work_dir/full-$engine.files" ]] \
    || fail "$engine with Podman no longer runs the complete default suite"
done

if run_gate contradictory webkit 1 "$work_dir/with-podman"; then
  fail 'no-Podman selection was accepted on a host with Podman'
fi
grep -Fq 'AXO_BROWSER_WITHOUT_PODMAN=1, but this host has' "$work_dir/contradictory.log" \
  || fail 'the contradictory selection did not fail for the expected reason'
[[ ! -e "$work_dir/contradictory.files" ]] || fail 'the contradictory selection ran journeys'

echo 'Product browser selection: PASS (hosted Mac omits Podman journeys; full gates retain every journey)'
