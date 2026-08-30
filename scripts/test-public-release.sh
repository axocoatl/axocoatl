#!/usr/bin/env bash
set -euo pipefail

script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
checker="$script_dir/check-public-release.sh"
work_dir=$(mktemp -d "${TMPDIR:-/tmp}/axocoatl-public-release-test.XXXXXX")
trap 'rm -rf -- "$work_dir"' EXIT HUP INT TERM

fail() {
  echo "test-public-release: $*" >&2
  exit 1
}

exact="$work_dir/exact.json"
jq -n --arg tag v1.0.1 '
  {
    id: 101,
    tag_name: $tag,
    draft: false,
    prerelease: false,
    assets: [
      "x86_64-unknown-linux-gnu",
      "aarch64-unknown-linux-gnu",
      "x86_64-apple-darwin",
      "aarch64-apple-darwin"
    ] | map(
      "axocoatl-\($tag)-\(.).tar.gz" as $archive
      | [{name: $archive, state: "uploaded", size: 100},
         {name: "\($archive).sha256", state: "uploaded", size: 64}]
    ) | add
  }
' > "$exact"

expect_result() {
  name=$1
  expected=$2
  shift 2
  actual=$("$checker" "$@")
  [[ "$actual" == "$expected" ]] || fail "$name returned '$actual', expected '$expected'"
}

expect_fail() {
  name=$1
  expected=$2
  shift 2
  output="$work_dir/$name.out"
  if "$checker" "$@" > "$output" 2>&1; then
    fail "$name unexpectedly passed"
  fi
  grep -F "$expected" "$output" >/dev/null \
    || fail "$name did not report '$expected': $(cat "$output")"
}

expect_result exact-required true required v1.0.1 200 "$exact"
expect_result exact-optional true optional v1.0.1 200 "$exact"
printf '{}\n' > "$work_dir/empty.json"
expect_result missing-optional false optional v1.0.1 404 "$work_dir/empty.json"
expect_fail missing-required 'not public' required v1.0.1 404 "$work_dir/empty.json"

draft="$work_dir/draft.json"
jq '.draft = true' "$exact" > "$draft"
expect_result draft-optional false optional v1.0.1 200 "$draft"
expect_fail draft-required 'not a public stable release' required v1.0.1 200 "$draft"

prerelease="$work_dir/prerelease.json"
jq '.prerelease = true' "$exact" > "$prerelease"
expect_result prerelease-optional false optional v1.0.1 200 "$prerelease"
expect_fail prerelease-required 'not a public stable release' required v1.0.1 200 "$prerelease"

missing_asset="$work_dir/missing-asset.json"
jq '.assets |= .[:-1]' "$exact" > "$missing_asset"
expect_fail missing-asset 'exact four-platform' required v1.0.1 200 "$missing_asset"

extra_asset="$work_dir/extra-asset.json"
jq '.assets += [{name:"surprise", state:"uploaded", size:1}]' "$exact" > "$extra_asset"
expect_fail extra-asset 'exact four-platform' required v1.0.1 200 "$extra_asset"

empty_asset="$work_dir/empty-asset.json"
jq '.assets[0].size = 0' "$exact" > "$empty_asset"
expect_fail empty-asset 'incomplete, empty' required v1.0.1 200 "$empty_asset"

pending_asset="$work_dir/pending-asset.json"
jq '.assets[0].state = "new"' "$exact" > "$pending_asset"
expect_fail pending-asset 'incomplete, empty' required v1.0.1 200 "$pending_asset"

wrong_tag="$work_dir/wrong-tag.json"
jq '.tag_name = "v1.0.0"' "$exact" > "$wrong_tag"
expect_fail wrong-tag "identifies 'v1.0.0'" required v1.0.1 200 "$wrong_tag"

expect_fail forbidden-http 'HTTP 403' required v1.0.1 403 "$work_dir/empty.json"
printf '%s\n' not-json > "$work_dir/malformed.json"
expect_fail malformed-json 'not valid JSON' required v1.0.1 200 "$work_dir/malformed.json"

