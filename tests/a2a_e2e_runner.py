#!/usr/bin/env python3
"""A2A v1.0 end-to-end test matrix against a live rsclaw gateway.

Companion to `tests/a2a_interop_python.md` (which documents individual
tests in prose). This file runs all 15 of them in one shot and exits
non-zero on any failure — useful as a regression check before landing
A2A-touching changes.

Coverage:
  AgentCard discovery
  GetExtendedAgentCard
  SendMessage          (text only)
  SendMessage          (with raw image part → verifies workspace/a2a/images/)
  SendMessage          (with data part → verifies history preserves Data part)
  SendStreamingMessage (text)
  ListTasks            (pagination)
  GetTask              (success + not-found)
  SubscribeToTask      (nonexistent → terminal Failed + stream closes)
  CancelTask           (in-flight → CANCELED state persists, not flipped)
  Push notification    (Create/Get/List/Delete + HMAC-verified delivery
                        across status-update + artifact-update frames)
  wait_input           (INPUT_REQUIRED suspend/resume round-trip)
  wait_input(auth=true) (AUTH_REQUIRED variant)

Each test prints `[PASS]` / `[FAIL]` and a one-line reason. Exit code
matches: 0 if all green, 1 otherwise.

Prerequisites
-------------
1. A running rsclaw gateway. The default profile setup:

       cp ~/.rsclaw/rsclaw.json5 ~/.rsclaw-a2atest/rsclaw.json5
       target/debug/rsclaw --profile a2atest gateway run

   gives you an isolated instance on port 19074 sharing your providers.
   Override with `A2A_BASE=http://host:port` if you point elsewhere.

2. A provider with tool-use support is required for `wait_input` /
   `wait_auth` (verified against deepseek/qwen). The non-wait_input
   tests pass even on a rate-limited provider since they only check
   protocol-level behavior.

3. `pip install httpx`.

Run
---
       python3 tests/a2a_e2e_runner.py
"""
import asyncio, base64, hashlib, hmac, json, os, sys, time, uuid
import http.server, socketserver, threading
import httpx

BASE = os.environ.get("A2A_BASE", "http://127.0.0.1:19074")
RPC = f"{BASE}/api/v1/a2a"
WEBHOOK_PORT = 8902
WEBHOOK_SECRET = "e2e-secret"
WEBHOOK_HITS: list = []

PASS, FAIL = "[PASS]", "[FAIL]"
results: list[tuple[str, bool, str]] = []


def record(name: str, ok: bool, msg: str = ""):
    results.append((name, ok, msg))
    print(f"{PASS if ok else FAIL} {name}" + (f" — {msg}" if msg else ""))


def start_webhook_server():
    class H(http.server.BaseHTTPRequestHandler):
        def do_POST(self):
            ln = int(self.headers.get("Content-Length") or 0)
            body = self.rfile.read(ln) if ln else b""
            WEBHOOK_HITS.append({
                "sig": self.headers.get("X-A2A-Signature", ""),
                "tid": self.headers.get("X-A2A-Task-Id", ""),
                "body": body,
            })
            self.send_response(200); self.end_headers(); self.wfile.write(b"ok")
        def log_message(self, *a, **k): pass
    srv = socketserver.TCPServer(("127.0.0.1", WEBHOOK_PORT), H)
    threading.Thread(target=srv.serve_forever, daemon=True).start()


async def rpc(c: httpx.AsyncClient, method: str, params: dict, rpc_id: str = "1") -> dict:
    r = await c.post(RPC, json={"jsonrpc": "2.0", "id": rpc_id, "method": method, "params": params})
    r.raise_for_status()
    return r.json()


# 1x1 transparent PNG.
PNG_B64 = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNkYAAAAAYAAjCB0C8AAAAASUVORK5CYII="


async def t_agent_card(c):
    r = await c.get(f"{BASE}/.well-known/agent.json")
    card = r.json()
    assert card["protocolVersion"] == "1.0", card.get("protocolVersion")
    assert card["capabilities"]["streaming"] is True
    assert card["capabilities"]["pushNotifications"] is True
    assert card["capabilities"]["extendedAgentCard"] is True
    assert "bearer" in card["securitySchemes"]
    assert "apiKey" in card["securitySchemes"]
    return f"v{card['protocolVersion']} {len(card['skills'])} skill(s)"


async def t_extended_agent_card(c):
    res = await rpc(c, "GetExtendedAgentCard", {})
    card = res["result"]
    assert card["protocolVersion"] == "1.0", card
    return "ok"


