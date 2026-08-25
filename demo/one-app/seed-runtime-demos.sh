#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)"
URL="${AXO_URL:-http://127.0.0.1:18080}"
BODY_FILE="$(mktemp)"
trap 'rm -f "$BODY_FILE"' EXIT

upsert_automation() {
  json_file="$1"
  automation_id="$2"
  label="$3"
  http_code="$(curl -sS -o "$BODY_FILE" -w '%{http_code}' \
    -X POST "$URL/api/automations" \
    -H 'content-type: application/json' \
    --data-binary "@$json_file" || true)"

  if [ "$http_code" = "200" ] || [ "$http_code" = "201" ]; then
    echo "Created $label."
  elif [ "$http_code" = "400" ] && grep -q "already exists" "$BODY_FILE"; then
    curl -fsS -o /dev/null -X PATCH \
      "$URL/api/automations/$automation_id" \
      -H 'content-type: application/json' \
      --data-binary "@$json_file"
    echo "Updated $label."
  else
    echo "Could not seed $label (HTTP $http_code):" >&2
    cat "$BODY_FILE" >&2
    exit 1
  fi
}

curl -fsS -o /dev/null "$URL/health"
upsert_automation "$SCRIPT_DIR/automation/spec-review.json" "spec-review-demo" "Spec review"
upsert_automation "$SCRIPT_DIR/automation/release-gate.json" "release-gate-review" "Release gate review"
upsert_automation "$SCRIPT_DIR/automation/weather-brief.json" "weather-brief-demo" "Weather brief"

echo
echo "Runtime demonstrations are ready in Settings → Automations."
echo "Fire Settings → Skills → Release candidate ready for the event-lattice path."
