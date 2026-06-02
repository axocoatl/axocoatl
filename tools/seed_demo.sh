#!/usr/bin/env bash
# Seed the "Spec Review · multi-perspective with HITL" demo automation.
#
# Hits POST /api/automations to land the demo in the unified store
# (data/automations.json). Re-running is safe — the API returns a 400
# Conflict if the id exists, in which case we PATCH it instead.
#
# Usage:
#   bash tools/seed_demo.sh                       # defaults to localhost:8080
#   AXO_URL=http://1.2.3.4:8080 bash tools/...    # remote
set -euo pipefail

URL="${AXO_URL:-http://127.0.0.1:8080}"
JSON='{
  "id": "spec-review-demo",
  "name": "Spec Review · multi-perspective with HITL",
  "description": "An architect proposes; reviewers critique in parallel from security/performance/ux angles; if any reviewer flags BLOCKING the operator is asked for guidance; finally a planner produces an action plan. Uses every Phase A node primitive.",
  "trigger": { "kind": "manual" },
  "enabled": true,
  "nodes": [
    {
      "id": "spec-prompt",
      "kind": {
        "type": "text_input",
        "label": "Spec description",
        "default_value": "A real-time chat app that stores every message in plaintext and has no rate limiting.",
        "placeholder": "Describe the spec to review…",
        "multiline": true
      },
      "position": { "x": -260, "y": 0 }
    },
    {
      "id": "architect",
      "kind": {
        "type": "agent",
        "agent_id": "architect",
        "input": { "kind": "from_upstream", "nodes": ["spec-prompt"] }
      },
      "position": { "x": 0, "y": 0 }
    },
    {
      "id": "review-each",
      "kind": {
        "type": "map",
        "input": { "kind": "literal", "value": "[\"security\", \"performance\", \"ux\"]" },
        "body_node": "reviewer-body"
      },
      "position": { "x": 280, "y": 0 }
    },
    {
      "id": "reviewer-body",
      "kind": {
        "type": "agent",
        "agent_id": "reviewer",
        "input": {
          "kind": "template",
          "template": "You are reviewing this spec specifically from a {{item}} angle. Identify problems unique to {{item}}. If any are SHIP-BLOCKING, write the literal word BLOCKING somewhere in your reply.\n\nSpec to review:\n{{node:architect}}"
        }
      },
      "position": { "x": 280, "y": 220 }
    },
    {
      "id": "approval-gate",
      "kind": {
        "type": "conditional",
        "input": { "kind": "from_upstream", "nodes": ["review-each"] },
        "branches": [
          { "name": "blocking", "when": { "op": "contains", "value": "BLOCKING" } },
          { "name": "ok", "when": { "op": "always" } }
        ],
        "default": "ok"
      },
      "position": { "x": 560, "y": 0 }
    },
    {
      "id": "approve",
      "kind": {
        "type": "interrupt",
        "input": {
          "kind": "template",
          "template": "Reviewers flagged BLOCKING issues. Please provide guidance on how to proceed (e.g. \"defer security work to v2 and ship\", \"reject and rework\", etc.). Your guidance becomes the planner'\''s input.\n\nReviews:\n{{node:review-each}}"
        },
        "resume_strategy": "replace"
      },
      "position": { "x": 840, "y": -160 }
    },
    {
      "id": "planner",
      "kind": {
        "type": "agent",
        "agent_id": "planner",
        "input": {
          "kind": "template",
          "template": "Produce a concise action plan.\n\nOriginal spec:\n{{node:architect}}\n\nReviews:\n{{node:review-each}}\n\nOperator guidance (may be empty if no BLOCKING):\n{{node:approve}}"
        }
      },
      "position": { "x": 1120, "y": 0 }
    }
  ],
  "edges": [
    { "from": "spec-prompt",   "to": "architect",     "label": null },
    { "from": "architect",     "to": "review-each",   "label": null },
    { "from": "review-each",   "to": "approval-gate", "label": null },
    { "from": "approval-gate", "to": "approve",       "label": "blocking" },
    { "from": "approval-gate", "to": "planner",       "label": "ok" },
    { "from": "approve",       "to": "planner",       "label": null }
  ]
}'

echo "→ POST $URL/api/automations  (spec-review-demo)"
RESP_BODY="$(mktemp)"
HTTP_CODE=$(curl -sS -o "$RESP_BODY" -w '%{http_code}' \
    -X POST "$URL/api/automations" \
    -H 'content-type: application/json' \
    -d "$JSON" || echo "000")

if [ "$HTTP_CODE" = "200" ] || [ "$HTTP_CODE" = "201" ]; then
    echo "✓ created"
elif [ "$HTTP_CODE" = "400" ] && grep -q "already exists" "$RESP_BODY"; then
    echo "  exists already — updating in place via PATCH"
    curl -sS -o /dev/null -X PATCH \
        "$URL/api/automations/spec-review-demo" \
        -H 'content-type: application/json' -d "$JSON"
    echo "✓ updated"
else
    echo "✗ HTTP $HTTP_CODE"
    cat "$RESP_BODY"; echo
    rm -f "$RESP_BODY"
    exit 1
fi
rm -f "$RESP_BODY"

cat <<EOF

Try it from the dashboard:
  1. Open the Automations tab — you should see "Spec Review · multi-perspective with HITL"
  2. Click the card → editor shows 6 nodes (architect → review-each → approval-gate → approve / planner)
  3. Click ✎ Edit to inspect any node, or ▶ Run to fire it with a prompt
  4. Watch Studio for live pulses; the Activity feed in the right rail shows every step
  5. If the reviewers find BLOCKING issues, the ⏸ pill in the top bar lights up — resume from there
  6. ⟲ Runs shows the full step-by-step history after each run
EOF
