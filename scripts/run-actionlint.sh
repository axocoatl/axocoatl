#!/usr/bin/env bash
# Run the reviewed actionlint version locally and in CI.
set -euo pipefail

version=v1.7.12
# GitHub introduced this hosted Intel runner label after actionlint v1.7.12's
# embedded label catalog. Ignore only that exact catalog warning.
runner_label_ignore='label "macos-15-intel" is unknown'
# GitHub added concurrency.queue in May 2026. actionlint v1.7.12 predates
# that schema addition, so ignore only its exact stale-schema diagnostic; the
# repository contract below validates every production queue structurally.
concurrency_queue_ignore='unexpected key "queue" for "concurrency" section'

if command -v actionlint >/dev/null 2>&1 \
  && [[ "$(actionlint -version 2>/dev/null | sed -n '1p')" == "$version" ]]; then
  exec actionlint \
    -ignore "$runner_label_ignore" \
    -ignore "$concurrency_queue_ignore" \
    "$@"
fi

command -v go >/dev/null 2>&1 || {
  echo "run-actionlint: Go is required to install actionlint $version" >&2
  exit 1
}

work_dir=$(mktemp -d "${TMPDIR:-/tmp}/axocoatl-actionlint.XXXXXX")
trap 'rm -rf -- "$work_dir"' EXIT HUP INT TERM
GOBIN="$work_dir/bin" go install "github.com/rhysd/actionlint/cmd/actionlint@$version"
[[ "$("$work_dir/bin/actionlint" -version | sed -n '1p')" == "$version" ]] \
  || { echo "run-actionlint: installed actionlint version did not match $version" >&2; exit 1; }
"$work_dir/bin/actionlint" \
  -ignore "$runner_label_ignore" \
  -ignore "$concurrency_queue_ignore" \
  "$@"