fixture_repo="$work_dir/repository"
fixture_remote="$work_dir/remote.git"
mkdir -p "$fixture_repo/axocoatl-cli"
git -C "$fixture_repo" init --quiet
git -C "$fixture_repo" config user.name 'Release Test'
git -C "$fixture_repo" config user.email release-test@example.invalid
printf '[package]\nname = "axocoatl-cli"\nversion = "1.0.0"\n' \
  > "$fixture_repo/axocoatl-cli/Cargo.toml"
git -C "$fixture_repo" add axocoatl-cli/Cargo.toml
git -C "$fixture_repo" commit --quiet -m base
base_commit=$(git -C "$fixture_repo" rev-parse HEAD)
printf '[package]\nname = "axocoatl-cli"\nversion = "1.0.1"\n' \
  > "$fixture_repo/axocoatl-cli/Cargo.toml"
git -C "$fixture_repo" add axocoatl-cli/Cargo.toml
git -C "$fixture_repo" commit --quiet -m release
release_commit=$(git -C "$fixture_repo" rev-parse HEAD)
git -C "$fixture_repo" tag v1.0.1
git clone --quiet --bare "$fixture_repo" "$fixture_remote"

wrapper_result=$(GITHUB_ACTIONS=false \
  AXO_PUBLIC_RELEASE_REPO_ROOT="$fixture_repo" \
  AXO_PUBLIC_RELEASE_REMOTE="$fixture_remote" \
  AXO_PUBLIC_RELEASE_FIXTURE="$exact" \
  AXO_PUBLIC_RELEASE_STATUS=200 \
  "$script_dir/verify-public-release.sh" required v1.0.1 "$release_commit")
[[ "$wrapper_result" == true ]] || fail "required wrapper returned '$wrapper_result'"

git --git-dir="$fixture_remote" update-ref refs/tags/v1.0.1 "$base_commit"
wrapper_output="$work_dir/wrapper-moved-tag.out"
if GITHUB_ACTIONS=false \
  AXO_PUBLIC_RELEASE_REPO_ROOT="$fixture_repo" \
  AXO_PUBLIC_RELEASE_REMOTE="$fixture_remote" \
  AXO_PUBLIC_RELEASE_FIXTURE="$exact" \
  AXO_PUBLIC_RELEASE_STATUS=200 \
  "$script_dir/verify-public-release.sh" required v1.0.1 "$release_commit" \
  > "$wrapper_output" 2>&1; then
  fail 'required wrapper accepted a moved remote tag'
fi
grep -F 'remote tag v1.0.1 resolves to' "$wrapper_output" >/dev/null \
  || fail "moved remote tag failure was unclear: $(cat "$wrapper_output")"

# Restore the remote and prove the forced fetch does not trust the stale local
# proof ref left by the preceding wrapper invocation.
git --git-dir="$fixture_remote" update-ref refs/tags/v1.0.1 "$release_commit"
wrapper_result=$(GITHUB_ACTIONS=false \
  AXO_PUBLIC_RELEASE_REPO_ROOT="$fixture_repo" \
  AXO_PUBLIC_RELEASE_REMOTE="$fixture_remote" \
  AXO_PUBLIC_RELEASE_FIXTURE="$exact" \
  AXO_PUBLIC_RELEASE_STATUS=200 \
  "$script_dir/verify-public-release.sh" required v1.0.1 "$release_commit")
[[ "$wrapper_result" == true ]] || fail 'required wrapper did not recover after the remote tag was restored'

stale_latest="$work_dir/stale-latest.json"
jq '.id = 202 | .tag_name = "v1.0.2"' "$exact" > "$stale_latest"
if GITHUB_ACTIONS=false \
  AXO_PUBLIC_RELEASE_REPO_ROOT="$fixture_repo" \
  AXO_PUBLIC_RELEASE_REMOTE="$fixture_remote" \
  AXO_PUBLIC_RELEASE_FIXTURE="$exact" \
  AXO_PUBLIC_RELEASE_LATEST_FIXTURE="$stale_latest" \
  AXO_PUBLIC_RELEASE_STATUS=200 \
  "$script_dir/verify-public-release.sh" required v1.0.1 "$release_commit" \
  > "$work_dir/wrapper-stale-latest.out" 2>&1; then
  fail 'required wrapper accepted a release that is no longer GitHub latest'
