# A2A v1.0 Python SDK Interop Harness

Manual verification that the rsclaw gateway speaks A2A v1.0 to Google's
official Python SDK.

## Setup

```bash
pip install a2a-sdk
# Or, while v1.0 is on the bleeding edge:
pip install git+https://github.com/google-a2a/a2a-python.git
```

## Running the gateway

```bash
cargo run -- gateway start --port 18888
```

Optional auth — set BEFORE starting the gateway:

```bash
export RSCLAW_A2A_BEARER_TOKENS="dev-token-1,dev-token-2"
# or:
export RSCLAW_A2A_API_KEYS="k-1,k-2"
```

## Test 1: Agent Card discovery

```python
import asyncio
import httpx

async def main():
    async with httpx.AsyncClient() as c:
        r = await c.get("http://localhost:18888/.well-known/agent.json")
        card = r.json()
    assert card["protocolVersion"] == "1.0"
    assert card["capabilities"]["streaming"] is True
    assert card["capabilities"]["pushNotifications"] is True
    assert card["capabilities"]["extendedAgentCard"] is True
    assert "bearer" in card["securitySchemes"]
    assert "apiKey" in card["securitySchemes"]
    print("OK:", card["name"], "skills:", len(card["skills"]))

asyncio.run(main())
```

## Test 2: SendMessage (synchronous)

```python
import asyncio, httpx, uuid

async def main():
    body = {
        "jsonrpc": "2.0",
        "id": "t1",
        "method": "SendMessage",
        "params": {
            "message": {
                "messageId": str(uuid.uuid4()),
                "role": "ROLE_USER",
                "parts": [{"type": "text", "text": "hi"}],
            }
        },
    }
    async with httpx.AsyncClient(timeout=120) as c:
        r = await c.post("http://localhost:18888/api/v1/a2a", json=body)
        result = r.json()["result"]
    assert result["status"]["state"] == "TASK_STATE_COMPLETED"
    print("task_id:", result["id"], "→", result["artifacts"][0]["parts"][0]["text"])

asyncio.run(main())
```

## Test 3: SendStreamingMessage (SSE)

```python
import asyncio, httpx, uuid, json

async def main():
    body = {
        "jsonrpc": "2.0",
        "id": "s1",
        "method": "SendStreamingMessage",
        "params": {
            "message": {
                "messageId": str(uuid.uuid4()),
                "role": "ROLE_USER",
                "parts": [{"type": "text", "text": "count to 3"}],
            }
        },
    }
    headers = {"Accept": "text/event-stream"}
    async with httpx.AsyncClient(timeout=120) as c:
        async with c.stream("POST", "http://localhost:18888/api/v1/a2a",
                            json=body, headers=headers) as r:
            async for line in r.aiter_lines():
                if line.startswith("data:"):
                    frame = json.loads(line[5:].strip())
                    res = frame.get("result", {})
                    print(res.get("kind"), res.get("status", {}).get("state"))
                    if res.get("final"):
                        break

asyncio.run(main())
```

## Test 4: GetTask / ListTasks / CancelTask

```python
import asyncio, httpx

async def main():
    async with httpx.AsyncClient(timeout=30) as c:
        listed = (await c.post("http://localhost:18888/api/v1/a2a", json={
            "jsonrpc": "2.0", "id": "l1", "method": "ListTasks",
            "params": {"pageSize": 5}
        })).json()
        tasks = listed["result"]["tasks"]
        if not tasks:
            print("no tasks yet — send one first via Test 2")
            return
        tid = tasks[0]["id"]
        got = (await c.post("http://localhost:18888/api/v1/a2a", json={
            "jsonrpc": "2.0", "id": "g1", "method": "GetTask",
            "params": {"id": tid}
        })).json()
        print(got["result"]["status"])

asyncio.run(main())
```

## Test 5: Push notification

Start a local sink first:

```bash
# Terminal A — echo every webhook to stdout
python -m http.server 9000 &
```

