#!/usr/bin/env bash
set -euo pipefail

URL="${AXO_URL:-http://127.0.0.1:18080}"
BODY_FILE="$(mktemp)"
trap 'rm -f "$BODY_FILE"' EXIT

JSON='{
  "id": "spec-review-demo",
  "name": "Spec review · multi-perspective with HITL",
  "description": "Architect a proposal, review security/performance/UX in parallel, pause on blocking issues, then produce an action plan.",
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
          "template": "Review this spec from the {{item}} angle. If an issue is ship-blocking, include BLOCKING.\n\nSpec:\n{{node:architect}}"
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
          "template": "Reviewers found blocking issues. Give the planner explicit operator guidance.\n\nReviews:\n{{node:review-each}}"
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
          "template": "Produce a concise action plan.\n\nDesign:\n{{node:architect}}\n\nReviews:\n{{node:review-each}}\n\nOperator guidance:\n{{node:approve}}"
        }
      },
      "position": { "x": 1120, "y": 0 }
    }
  ],
  "edges": [
    { "from": "spec-prompt", "to": "architect", "label": null },
    { "from": "architect", "to": "review-each", "label": null },
    { "from": "review-each", "to": "approval-gate", "label": null },
    { "from": "approval-gate", "to": "approve", "label": "blocking" },
    { "from": "approval-gate", "to": "planner", "label": "ok" },
    { "from": "approve", "to": "planner", "label": null }
  ]
}'

HTTP_CODE="$(curl -sS -o "$BODY_FILE" -w '%{http_code}' \
  -X POST "$URL/api/automations" \
  -H 'content-type: application/json' \
  -d "$JSON" || true)"

if [ "$HTTP_CODE" = "200" ] || [ "$HTTP_CODE" = "201" ]; then
  echo "Created spec-review-demo."
elif [ "$HTTP_CODE" = "400" ] && grep -q "already exists" "$BODY_FILE"; then
  curl -fsS -o /dev/null -X PATCH \
    "$URL/api/automations/spec-review-demo" \
    -H 'content-type: application/json' \
    -d "$JSON"
  echo "Updated spec-review-demo."
else
  echo "Automation seed failed with HTTP $HTTP_CODE:" >&2
  cat "$BODY_FILE" >&2
  exit 1
fi

echo "Open Settings → Automations → Spec review · multi-perspective with HITL."
