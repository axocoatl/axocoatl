"""A loopback stand-in for Ollama that Axocoatl 1.4.0 (Ollama 0.20.6) and
1.5 (0.40.6, with its contract probes) both accept, for upgrade/rollback
experiments. Its version is read from VERSION_FILE on every request so a
test can "upgrade" Ollama under a running daemon.

Models: writer-model:latest delegates DELEGATED_TASK to the helper scout
when it is offered `delegate`, and otherwise calls bash once with the
command in WRITER_CMD_FILE (read per turn); then it answers.
helper-model:latest answers the delegated task. reviewer-model:latest
answers VERDICT: APPROVE. Every request is logged to LOG_FILE.
"""
import json
import os
import sys
import uuid
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

PORT = int(sys.argv[1])
STATE = sys.argv[2]
VERSION_FILE = os.path.join(STATE, "version")
WRITER_CMD_FILE = os.path.join(STATE, "writer-cmd")
LOG_FILE = os.path.join(STATE, "requests.log")
DIGESTS = {
    "writer-model:latest": "a80c4f17acd55265feec403c7aef86be0c25983ab279d83f3bcd3abbcb5b8b72",
    "reviewer-model:latest": "b80c4f17acd55265feec403c7aef86be0c25983ab279d83f3bcd3abbcb5b8b73",
    "helper-model:latest": "c80c4f17acd55265feec403c7aef86be0c25983ab279d83f3bcd3abbcb5b8b74",
}
DELEGATED_TASK = "Read README.md and say in one sentence what this repository is for."
# The prompts of 1.5's contract probes other than the over-long one.
PROBES = {
    "Reply with the single word: ready": "stop",
    "Write the numbers from 1 to 300, one per line. Write only the numbers.": "long",
    "Call the report_ready tool with word set to ready.": "tool",
}
loaded = {"model": "writer-model:latest"}


def version():
    with open(VERSION_FILE) as handle:
        return handle.read().strip()


def full(name):
    return name if ":" in name else name + ":latest"


def log(line):
    with open(LOG_FILE, "a") as handle:
        handle.write(line + "\n")


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *args):
        pass

    def send_json(self, status, value):
        body = json.dumps(value).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def send_ndjson(self, records):
        body = "".join(json.dumps(r) + "\n" for r in records).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/x-ndjson")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_GET(self):
        log(f"GET {self.path} v={version()}")
        if self.path == "/api/version":
            return self.send_json(200, {"version": version()})
        if self.path == "/api/status":
            return self.send_json(200, {"cloud": {"disabled": True}})
        if self.path == "/api/tags":
            return self.send_json(200, {"models": [
                {"name": name, "model": name, "digest": digest,
                 "details": {"format": "gguf"}}
                for name, digest in DIGESTS.items()]})
        if self.path == "/api/ps":
            name = loaded["model"]
            return self.send_json(200, {"models": [
                {"name": name, "model": name, "digest": DIGESTS[name],
                 "details": {"format": "gguf"}, "context_length": 32768,
                 "runner": "llamacpp"}]})
        self.send_json(404, {"error": "not found"})

    def do_POST(self):
        length = int(self.headers.get("Content-Length") or 0)
        raw = self.rfile.read(length)
        body = json.loads(raw or b"{}")
        model = full(body.get("model", ""))
        log(f"POST {self.path} model={model} v={version()}")
        if self.path == "/api/show":
            return self.send_json(200, {"details": {"format": "gguf"},
                                        "capabilities": ["completion", "tools"]})
        if self.path == "/api/generate":
            loaded["model"] = model
            return self.send_json(200, {"model": model, "created_at": "2026-10-23T00:00:00Z",
                                        "response": "", "done": True, "done_reason": "load"})
        if self.path != "/api/chat":
            return self.send_json(404, {"error": "not found"})
        messages = body.get("messages", [])
        options = body.get("options", {})
        first = messages[0].get("content", "") if messages else ""
        # 1.5's contract probes, answered as Ollama 0.40 does
        # (axocoatl_llm_ollama::ContractProbe).
        if body.get("truncate") is False and first.startswith("x x "):
            return self.send_json(400, {"error": "request exceeds the available context size, try increasing it"})
        probe = PROBES.get(first) if body.get("truncate") is False else None
        if probe is not None:
            def record(message, **fields):
                return {"model": body["model"], "created_at": "2026-10-23T00:00:00Z",
                        "message": {"role": "assistant", **message}, **fields}
            if probe == "stop":
                return self.send_ndjson([record({"content": "ready"}, done=True, done_reason="length",
                                                prompt_eval_count=14, eval_count=1)])
            if probe == "long":
                return self.send_ndjson([
                    record({"content": "1\n2\n3\n"}, done=False),
                    record({"content": ""}, done=True, done_reason="length",
                           prompt_eval_count=33, eval_count=options.get("num_predict"))])
            return self.send_ndjson([record(
                {"content": "", "tool_calls": [{"id": "call_probe", "function": {
                    "index": 0, "name": "report_ready", "arguments": {"word": "ready"}}}]},
                done=True, done_reason="stop", prompt_eval_count=138, eval_count=31)])
        last_user = max((i for i, m in enumerate(messages) if m.get("role") == "user"), default=-1)
        tool_after = any(m.get("role") == "tool" for m in messages[last_user + 1:])
        created = "2026-10-23T00:00:00Z"
        offered = [t.get("function", {}).get("name") for t in body.get("tools", [])]
        if model.startswith("reviewer-model"):
            message = {"role": "assistant", "content": "The change does what was asked.\n\nVERDICT: APPROVE"}
        elif model.startswith("helper-model"):
            message = {"role": "assistant", "content": "It is a tiny repository for the Axocoatl 1.4.0 data-root fixture."}
        elif not tool_after and "delegate" in offered:
            message = {"role": "assistant", "content": "", "tool_calls": [
                {"id": "call_" + uuid.uuid4().hex[:12],
                 "function": {"index": 0, "name": "delegate",
                              "arguments": {"helper": "scout", "task": DELEGATED_TASK}}}]}
        elif not tool_after and "bash" in offered:
            with open(WRITER_CMD_FILE) as handle:
                command = handle.read().strip()
            message = {"role": "assistant", "content": "", "tool_calls": [
                {"id": "call_" + uuid.uuid4().hex[:12],
                 "function": {"index": 0, "name": "bash", "arguments": {"command": command}}}]}
        else:
            message = {"role": "assistant", "content": "Done: the change is made."}
        log(f"  -> {json.dumps(message)[:300]}")
        self.send_ndjson([
            {"model": body["model"], "created_at": created, "message": message, "done": False},
            {"model": body["model"], "created_at": created,
             "message": {"role": "assistant", "content": ""}, "done": True,
             "done_reason": "stop", "prompt_eval_count": 200, "eval_count": 20,
             "total_duration": 1000, "eval_duration": 500}])


ThreadingHTTPServer(("127.0.0.1", PORT), Handler).serve_forever()
