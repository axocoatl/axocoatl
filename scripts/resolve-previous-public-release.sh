#!/usr/bin/env bash
# Fetch every GitHub Release page and resolve the prior public stable release.
set -euo pipefail

fail() {
  echo "resolve-previous-public-release: $*" >&2
  exit 1
}

[[ $# -eq 1 ]] || {
  echo 'Usage: resolve-previous-public-release.sh <candidate-tag>' >&2
  exit 2
}

candidate_tag=$1
script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
work_dir=$(mktemp -d "${TMPDIR:-/tmp}/axocoatl-previous-release.XXXXXX")
response="$work_dir/releases.json"
trap 'rm -rf -- "$work_dir"' EXIT HUP INT TERM

if [[ "${GITHUB_ACTIONS:-}" != true && -n "${AXO_PREVIOUS_PUBLIC_RELEASE_FIXTURE:-}" ]]; then
  [[ -f "$AXO_PREVIOUS_PUBLIC_RELEASE_FIXTURE" ]] || fail 'fixture response is missing'
  cp "$AXO_PREVIOUS_PUBLIC_RELEASE_FIXTURE" "$response"
  status=${AXO_PREVIOUS_PUBLIC_RELEASE_STATUS:-200}
else
  [[ -z "${AXO_PREVIOUS_PUBLIC_RELEASE_FIXTURE:-}" ]] \
    || fail 'previous-public-release fixtures are forbidden in GitHub Actions'
  [[ -n "${GH_TOKEN:-}" ]] || fail 'GH_TOKEN is required'
  [[ -n "${GITHUB_API_URL:-}" && -n "${GITHUB_REPOSITORY:-}" ]] \
    || fail 'GitHub API context is required'

  page=1
  pages=()
  while true; do
    page_file="$work_dir/page-$page.json"
    pages+=("$page_file")
    status=$(curl --retry 3 --retry-all-errors --connect-timeout 10 --max-time 30 \
      --silent --show-error --output "$page_file" --write-out '%{http_code}' \
      --header 'Accept: application/vnd.github+json' \
      --header "Authorization: Bearer $GH_TOKEN" \
      --header 'X-GitHub-Api-Version: 2022-11-28' \
      "$GITHUB_API_URL/repos/$GITHUB_REPOSITORY/releases?per_page=100&page=$page")
    [[ "$status" == 200 ]] \
      || fail "GitHub release-list page $page returned HTTP $status"
    jq --exit-status 'type == "array"' "$page_file" >/dev/null 2>&1 \
      || fail "GitHub release-list page $page is not a JSON array"
    count=$(jq 'length' "$page_file")
    [[ "$count" -le 100 ]] \
      || fail "GitHub release-list page $page exceeds the requested page size"
    [[ "$count" -eq 100 ]] || break
    page=$((page + 1))
    [[ "$page" -le 1000 ]] \
      || fail 'GitHub release list exceeds the reviewed pagination ceiling'
  done
  jq --slurp 'add // []' "${pages[@]}" > "$response"
  status=200
fi

"$script_dir/check-previous-public-release.sh" "$candidate_tag" "$status" "$response"
