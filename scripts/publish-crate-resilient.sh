#!/usr/bin/env bash
# Idempotently publish one reviewed crate and prove API plus sparse-index visibility.
set -euo pipefail

fail() {
  echo "publish-crate-resilient: $*" >&2
  exit 1
}

[[ $# -eq 6 ]] || {
  echo 'Usage: publish-crate-resilient.sh <crate> <version> <archive> <sha256> <tag> <commit>' >&2
  exit 2
}

crate=$1
version=$2
archive=$3
expected_checksum=$4
release_tag=$5
expected_commit=$6

[[ "$crate" =~ ^[a-z0-9][a-z0-9_-]*$ ]] || fail "invalid crate name '$crate'"
[[ "$version" =~ ^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$ ]] \
  || fail "version '$version' is not stable MAJOR.MINOR.PATCH SemVer"
[[ "$release_tag" == "v$version" ]] \
  || fail "release tag $release_tag does not match crate version $version"
[[ "$expected_commit" =~ ^[0-9a-f]{40}$ ]] || fail 'expected commit must be a full lowercase SHA'
[[ "$expected_checksum" =~ ^[0-9a-f]{64}$ ]] || fail 'expected checksum must be lowercase SHA-256'
[[ -f "$archive" && ! -L "$archive" ]] || fail "reviewed archive is missing or unsafe: $archive"
command -v jq >/dev/null 2>&1 || fail 'jq is required'

script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
test_driver=${AXO_CRATE_TEST_DRIVER:-}
if [[ "${GITHUB_ACTIONS:-}" == true && -n "$test_driver" ]]; then
  fail 'test drivers are forbidden in GitHub Actions'
fi
if [[ -n "$test_driver" ]]; then
  [[ -x "$test_driver" ]] || fail "test driver is not executable: $test_driver"
fi

hash_file() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1" | awk '{print $1}'
  elif command -v shasum >/dev/null 2>&1; then
    shasum -a 256 "$1" | awk '{print $1}'
  else
    fail 'sha256sum or shasum is required'
  fi
}

assert_archive_unchanged() {
  local actual
  actual=$(hash_file "$archive")
  [[ "$actual" == "$expected_checksum" ]] \
    || fail "reviewed archive checksum changed: expected $expected_checksum, got $actual"
}

assert_archive_unchanged

max_publish_attempts=3
api_poll_attempts=18
index_poll_attempts=30
retry_seconds=10
if [[ "${GITHUB_ACTIONS:-}" != true ]]; then
  max_publish_attempts=${AXO_CRATE_MAX_PUBLISH_ATTEMPTS:-$max_publish_attempts}
  api_poll_attempts=${AXO_CRATE_API_POLL_ATTEMPTS:-$api_poll_attempts}
  index_poll_attempts=${AXO_CRATE_INDEX_POLL_ATTEMPTS:-$index_poll_attempts}
  retry_seconds=${AXO_CRATE_RETRY_SECONDS:-$retry_seconds}
fi
for setting in "$max_publish_attempts" "$api_poll_attempts" "$index_poll_attempts" "$retry_seconds"; do
  [[ "$setting" =~ ^[0-9]+$ ]] || fail 'retry settings must be non-negative integers'
done
[[ "$max_publish_attempts" -gt 0 && "$api_poll_attempts" -gt 0 && "$index_poll_attempts" -gt 0 ]] \
  || fail 'publish and visibility attempt counts must be positive'

api_response=$(mktemp "${TMPDIR:-/tmp}/axocoatl-crate-api.XXXXXX")
index_response=$(mktemp "${TMPDIR:-/tmp}/axocoatl-crate-index.XXXXXX")
trap 'rm -f -- "$api_response" "$index_response"' EXIT HUP INT TERM

fetch_api() {
  : > "$api_response"
  if [[ -n "$test_driver" ]]; then
    "$test_driver" fetch-api "$crate" "$version" "$api_response"
  else
    curl --retry 3 --retry-all-errors --connect-timeout 10 --max-time 30 \
      --user-agent "axocoatl-release-workflow (${GITHUB_SERVER_URL:-https://github.com}/${GITHUB_REPOSITORY:-unknown})" \
      --silent --show-error --output "$api_response" --write-out '%{http_code}' \
      "https://crates.io/api/v1/crates/${crate}/${version}"
  fi
}

sparse_index_path() {
  local normalized length
  normalized=$(printf '%s' "$crate" | tr '[:upper:]' '[:lower:]')
  length=${#normalized}
  case "$length" in
    1) printf '1/%s' "$normalized" ;;
    2) printf '2/%s' "$normalized" ;;
    3) printf '3/%s/%s' "${normalized:0:1}" "$normalized" ;;
    *) printf '%s/%s/%s' "${normalized:0:2}" "${normalized:2:2}" "$normalized" ;;
  esac
}

