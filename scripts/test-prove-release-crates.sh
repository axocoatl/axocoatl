#!/usr/bin/env bash
set -euo pipefail

script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
verifier="$script_dir/prove-release-crates.sh"
index_driver="$script_dir/test-prove-crate-index-driver.sh"
work_dir=$(mktemp -d "${TMPDIR:-/tmp}/axocoatl-release-crates-test.XXXXXX")
trap 'rm -rf -- "$work_dir"' EXIT HUP INT TERM
fixtures="$work_dir/api"
mkdir -p "$fixtures"
checksum=$(printf 'a%.0s' {1..64})
plan="$work_dir/plan.txt"
manifest="$work_dir/checksums.sha256"
printf '%s\n' axocoatl-core axocoatl-cli > "$plan"
printf '%s  %s\n' \
  "$checksum" target/package/axocoatl-core-1.0.1.crate \
  "$checksum" target/package/axocoatl-cli-1.0.1.crate > "$manifest"

write_api() {
  local crate=$1 effective_checksum=${2:-$checksum} yanked=${3:-false}
  jq -n --arg version 1.0.1 --arg checksum "$effective_checksum" --argjson yanked "$yanked" \
    '{version:{num:$version, checksum:$checksum, yanked:$yanked}}' > "$fixtures/$crate.json"
}
write_api axocoatl-core
write_api axocoatl-cli

fail() {
  echo "test-prove-release-crates: $*" >&2
  exit 1
}

run_verifier() {
  GITHUB_ACTIONS=false \
  AXO_RELEASE_CRATES_API_FIXTURE_DIR="$fixtures" \
  AXO_CRATE_INDEX_TEST_DRIVER="$index_driver" \
  AXO_CRATE_INDEX_SCENARIO=visible \
  AXO_CRATE_INDEX_CHECKSUM="$checksum" \
  AXO_CRATE_INDEX_ATTEMPTS=1 \
  AXO_CRATE_INDEX_RETRY_SECONDS=0 \
    "$verifier" 1.0.1 "$plan" "$manifest"
}

run_verifier >/dev/null

write_api axocoatl-cli "$(printf 'f%.0s' {1..64})"
if run_verifier > "$work_dir/mismatch.out" 2>&1; then
  fail 'API checksum mismatch unexpectedly passed'
fi
grep -F 'differs from the reviewed checksum' "$work_dir/mismatch.out" >/dev/null
write_api axocoatl-cli

cp "$manifest" "$work_dir/bad-manifest"
sed '1s/axocoatl-core/axocoatl-cli/' "$manifest" > "$work_dir/bad-manifest"
if GITHUB_ACTIONS=false \
  AXO_RELEASE_CRATES_API_FIXTURE_DIR="$fixtures" \
  "$verifier" 1.0.1 "$plan" "$work_dir/bad-manifest" \
  > "$work_dir/order.out" 2>&1; then
  fail 'out-of-order manifest unexpectedly passed'
fi
grep -F 'does not bind' "$work_dir/order.out" >/dev/null

printf '%s\n' axocoatl-cli axocoatl-cli > "$work_dir/duplicate-plan"
printf '%s  %s\n' \
  "$checksum" target/package/axocoatl-cli-1.0.1.crate \
  "$checksum" target/package/axocoatl-cli-1.0.1.crate > "$work_dir/duplicate-manifest"
if GITHUB_ACTIONS=false AXO_RELEASE_CRATES_API_FIXTURE_DIR="$fixtures" \
  "$verifier" 1.0.1 "$work_dir/duplicate-plan" "$work_dir/duplicate-manifest" \
  > "$work_dir/duplicate.out" 2>&1; then
  fail 'duplicate plan unexpectedly passed'
fi
grep -F 'duplicate planned crate' "$work_dir/duplicate.out" >/dev/null

if GITHUB_ACTIONS=true AXO_RELEASE_CRATES_API_FIXTURE_DIR="$fixtures" \
  "$verifier" 1.0.1 "$plan" "$manifest" \
  > "$work_dir/injection.out" 2>&1; then
  fail 'API fixtures unexpectedly ran under GitHub Actions'
fi
grep -F 'fixtures are forbidden' "$work_dir/injection.out" >/dev/null

echo 'Release crate proof contract: PASS (API/index, checksum, order, duplicate, and injection cases)'
