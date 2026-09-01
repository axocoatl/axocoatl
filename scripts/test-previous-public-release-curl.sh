#!/usr/bin/env bash
# Minimal curl double used only by test-previous-public-release.sh.
set -euo pipefail

output=''
url=''
while [[ $# -gt 0 ]]; do
  case "$1" in
    --output)
      output=$2
      shift 2
      ;;
    --retry|--connect-timeout|--max-time|--write-out|--header)
      shift 2
      ;;
    --retry-all-errors|--silent|--show-error)
      shift
      ;;
    http://*|https://*)
      url=$1
      shift
      ;;
    *)
      echo "test-previous-public-release-curl: unexpected argument '$1'" >&2
      exit 1
      ;;
  esac
done

[[ -n "$output" && -n "$url" ]] || {
  echo 'test-previous-public-release-curl: output path and URL are required' >&2
  exit 1
}
[[ "$url" == *'/releases?per_page=100&page='* ]] || {
  echo "test-previous-public-release-curl: unexpected endpoint $url" >&2
  exit 1
}

page=${url##*page=}
source_file="$AXO_FAKE_RELEASE_PAGE_DIR/page-$page.json"
[[ -f "$source_file" ]] || {
  echo "test-previous-public-release-curl: no fixture for page $page" >&2
  exit 1
}

cp "$source_file" "$output"
printf '%s\n' "$url" >> "$AXO_FAKE_CURL_LOG"
printf '200'
