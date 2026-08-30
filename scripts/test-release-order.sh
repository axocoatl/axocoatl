#!/usr/bin/env bash
set -euo pipefail

script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
checker="$script_dir/check-release-order.sh"
verifier="$script_dir/verify-release-order.sh"
work_dir=$(mktemp -d "${TMPDIR:-/tmp}/axocoatl-release-order-test.XXXXXX")
trap 'rm -rf -- "$work_dir"' EXIT HUP INT TERM

fail() {
  echo "test-release-order: $*" >&2
  exit 1
}

releases="$work_dir/releases.json"
jq -n '[
  {tag_name:"v1.2.3", draft:false, prerelease:false},
  {tag_name:"v1.1.9", draft:false, prerelease:false}
]' > "$releases"
empty="$work_dir/empty.json"
printf '[]\n' > "$empty"

expect_result() {
  local name=$1 expected=$2
  shift 2
  local actual
  actual=$("$checker" "$@")
  [[ "$actual" == "$expected" ]] \
    || fail "$name returned '$actual', expected '$expected'"
}

expect_fail() {
  local name=$1 expected=$2
  shift 2
  local output="$work_dir/$name.out"
  if "$checker" "$@" > "$output" 2>&1; then
    fail "$name unexpectedly passed"
  fi
  grep -F "$expected" "$output" >/dev/null \
    || fail "$name did not report '$expected': $(cat "$output")"
}

expect_result first first v0.0.0 200 "$empty"
expect_result same same v1.2.3 200 "$releases"
expect_result patch advance v1.2.4 200 "$releases"
expect_result minor advance v1.3.0 200 "$releases"
expect_result major advance v2.0.0 200 "$releases"
expect_result huge advance v100000000000000000000.0.0 200 "$releases"
expect_fail rollback 'older than public stable release frontier' v1.2.2 200 "$releases"
expect_fail prerelease 'not a stable' v1.2.4-rc.1 200 "$releases"
expect_fail build-metadata 'not a stable' v1.2.4+build 200 "$releases"
expect_fail leading-zero 'not a stable' v01.2.4 200 "$releases"

jq '. + [
  {tag_name:"v9.0.0", draft:true, prerelease:false},
  {tag_name:"v10.0.0-rc.1", draft:false, prerelease:true}
]' "$releases" > "$work_dir/non-public.json"
expect_result ignore-non-public advance v1.2.4 200 "$work_dir/non-public.json"

jq '[.[0], {tag_name:"v1.2.5", draft:false, prerelease:false}]' \
  "$releases" > "$work_dir/moved-latest-pointer.json"
expect_fail enumerate-frontier 'frontier v1.2.5' \
  v1.2.4 200 "$work_dir/moved-latest-pointer.json"
expect_result actual-frontier same v1.2.5 200 "$work_dir/moved-latest-pointer.json"

jq '. + [{tag_name:"not-semver", draft:false, prerelease:false}]' \
  "$releases" > "$work_dir/invalid-public-tag.json"
expect_fail invalid-public-tag 'is not stable' v1.2.4 200 "$work_dir/invalid-public-tag.json"
jq '. + [.[0]]' "$releases" > "$work_dir/duplicate.json"
expect_fail duplicate 'duplicate public tag' v1.2.4 200 "$work_dir/duplicate.json"
jq '.[0].draft = "false"' "$releases" > "$work_dir/malformed-state.json"
expect_fail malformed-state 'not a well-formed JSON array' v1.2.4 200 "$work_dir/malformed-state.json"
printf '%s\n' not-json > "$work_dir/malformed.json"
expect_fail malformed 'not a well-formed JSON array' v1.2.4 200 "$work_dir/malformed.json"
expect_fail forbidden-http 'HTTP 403' v1.2.4 403 "$empty"
expect_fail unexpected-not-found 'HTTP 404' v0.0.0 404 "$empty"

wrapper_result=$(GITHUB_ACTIONS=false AXO_RELEASE_ORDER_FIXTURE="$releases" \
  AXO_RELEASE_ORDER_STATUS=200 "$verifier" v1.2.4)
[[ "$wrapper_result" == advance ]] || fail "wrapper returned '$wrapper_result'"

if GITHUB_ACTIONS=true AXO_RELEASE_ORDER_FIXTURE="$releases" \
  "$verifier" v1.2.4 > "$work_dir/forbidden-fixture.out" 2>&1; then
  fail 'GitHub Actions fixture injection unexpectedly passed'
fi
grep -F 'fixtures are forbidden' "$work_dir/forbidden-fixture.out" >/dev/null \
  || fail 'GitHub Actions fixture injection did not report the guard'

echo 'Release-order contract: PASS (21 pure and wrapper simulations)'
