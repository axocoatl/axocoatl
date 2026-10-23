"""Make one workbench Session the way the app does: register the folder,
create a Session on the configured `lead` Agent, apply its Team with the
suggested budget, and send one request; with an empty REQUEST, only create
it. Usage: workbench_session.py BASE_URL TOKEN_FILE REPO NAME REQUEST"""
import json
import sys
import time
import urllib.error
import urllib.request
import uuid

base, token_file, repo, name, request_text = sys.argv[1:6]
token = open(token_file).read().strip()


def call(method, path, body=None, timeout=600):
    data = None if body is None else json.dumps(body).encode()
    request = urllib.request.Request(base + path, data=data, method=method, headers={
        "Authorization": "Bearer " + token, "Content-Type": "application/json"})
    try:
        with urllib.request.urlopen(request, timeout=timeout) as response:
            return response.status, json.loads(response.read() or b"null")
    except urllib.error.HTTPError as error:
        return error.code, error.read().decode()


status, workspace = call("POST", "/api/workspaces", {"path": repo})
assert status == 200, (status, workspace)
status, session = call("POST", f"/api/workspaces/{workspace['id']}/sessions", {
    "name": name, "mode": {"kind": "single_agent", "agent_id": "lead"},
    "setup_reviewed": True})
assert status == 200, (status, session)
session_id = session["id"]
print("session", session_id)
for _ in range(120):
    status, current = call("GET", f"/api/sessions/{session_id}")
    state = current.get("environment", {}).get("state") if isinstance(current, dict) else None
    if state in ("ready", "failed"):
        break
    time.sleep(1)
print("environment", state)
if not request_text:
    sys.exit(0)
status, view = call("GET", f"/api/sessions/{session_id}/team")
assert status == 200, (status, view)
# 1.4.0 suggests no budget: the fix loadout's Agent budget, as 1.5 suggests.
budget = view.get("suggested_budget") or {
    "agent": {"activations": 4, "invocations": 400, "tokens": 4000000, "cost_microunits": 0},
    "expires_in_ms": 24 * 60 * 60 * 1000}
now_ms = int(time.time() * 1000)
slots = []
for slot in view["slots"]:
    slot = dict(slot)
    limits = dict(budget["agent"])
    limits["cost_microunits"] = 0
    slot["limits"] = limits
    slot["expires_at_ms"] = now_ms + budget["expires_in_ms"]
    slots.append(slot)
edit = {
    "command_id": "team-" + uuid.uuid4().hex[:12],
    "expected_configuration_revision": view["configuration_revision"],
    "slots": slots,
    "dependencies": view["dependencies"],
    "layout": view["layout"],
}
status, preview = call("POST", f"/api/sessions/{session_id}/team/preview", edit)
assert status == 200, (status, preview)
print("preview", json.dumps(preview["changes"]))
status, applied = call("POST", f"/api/sessions/{session_id}/team/apply",
                       {"edit": edit, "review_digest": preview["review_digest"]})
assert status == 200, (status, applied)
status, answer = call("POST", f"/api/sessions/{session_id}/execute", {"input": request_text})
print("execute", status, json.dumps(answer)[:300])
assert status == 200, (status, answer)
