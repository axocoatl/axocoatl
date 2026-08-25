#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)"

echo "seed-automation.sh now seeds the complete runtime demo set."
exec "$SCRIPT_DIR/seed-runtime-demos.sh" "$@"
