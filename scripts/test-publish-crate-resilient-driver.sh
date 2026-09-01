#!/usr/bin/env bash
# Offline state driver used only by test-publish-crate-resilient.sh.
set -euo pipefail

operation=$1
crate=${2:-}
version=${3:-}
argument=${4:-}
state_dir=${AXO_CRATE_TEST_STATE:?}
scenario=${AXO_CRATE_TEST_SCENARIO:?}
checksum=${AXO_CRATE_TEST_CHECKSUM:?}

counter() {
  local name=$1 file="$state_dir/$1"
  if [[ -f "$file" ]]; then
    tr -d '[:space:]' < "$file"
  else
    echo 0
  fi
}

increment() {
  local name=$1 value
  value=$(counter "$name")
  value=$((value + 1))
  printf '%s\n' "$value" > "$state_dir/$name"
  echo "$value"
}

write_api() {
  local response=$1 effective_checksum=${2:-$checksum}
  jq -n --arg version "$version" --arg checksum "$effective_checksum" \
    '{version:{num:$version, checksum:$checksum, yanked:false}}' > "$response"
}

write_index() {
  local response=$1
  jq -cn --arg crate "$crate" --arg version "$version" --arg checksum "$checksum" \
    '{name:$crate, vers:$version, cksum:$checksum, yanked:false}' > "$response"
}

case "$operation" in
  require-tag)
    tag_count=$(increment tag-count)
    if [[ "$scenario" == tag-failure ]]; then
      exit 18
    fi
    if [[ "$scenario" == tag-failure-after-retry && "$tag_count" -ge 2 ]]; then
      exit 18
    fi
    ;;
  publish)
    publish_count=$(increment publish-count)
    case "$scenario" in
      already|divergent) exit 90 ;;
      ambiguous|tag-failure-after-retry) exit 17 ;;
      retry) [[ "$publish_count" -ge 2 ]] ;;
      *) exit 0 ;;
    esac
    ;;
  fetch-api)
    response=$argument
    publish_count=$(counter publish-count)
    case "$scenario" in
      already)
        write_api "$response"
        echo 200
        ;;
      divergent)
        write_api "$response" ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff
        echo 200
        ;;
      retry)
        if [[ "$publish_count" -ge 2 ]]; then
          write_api "$response"
          echo 200
        else
          echo 404
        fi
        ;;
      tag-failure-after-retry)
        echo 404
        ;;
      api-before-index|ambiguous|index-never)
        if [[ "$publish_count" -ge 1 ]]; then
          write_api "$response"
          echo 200
        else
          echo 404
        fi
        ;;
      tag-failure) echo 404 ;;
      *) exit 91 ;;
    esac
    ;;
  fetch-index)
    response=$argument
    case "$scenario" in
      api-before-index)
        index_count=$(increment index-count)
        if [[ "$index_count" -lt 3 ]]; then
          echo 404
        else
          write_index "$response"
          echo 200
        fi
        ;;
      index-never) echo 404 ;;
      already|ambiguous|retry)
        write_index "$response"
        echo 200
        ;;
      *) exit 92 ;;
    esac
    ;;
  *) exit 93 ;;
esac
