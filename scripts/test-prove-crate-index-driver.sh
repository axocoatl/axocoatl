#!/usr/bin/env bash
set -euo pipefail

crate=$1
version=$2
attempt=$3
response=$4
scenario=${AXO_CRATE_INDEX_SCENARIO:?}
checksum=${AXO_CRATE_INDEX_CHECKSUM:?}

write_entry() {
  local effective_checksum=${1:-$checksum} yanked=${2:-false}
  jq -cn \
    --arg crate "$crate" --arg version "$version" --arg checksum "$effective_checksum" \
    --argjson yanked "$yanked" \
    '{name:$crate, vers:$version, cksum:$checksum, yanked:$yanked}' >> "$response"
}

case "$scenario" in
  visible) write_entry; echo 200 ;;
  lag)
    if [[ "$attempt" -lt 3 ]]; then echo 404; else write_entry; echo 200; fi
    ;;
  mismatch) write_entry "$(printf 'f%.0s' {1..64})"; echo 200 ;;
  yanked) write_entry "$checksum" true; echo 200 ;;
  duplicate) write_entry; write_entry; echo 200 ;;
  malformed) printf '%s\n' '{bad' > "$response"; echo 200 ;;
  never) echo 404 ;;
  http-error) echo 503 ;;
  *) exit 90 ;;
esac
