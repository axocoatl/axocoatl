#!/usr/bin/env bash
# Reprove a reviewed crate plan against crates.io's API and sparse index.
set -euo pipefail

fail() {
  echo "prove-release-crates: $*" >&2
  exit 1
}

[[ $# -eq 3 ]] || {
  echo 'Usage: prove-release-crates.sh <version> <release-crates.txt> <checksums.sha256>' >&2
  exit 2
}

version=$1
plan=$2
manifest=$3
[[ "$version" =~ ^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$ ]] \
  || fail "version '$version' is not stable MAJOR.MINOR.PATCH SemVer"
for input in "$plan" "$manifest"; do
  [[ -f "$input" && ! -L "$input" ]] || fail "reviewed input is missing or unsafe: $input"
done
command -v jq >/dev/null 2>&1 || fail 'jq is required'

fixture_dir=${AXO_RELEASE_CRATES_API_FIXTURE_DIR:-}
if [[ "${GITHUB_ACTIONS:-}" == true && -n "$fixture_dir" ]]; then
  fail 'API fixtures are forbidden in GitHub Actions'
fi
if [[ -n "$fixture_dir" ]]; then
  [[ -d "$fixture_dir" && ! -L "$fixture_dir" ]] || fail 'API fixture directory is missing or unsafe'
fi

plan_lines=$(wc -l < "$plan" | tr -d '[:space:]')
manifest_lines=$(wc -l < "$manifest" | tr -d '[:space:]')
[[ "$plan_lines" -gt 0 ]] || fail 'release crate plan is empty'
[[ "$manifest_lines" == "$plan_lines" ]] \
  || fail "checksum manifest has $manifest_lines lines for $plan_lines planned crates"

seen=$'\n'
validated=0
while IFS= read -r crate; do
  validated=$((validated + 1))
  [[ "$crate" =~ ^[a-z0-9][a-z0-9_-]*$ ]] || fail "invalid planned crate '$crate'"
  [[ "$seen" != *$'\n'"$crate"$'\n'* ]] || fail "duplicate planned crate $crate"
  seen+="$crate"$'\n'
done < "$plan"
[[ "$validated" == "$plan_lines" ]] || fail 'release plan contains an unterminated or unreadable line'

index=0
while IFS= read -r crate; do
  index=$((index + 1))
  line=$(sed -n "${index}p" "$manifest")
  expected_path="target/package/${crate}-${version}.crate"
  expected_checksum=${line%%  *}
  [[ "$expected_checksum" =~ ^[0-9a-f]{64}$ ]] \
    || fail "manifest checksum for $crate is malformed"
  [[ "$line" == "$expected_checksum  $expected_path" ]] \
    || fail "manifest line $index does not bind $crate $version in plan order"

  response=$(mktemp "${TMPDIR:-/tmp}/axocoatl-release-crate-api.XXXXXX")
  if [[ -n "$fixture_dir" ]]; then
    fixture="$fixture_dir/$crate.json"
    [[ -f "$fixture" && ! -L "$fixture" ]] || fail "API fixture is missing for $crate"
    cp "$fixture" "$response"
    status_file="$fixture_dir/$crate.status"
    if [[ -f "$status_file" && ! -L "$status_file" ]]; then
      status=$(tr -d '[:space:]' < "$status_file")
    else
      status=200
    fi
  else
    status=$(curl --retry 3 --retry-all-errors --connect-timeout 10 --max-time 30 \
      --user-agent "axocoatl-release-proof (${GITHUB_SERVER_URL:-https://github.com}/${GITHUB_REPOSITORY:-unknown})" \
      --silent --show-error --output "$response" --write-out '%{http_code}' \
      "https://crates.io/api/v1/crates/${crate}/${version}")
  fi
  [[ "$status" == 200 ]] || {
    rm -f "$response"
    fail "$crate $version is not public in the crates.io API (HTTP $status)"
  }
  jq --exit-status \
    --arg version "$version" --arg checksum "$expected_checksum" '
      .version.num == $version
      and .version.checksum == $checksum
      and .version.yanked == false
    ' "$response" >/dev/null || {
      rm -f "$response"
      fail "$crate $version differs from the reviewed checksum or is yanked in the crates.io API"
    }
  rm -f "$response"
  "$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)/prove-crate-index.sh" \
    "$crate" "$version" "$expected_checksum" >/dev/null
done < "$plan"

[[ "$index" == "$plan_lines" ]] || fail 'release plan contains an unterminated or unreadable line'
echo "Release crate proof: PASS ($index planned package(s) are exact in the API and sparse index)"
