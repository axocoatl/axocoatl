#!/usr/bin/env bash
# Resolve GitHub release state and evaluate it with the fixture-tested pure checker.
set -euo pipefail

fail() {
  echo "verify-public-release: $*" >&2
  exit 1
}

usage() {
  echo "Usage: verify-public-release.sh <required|optional> [called-tag] [called-sha]" >&2
  exit 2
}

[[ $# -ge 1 && $# -le 3 ]] || usage
mode=$1
called_tag=${2:-}
called_sha=${3:-}
case "$mode" in required|optional) ;; *) usage ;; esac

script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
repo_root=$(CDPATH= cd -- "$script_dir/.." && pwd)
if [[ "${GITHUB_ACTIONS:-}" != true && -n "${AXO_PUBLIC_RELEASE_REPO_ROOT:-}" ]]; then
  [[ -d "$AXO_PUBLIC_RELEASE_REPO_ROOT/.git" ]] \
    || fail 'local repository fixture is missing .git'
  repo_root=$(CDPATH= cd -- "$AXO_PUBLIC_RELEASE_REPO_ROOT" && pwd)
fi
cd "$repo_root"

version=$(awk '
  $0 == "[package]" { package_section = 1; next }
  package_section && /^\[/ { exit }
  package_section && $1 == "version" {
    gsub(/"/, "", $3)
    print $3
    exit
  }
' axocoatl-cli/Cargo.toml)
[[ -n "$version" ]] || fail "could not read axocoatl-cli product version"
expected_tag="v$version"

if [[ "$mode" == required ]]; then
  [[ -n "$called_tag" && "$called_tag" == "$expected_tag" ]] \
    || fail "called release '$called_tag' does not match product $expected_tag"
  [[ "$called_sha" =~ ^[0-9a-f]{40}$ ]] \
    || fail "required deployment source SHA is malformed: $called_sha"
  [[ "$(git rev-parse 'HEAD^{commit}')" == "$called_sha" ]] \
    || fail "deployment checkout does not match frozen source $called_sha"
  remote_name=origin
  if [[ "${GITHUB_ACTIONS:-}" != true && -n "${AXO_PUBLIC_RELEASE_REMOTE:-}" ]]; then
    remote_name=$AXO_PUBLIC_RELEASE_REMOTE
  fi
  remote_ref=refs/axocoatl-deploy/remote-tag
  git fetch --no-tags --force "$remote_name" \
    "+refs/tags/$called_tag:$remote_ref"
  remote_commit=$(git rev-parse "$remote_ref^{commit}")
  [[ "$remote_commit" == "$called_sha" ]] \
    || fail "remote tag $called_tag resolves to $remote_commit, expected $called_sha"
else
  [[ -z "$called_tag" && -z "$called_sha" ]] \
    || fail "optional deployment must not receive frozen release inputs"
  # A workflow_dispatch UI lets a caller select any branch. Production deploys
  # outside a release are allowed only from main.
  if [[ "${GITHUB_ACTIONS:-}" == true ]]; then
    [[ "${GITHUB_REF:-}" == refs/heads/main ]] \
      || fail "standalone production deployment must run from refs/heads/main"
    [[ "$(git rev-parse 'HEAD^{commit}')" == "${GITHUB_SHA:-}" ]] \
      || fail "standalone deployment checkout differs from GITHUB_SHA"
  fi
fi

response=$(mktemp "${TMPDIR:-/tmp}/axocoatl-public-release.XXXXXX")
latest_response=$(mktemp "${TMPDIR:-/tmp}/axocoatl-latest-release.XXXXXX")
trap 'rm -f -- "$response" "$latest_response"' EXIT HUP INT TERM

if [[ "${GITHUB_ACTIONS:-}" != true && -n "${AXO_PUBLIC_RELEASE_FIXTURE:-}" ]]; then
  [[ -f "$AXO_PUBLIC_RELEASE_FIXTURE" ]] || fail "fixture response is missing"
  cp "$AXO_PUBLIC_RELEASE_FIXTURE" "$response"
  status=${AXO_PUBLIC_RELEASE_STATUS:-200}
  latest_fixture=${AXO_PUBLIC_RELEASE_LATEST_FIXTURE:-$AXO_PUBLIC_RELEASE_FIXTURE}
  [[ -f "$latest_fixture" ]] || fail "latest-release fixture response is missing"
  cp "$latest_fixture" "$latest_response"
  latest_status=${AXO_PUBLIC_RELEASE_LATEST_STATUS:-200}
else
  [[ -n "${GH_TOKEN:-}" ]] || fail "GH_TOKEN is required"
  [[ -n "${GITHUB_API_URL:-}" && -n "${GITHUB_REPOSITORY:-}" ]] \
    || fail "GitHub API context is required"
  attempts=1
  [[ "$mode" == required ]] && attempts=12
  status=''
  for attempt in $(seq 1 "$attempts"); do
    status=$(curl --retry 2 --retry-all-errors --connect-timeout 10 --max-time 30 \
      --silent --show-error --output "$response" --write-out '%{http_code}' \
      --header 'Accept: application/vnd.github+json' \
      --header "Authorization: Bearer $GH_TOKEN" \
      --header 'X-GitHub-Api-Version: 2022-11-28' \
      "$GITHUB_API_URL/repos/$GITHUB_REPOSITORY/releases/tags/$expected_tag")
    [[ "$status" == 200 ]] && break
    if [[ "$mode" == required && "$status" == 404 && "$attempt" -lt "$attempts" ]]; then
      sleep "${AXO_PUBLIC_RELEASE_RETRY_SECONDS:-5}"
      continue
    fi
    break
  done
fi

published=$("$script_dir/check-public-release.sh" \
  "$mode" "$expected_tag" "$status" "$response")
if [[ "$published" != true ]]; then
  printf '%s\n' "$published"
  exit 0
fi

if [[ -z "${latest_status:-}" ]]; then
  latest_status=$(curl --retry 2 --retry-all-errors --connect-timeout 10 --max-time 30 \
    --silent --show-error --output "$latest_response" --write-out '%{http_code}' \
    --header 'Accept: application/vnd.github+json' \
    --header "Authorization: Bearer $GH_TOKEN" \
    --header 'X-GitHub-Api-Version: 2022-11-28' \
    "$GITHUB_API_URL/repos/$GITHUB_REPOSITORY/releases/latest")
fi

if [[ "$latest_status" != 200 ]] \
  || ! jq --exit-status . "$latest_response" >/dev/null 2>&1; then
  fail "could not prove GitHub's latest release (HTTP $latest_status)"
fi

release_id=$(jq --exit-status --raw-output '.id | numbers' "$response") \
  || fail "public release is missing a numeric ID"
latest_id=$(jq --exit-status --raw-output '.id | numbers' "$latest_response") \
  || fail "latest release is missing a numeric ID"
latest_tag=$(jq --exit-status --raw-output '.tag_name | strings' "$latest_response") \
  || fail "latest release is missing its tag"
if [[ "$release_id" != "$latest_id" || "$latest_tag" != "$expected_tag" ]]; then
  if [[ "$mode" == optional ]]; then
    printf 'false\n'
    exit 0
  fi
  fail "$expected_tag is public but is not GitHub's latest release"
fi

printf 'true\n'
