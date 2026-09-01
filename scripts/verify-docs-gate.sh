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
npm audit --audit-level=high
npm run check:content
npm run build
npm run check:links

echo 'Documentation gate: PASS'
