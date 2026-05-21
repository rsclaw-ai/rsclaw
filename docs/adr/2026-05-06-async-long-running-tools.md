# Async long-running tools (computer_use ui_tars, web_browser, etc.)

> Status: **proposed** (not yet implemented)
> Branch the work will land on: TBD
> Discovered: 2026-05-06 while debugging `heartbeat timed out after 300s`

## Problem

Each `AgentRuntime` is a `tokio::spawn`-ed task with a single mpsc
inbox:

```rust
// src/gateway/startup.rs::spawn_agent_tasks
tokio::spawn(async move {
    while let Some(msg) = rx.recv().await {
        runtime.run_turn(...).await;   // <-- blocks the loop
    }
});
```

`run_turn` is async but the **outer loop awaits it sequentially**.
While `run_turn` is running, the agent does NOT recv the next message.
This is intentional (per-agent serial execution preserves session
state, history, KV-cache prefix integrity). The cost: anything sent
to that agent's queue while it's busy waits.

That cost surfaces today as:

1. **Heartbeat timeout (300 s)**
   `src/heartbeat/mod.rs::run_heartbeat` sends a heartbeat message
   into the same agent inbox and awaits a reply. If `run_turn` is
   currently driving a `computer_use ui_tars` loop (30 turns × ~10 s
   each = ~5 min), the heartbeat enqueues, never gets recv'd in
   time, and the heartbeat caller `bail!`s with `heartbeat timed out
   after 300s`.

2. **Cross-channel latency**
   The same agent serves Feishu, WeChat, Telegram, etc. A long
   `computer_use` from one channel makes every other channel's
   reply wait its turn.

3. **No progress signal to the UI**
   The Tauri UI has no way to surface "ui_tars is on step 7 / 30"
   because nothing emits intermediate events.

## Mitigation already in place (2026-05-06)

`VlmDriver` now bails after 3 consecutive turns with no parseable
`Action:` line (`MAX_CONSECUTIVE_UNPARSEABLE = 3` in `driver.rs`).
This caps the worst case (model-spinning-its-wheels) to ~30 s instead
of ~5 min, so the heartbeat path no longer triggers in the common
"VLM produces meta-prose" failure mode. **It does not solve a
correctly-running 30-turn ui_tars task — that still blocks the
agent for minutes.**

## Proposed fix (Plan B)

Treat `computer_use ui_tars` (and structurally similar long tools
like `web_browser` deep flows) as **detached background tasks**:

```
agent.run_turn()
   ├─ tool_dispatch("computer_use ui_tars")
   ├─    spawn an async task with the VlmDriver
   ├─    register it in `pending_long_running_tools` (HashMap<task_id, _>)
   ├─    immediately return a tool_result like:
   │       { "task_id": "ui_tars-7af3...", "status": "running",
   │         "estimated_seconds": 60, "subscribe_event": "ui_tars.progress" }
   └─ run_turn ends; agent recv next msg right away
                            │
                            ▼
                  (heartbeat / other channels not blocked)
```

Then the long task runs detached:

```
spawned task
   ├─ for each driver step:
   │    ├─ screenshot
   │    ├─ predict
   │    ├─ execute
   │    └─ event_bus.send(UiTarsProgress { task_id, step, action,
   │                                        thumbnail_url, .. })
   └─ on terminal action (finished / call_user / max_loop / error):
        ├─ event_bus.send(UiTarsFinished { task_id, outcome, summary })
        ├─ remove from pending_long_running_tools
        └─ if the agent had set up a "feed result back" continuation,
          re-enqueue an internal AgentMessage with the outcome attached
```

## Design decisions to nail down before coding

- **Continuation model.** Does the upstream agent loop (e.g. Kimi)
  block waiting for the ui_tars outcome, or does it return immediately
  and treat the result as a follow-up tool call? Both are viable;
  the first gives the agent a synchronous-feeling tool, the second
  is more concurrent but harder to reason about. The simplest:
  surface `task_id` to the agent and arm an internal `wait_for_task`
  pseudo-tool the agent can call on the next turn.

- **Task ownership / cancellation.** Where lives the
  `pending_long_running_tools` map? `AgentRuntime` (per-agent) or
  `AppState` (gateway-wide)? Probably `AgentRuntime`, so cancel-by-
  user-abort flows naturally.

- **Reconnect behaviour.** If the WS UI drops mid-task, can it
  reconnect and re-subscribe to progress events for `task_id`? Mirror
  the `restart.required` latch pattern (`AppState::pending_restart`).

- **Heartbeat path.** Heartbeat should still go through the agent
  queue (so the agent's introspection logic can fire) — but with the
  long task detached, queue depth stays shallow and the 300 s timeout
  becomes a non-issue. No need to invent a separate heartbeat
  channel.

- **Existing task_queue?** `src/agent/task_queue.rs` already exists
  for some kinds of background work. Investigate whether to extend it
  or build a parallel queue dedicated to in-tool-call long ops. A
  `--task-queue computer_use` reservation may be enough.

## Acceptance criteria

1. A `computer_use ui_tars` call with `max_steps=30` does NOT block
   subsequent messages on the same agent. After the call dispatches
   the agent loop returns within ~50 ms.
2. Heartbeat fires on schedule even mid-`ui_tars`.
3. Tauri UI receives at least one progress event per driver step.
4. Cancel-by-user (existing abort flag) interrupts the detached
   task within ~250 ms.
5. The new test `ui_tars_does_not_block_heartbeat` passes — sends a
   ui_tars task and a heartbeat back-to-back, asserts both finish
   in <30 s combined.

## Out of scope for this ADR

- Per-agent multi-worker concurrency (still serial within an agent).
- WS protocol redesign — reuse existing event-frame infra.
- Retry / failover for long tasks (handle that in a follow-up).

## File touch list (estimate)

- `src/agent/runtime.rs` — surface `task_id` from
  `tool_computer_use("ui_tars", ...)`, add
  `pending_long_running_tools: HashMap<String, JoinHandle<()>>`.
- `src/agent/tools_computer.rs` — refactor `tool_ui_tars` to
  `spawn_ui_tars_task` returning `task_id` + emit progress.
- `src/events.rs` — new event types: `UiTarsProgress`,
  `UiTarsFinished`.
- `src/server/mod.rs` — broadcast wiring for the new events.
- `src/ws/handshake.rs` — relay ui_tars events to clients (mirror the
  `permission_request` relay added 2026-05-05).
- `ui/app/lib/rsclaw-ws.ts` + `ui/app/hooks/useUiTarsTask.ts` —
  client-side subscription.
- `tests/computer_e2e.rs` — heartbeat-non-blocking integration test.

## Estimated effort

1–2 hours focused work for the backend wire-through + small unit
test, ~1 hour for the UI subscription, ~30 min for docs / CHANGELOG.
Total: half a day.