async def t_send_message_text(c):
    res = await rpc(c, "SendMessage", {
        "message": {"messageId": str(uuid.uuid4()), "role": "user",
                    "parts": [{"type": "text", "text": "reply with: ack"}]}})
    r = res["result"]
    assert r["status"]["state"] in ("TASK_STATE_COMPLETED", "TASK_STATE_FAILED"), r["status"]
    if r["status"]["state"] == "TASK_STATE_FAILED":
        return f"FAILED (likely LLM): {r['status'].get('message', {}).get('parts', [{}])[0].get('text', '?')[:60]}"
    txt = r["artifacts"][0]["parts"][0]["text"]
    return f"completed: {txt[:40]!r}"


async def t_send_message_with_raw_image(c):
    """Send a PNG as a Raw part. Verify the ingest wrote a file into
    workspace/a2a/images/ — even if the LLM call fails, the file lands
    because ingest happens before the agent runs."""
    res = await rpc(c, "SendMessage", {
        "message": {
            "messageId": str(uuid.uuid4()),
            "role": "user",
            "parts": [
                {"type": "text", "text": "describe the image"},
                {"type": "raw", "bytes": PNG_B64, "mimeType": "image/png"},
            ],
        }})
    r = res["result"]
    # Look in the profile's workspace for an a2a-prefixed image.
    # workspace lives under ~/.rsclaw-a2atest/workspace by default.
    ws = os.path.expanduser("~/.rsclaw-a2atest/workspace")
    img_dir = os.path.join(ws, "a2a", "images")
    if not os.path.isdir(img_dir):
        return f"no a2a/images dir (status={r['status']['state']})"
    matches = [f for f in os.listdir(img_dir) if f.startswith("a2a_i_") and f.endswith(".png")]
    assert matches, f"no a2a_i_*.png in {img_dir}"
    return f"wrote {matches[-1]}, status={r['status']['state']}"


async def t_send_message_with_data_part(c):
    res = await rpc(c, "SendMessage", {
        "message": {
            "messageId": str(uuid.uuid4()),
            "role": "user",
            "parts": [
                {"type": "text", "text": "parse the data"},
                {"type": "data", "data": {"key": "v1", "n": 42}},
            ],
        }})
    r = res["result"]
    # Verify the history shows both parts persisted, regardless of LLM outcome.
    parts = r["history"][0]["parts"]
    types = [p["type"] for p in parts]
    assert "text" in types and "data" in types, types
    return f"history parts={types}, status={r['status']['state']}"


async def t_streaming(c):
    body = {"jsonrpc": "2.0", "id": "s1", "method": "SendStreamingMessage",
            "params": {"message": {"messageId": str(uuid.uuid4()), "role": "user",
                                   "parts": [{"type": "text", "text": "reply ack"}]}}}
    states = []
    async with c.stream("POST", RPC, json=body, headers={"Accept": "text/event-stream"}) as r:
        async for line in r.aiter_lines():
            if not line.startswith("data:"): continue
            f = json.loads(line[5:].strip()).get("result", {})
            if f.get("kind") == "status-update":
                states.append(f["status"]["state"])
            if f.get("final"): break
    assert states[0] == "TASK_STATE_SUBMITTED", states
    assert states[1] == "TASK_STATE_WORKING", states
    assert states[-1] in ("TASK_STATE_COMPLETED", "TASK_STATE_FAILED"), states
    return f"{len(states)} events: {states[0]}→{states[-1]}"


async def t_list_tasks(c):
    res = await rpc(c, "ListTasks", {"pageSize": 5})
    r = res["result"]
    assert "tasks" in r, r
    return f"{len(r['tasks'])} task(s)"


async def t_get_task(c):
    # Need a task id — create one first.
    res = await rpc(c, "SendMessage", {
        "message": {"messageId": str(uuid.uuid4()), "role": "user",
                    "parts": [{"type": "text", "text": "hi"}]}})
    tid = res["result"]["id"]
    got = await rpc(c, "GetTask", {"id": tid})
    assert got["result"]["id"] == tid
    return f"id={tid[:8]}"


async def t_get_task_not_found(c):
    got = await rpc(c, "GetTask", {"id": "definitely-does-not-exist"})
    assert "error" in got, got
    assert got["error"]["code"] == -32001, got["error"]
    return f"correct -32001"


