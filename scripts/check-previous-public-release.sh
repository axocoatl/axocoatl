#!/usr/bin/env bash
# Resolve the greatest public stable GitHub Release strictly below a candidate.
set -euo pipefail
LC_ALL=C

fail() {
  echo "check-previous-public-release: $*" >&2
  exit 1
}

[[ $# -eq 3 ]] || {
  echo 'Usage: check-previous-public-release.sh <candidate-tag> <http-status> <releases.json>' >&2
  exit 2
}

candidate_tag=$1
status=$2
response=$3

command -v jq >/dev/null 2>&1 || fail 'jq is required'
[[ -f "$response" ]] || fail "response file is missing: $response"
[[ "$candidate_tag" =~ ^v(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$ ]] \
  || fail "candidate tag '$candidate_tag' is not a stable MAJOR.MINOR.PATCH SemVer tag"

[[ "$status" == 200 ]] \
  || fail "GitHub release-list lookup returned HTTP $status"
jq --exit-status '
  type == "array"
  and all(.[];
    type == "object"
    and (.tag_name | type) == "string"
    and (.draft | type) == "boolean"
    and (.prerelease | type) == "boolean")
' "$response" >/dev/null 2>&1 \
  || fail 'release-list response is not a well-formed JSON array'

compare_numeric() {
  local left=$1 right=$2
  if [[ ${#left} -lt ${#right} ]]; then
    echo -1
  elif [[ ${#left} -gt ${#right} ]]; then
    echo 1
  elif [[ "$left" == "$right" ]]; then
    echo 0
  elif [[ "$left" < "$right" ]]; then
    echo -1
  else
    echo 1
  fi
}

compare_tags() {
  local left_tag=$1 right_tag=$2
  local left_major left_minor left_patch right_major right_minor right_patch
  IFS=. read -r left_major left_minor left_patch <<<"${left_tag#v}"
  IFS=. read -r right_major right_minor right_patch <<<"${right_tag#v}"
  local component comparison
  for component in \
    "$left_major:$right_major" \
    "$left_minor:$right_minor" \
    "$left_patch:$right_patch"; do
    comparison=$(compare_numeric "${component%%:*}" "${component#*:}")
    if [[ "$comparison" != 0 ]]; then
      echo "$comparison"
      return
    fi
  done
  echo 0
}

previous=''
seen=$'\n'
while IFS= read -r public_tag; do
  [[ "$public_tag" =~ ^v(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$ ]] \
    || fail "public stable release tag '$public_tag' is not stable MAJOR.MINOR.PATCH SemVer"
  [[ "$seen" != *$'\n'"$public_tag"$'\n'* ]] \
    || fail "release list contains duplicate public tag $public_tag"
  seen+="$public_tag"$'\n'

  [[ "$(compare_tags "$public_tag" "$candidate_tag")" == -1 ]] || continue
  if [[ -z "$previous" || "$(compare_tags "$public_tag" "$previous")" == 1 ]]; then
    previous=$public_tag
  fi
done < <(jq --raw-output '.[] | select(.draft == false and .prerelease == false) | .tag_name' "$response")

[[ -n "$previous" ]] \
  || fail "no prior public stable GitHub Release exists below candidate $candidate_tag"

printf '%s\n' "$previous"
