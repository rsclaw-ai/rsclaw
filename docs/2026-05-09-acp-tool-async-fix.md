# ACP Tool Async Fix - 2026-05-09

## Problem

The `tool_claudecode` and `tool_opencode` functions blocked the main agent loop when executed. The agent would freeze until the ACP client initialization completed (which could take several seconds).

## Root Cause

Both functions called `get_xxx_client().await` **before** `tokio::spawn`:

```rust
// WRONG: blocks main loop
let client = match self.get_opencode_client().await {  // <-- BLOCKS HERE
    Ok(c) => c,
    Err(e) => { ... return Err(e); }
};
// ... more blocking code ...
tokio::spawn(async move {  // <-- spawn happens AFTER blocking
    // ...
});
```

The client initialization involves:
1. Spawning subprocess (claude-agent-acp / opencode)
2. Sending `initialize` JSON-RPC request
3. Waiting for response
4. Sending `session/new` JSON-RPC request
5. Waiting for response

This can take 2-10+ seconds, during which the main agent loop is blocked and cannot process other messages.

## Solution

Move all client initialization **inside** the `tokio::spawn` block. Send initial notification and return immediately.

### Key Changes

1. **Send notification immediately** - User sees "submitted" status right away
2. **Spawn background task immediately** - No blocking before spawn
3. **Inline client creation logic** - Cannot call `self.get_xxx_client()` inside spawn because `&self` doesn't outlive `'static`
4. **Clone only Arc fields needed** - `opencode_client_cell`, `claudecode_client_cell`, `handle`, `config`
5. **Return immediately** - With `"status": "submitted"` response

### Correct Pattern

```rust
pub(crate) async fn tool_claudecode(&self, ctx: &RunContext, args: Value) -> Result<Value> {
    // 1. Extract arguments
    let task = args["task"].as_str()...;

    // 2. Send initial notification IMMEDIATELY
    if let Some(ref tx) = notif_tx {
        let _ = tx.send(...);  // "Claude Code task submitted"
    }

    // 3. Clone Arc references (not self!)
    let claudecode_client_cell = self.claudecode_client.clone();
    let handle = self.handle.clone();
    let config = self.config.clone();

    // 4. Spawn background task IMMEDIATELY
    tokio::spawn(async move {
        // 5. Get or create client INSIDE spawn
        let client = if let Some(c) = claudecode_client_cell.get() {
            c.clone()
        } else {
            // Create client inline (subprocess spawn, initialize, session/new)
            ...
        };

        // 6. Add notification sink, subscribe events
        ...

        // 7. Send prompt and handle result
        let send_result = client.send_prompt(&task_str).await;
        ...

        // 8. Send completion notification
        ...
    });

    // 9. Return immediately
    Ok(serde_json::json!({
        "output": "...",
        "status": "submitted"
    }))
}
```

## Additional Fixes

### ChannelNotifier Priority

Changed from `NotificationPriority::High` to `NotificationPriority::Medium` so users can see tool execution progress notifications.

```rust
// Before: only High priority forwarded
fn priority_filter(&self) -> NotificationPriority {
    NotificationPriority::High
}

// After: Medium+ priority forwarded
fn priority_filter(&self) -> NotificationPriority {
    NotificationPriority::Medium
}
```

### session/update Logging

Added detailed logging to show the `sessionUpdate` type and `sessionId` for debugging:

```rust
let session_update_type = update.get("sessionUpdate").and_then(|s| s.as_str());
tracing::info!(
    "ACP session/update: type={} session_id={}",
    session_update_type.unwrap_or("?"),
    session_id.as_deref().unwrap_or("?")
);
```

## Files Modified

- `src/agent/tools_acp.rs` - Main fix for both tool functions
- `src/acp/client.rs` - Added session/update logging

## Why Not Clone AgentRuntime?

`AgentRuntime` contains non-Clone fields like `FailoverManager`. Also, `&self` is a reference that only lives within the method body, but `tokio::spawn` requires `'static` lifetime.

Solution: Clone only the `Arc` fields that are needed for client creation:
- `opencode_client_cell: Arc<OnceCell<AcpClient>>` - cached client
- `claudecode_client_cell: Arc<OnceCell<AcpClient>>` - cached client
- `handle: Arc<AgentHandle>` - contains config.workspace
- `config: Arc<RuntimeConfig>` - contains agents.defaults.workspace

## JSON-RPC Note

The log shows `session/update id=None`. This is **correct behavior** per JSON-RPC 2.0 spec:

- **Requests** have `id` - expect response
- **Notifications** have NO `id` - no response expected

`session/update` is a notification from the agent to the client, so it has no `id` field. The session ID is in `params.sessionId`, not top-level `id`.

## Verification

After fix, logs show:
- `16:49:56.464` - Background task started
- `16:49:57.885` - Main loop finished (~1.4 seconds)
- `16:49:58.503` - Task marked complete
- `16:49:59.377` - Claude Code session created (in background, ~3 seconds later)

Main agent loop completes in <2 seconds instead of being blocked for the full initialization time.