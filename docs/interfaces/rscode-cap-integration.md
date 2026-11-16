# RsCode CAP Protocol Integration Specification

> **Status:** Draft  
> **Date:** 2026-07-24  
> **Owner:** rsclaw team  
> **Implementer:** rscode team  
> **Related:** [CAP Protocol v1.0](https://cap-protocol.org), [cap-rs crate](https://docs.rs/cap-rs/0.1.2)

---

## 1. Overview

This document specifies how **rscode** should implement the **CAP (CLI Agent Protocol)** server side, enabling **rsclaw** to drive rscode as a coding agent backend through its existing `tool_cap` infrastructure.

### 1.1 Goal

Allow rsclaw users to invoke rscode from any messaging channel (WeChat, Telegram, Discord, etc.) via:

```
tool_cap(agent="rscode", task="fix the failing test", cwd="/path/to/project")
```

rsclaw's `CapAgentManager` spawns rscode as a subprocess, communicates via stdin/stdout JSON-RPC (CAP protocol), and receives streaming events back.

### 1.2 Current State

- **rsclaw** already supports 5 coding agents: `claudecode`, `openclaude`, `opencode`, `codex`, `qoder`
- **rscode** already has a Claude Code-compatible stream-json mode (`rscode run --input-stream-json --output-stream-json`)
- **cap-rs** (v0.1.2) is the reference implementation of CAP protocol, used by rsclaw

### 1.3 Why CAP Protocol (Not stream-json)

rscode's existing stream-json mode is **Claude Code wire-compatible** but uses Claude's event schema. CAP protocol is a **vendor-neutral standard** with:

- Explicit session lifecycle (`Ready` → `Prompt` → `Done`)
- Structured permission negotiation (`PermissionRequest` / `PermissionResponse`)
- Reverse RPC for interactive features (`AskUser`, `ShowNotification`)
- Clear separation of concerns (client frames vs agent events)

Implementing CAP makes rscode a first-class citizen in the cap-protocol ecosystem, compatible with any CAP-compliant orchestrator (not just rsclaw).

---

## 2. Architecture

```
┌─────────────────────────────────────────────────────────────┐
│  rsclaw (Orchestrator)                                      │
│                                                             │
│  ┌──────────────────────────────────────────────────────┐  │
│  │  CapAgentManager                                     │  │
│  │  ├─ AgentKind::Rscode                               │  │
│  │  ├─ spawn_driver(Rscode) → cap_rs::driver::Driver   │  │
│  │  └─ actor_loop: send(ClientFrame) / recv(AgentEvent) │  │
│  └──────────────────────────────────────────────────────┘  │
│                           │ stdin/stdout (JSON-RPC)        │
└───────────────────────────┼─────────────────────────────────┘
                            │
                            ▼
┌─────────────────────────────────────────────────────────────┐
│  rscode (CAP Server)                                        │
│                                                             │
│  ┌──────────────────────────────────────────────────────┐  │
│  │  rscode cap (new subcommand)                         │  │
│  │  ├─ Read ClientFrame from stdin (line-delimited JSON)│  │
│  │  ├─ Write AgentEvent to stdout (line-delimited JSON) │  │
│  │  ├─ Handle Prompt → run engine turn                  │  │
│  │  ├─ Emit TextChunk / ToolCallStart / ToolCallEnd     │  │
│  │  ├─ Handle PermissionRequest → wait for response     │  │
│  │  └─ Emit Done when turn completes                    │  │
│  └──────────────────────────────────────────────────────┘  │
│                           │                                │
│                           ▼                                │
│  ┌──────────────────────────────────────────────────────┐  │
│  │  Engine (existing)                                   │  │
│  │  ├─ run_turn(prompt) → stream of Events              │  │
│  │  ├─ permission_prompt callback                       │  │
│  │  └─ event_sink channel                               │  │
│  └──────────────────────────────────────────────────────┘  │
└─────────────────────────────────────────────────────────────┘
```

---

## 3. CAP Protocol Specification

### 3.1 Wire Format

**Transport:** stdin/stdout, line-delimited JSON (one JSON object per line, UTF-8 encoded).

**Direction:**
- **Client → Agent (stdin):** `ClientFrame` objects
- **Agent → Client (stdout):** `AgentEvent` objects

**Encoding:** All strings are UTF-8. Binary data (e.g., `Content::Image` bytes) is base64-encoded.

**Version:** CAP protocol version `1.0` (constant `CAP_PROTOCOL_VERSION`).

---

### 3.2 Core Types

#### 3.2.1 `ClientFrame` (Client → Agent)

Enum with the following variants:

```rust
pub enum ClientFrame {
    /// Start a new prompt. The agent processes the content and emits
    /// streaming events until Done.
    Prompt {
        content: Vec<Content>,
    },
    
    /// Respond to a PermissionRequest.
    PermissionResponse {
        req_id: String,
        decision: PermissionDecision,
    },
    
    /// Respond to an AskUser request.
    AskUserAnswer {
        ask_id: String,
        value: serde_json::Value,
    },
    
    /// Cancel the current operation.
    Cancel {
        scope: CancelScope,
    },
    
    /// Shutdown the agent process gracefully.
    Shutdown,
}
```

**JSON wire format (examples):**

```json
// Prompt
{"kind": "cap.prompt", "content": [{"type": "text", "text": "fix the test"}]}

// PermissionResponse
{"kind": "cap.permission.response", "req_id": "perm-123", "decision": "allow_once"}

// AskUserAnswer
{"kind": "cap.ask_user.answer", "ask_id": "ask-456", "value": "yes"}

// Cancel
{"kind": "cap.cancel", "scope": "turn"}

// Shutdown
{"kind": "cap.shutdown"}
```

---

#### 3.2.2 `AgentEvent` (Agent → Client)

Enum with the following variants:

```rust
pub enum AgentEvent {
    /// Agent is ready to receive prompts. Must be the first event emitted
    /// after spawn.
    Ready {
        session_id: String,
        version: String,
        model: String,
    },
    
    /// Streaming text chunk (assistant response, thought, or system message).
    TextChunk {
        msg_id: String,
        text: String,
        channel: TextChannel,
    },
    
    /// Reasoning/thinking event (distinct from TextChunk-Thought).
    Thought {
        msg_id: String,
        text: String,
    },
    
    /// Tool invocation started.
    ToolCallStart {
        call_id: String,
        name: String,
        input: serde_json::Value,
    },
    
    /// Tool invocation completed.
    ToolCallEnd {
        call_id: String,
        output: serde_json::Value,
        is_error: bool,
    },
    
    /// Agent needs permission to proceed (e.g., execute a shell command).
    PermissionRequest {
        req_id: String,
        tool: String,
        intent: String,
        scope: PermissionScope,
        risk_level: RiskLevel,
    },
    
    /// Agent needs user input (interactive question).
    AskUser {
        ask_id: String,
        question: String,
        options: Vec<AskOption>,
    },
    
    /// Turn completed.
    Done {
        stop_reason: StopReason,
        usage: Usage,
    },
    
    /// Error occurred.
    Error {
        message: String,
        code: Option<String>,
    },
}
```

**JSON wire format (examples):**

```json
// Ready
{"kind": "cap.ready", "session_id": "sess-abc123", "version": "1.0", "model": "deepseek-coder"}

// TextChunk (assistant)
{"kind": "cap.text_chunk", "msg_id": "m1", "text": "I'll fix the test...", "channel": "assistant"}

// TextChunk (thought)
{"kind": "cap.text_chunk", "msg_id": "m2", "text": "Let me analyze...", "channel": "thought"}

// Thought
{"kind": "cap.thought", "msg_id": "t1", "text": "Reasoning about the bug..."}

// ToolCallStart
{"kind": "cap.tool_call.start", "call_id": "c1", "name": "Bash", "input": {"command": "cargo test"}}

// ToolCallEnd
{"kind": "cap.tool_call.end", "call_id": "c1", "output": "test result: ok", "is_error": false}

// PermissionRequest
{"kind": "cap.permission.request", "req_id": "perm-123", "tool": "Bash", "intent": "run cargo test", "scope": "workspace", "risk_level": "low"}

// AskUser
{"kind": "cap.ask_user", "ask_id": "ask-456", "question": "Which test should I fix?", "options": [{"label": "test_foo", "description": "Fix test_foo"}, {"label": "test_bar", "description": "Fix test_bar"}]}

// Done
{"kind": "cap.done", "stop_reason": "end_turn", "usage": {"input_tokens": 1000, "output_tokens": 500}}

// Error
{"kind": "cap.error", "message": "compilation failed", "code": "E0308"}
```

---

#### 3.2.3 Supporting Types

```rust
pub enum Content {
    Text { text: String },
    Image { data: String, media_type: String }, // data is base64
}

pub enum TextChannel {
    Assistant,
    Thought,
    System,
}

pub enum PermissionDecision {
    AllowOnce,
    AllowAlways,
    Deny,
}

pub enum PermissionScope {
    Workspace,
    System,
    Network,
}

pub enum RiskLevel {
    Low,
    Medium,
    High,
}

pub enum CancelScope {
    Turn,
    Session,
}

pub enum StopReason {
    EndTurn,
    MaxTokens,
    Cancelled,
    Error,
}

pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub total_tokens: u64,
    pub cost_usd: Option<f64>,
}

pub struct AskOption {
    pub label: String,
    pub description: Option<String>,
    pub value: serde_json::Value,
}
```

---

### 3.3 Lifecycle

```
1. rsclaw spawns: rscode cap --cwd /path/to/project
   ↓
2. rscode emits: Ready { session_id, version, model }
   ↓
3. rsclaw sends: Prompt { content: [Text { text: "fix the test" }] }
   ↓
4. rscode processes:
   ├─ emits: TextChunk { channel: "thought", text: "analyzing..." }
   ├─ emits: ToolCallStart { name: "Bash", input: {...} }
   ├─ emits: PermissionRequest { tool: "Bash", risk_level: "low" }
   ├─ waits for rsclaw response...
   ├─ receives: PermissionResponse { decision: "allow_once" }
   ├─ emits: ToolCallEnd { output: "...", is_error: false }
   ├─ emits: TextChunk { channel: "assistant", text: "I fixed the test..." }
   └─ emits: Done { stop_reason: "end_turn", usage: {...} }
   ↓
5. rsclaw receives Done → turn complete
   ↓
6. rsclaw sends next Prompt OR Shutdown
```

**Critical invariants:**

1. **Ready must be first event.** rsclaw's driver waits up to 30s for `Ready` after spawn.
2. **Done terminates the turn.** After `Done`, rsclaw expects no more events until the next `Prompt`.
3. **PermissionRequest blocks.** The agent must wait for `PermissionResponse` before proceeding.
4. **AskUser blocks.** The agent must wait for `AskUserAnswer` before proceeding.

---

### 3.4 Permission Handling

**Current rsclaw behavior (P1):**

rsclaw's `permission::auto_approve` **always returns `AllowOnce`**, regardless of `risk_level`. This is a known limitation (spec §7.6 forbids orchestrators from auto-approving High risk by default).

**Implication for rscode:**

- rscode can emit `PermissionRequest` for any tool call.
- rsclaw will auto-approve with `AllowOnce`.
- rscode should **not** rely on `AllowAlways` being honored (rsclaw doesn't cache decisions).
- Future (P2): rsclaw may implement interactive permission UI; rscode should be prepared for `Deny` responses.

**Recommendation:**

rscode's `cap` subcommand should use `--yes` semantics internally (auto-approve all permissions) to match rsclaw's current behavior. Emit `PermissionRequest` events for observability, but don't block on them unless rsclaw explicitly implements interactive approval.

---

### 3.5 Error Handling

**Agent-side errors:**

- If rscode encounters a fatal error (e.g., engine crash), emit `Error { message, code }` then exit.
- If a turn fails but the agent can continue, emit `Error` then `Done { stop_reason: "error" }`.

**Client-side errors:**

- If rsclaw's driver crashes, the subprocess receives SIGTERM/SIGKILL.
- rscode should handle graceful shutdown on SIGTERM (flush state, exit 0).

---

## 4. Implementation Guide for rscode

### 4.1 New Subcommand: `rscode cap`

Add a `cap` subcommand to rscode's CLI:

```bash
rscode cap [OPTIONS]

Options:
  --cwd <PATH>          Working directory (default: current dir)
  --session <ID>        Resume an existing session
  --yes                 Auto-approve all permissions (recommended for CAP)
  --model <MODEL>       Override model selection
```

**Behavior:**

1. Parse args, load config, initialize engine.
2. Emit `Ready { session_id, version, model }` to stdout.
3. Enter main loop:
   - Read line from stdin → parse as `ClientFrame`.
   - Match on frame kind:
     - `Prompt` → run engine turn, emit streaming events, emit `Done`.
     - `PermissionResponse` → unblock pending permission request.
     - `AskUserAnswer` → unblock pending ask.
     - `Cancel` → abort current turn, emit `Done { stop_reason: "cancelled" }`.
     - `Shutdown` → cleanup, exit 0.
4. On EOF (stdin closed), exit gracefully.

---

### 4.2 Engine Integration

rscode's `Engine` already has the necessary hooks:

```rust
// From rscode-engine/src/engine/mod.rs

pub struct Engine<M: ModelClient> {
    // ...
    permission_prompt: Option<Box<dyn Fn(&str, &str) -> bool>>,
    event_sink: Option<tokio::sync::mpsc::UnboundedSender<Event>>,
}

impl<M: ModelClient> Engine<M> {
    pub fn set_permission_prompt(&mut self, cb: Box<dyn Fn(&str, &str) -> bool>) { ... }
    pub fn set_event_sink(&mut self, sink: Option<tokio::sync::mpsc::UnboundedSender<Event>>) { ... }
    pub async fn run_turn(&mut self, prompt: String) -> Vec<Event> { ... }
}
```

**Integration strategy:**

1. **Permission callback:** Wire `permission_prompt` to emit `PermissionRequest` and wait for `PermissionResponse` via a shared channel.

2. **Event sink:** Wire `event_sink` to translate rscode's internal `Event` enum to CAP `AgentEvent`:
   - `Event::ToolRequested` → `AgentEvent::ToolCallStart`
   - `Event::ToolCompleted` → `AgentEvent::ToolCallEnd`
   - `Event::AssistantText` → `AgentEvent::TextChunk { channel: "assistant" }`
   - `Event::Thinking` → `AgentEvent::TextChunk { channel: "thought" }` or `AgentEvent::Thought`

3. **Turn completion:** After `run_turn` returns, emit `AgentEvent::Done`.

---

### 4.3 Wire Protocol Serialization

Use `cap_rs::core` types directly (add `cap-rs = "0.1"` to `Cargo.toml`):

```rust
use cap_rs::core::{ClientFrame, AgentEvent, Content, TextChannel, ...};

// Read from stdin
let line = read_line_from_stdin()?;
let frame: ClientFrame = serde_json::from_str(&line)?;

// Write to stdout
let event = AgentEvent::Ready { ... };
let json = serde_json::to_string(&event)?;
println!("{}", json);
```

**Alternative (no cap-rs dependency):**

If rscode prefers not to depend on `cap-rs`, implement the JSON schema manually:

```rust
#[derive(Serialize, Deserialize)]
#[serde(tag = "kind")]
enum ClientFrame {
    #[serde(rename = "cap.prompt")]
    Prompt { content: Vec<Content> },
    // ...
}
```

**Recommendation:** Use `cap-rs` for type safety and forward compatibility.

---

### 4.4 Testing

**Unit tests:**

- Serialize/deserialize all `ClientFrame` and `AgentEvent` variants.
- Verify JSON wire format matches spec examples.

**Integration tests:**

1. Spawn `rscode cap` as a subprocess.
2. Send `Prompt`, verify streaming events are received.
3. Send `PermissionRequest` → `PermissionResponse`, verify flow.
4. Send `Shutdown`, verify graceful exit.

**E2E test with rsclaw:**

1. Build rscode with `cap` subcommand.
2. Set `RSCODE_BIN=/path/to/rscode` in rsclaw environment.
3. From rsclaw, invoke `tool_cap(agent="rscode", task="hello")`.
4. Verify rsclaw receives `Ready`, processes `Prompt`, receives `Done`.

---

## 5. rsclaw-Side Changes (Minimal)

Once rscode implements CAP, rsclaw needs:

1. **Add `AgentKind::Rscode` variant** in `crates/rsclaw-cap/src/runtime.rs`.
2. **Add `rscode` slot** to `CapAgentManager`.
3. **Spawn logic:** Use `cap_rs::driver::stream_json::ClaudeCodeDriver` with custom binary:
   ```rust
   ClaudeCodeDriver::builder(cwd)
       .bin("rscode")
       .args(&["cap", "--yes"])
       .dangerously_skip_permissions(true)
       .spawn()
       .await
   ```
4. **Environment variable:** Honor `RSCODE_BIN` for binary path override.

**Estimated rsclaw changes:** ~100 lines of code.

---

## 6. Migration Path

### Phase 1 (Current): stream-json Compatibility

rscode's existing `rscode run --input-stream-json --output-stream-json` is **Claude Code wire-compatible**. rsclaw can integrate this immediately using the `ClaudeCodeDriver` with custom binary:

```rust
ClaudeCodeDriver::builder(cwd)
    .bin("rscode")
    .args(&["run", "--input-stream-json", "--output-stream-json", "--yes"])
    .spawn()
    .await
```

**Limitation:** Uses Claude's event schema, not CAP. rsclaw's `bridge::dispatch` expects CAP `AgentEvent` variants.

### Phase 2 (This Spec): CAP Protocol

rscode implements `rscode cap` subcommand with full CAP protocol support. rsclaw uses `AgentKind::Rscode` with dedicated spawn logic.

**Benefit:** Vendor-neutral, compatible with other CAP orchestrators.

### Phase 3 (Future): Advanced Features

- **Reverse RPC:** rscode emits `AskUser` for interactive questions; rsclaw implements UI to collect answers.
- **Permission UI:** rsclaw implements interactive permission approval (replaces `auto_approve`).
- **Session persistence:** rscode supports `--session` to resume across restarts.

---

## 7. Open Questions

1. **Should rscode depend on `cap-rs` or implement the wire format manually?**
   - Recommendation: Use `cap-rs` for type safety.

2. **Should rscode's `cap` subcommand auto-approve permissions internally (`--yes`)?**
   - Recommendation: Yes, to match rsclaw's current `auto_approve` behavior.

3. **How should rscode handle `AskUser` if rsclaw always responds with `"cancelled"`?**
   - Recommendation: rscode should avoid `AskUser` in non-interactive contexts; use tool defaults.

4. **Should rscode emit `Ready` immediately or after engine initialization?**
   - Recommendation: After engine init (model loaded, config validated), so rsclaw knows the agent is truly ready.

---

## 8. References

- [CAP Protocol Specification](https://cap-protocol.org)
- [cap-rs crate documentation](https://docs.rs/cap-rs/0.1.2)
- [rsclaw AGENTS.md](../AGENTS.md) — cap-protocol integration section
- [rscode README](../../dev-rscode/README.md) — stream-json mode

---

## 9. Appendix: JSON Schema Examples

### 9.1 Complete Turn (No Permissions)

```json
// Client → Agent
{"kind": "cap.prompt", "content": [{"type": "text", "text": "What is 2+2?"}]}

// Agent → Client
{"kind": "cap.ready", "session_id": "sess-123", "version": "1.0", "model": "deepseek-coder"}
{"kind": "cap.text_chunk", "msg_id": "m1", "text": "2+2 equals 4.", "channel": "assistant"}
{"kind": "cap.done", "stop_reason": "end_turn", "usage": {"input_tokens": 10, "output_tokens": 5, "total_tokens": 15}}
```

### 9.2 Turn with Tool Call and Permission

```json
// Client → Agent
{"kind": "cap.prompt", "content": [{"type": "text", "text": "Run the tests"}]}

// Agent → Client
{"kind": "cap.text_chunk", "msg_id": "m1", "text": "I'll run the tests...", "channel": "assistant"}
{"kind": "cap.tool_call.start", "call_id": "c1", "name": "Bash", "input": {"command": "cargo test"}}
{"kind": "cap.permission.request", "req_id": "perm-1", "tool": "Bash", "intent": "run cargo test", "scope": "workspace", "risk_level": "low"}

// Client → Agent
{"kind": "cap.permission.response", "req_id": "perm-1", "decision": "allow_once"}

// Agent → Client
{"kind": "cap.tool_call.end", "call_id": "c1", "output": "test result: ok\n10 passed", "is_error": false}
{"kind": "cap.text_chunk", "msg_id": "m2", "text": "All tests passed!", "channel": "assistant"}
{"kind": "cap.done", "stop_reason": "end_turn", "usage": {"input_tokens": 100, "output_tokens": 50, "total_tokens": 150}}
```

---

**End of Specification**
