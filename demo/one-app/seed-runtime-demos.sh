#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)"
URL="${AXO_URL:-http://127.0.0.1:18080}"
case "$(uname -s)" in
  Darwin) DEFAULT_DEMO_ROOT="/private/tmp/axocoatl-one-app-showcase" ;;
  *) DEFAULT_DEMO_ROOT="/tmp/axocoatl-one-app-showcase" ;;
esac
DEMO_ROOT="${AXOCOATL_DEMO_ROOT:-$DEFAULT_DEMO_ROOT}"
TOKEN_FILE="${AXOCOATL_DATA_DIR:-$DEMO_ROOT/data}/local-api-token"

# The local API needs the daemon's token. It goes to curl through a private
# header file so it never appears in a process listing.
TOKEN="${AXO_TOKEN:-}"
if [ -z "$TOKEN" ]; then
  if [ ! -r "$TOKEN_FILE" ]; then
    echo "No local API token at $TOKEN_FILE. Start the demo with start.sh, or set AXO_TOKEN or AXOCOATL_DATA_DIR." >&2
    exit 1
  fi
  TOKEN="$(cat "$TOKEN_FILE")"
fi

BODY_FILE="$(mktemp)"
HEADER_FILE="$(mktemp)"
trap 'rm -f "$BODY_FILE" "$HEADER_FILE"' EXIT
chmod 600 "$HEADER_FILE"
printf 'Authorization: Bearer %s\n' "$TOKEN" > "$HEADER_FILE"

upsert_automation() {
  json_file="$1"
  automation_id="$2"
  label="$3"
  http_code="$(curl -sS -o "$BODY_FILE" -w '%{http_code}' \
    -X POST "$URL/api/automations" \
    -H "@$HEADER_FILE" \
    -H 'content-type: application/json' \
    --data-binary "@$json_file" || true)"

  if [ "$http_code" = "200" ] || [ "$http_code" = "201" ]; then
    echo "Created $label."
  elif [ "$http_code" = "400" ] && grep -q "already exists" "$BODY_FILE"; then
    curl -fsS -o /dev/null -X PATCH \
      "$URL/api/automations/$automation_id" \
      -H "@$HEADER_FILE" \
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
echo "Fire Settings → Skills → Release candidate ready once; its ReleaseCandidateReady"
echo "event starts the on_event Release gate review Automation."