fi
grep -F "is public but is not GitHub's latest release" \
  "$work_dir/wrapper-stale-latest.out" >/dev/null \
  || fail "stale latest failure was unclear: $(cat "$work_dir/wrapper-stale-latest.out")"

if GITHUB_ACTIONS=false \
  AXO_PUBLIC_RELEASE_REPO_ROOT="$fixture_repo" \
  AXO_PUBLIC_RELEASE_REMOTE="$fixture_remote" \
  AXO_PUBLIC_RELEASE_FIXTURE="$exact" \
  "$script_dir/verify-public-release.sh" required v1.0.1 "$base_commit" \
  > "$work_dir/wrapper-wrong-checkout.out" 2>&1; then
  fail 'required wrapper accepted a checkout/called-SHA mismatch'
fi
grep -F 'deployment checkout does not match frozen source' \
  "$work_dir/wrapper-wrong-checkout.out" >/dev/null \
  || fail 'checkout/called-SHA mismatch was unclear'

wrapper_result=$(GITHUB_ACTIONS=false \
  AXO_PUBLIC_RELEASE_REPO_ROOT="$fixture_repo" \
  AXO_PUBLIC_RELEASE_FIXTURE="$exact" \
  AXO_PUBLIC_RELEASE_STATUS=200 \
  "$script_dir/verify-public-release.sh" optional)
[[ "$wrapper_result" == true ]] || fail "optional wrapper returned '$wrapper_result'"

wrapper_result=$(GITHUB_ACTIONS=false \
  AXO_PUBLIC_RELEASE_REPO_ROOT="$fixture_repo" \
  AXO_PUBLIC_RELEASE_FIXTURE="$exact" \
  AXO_PUBLIC_RELEASE_LATEST_FIXTURE="$stale_latest" \
  AXO_PUBLIC_RELEASE_STATUS=200 \
  "$script_dir/verify-public-release.sh" optional)
[[ "$wrapper_result" == false ]] \
  || fail "optional wrapper deployed a stale release instead of returning false"

if GITHUB_ACTIONS=false \
  AXO_PUBLIC_RELEASE_REPO_ROOT="$fixture_repo" \
  AXO_PUBLIC_RELEASE_FIXTURE="$exact" \
  AXO_PUBLIC_RELEASE_LATEST_STATUS=403 \
  "$script_dir/verify-public-release.sh" optional \
  > "$work_dir/wrapper-latest-forbidden.out" 2>&1; then
  fail 'optional wrapper hid a latest-release HTTP failure as an unpublished version'
fi
grep -F "could not prove GitHub's latest release (HTTP 403)" \
  "$work_dir/wrapper-latest-forbidden.out" >/dev/null \
  || fail 'optional latest-release HTTP failure was unclear'

malformed_latest="$work_dir/malformed-latest.json"
printf '%s\n' not-json > "$malformed_latest"
if GITHUB_ACTIONS=false \
  AXO_PUBLIC_RELEASE_REPO_ROOT="$fixture_repo" \
  AXO_PUBLIC_RELEASE_FIXTURE="$exact" \
  AXO_PUBLIC_RELEASE_LATEST_FIXTURE="$malformed_latest" \
  "$script_dir/verify-public-release.sh" optional \
  > "$work_dir/wrapper-latest-malformed.out" 2>&1; then
  fail 'optional wrapper hid malformed latest-release JSON'
fi
grep -F "could not prove GitHub's latest release" \
  "$work_dir/wrapper-latest-malformed.out" >/dev/null \
  || fail 'optional malformed latest-release failure was unclear'

echo 'Public release contract: PASS (24 pure and wrapper simulations)'
