#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)"
# shellcheck source=scenario-contract.sh
source "$SCRIPT_DIR/scenario-contract.sh"

usage() {
  cat <<EOF
Usage: $0 SCENARIO WORKSPACE [CHECK_LOG]

Verify that a prepared demo workspace still has the exact intentional red
check for SCENARIO. CHECK_LOG defaults to a file inside WORKSPACE/.git.

Available scenarios:
$(scenario_list | sed 's/^/  /')
EOF
}

if [ "$#" -lt 2 ] || [ "$#" -gt 3 ]; then
  usage >&2
  exit 2
fi

SCENARIO="$1"
WORKSPACE="$2"
scenario_load "$SCENARIO"

if [ ! -d "$WORKSPACE" ] || [ ! -f "$WORKSPACE/package.json" ]; then
  echo "Not a demo workspace: $WORKSPACE" >&2
  exit 2
fi

if [ "$#" -eq 3 ]; then
  CHECK_LOG="$3"
elif [ -d "$WORKSPACE/.git" ]; then
  CHECK_LOG="$WORKSPACE/.git/axocoatl-seed-check.log"
else
  echo "CHECK_LOG is required when WORKSPACE is not a Git repository." >&2
  exit 2
fi

for required in node npm grep; do
  if ! command -v "$required" >/dev/null 2>&1; then
    echo "Missing required command: $required" >&2
    exit 1
  fi
done

ACTUAL_PACKAGE="$(
  node -e '
    const fs = require("node:fs");
    const packagePath = process.argv[1];
    process.stdout.write(JSON.parse(fs.readFileSync(packagePath, "utf8")).name ?? "");
  ' "$WORKSPACE/package.json"
)"
if [ "$ACTUAL_PACKAGE" != "$SCENARIO_PACKAGE" ]; then
  echo "Scenario $SCENARIO expects package $SCENARIO_PACKAGE, found $ACTUAL_PACKAGE." >&2
  exit 1
fi

mkdir -p "$(dirname -- "$CHECK_LOG")"
if (cd "$WORKSPACE" && npm run check >"$CHECK_LOG" 2>&1); then
  echo "Scenario $SCENARIO unexpectedly passed. Its seed must remain intentionally red." >&2
  exit 1
fi

for title in "${SCENARIO_FAILURE_TITLES[@]}"; do
  if ! grep -Fq "$title" "$CHECK_LOG"; then
    echo "Scenario $SCENARIO failed for an unexpected reason; missing failure: $title" >&2
    echo "Read $CHECK_LOG" >&2
    exit 1
  fi
done

if ! grep -Eq "tests[[:space:]]+$SCENARIO_TESTS[[:space:]]*$" "$CHECK_LOG" ||
   ! grep -Eq "pass[[:space:]]+$SCENARIO_PASS[[:space:]]*$" "$CHECK_LOG" ||
   ! grep -Eq "fail[[:space:]]+$SCENARIO_FAIL[[:space:]]*$" "$CHECK_LOG" ||
   ! grep -Eq 'cancelled[[:space:]]+0[[:space:]]*$' "$CHECK_LOG" ||
   ! grep -Eq 'skipped[[:space:]]+0[[:space:]]*$' "$CHECK_LOG" ||
   ! grep -Eq 'todo[[:space:]]+0[[:space:]]*$' "$CHECK_LOG"; then
  echo "Scenario $SCENARIO does not match its expected initial check summary:" >&2
  echo "  tests=$SCENARIO_TESTS pass=$SCENARIO_PASS fail=$SCENARIO_FAIL cancelled=0 skipped=0 todo=0" >&2
  echo "Read $CHECK_LOG" >&2
  exit 1
fi

echo "Verified $SCENARIO: $SCENARIO_TESTS tests, $SCENARIO_PASS passing, $SCENARIO_FAIL intentionally failing."
echo "Check log: $CHECK_LOG"