```python
import asyncio, httpx, uuid

async def main():
    tid = str(uuid.uuid4())
    async with httpx.AsyncClient() as c:
        # 1) Register a push config
        await c.post("http://localhost:18888/api/v1/a2a", json={
            "jsonrpc": "2.0", "id": "p1",
            "method": "CreateTaskPushNotificationConfig",
            "params": {
                "taskId": tid,
                "pushNotificationConfig": {
                    "id": "cfg-1",
                    "url": "http://localhost:9000/hook",
                    "token": "dev-secret",
                }
            }
        })
        # 2) Send a streaming task with that taskId.
        body = {
            "jsonrpc": "2.0", "id": "p2",
            "method": "SendStreamingMessage",
            "params": {
                "message": {
                    "messageId": str(uuid.uuid4()),
                    "role": "ROLE_USER",
                    "parts": [{"type": "text", "text": "hello"}],
                    "taskId": tid,
                }
            }
        }
        # ...stream the response and watch the python http.server log
        # for POSTs from the gateway with X-A2A-Signature: <HMAC>.

asyncio.run(main())
```

## Test 6: INPUT_REQUIRED suspend / resume (`wait_input`)

Requires a working LLM with tool-use support (DeepSeek, Qwen, Claude,
GPT, etc.). Verifies the `wait_input` built-in tool and the same-`taskId`
resume short-path end-to-end.

```python
import asyncio, httpx, json, uuid

BASE = "http://localhost:18888/api/v1/a2a"

async def main():
    # 1) Stream a turn that forces the LLM to call wait_input.
    body = {
        "jsonrpc": "2.0", "id": "w1",
        "method": "SendStreamingMessage",
        "params": {"message": {
            "messageId": str(uuid.uuid4()),
            "role": "ROLE_USER",
            "parts": [{"type": "text",
                "text": "Use the wait_input tool to ask the user for their "
                        "favorite color. Echo their answer back as the final "
                        "reply, no other text."}],
        }},
    }
    headers = {"Accept": "text/event-stream"}
    task_id = None
    seen_input_required = False
    artifact_text = None

    async with httpx.AsyncClient(timeout=120) as c:
        async with c.stream("POST", BASE, json=body, headers=headers) as r:
            async for line in r.aiter_lines():
                if not line.startswith("data:"): continue
                frame = json.loads(line[5:].strip())
                res = frame.get("result", {})
                if res.get("kind") == "status-update":
                    state = res["status"]["state"]
                    print("status:", state)
                    task_id = res["taskId"]
                    if state == "TASK_STATE_INPUT_REQUIRED" and not seen_input_required:
                        seen_input_required = True
                        # 2) Resume via SendMessage on the SAME taskId.
                        resume = await c.post(BASE, json={
                            "jsonrpc": "2.0", "id": "r1",
                            "method": "SendMessage",
                            "params": {"message": {
                                "messageId": str(uuid.uuid4()),
                                "taskId": task_id,         # ← same id triggers resume
                                "role": "ROLE_USER",
                                "parts": [{"type": "text", "text": "chartreuse"}],
                            }},
                        })
                        print("resume RPC ok:", resume.status_code)
                elif res.get("kind") == "artifact-update":
                    artifact_text = res["artifact"]["parts"][0]["text"]
                if res.get("final"): break

    assert seen_input_required, "expected TASK_STATE_INPUT_REQUIRED frame"
    assert artifact_text and "chartreuse" in artifact_text, \
        f"expected resumed text in final artifact, got: {artifact_text!r}"
    print("OK — wait_input resumed and final artifact contained the answer")

asyncio.run(main())
```

Expected SSE timeline:

```
SUBMITTED
WORKING
WORKING  (message: "calling tool wait_input")
INPUT_REQUIRED  (message: "What is your favorite color?")
   ↳ client POSTs SendMessage with same taskId carrying "chartreuse"
artifact-update  (parts[0].text contains "chartreuse")
COMPLETED  (final: true)
```

## Expected outcomes

- All 6 tests run without protocol errors
- Agent Card shows `protocolVersion: "1.0"` and all 3 capabilities `true`
- SendMessage returns `TASK_STATE_COMPLETED` with an artifact
- SendStreamingMessage delivers at least one `status-update` event with `final: true`
- Push sink receives signed POSTs with `X-A2A-Signature` and `X-A2A-Task-Id` headers
- `wait_input` round-trip: INPUT_REQUIRED observed → resume RPC succeeds → final artifact contains the resumed text

## Recording

Record outcomes (date, SDK version, pass/fail per test, any wire mismatches)
in `docs/a2a-interop.md` when you run them.
