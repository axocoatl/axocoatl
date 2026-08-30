#!/usr/bin/env bash
set -euo pipefail

script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
checker="$script_dir/check-previous-public-release.sh"
resolver="$script_dir/resolve-previous-public-release.sh"
work_dir=$(mktemp -d "${TMPDIR:-/tmp}/axocoatl-previous-release-test.XXXXXX")
trap 'rm -rf -- "$work_dir"' EXIT HUP INT TERM

fail() {
  echo "test-previous-public-release: $*" >&2
  exit 1
}

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

releases="$work_dir/releases.json"
jq -n '[
  {tag_name:"v1.2.3", draft:false, prerelease:false},
  {tag_name:"v1.9.8", draft:false, prerelease:false},
  {tag_name:"v1.4.50", draft:false, prerelease:false}
]' > "$releases"

expect_result greatest-below v1.9.8 v2.0.0 200 "$releases"
expect_result candidate-present v1.4.50 v1.9.8 200 "$releases"
expect_result patch-boundary v1.2.3 v1.2.4 200 "$releases"

# API order is not SemVer order. The first entry stands in for a release that a
# maintainer manually selected as /releases/latest; the complete list still wins.
jq -n '[
  {tag_name:"v1.2.3", draft:false, prerelease:false, manually_selected_latest:true},
  {tag_name:"v1.999.0", draft:false, prerelease:false},
  {tag_name:"v1.10.0", draft:false, prerelease:false}
]' > "$work_dir/moved-latest-pointer.json"
expect_result moved-latest-pointer v1.999.0 v2.0.0 200 \
  "$work_dir/moved-latest-pointer.json"

jq -n '[
  {tag_name:"v999.0.0-rc.1", draft:false, prerelease:true},
  {tag_name:"internal-nightly", draft:true, prerelease:false},
  {tag_name:"v1.8.0", draft:false, prerelease:false}
]' > "$work_dir/non-public-tags.json"
expect_result ignore-non-public-tags v1.8.0 v2.0.0 200 \
  "$work_dir/non-public-tags.json"

jq -n '[
  {tag_name:"v999999999999999999999999999999.0.0", draft:false, prerelease:false},
  {tag_name:"v1000000000000000000000000000000.0.0", draft:false, prerelease:false},
  {tag_name:"v9.999999999999999999999999999999.0", draft:false, prerelease:false}
]' > "$work_dir/huge.json"
expect_result arbitrary-precision v1000000000000000000000000000000.0.0 \
  v1000000000000000000000000000001.0.0 200 "$work_dir/huge.json"

empty="$work_dir/empty.json"
printf '[]\n' > "$empty"
expect_fail empty-history 'no prior public stable GitHub Release exists' \
  v1.0.0 200 "$empty"
jq -n '[{tag_name:"v1.0.0", draft:false, prerelease:false}]' \
  > "$work_dir/candidate-only.json"
expect_fail candidate-only 'no prior public stable GitHub Release exists' \
  v1.0.0 200 "$work_dir/candidate-only.json"
jq -n '[{tag_name:"v2.0.0", draft:false, prerelease:false}]' \
  > "$work_dir/higher-only.json"
expect_fail higher-only 'no prior public stable GitHub Release exists' \
  v1.0.0 200 "$work_dir/higher-only.json"

expect_fail prerelease-candidate 'not a stable' v2.0.0-rc.1 200 "$releases"
expect_fail metadata-candidate 'not a stable' v2.0.0+build 200 "$releases"
expect_fail leading-zero-candidate 'not a stable' v02.0.0 200 "$releases"
expect_fail http-error 'HTTP 403' v2.0.0 403 "$releases"

jq '. + [{tag_name:"not-semver", draft:false, prerelease:false}]' \
  "$releases" > "$work_dir/invalid-public-tag.json"
expect_fail invalid-public-tag 'is not stable' v2.0.0 200 \
  "$work_dir/invalid-public-tag.json"
jq '. + [.[0]]' "$releases" > "$work_dir/duplicate.json"
expect_fail duplicate-public-tag 'duplicate public tag' v2.0.0 200 \
  "$work_dir/duplicate.json"
