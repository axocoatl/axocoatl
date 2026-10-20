#!/usr/bin/env bash
# Prove on this host that each new-user fixture (tests/fixtures/newuser) fails
# its own test command before the gate stub's fix and passes after it, so a
# failing gate run points at Axocoatl, not at a broken fixture.
#
# Usage: test-fixtures.sh [fixture...]
# A fixture whose toolchain this host lacks is skipped, unless
# GATE_REQUIRE_ALL_TOOLCHAINS=1 (CI), which makes it a failure. A fixture
# that another workstream owns (fixtures.json "owner") and that is not in
# the tree yet is reported PENDING; the gate itself fails its runs until it
# is there, so nothing passes without it.
set -euo pipefail

script_dir=$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)
repo_root=$(CDPATH='' cd -- "$script_dir/../.." && pwd)
fixtures_root="$repo_root/tests/fixtures/newuser"
work=$(mktemp -d "${TMPDIR:-/tmp}/axocoatl-gate-fixtures.XXXXXX")
trap 'rm -rf -- "$work"' EXIT

fail() {
  echo "test-fixtures: $*" >&2
  exit 1
}

command -v node >/dev/null 2>&1 || fail "node is required"

if [[ $# -gt 0 ]]; then
  fixtures=("$@")
else
  fixtures=()
  while IFS= read -r name; do
    fixtures+=("$name")
  done < <(node -e '
    const m = require(process.argv[1]);
    for (const name of Object.keys(m.fixtures)) console.log(name);
  ' "$fixtures_root/fixtures.json")
fi

require_tools() {
  local fixture=$1
  shift
  local tool
  for tool in "$@"; do
    if ! command -v "$tool" >/dev/null 2>&1; then
      if [[ "${GATE_REQUIRE_ALL_TOOLCHAINS:-0}" == 1 ]]; then
        fail "$fixture needs $tool, which this host does not have"
      fi
      echo "SKIP $fixture: no $tool on this host"
      return 1
    fi
  done
}

# Run the fixture's test command in $1; print its output on failure of the
# expectation $2 (pass|fail).
run_tests() {
  local dir=$1 expect=$2 fixture=$3 log status
  log="$work/$fixture-$expect.log"
  set +e
  case "$fixture" in
    node-plain | node-deps) (cd "$dir" && npm test) >"$log" 2>&1 ;;
    python-pytest) (cd "$dir" && "$dir/.venv/bin/python" -m pytest -q) >"$log" 2>&1 ;;
    rust) (cd "$dir" && CARGO_TARGET_DIR="$work/$fixture-target" cargo test --quiet) >"$log" 2>&1 ;;
    go) (cd "$dir" && GOFLAGS=-mod=mod go test ./...) >"$log" 2>&1 ;;
    *) set -e; fail "no test command for fixture $fixture" ;;
  esac
  status=$?
  set -e
  if [[ "$expect" == pass && $status -ne 0 ]] || [[ "$expect" == fail && $status -eq 0 ]]; then
    cat "$log" >&2
    fail "$fixture: tests were expected to $expect (exit $status)"
  fi
}

setup() {
  local dir=$1 fixture=$2
  case "$fixture" in
    node-deps)
      (cd "$dir" && npm ci --ignore-scripts --no-audit --no-fund) >"$work/$fixture-setup.log" 2>&1 \
        || { cat "$work/$fixture-setup.log" >&2; fail "$fixture: npm ci failed"; }
      ;;
    python-pytest)
      { python3 -m venv "$dir/.venv" && "$dir/.venv/bin/python" -m pip install --quiet -e "${dir}[test]"; } \
        >"$work/$fixture-setup.log" 2>&1 \
        || { cat "$work/$fixture-setup.log" >&2; fail "$fixture: pip install -e .[test] failed"; }
      ;;
  esac
}

checked=0
for fixture in "${fixtures[@]}"; do
  source_dir="$fixtures_root/$fixture"
  if [[ ! -d "$source_dir" ]]; then
    owner=$(node -e 'process.stdout.write(require(process.argv[1]).fixtures[process.argv[2]]?.owner ?? "")' \
      "$fixtures_root/fixtures.json" "$fixture")
    if [[ -n "$owner" ]]; then
      echo "PENDING $fixture: made by $owner, not in this tree yet (the gate fails its runs until it is)"
      continue
    fi
    if [[ "${GATE_REQUIRE_ALL_TOOLCHAINS:-0}" == 1 ]]; then
      fail "fixture $fixture is listed in fixtures.json but missing"
    fi
    echo "SKIP $fixture: not in this tree yet"
    continue
  fi
  case "$fixture" in
    node-plain | node-deps) require_tools "$fixture" node npm || continue ;;
    python-pytest) require_tools "$fixture" python3 || continue ;;
    rust) require_tools "$fixture" cargo || continue ;;
    go) require_tools "$fixture" go || continue ;;
  esac
  dir="$work/$fixture"
  cp -R "$source_dir" "$dir"
  setup "$dir" "$fixture"
  run_tests "$dir" fail "$fixture"
  node "$script_dir/apply-stub-fix.mjs" "$fixture" "$dir"
  run_tests "$dir" pass "$fixture"
  echo "PASS $fixture: fails before the stub's fix, passes after it"
  checked=$((checked + 1))
done

(( checked > 0 )) || fail "no fixture was checked"
echo "New-user fixtures: PASS ($checked checked)"
