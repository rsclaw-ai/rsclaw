//! Real-time GUI-agent status events.
//!
//! `VlmDriver` emits one `Started` at the top of `run`, one `Step` after
//! each action, and one `Finished` when the loop exits. The gateway
//! broadcasts these on `AppState::computer_status_tx` and the WS handshake
//! relays them to the desktop UI as `computer_use_status` frames so the
//! settings panel can show "what is computer_use doing right now."
//!
//! Screenshots are deliberately NOT included — the events are tight
//! status pings (sub-kilobyte) and the user can see their own screen.

use serde::{Deserialize, Serialize};

/// One status frame. Discriminated by `kind` (snake_case JSON) to match
/// the existing event-frame conventions on the WS layer.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ComputerUseStatus {
    /// Driver entered its loop after the permission gate cleared.
    Started {
        run_id: String,
        agent_id: String,
        /// Display name of the target app (may be empty for generic desktop).
        app: String,
        /// User instruction, truncated to ~200 chars.
        instruction: String,
        max_steps: usize,
    },
    /// Emitted at the start of each iteration, immediately after the
    /// screenshot is captured and BEFORE the VLM call. Without this the
    /// UI shows nothing between `Started` and the first `Step` (5-30 s
    /// on heavy VLMs) — operators / users assume the agent is hung.
    /// `step_index` is the iteration about to be processed (1-indexed,
    /// same numbering the subsequent `Step` will use).
    /// R3 review I3.
    Thinking {
        run_id: String,
        step_index: usize,
    },
    /// One executed step. Emitted after the operator returns, including
    /// failed actions so the UI can surface "step failed" feedback.
    Step {
        run_id: String,
        /// 1-indexed.
        step_index: usize,
        /// e.g. `click(point=<box>(123,456)</box>)`.
        action_summary: String,
        /// Model thought, truncated to ~200 chars.
        thought: String,
        result_ok: bool,
        /// Operator-returned message (truncated to ~120 chars), e.g.
        /// "operator does not support hotkey".
        result_message: Option<String>,
    },
    /// Driver loop exited. `outcome_kind` is one of:
    /// `finished` | `call_user` | `max_loop` | `user_abort`
    /// | `permission_denied` | `operator_error`.
    Finished {
        run_id: String,
        outcome_kind: String,
        steps: usize,
        /// Human-readable summary (truncated to ~200 chars). Empty for
        /// `permission_denied` / `user_abort` where there is no payload.
        summary: String,
    },
}
