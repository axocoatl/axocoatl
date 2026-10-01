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

# source-bound also proves the site describes the release being deployed: the
# film portfolio and the newest changelog entry name the CLI version.
validate_site() {
  if [[ "$film_mode" == source-bound ]]; then
    node sites/marketing/scripts/validate.mjs "$@" --release-bound
  else
    node sites/marketing/scripts/validate.mjs "$@"
  fi
}

build_site() {
  ./sites/marketing/scripts/sync-assets.sh
  validate_site "$@"
  node sites/marketing/scripts/build.mjs "$output"
  cp scripts/install.sh "$output/install.sh"
  sh -n "$output/install.sh"
  cmp -s scripts/install.sh "$output/install.sh"
  validate_site "$output" "$@"
}

./scripts/test-install.sh

# While demo/one-app/films/PENDING names the CLI version, the films are not
# recorded. Prove the film manifest alone, then build and validate the site
# without films: build.mjs and validate.mjs read the same declaration.
if ./scripts/verify-film-gate.sh pending; then
  ./scripts/verify-film-gate.sh portable
  build_site
  echo "Marketing gate: PASS ($film_mode, $output; films pending, site built without films)"
  exit 0
fi

./scripts/verify-film-gate.sh "$film_mode"
build_site --strict-films

echo "Marketing gate: PASS ($film_mode, $output)"
