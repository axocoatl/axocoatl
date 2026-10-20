#!/usr/bin/env bash
# Build the exact documentation payload accepted by CI, release, and deployment.
set -euo pipefail

[[ $# -eq 0 ]] || {
  echo 'Usage: verify-docs-gate.sh' >&2
  exit 2
}

script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
repo_root=$(CDPATH= cd -- "$script_dir/.." && pwd)
cd "$repo_root/sites/docs"

npm ci
node --test ./scripts/audit-gate.test.mjs
node ./scripts/audit-gate.mjs
# Every first-run command and snippet in the README, llms.txt, the marketing
# site and the docs' first-run pages runs as written, and the claims around them
# hold (js-yaml is a devDependency of sites/docs, installed by the npm ci above).
# AXOCOATL_COPY_PASTE_BIN names an axocoatl binary that also validates the
# configuration and loadout snippets; the product browser gate runs the same
# check with the binary it builds.
node --test "$repo_root/scripts/check-copy-paste.test.mjs"
node "$repo_root/scripts/check-copy-paste.mjs"
npm run check:content
npm run build
npm run check:links

echo 'Documentation gate: PASS'