fetch_index() {
  : > "$index_response"
  if [[ -n "$test_driver" ]]; then
    "$test_driver" fetch-index "$crate" "$version" "$index_response"
  else
    local path
    path=$(sparse_index_path)
    curl --retry 3 --retry-all-errors --connect-timeout 10 --max-time 30 \
      --user-agent "axocoatl-release-workflow (${GITHUB_SERVER_URL:-https://github.com}/${GITHUB_REPOSITORY:-unknown})" \
      --silent --show-error --output "$index_response" --write-out '%{http_code}' \
      "https://index.crates.io/$path"
  fi
}

verify_api_exact() {
  jq --exit-status \
    --arg version "$version" \
    --arg checksum "$expected_checksum" \
    '
      .version.num == $version
      and .version.checksum == $checksum
      and .version.yanked == false
    ' "$api_response" >/dev/null \
    || fail "$crate $version differs from the reviewed archive or is yanked in the crates.io API"
}

index_version_state() {
  local matching
  matching=$(jq --slurp --exit-status \
    --arg crate "$crate" --arg version "$version" \
    '[.[] | select(.name == $crate and .vers == $version)] | length' \
    "$index_response") \
    || fail "sparse-index response for $crate is malformed"
  case "$matching" in
    0) echo absent ;;
    1)
      jq --slurp --exit-status \
        --arg crate "$crate" --arg version "$version" --arg checksum "$expected_checksum" \
        '
          [.[] | select(.name == $crate and .vers == $version)] as $matches
          | $matches[0].cksum == $checksum
            and $matches[0].yanked == false
        ' "$index_response" >/dev/null \
        || fail "$crate $version differs from the reviewed archive or is yanked in the sparse index"
      echo visible
      ;;
    *) fail "sparse index contains $matching entries for $crate $version" ;;
  esac
}

wait_for_index() {
  local attempt status state
  for attempt in $(seq 1 "$index_poll_attempts"); do
    status=$(fetch_index)
    case "$status" in
      200)
        state=$(index_version_state)
        if [[ "$state" == visible ]]; then
          echo "$crate $version is exact and visible in the sparse index."
          return 0
        fi
        ;;
      404) ;;
      *) fail "unexpected sparse-index HTTP $status for $crate $version" ;;
    esac
    if [[ "$attempt" -lt "$index_poll_attempts" ]]; then
      sleep "$retry_seconds"
    fi
  done
  fail "$crate $version is API-visible but not exact and visible in the sparse index"
}

require_remote_tag() {
  if [[ -n "$test_driver" ]]; then
    "$test_driver" require-tag "$release_tag" "$expected_commit" \
      || fail "remote tag proof failed before publishing $crate $version"
    return
  fi
  local remote_ref=refs/axocoatl-release/crate-publish-tag remote_commit
  git fetch --no-tags --force origin \
    "+refs/tags/$release_tag:$remote_ref" \
    || fail "could not refresh remote tag $release_tag before publishing $crate $version"
  remote_commit=$(git rev-parse "$remote_ref^{commit}") \
    || fail "could not resolve refreshed remote tag $release_tag"
  [[ "$remote_commit" == "$expected_commit" ]] \
    || fail "remote tag $release_tag resolves to $remote_commit, expected $expected_commit"
}

publish_archive() {
  if [[ -n "$test_driver" ]]; then
    "$test_driver" publish "$crate" "$version"
  else
    cargo publish --locked --no-verify -p "$crate"
  fi
}

status=$(fetch_api)
case "$status" in
  200)
    verify_api_exact
    wait_for_index
    exit 0
    ;;
  404) ;;
  *) fail "unexpected crates.io API HTTP $status for $crate $version" ;;
esac

for publish_attempt in $(seq 1 "$max_publish_attempts"); do
  # Both public-release order and tag identity are read again immediately
  # before every irreversible upload attempt, including retries.
  "$script_dir/verify-release-order.sh" "$release_tag" >/dev/null
  require_remote_tag

  status=$(fetch_api)
  case "$status" in
    200)
      verify_api_exact
      wait_for_index
      exit 0
      ;;
    404) ;;
    *) fail "unexpected crates.io API HTTP $status immediately before publishing $crate $version" ;;
  esac

  publish_status=0
  if publish_archive; then
    echo "cargo accepted the $crate $version upload."
  else
    publish_status=$?
    echo "cargo returned $publish_status for $crate $version; resolving the ambiguous upload from public state."
  fi
  assert_archive_unchanged

  api_visible=false
  for poll_attempt in $(seq 1 "$api_poll_attempts"); do
    status=$(fetch_api)
    case "$status" in
      200)
        verify_api_exact
        api_visible=true
        break
        ;;
      404) ;;
      *) fail "unexpected crates.io API HTTP $status while proving $crate $version" ;;
    esac
    if [[ "$poll_attempt" -lt "$api_poll_attempts" ]]; then
      sleep "$retry_seconds"
    fi
  done

  if [[ "$api_visible" == true ]]; then
    wait_for_index
    exit 0
  fi

  if [[ "$publish_attempt" -lt "$max_publish_attempts" ]]; then
    echo "$crate $version is still absent after publish status $publish_status; retrying safely after another public-state proof."
  fi
done

fail "$crate $version remains absent after $max_publish_attempts guarded publish attempts"
