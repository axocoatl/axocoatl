#!/usr/bin/env bash
set -euo pipefail

script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
verifier="$script_dir/prove-crate-index.sh"
driver="$script_dir/test-prove-crate-index-driver.sh"
work_dir=$(mktemp -d "${TMPDIR:-/tmp}/axocoatl-crate-index-test.XXXXXX")
trap 'rm -rf -- "$work_dir"' EXIT HUP INT TERM
checksum=$(printf 'a%.0s' {1..64})

fail() {
  echo "test-prove-crate-index: $*" >&2
  exit 1
}

run_scenario() {
  local scenario=$1
  GITHUB_ACTIONS=false \
  AXO_CRATE_INDEX_TEST_DRIVER="$driver" \
  AXO_CRATE_INDEX_SCENARIO="$scenario" \
  AXO_CRATE_INDEX_CHECKSUM="$checksum" \
  AXO_CRATE_INDEX_ATTEMPTS=3 \
  AXO_CRATE_INDEX_RETRY_SECONDS=0 \
    "$verifier" axocoatl-cli 1.0.1 "$checksum"
}

run_scenario visible >/dev/null
run_scenario lag >/dev/null

expect_fail() {
  local scenario=$1 expected=$2
  if run_scenario "$scenario" > "$work_dir/$scenario.out" 2>&1; then
    fail "$scenario unexpectedly passed"
  fi
  grep -F "$expected" "$work_dir/$scenario.out" >/dev/null \
    || fail "$scenario did not report '$expected': $(cat "$work_dir/$scenario.out")"
}

expect_fail mismatch 'differs from the reviewed archive'
expect_fail yanked 'differs from the reviewed archive'
expect_fail duplicate 'contains 2 entries'
expect_fail malformed 'malformed'
expect_fail never 'not exact and visible'
expect_fail http-error 'HTTP 503'

if GITHUB_ACTIONS=true AXO_CRATE_INDEX_TEST_DRIVER="$driver" \
  "$verifier" axocoatl-cli 1.0.1 "$checksum" \
  > "$work_dir/injection.out" 2>&1; then
  fail 'test driver unexpectedly ran under GitHub Actions'
fi
grep -F 'test drivers are forbidden' "$work_dir/injection.out" >/dev/null \
  || fail 'GitHub Actions injection guard did not explain the failure'

echo 'Crate sparse-index contract: PASS (9 offline simulations)'