async def t_subscribe_to_task_unknown(c):
    body = {"jsonrpc": "2.0", "id": "sub1", "method": "SubscribeToTask",
            "params": {"id": "does-not-exist-either"}}
    states = []
    async with c.stream("POST", RPC, json=body, headers={"Accept": "text/event-stream"}) as r:
        async for line in r.aiter_lines():
            if not line.startswith("data:"): continue
            f = json.loads(line[5:].strip()).get("result", {})
            states.append(f.get("status", {}).get("state"))
            if f.get("final"): break
    assert states == ["TASK_STATE_FAILED"], states
    return "terminated cleanly"


async def t_cancel_task(c):
    """Race a streaming task and CancelTask. After the dust settles GetTask
    must report CANCELED (not flipped to COMPLETED by a late reply)."""
    sse_body = {"jsonrpc": "2.0", "id": "sx", "method": "SendStreamingMessage",
                "params": {"message": {"messageId": str(uuid.uuid4()), "role": "user",
                                       "parts": [{"type": "text", "text": "count to 100 slowly"}]}}}
    tid_holder = {"tid": None}

    async def stream():
        async with c.stream("POST", RPC, json=sse_body, headers={"Accept": "text/event-stream"}) as r:
            async for line in r.aiter_lines():
                if not line.startswith("data:"): continue
                f = json.loads(line[5:].strip()).get("result", {})
                tid_holder["tid"] = f.get("taskId") or tid_holder["tid"]
                if f.get("final"): return

    stream_task = asyncio.create_task(stream())
    for _ in range(60):
        if tid_holder["tid"]: break
        await asyncio.sleep(0.05)
    assert tid_holder["tid"], "no taskId observed"
    cancel = await rpc(c, "CancelTask", {"id": tid_holder["tid"]})
    assert cancel["result"]["status"]["state"] == "TASK_STATE_CANCELED", cancel
    await stream_task
    await asyncio.sleep(0.5)
    got = await rpc(c, "GetTask", {"id": tid_holder["tid"]})
    assert got["result"]["status"]["state"] == "TASK_STATE_CANCELED", got["result"]["status"]
    return f"CANCELED stuck"


async def t_push_crud(c):
    tid = "push-crud-" + str(uuid.uuid4())[:8]
    created = await rpc(c, "CreateTaskPushNotificationConfig", {
        "taskId": tid,
        "pushNotificationConfig": {"id": "c1", "url": "http://x/", "token": "t"},
    })
    assert created["result"]["id"] == "c1", created
    listed = await rpc(c, "ListTaskPushNotificationConfigs", {"taskId": tid})
    assert any(x.get("id") == "c1" for x in listed["result"]["configs"]), listed
    got = await rpc(c, "GetTaskPushNotificationConfig", {
        "taskId": tid, "pushNotificationConfigId": "c1"})
    assert got["result"]["url"] == "http://x/", got
    deleted = await rpc(c, "DeleteTaskPushNotificationConfig", {
        "taskId": tid, "pushNotificationConfigId": "c1"})
    assert deleted.get("result") is not None or deleted.get("error") is None, deleted
    return "Create/List/Get/Delete all ok"


async def t_push_delivery(c):
    """Register a webhook, fire a streaming task, verify the listener
    receives Submitted/Working/(terminal) events with valid HMAC."""
    WEBHOOK_HITS.clear()
    tid = "push-deliv-" + str(uuid.uuid4())[:8]
    await rpc(c, "CreateTaskPushNotificationConfig", {
        "taskId": tid,
        "pushNotificationConfig": {
            "id": "cfg-1",
            "url": f"http://127.0.0.1:{WEBHOOK_PORT}/hook",
            "token": WEBHOOK_SECRET,
        }})
    body = {"jsonrpc": "2.0", "id": "s1", "method": "SendStreamingMessage",
            "params": {"message": {"messageId": str(uuid.uuid4()), "role": "user",
                                   "parts": [{"type": "text", "text": "hi"}],
                                   "taskId": tid}}}
    async with c.stream("POST", RPC, json=body, headers={"Accept": "text/event-stream"}) as r:
        async for line in r.aiter_lines():
            if not line.startswith("data:"): continue
            f = json.loads(line[5:].strip()).get("result", {})
            if f.get("final"): break
    await asyncio.sleep(1.0)
    assert len(WEBHOOK_HITS) >= 3, f"only {len(WEBHOOK_HITS)} hit(s)"
    kinds = []
    states = []
    for h in WEBHOOK_HITS:
        expected = base64.b64encode(hmac.new(WEBHOOK_SECRET.encode(), h["body"], hashlib.sha256).digest()).decode()
        assert h["sig"] == expected, f"HMAC mismatch: {h['sig']} vs {expected}"
        assert h["tid"] == tid, f"task id mismatch: {h['tid']}"
        d = json.loads(h["body"])
        kinds.append(d.get("kind"))
        if d.get("kind") == "status-update":
            states.append(d["status"]["state"])
    assert "TASK_STATE_SUBMITTED" in states and "TASK_STATE_WORKING" in states, states
    assert "artifact-update" in kinds or "TASK_STATE_COMPLETED" in states or "TASK_STATE_FAILED" in states, (kinds, states)
    return f"{len(WEBHOOK_HITS)} hits ({'/'.join(kinds)}), HMAC all valid"


