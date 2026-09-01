#!/usr/bin/env bash
# Pure evaluator for the GitHub release state required by production deploys.
set -euo pipefail

fail() {
  echo "public-release: $*" >&2
  exit 1
}

usage() {
  echo "Usage: check-public-release.sh <required|optional> <expected-tag> <http-status> <response.json>" >&2
  exit 2
}

[[ $# -eq 4 ]] || usage
mode=$1
expected_tag=$2
status=$3
response_file=$4

case "$mode" in required|optional) ;; *) usage ;; esac
[[ "$expected_tag" =~ ^v[0-9]+\.[0-9]+\.[0-9]+([+-][0-9A-Za-z.-]+)?$ ]] \
  || fail "expected tag is not a supported product version: $expected_tag"
[[ "$status" =~ ^[0-9]{3}$ ]] || fail "HTTP status is malformed: $status"
[[ -f "$response_file" && ! -L "$response_file" ]] \
  || fail "response is missing or is not a regular file: $response_file"
command -v jq >/dev/null 2>&1 || fail "jq is required"

if [[ "$status" == 404 ]]; then
  if [[ "$mode" == optional ]]; then
    printf '%s\n' false
    exit 0
  fi
  fail "$expected_tag is not public (HTTP 404)"
fi
[[ "$status" == 200 ]] || fail "could not prove $expected_tag is public (HTTP $status)"
jq -e 'type == "object"' "$response_file" >/dev/null \
  || fail "GitHub release response is not valid JSON"

tag_name=$(jq -r '.tag_name // ""' "$response_file")
draft=$(jq -r 'if (.draft | type) == "boolean" then (.draft | tostring) else "" end' "$response_file")
prerelease=$(jq -r 'if (.prerelease | type) == "boolean" then (.prerelease | tostring) else "" end' "$response_file")
[[ "$tag_name" == "$expected_tag" ]] \
  || fail "release response identifies '$tag_name', expected '$expected_tag'"

if [[ "$draft" == true || "$prerelease" == true ]]; then
  if [[ "$mode" == optional ]]; then
    printf '%s\n' false
    exit 0
  fi
  fail "$expected_tag is not a public stable release"
fi
[[ "$draft" == false && "$prerelease" == false ]] \
  || fail "$expected_tag release state is malformed"

expected_assets=$(
  for target in \
    x86_64-unknown-linux-gnu \
    aarch64-unknown-linux-gnu \
    x86_64-apple-darwin \
    aarch64-apple-darwin
  do
    archive="axocoatl-${expected_tag}-${target}.tar.gz"
    printf '%s\n%s\n' "$archive" "$archive.sha256"
  done | LC_ALL=C sort
)
actual_assets=$(jq -r '
  if (.assets | type) == "array" then .assets[].name // "" else empty end
' "$response_file" | LC_ALL=C sort)
[[ "$actual_assets" == "$expected_assets" ]] \
  || fail "$expected_tag does not expose the exact four-platform archive and checksum set"
jq -e '
  (.assets | type) == "array"
  and (.assets | length) == 8
  and all(.assets[];
    (.name | type) == "string"
    and .state == "uploaded"
    and (.size | type) == "number"
    and (.size | floor) == .size
    and .size > 0)
' "$response_file" >/dev/null \
  || fail "$expected_tag contains an incomplete, empty, duplicate, or malformed release asset"

printf '%s\n' true
