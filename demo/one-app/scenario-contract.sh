#!/usr/bin/env bash

# Shared immutable fixture metadata for prepare.sh and verify-scenario.sh.
# This file is sourced; callers must set SCRIPT_DIR first.

scenario_list() {
  cat <<'EOF'
northstar-storefront
harbor-catalog
signal-desk
EOF
}

scenario_load() {
  local requested="$1"

  SCENARIO_KEY="$requested"
  SCENARIO_FAILURE_TITLES=()

  case "$requested" in
    northstar-storefront)
      SCENARIO_FIXTURE="$SCRIPT_DIR/workspace-template"
      SCENARIO_PACKAGE="northstar-supply-orders"
      SCENARIO_COMMIT="Seed storefront discount regression"
      SCENARIO_TESTS=6
      SCENARIO_PASS=5
      SCENARIO_FAIL=1
      SCENARIO_FAILURE_TITLES=(
        "never returns a negative payable total"
      )
      ;;
    harbor-catalog)
      SCENARIO_FIXTURE="$SCRIPT_DIR/fixtures/harbor-catalog"
      SCENARIO_PACKAGE="harbor-catalog"
      SCENARIO_COMMIT="Seed catalog cache coherency regression"
      SCENARIO_TESTS=6
      SCENARIO_PASS=3
      SCENARIO_FAIL=3
      SCENARIO_FAILURE_TITLES=(
        "reflects a newly added match after a query has been cached"
        "reflects an updated item after a query has been cached"
        "does not return a removed item from a cached result"
      )
      ;;
    signal-desk)
      SCENARIO_FIXTURE="$SCRIPT_DIR/fixtures/signal-desk"
      SCENARIO_PACKAGE="signal-desk"
      SCENARIO_COMMIT="Seed incident correlation regression"
      SCENARIO_TESTS=5
      SCENARIO_PASS=3
      SCENARIO_FAIL=2
      SCENARIO_FAILURE_TITLES=(
        "correlates evidence from one deployment into one incident"
        "retains the strongest severity across correlated signals"
      )
      ;;
    *)
      echo "Unknown demo scenario: $requested" >&2
      echo "Available scenarios:" >&2
      scenario_list | sed 's/^/  /' >&2
      return 2
      ;;
  esac
}