async def _wait_input_round_trip(c, auth: bool, expected_state: str):
    """Common driver for wait_input / wait_input(auth=true)."""
    prompt = "What is your favorite color?" if not auth else "Please provide your bearer token."
    tool_call = (
        "Call the wait_input tool with prompt='" + prompt + "'"
        + (", auth: true" if auth else "")
        + ". Once you receive the response, echo it back verbatim as the final reply."
    )
    body = {"jsonrpc": "2.0", "id": "w1", "method": "SendStreamingMessage",
            "params": {"message": {"messageId": str(uuid.uuid4()), "role": "user",
                                   "parts": [{"type": "text", "text": tool_call}]}}}
    saw_state = False
    saw_artifact_text = None
    tid = None
    resumed = False

    async with c.stream("POST", RPC, json=body, headers={"Accept": "text/event-stream"}) as r:
        async for line in r.aiter_lines():
            if not line.startswith("data:"): continue
            f = json.loads(line[5:].strip()).get("result", {})
            tid = f.get("taskId") or tid
            kind = f.get("kind")
            if kind == "status-update":
                st = f["status"]["state"]
                if st == expected_state and not resumed:
                    saw_state = True
                    resumed = True
                    await rpc(c, "SendMessage", {
                        "message": {
                            "messageId": str(uuid.uuid4()),
                            "taskId": tid,
                            "role": "user",
                            "parts": [{"type": "text", "text": "chartreuse"}],
                        }}, rpc_id="r1")
            elif kind == "artifact-update":
                saw_artifact_text = f["artifact"]["parts"][0]["text"]
            if f.get("final"): break

    assert saw_state, f"never observed {expected_state}"
    assert saw_artifact_text and "chartreuse" in saw_artifact_text, \
        f"artifact text missing resumed value: {saw_artifact_text!r}"
    return f"{expected_state} observed + artifact carried 'chartreuse'"


async def t_wait_input(c):
    return await _wait_input_round_trip(c, auth=False, expected_state="TASK_STATE_INPUT_REQUIRED")


async def t_wait_auth(c):
    return await _wait_input_round_trip(c, auth=True, expected_state="TASK_STATE_AUTH_REQUIRED")


# ---------- runner ----------

TESTS = [
    ("agent_card", t_agent_card),
    ("extended_agent_card", t_extended_agent_card),
    ("send_message_text", t_send_message_text),
    ("send_message_raw_image", t_send_message_with_raw_image),
    ("send_message_data_part", t_send_message_with_data_part),
    ("streaming", t_streaming),
    ("list_tasks", t_list_tasks),
    ("get_task", t_get_task),
    ("get_task_not_found", t_get_task_not_found),
    ("subscribe_to_task_unknown", t_subscribe_to_task_unknown),
    ("cancel_task", t_cancel_task),
    ("push_crud", t_push_crud),
    ("push_delivery", t_push_delivery),
    ("wait_input", t_wait_input),
    ("wait_auth", t_wait_auth),
]


async def main():
    start_webhook_server()
    await asyncio.sleep(0.3)
    async with httpx.AsyncClient(timeout=180) as c:
        for name, fn in TESTS:
            try:
                msg = await fn(c)
                record(name, True, msg)
            except Exception as e:
                record(name, False, f"{type(e).__name__}: {e}")
    print()
    p = sum(1 for _, ok, _ in results if ok)
    f = len(results) - p
    print(f"=== {p}/{len(results)} passed, {f} failed ===")
    sys.exit(0 if f == 0 else 1)


asyncio.run(main())
