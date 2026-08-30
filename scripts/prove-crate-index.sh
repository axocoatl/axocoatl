#!/usr/bin/env bash
# Poll crates.io's sparse index until one exact, non-yanked package is installable.
set -euo pipefail

fail() {
  echo "prove-crate-index: $*" >&2
  exit 1
}

[[ $# -eq 3 ]] || {
  echo 'Usage: prove-crate-index.sh <crate> <version> <sha256>' >&2
  exit 2
}

crate=$1
version=$2
expected_checksum=$3
[[ "$crate" =~ ^[a-z0-9][a-z0-9_-]*$ ]] || fail "invalid crate name '$crate'"
[[ "$version" =~ ^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$ ]] \
  || fail "version '$version' is not stable MAJOR.MINOR.PATCH SemVer"
[[ "$expected_checksum" =~ ^[0-9a-f]{64}$ ]] || fail 'checksum must be lowercase SHA-256'
command -v jq >/dev/null 2>&1 || fail 'jq is required'

test_driver=${AXO_CRATE_INDEX_TEST_DRIVER:-}
if [[ "${GITHUB_ACTIONS:-}" == true && -n "$test_driver" ]]; then
  fail 'test drivers are forbidden in GitHub Actions'
fi
if [[ -n "$test_driver" ]]; then
  [[ -x "$test_driver" ]] || fail "test driver is not executable: $test_driver"
fi

attempts=30
retry_seconds=10
if [[ "${GITHUB_ACTIONS:-}" != true ]]; then
  attempts=${AXO_CRATE_INDEX_ATTEMPTS:-$attempts}
  retry_seconds=${AXO_CRATE_INDEX_RETRY_SECONDS:-$retry_seconds}
fi
[[ "$attempts" =~ ^[1-9][0-9]*$ && "$retry_seconds" =~ ^[0-9]+$ ]] \
  || fail 'poll attempts must be positive and retry seconds must be non-negative'

normalized=$(printf '%s' "$crate" | tr '[:upper:]' '[:lower:]')
case ${#normalized} in
  1) index_path="1/$normalized" ;;
  2) index_path="2/$normalized" ;;
  3) index_path="3/${normalized:0:1}/$normalized" ;;
  *) index_path="${normalized:0:2}/${normalized:2:2}/$normalized" ;;
esac

response=$(mktemp "${TMPDIR:-/tmp}/axocoatl-crate-index.XXXXXX")
trap 'rm -f -- "$response"' EXIT HUP INT TERM

fetch_index() {
  local attempt=$1
  : > "$response"
  if [[ -n "$test_driver" ]]; then
    "$test_driver" "$crate" "$version" "$attempt" "$response"
  else
    curl --retry 3 --retry-all-errors --connect-timeout 10 --max-time 30 \
      --user-agent "axocoatl-release-proof (${GITHUB_SERVER_URL:-https://github.com}/${GITHUB_REPOSITORY:-unknown})" \
      --silent --show-error --output "$response" --write-out '%{http_code}' \
      "https://index.crates.io/$index_path"
  fi
}

for attempt in $(seq 1 "$attempts"); do
  status=$(fetch_index "$attempt")
  case "$status" in
    200)
      jq --slurp --exit-status 'all(.[]; type == "object")' "$response" >/dev/null 2>&1 \
        || fail "sparse-index response for $crate is malformed"
      count=$(jq --slurp --arg crate "$crate" --arg version "$version" \
        '[.[] | select(.name == $crate and .vers == $version)] | length' "$response")
      case "$count" in
        0) ;;
        1)
          jq --slurp --exit-status \
            --arg crate "$crate" --arg version "$version" --arg checksum "$expected_checksum" '
              [.[] | select(.name == $crate and .vers == $version)][0]
              | .cksum == $checksum and .yanked == false
            ' "$response" >/dev/null \
            || fail "$crate $version differs from the reviewed archive or is yanked in the sparse index"
          echo "Crate sparse-index proof: PASS ($crate $version is exact, non-yanked, and installable)"
          exit 0
          ;;
        *) fail "sparse index contains $count entries for $crate $version" ;;
      esac
      ;;
    404) ;;
    *) fail "unexpected sparse-index HTTP $status for $crate $version" ;;
  esac
  if [[ "$attempt" -lt "$attempts" ]]; then
    sleep "$retry_seconds"
  fi
done

fail "$crate $version is not exact and visible in the sparse index after $attempts attempts"
