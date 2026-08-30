#!/usr/bin/env bash
# Build the exact marketing payload accepted by CI, release, and deployment.
set -euo pipefail

usage() {
  echo "Usage: verify-marketing-gate.sh <portable|source-bound> <output-directory>" >&2
  exit 2
}

[[ $# -eq 2 ]] || usage
film_mode=$1
output=$2
case "$film_mode" in portable|source-bound) ;; *) usage ;; esac
[[ -n "$output" ]] || usage

script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
repo_root=$(CDPATH= cd -- "$script_dir/.." && pwd)
cd "$repo_root"

./scripts/test-install.sh
./scripts/verify-film-gate.sh "$film_mode"
./sites/marketing/scripts/sync-assets.sh
node sites/marketing/scripts/validate.mjs --strict-films
node sites/marketing/scripts/build.mjs "$output"
cp scripts/install.sh "$output/install.sh"
sh -n "$output/install.sh"
cmp -s scripts/install.sh "$output/install.sh"
node sites/marketing/scripts/validate.mjs "$output" --strict-films

echo "Marketing gate: PASS ($film_mode, $output)"
