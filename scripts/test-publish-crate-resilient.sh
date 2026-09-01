#!/usr/bin/env bash
set -euo pipefail

script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
publisher="$script_dir/publish-crate-resilient.sh"
driver="$script_dir/test-publish-crate-resilient-driver.sh"
work_dir=$(mktemp -d "${TMPDIR:-/tmp}/axocoatl-crate-publish-test.XXXXXX")
trap 'rm -rf -- "$work_dir"' EXIT HUP INT TERM

fail() {
  echo "test-publish-crate-resilient: $*" >&2
  exit 1
}

archive="$work_dir/axocoatl-core-1.0.1.crate"
printf 'reviewed crate bytes\n' > "$archive"
if command -v sha256sum >/dev/null 2>&1; then
  checksum=$(sha256sum "$archive" | awk '{print $1}')
else
  checksum=$(shasum -a 256 "$archive" | awk '{print $1}')
fi
latest="$work_dir/latest.json"
jq -n '[{tag_name:"v1.0.0", draft:false, prerelease:false}]' > "$latest"
commit=0123456789abcdef0123456789abcdef01234567

run_scenario() {
  local scenario=$1 state_dir="$work_dir/state-$1"
  mkdir "$state_dir"
  GITHUB_ACTIONS=false \
  AXO_CRATE_TEST_DRIVER="$driver" \
  AXO_CRATE_TEST_STATE="$state_dir" \
  AXO_CRATE_TEST_SCENARIO="$scenario" \
  AXO_CRATE_TEST_CHECKSUM="$checksum" \
  AXO_CRATE_MAX_PUBLISH_ATTEMPTS=3 \
  AXO_CRATE_API_POLL_ATTEMPTS=2 \
  AXO_CRATE_INDEX_POLL_ATTEMPTS=4 \
  AXO_CRATE_RETRY_SECONDS=0 \
  AXO_RELEASE_ORDER_FIXTURE="$latest" \
    "$publisher" axocoatl-core 1.0.1 "$archive" "$checksum" v1.0.1 "$commit"
}

publish_count() {
  local scenario=$1 file="$work_dir/state-$1/publish-count"
  if [[ -f "$file" ]]; then tr -d '[:space:]' < "$file"; else echo 0; fi
}

run_scenario already >/dev/null
[[ "$(publish_count already)" == 0 ]] || fail 'already-public scenario attempted a publish'

run_scenario api-before-index >/dev/null
[[ "$(publish_count api-before-index)" == 1 ]] || fail 'API-before-index scenario did not publish exactly once'
[[ "$(tr -d '[:space:]' < "$work_dir/state-api-before-index/index-count")" == 3 ]] \
  || fail 'publisher did not wait for sparse-index visibility'

run_scenario ambiguous >/dev/null
[[ "$(publish_count ambiguous)" == 1 ]] || fail 'ambiguous success was republished'

run_scenario retry >/dev/null
[[ "$(publish_count retry)" == 2 ]] || fail 'absent ambiguous upload was not retried exactly once'

expect_fail() {
  local scenario=$1 expected=$2 output="$work_dir/$1.out"
  if run_scenario "$scenario" > "$output" 2>&1; then
    fail "$scenario unexpectedly passed"
  fi
  grep -F "$expected" "$output" >/dev/null \
    || fail "$scenario did not report '$expected': $(cat "$output")"
}

expect_fail divergent 'differs from the reviewed archive'
expect_fail index-never 'not exact and visible in the sparse index'
expect_fail tag-failure 'remote tag proof failed'
[[ "$(publish_count tag-failure)" == 0 ]] \
  || fail 'failed initial tag refresh still reached cargo publish'
expect_fail tag-failure-after-retry 'remote tag proof failed'
[[ "$(publish_count tag-failure-after-retry)" == 1 ]] \
  || fail 'failed retry tag refresh allowed another irreversible publish attempt'

if GITHUB_ACTIONS=true \
  AXO_CRATE_TEST_DRIVER="$driver" \
  "$publisher" axocoatl-core 1.0.1 "$archive" "$checksum" v1.0.1 "$commit" \
  > "$work_dir/driver-in-actions.out" 2>&1; then
  fail 'test driver unexpectedly ran under GitHub Actions'
fi
grep -F 'test drivers are forbidden' "$work_dir/driver-in-actions.out" >/dev/null \
  || fail 'GitHub Actions driver guard did not explain the failure'

echo 'Resilient crate publication: PASS (9 injected offline simulations)'