jq '.[0].draft = "false"' "$releases" > "$work_dir/malformed-state.json"
expect_fail malformed-state 'not a well-formed JSON array' v2.0.0 200 \
  "$work_dir/malformed-state.json"
printf 'not-json\n' > "$work_dir/malformed.json"
expect_fail malformed-json 'not a well-formed JSON array' v2.0.0 200 \
  "$work_dir/malformed.json"

wrapper_result=$(GITHUB_ACTIONS=false \
  AXO_PREVIOUS_PUBLIC_RELEASE_FIXTURE="$releases" \
  AXO_PREVIOUS_PUBLIC_RELEASE_STATUS=200 \
  "$resolver" v2.0.0)
[[ "$wrapper_result" == v1.9.8 ]] \
  || fail "fixture wrapper returned '$wrapper_result'"

if GITHUB_ACTIONS=true AXO_PREVIOUS_PUBLIC_RELEASE_FIXTURE="$releases" \
  "$resolver" v2.0.0 > "$work_dir/forbidden-fixture.out" 2>&1; then
  fail 'GitHub Actions fixture injection unexpectedly passed'
fi
grep -F 'fixtures are forbidden' "$work_dir/forbidden-fixture.out" >/dev/null \
  || fail 'GitHub Actions fixture injection did not report the guard'

# Even merged stable, internal, and RC tags in the current clone are irrelevant:
# the resolver has exactly one source of truth, the public GitHub Releases list.
local_repo="$work_dir/local-tags"
git init --quiet "$local_repo"
git -C "$local_repo" -c user.name=Test -c user.email=test@example.invalid \
  commit --quiet --allow-empty -m fixture
git -C "$local_repo" tag v999.0.0
git -C "$local_repo" tag internal-nightly
git -C "$local_repo" tag v2.0.0-rc.9
local_result=$(cd "$local_repo" && GITHUB_ACTIONS=false \
  AXO_PREVIOUS_PUBLIC_RELEASE_FIXTURE="$releases" \
  "$resolver" v2.0.0)
[[ "$local_result" == v1.9.8 ]] \
  || fail "local Git tags changed the result to '$local_result'"

# Exercise actual pagination. Page 1 is full and contains only prereleases;
# the highest prior public stable release exists only on page 2. The curl double
# rejects /releases/latest, so a manually moved pointer can never be consulted.
page_dir="$work_dir/pages"
mkdir -p "$page_dir"
jq -n '[range(0; 100) | {
  tag_name:("v9.0.0-rc." + (.|tostring)),
  draft:false,
  prerelease:true
}]' > "$page_dir/page-1.json"
jq -n '[
  {tag_name:"v1.999.0", draft:false, prerelease:false},
  {tag_name:"v1.2.3", draft:false, prerelease:false}
]' > "$page_dir/page-2.json"
fake_bin="$work_dir/bin"
mkdir -p "$fake_bin"
ln -s "$script_dir/test-previous-public-release-curl.sh" "$fake_bin/curl"
curl_log="$work_dir/curl.log"
paginated_result=$(PATH="$fake_bin:$PATH" \
  GITHUB_ACTIONS=true GH_TOKEN=test-token \
  GITHUB_API_URL=https://api.example.invalid \
  GITHUB_REPOSITORY=owner/repo \
  AXO_FAKE_RELEASE_PAGE_DIR="$page_dir" AXO_FAKE_CURL_LOG="$curl_log" \
  "$resolver" v2.0.0)
[[ "$paginated_result" == v1.999.0 ]] \
  || fail "paginated wrapper returned '$paginated_result'"
[[ $(wc -l < "$curl_log" | tr -d ' ') == 2 ]] \
  || fail 'paginated wrapper did not fetch exactly two pages'
grep -F '/releases?per_page=100&page=1' "$curl_log" >/dev/null \
  || fail 'paginated wrapper did not fetch page 1'
grep -F '/releases?per_page=100&page=2' "$curl_log" >/dev/null \
  || fail 'paginated wrapper did not fetch page 2'

echo 'Previous-public-release contract: PASS (21 pure, wrapper, local-tag, and pagination simulations)'
