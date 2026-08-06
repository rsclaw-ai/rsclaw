//! /goal — result-driven turn loop.
//!
//! Semantically different from /loop:
//!
//!   * /loop is **schedule-driven** — fires on a wallclock cron tick whether or
//!     not the previous turn completed. Cron is a timer.
//!   * /goal is **completion-driven** — waits for the previous turn to fully
//!     finish, then evaluates whether the goal has been reached. If not, fires
//!     the next turn back-to-back. No interval.
//!
//! How a goal terminates
//!
//! The LLM running inside a goal session is told to write one of three
//! markers as the very last line of its reply:
//!
//!   * `GOAL_ACHIEVED`     — goal met. Stop.
//!   * `GOAL_FAILED <why>` — goal cannot be met from here. Stop.
//!   * (no GOAL_* line)    — still working. Re-invoke for another turn.
//!
//! Mirrors Claude Code's `/goal` semantics, but the verdict comes from
//! the model itself (not a separate judge / Stop-hook process) — the
//! model has full context for the goal and its own progress, so a
//! second-opinion judge is redundant. We trust the model's self-report
//! and bound the runaway case with an iter cap (default 30) so a
//! confused model can't burn unlimited tokens.
//!
//! How state is stored
//!
//! One `MemoryDoc` per active goal:
//!
//!   * `kind`           = "active_goal"
//!   * `scope`          = session key (e.g. "agent:main:feishu:direct:ou_…")
//!   * `text`           = the human-language goal text
//!   * `abstract_text`  = JSON `{"iter": N, "max": M}` — current iteration
//!     count and cap. (We could store this in tags, but keeping it in a
//!     structured field makes the redb inspection / debugging easier.)
//!   * `pinned`         = true (no decay; goal stays alive across the
//!     crystallizer's lifecycle gates)
//!
//! Survives gateway restart, because the memory store is on disk.
//! Survives `/clear` only if `/clear` doesn't wipe memory docs — which
//! it doesn't (it touches session messages, not memory). A user can
//! manually wipe with `/goal clear`.
//!
//! Hook into the turn loop
//!
//! `gateway/startup.rs` calls `check_after_turn` once after every
//! agent reply lands. Three things can happen:
//!
//!   1. **No active goal for this session** — returns `None`, the turn loop
//!      continues as normal.
//!   2. **Active goal + terminal marker found** — returns
//!      `Reaction::Done(message)`. The hook caller appends `message` to the
//!      reply text so the user sees ✅/❌/⚠ in the same chat bubble.
//!   3. **Active goal + still working** — returns `Reaction::Continue(prompt)`.
//!      The hook caller submits `prompt` back to the task queue, producing a
//!      fresh turn back-to-back.
//!
//! Idempotency: the hook only reads + (conditionally) mutates the goal
//! doc; it doesn't write a new doc on a no-op. Re-entry is safe.

use std::sync::Arc;

use anyhow::Result;
use tokio::sync::Mutex;

use crate::memory::{MemoryDoc, MemoryStore};

/// Memory doc kind tag for goal docs.
pub const GOAL_KIND: &str = "active_goal";

/// Default iteration cap when the user doesn't override.
pub const DEFAULT_MAX_ITER: u32 = 30;

/// Hard upper bound — even if the user says `--max 9999`, we cap here
/// so a misconfigured goal can't loop forever.
pub const HARD_MAX_ITER: u32 = 200;

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------

/// In-memory snapshot of an active goal's persistent state.
#[derive(Debug, Clone)]
pub struct ActiveGoal {
    /// The human-language goal the user typed.
    pub condition: String,
    /// 1-based iteration count of turns that have run so far.
    pub iter: u32,
    /// Iteration ceiling. Reaching it terminates with `⚠ iter cap`.
    pub max_iter: u32,
    /// Unix timestamp (seconds) when the goal was set.
    pub started_at: i64,
}

/// Terminal-marker classification of an LLM reply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TerminalSignal {
    /// Reply's last non-blank line was `GOAL_ACHIEVED`.
    Achieved,
    /// Reply's last non-blank line started with `GOAL_FAILED` —
    /// inner string is the trimmed reason after the marker.
    Failed(String),
    /// No terminal marker — agent is still working.
    Continue,
}

/// What the hook caller should do after a turn completes for a session
/// with an active goal.
pub enum Reaction {
    /// Goal hit a terminal state. The included message describes the
    /// outcome (✅ / ❌ / ⚠) and should be surfaced to the user.
    Done(String),
    /// Goal is still active. The included string is the next-turn
    /// prompt; the caller should submit it via the task queue.
    Continue(String),
}

// ---------------------------------------------------------------------------
// Terminal marker parsing
// ---------------------------------------------------------------------------

/// Extract the last non-blank line and check it for the terminal markers.
///
/// `Achieved` requires the line to be exactly `GOAL_ACHIEVED` (after
/// trimming) — substring match would be too eager, e.g. if the model
/// quoted the instructions back into its reply.
/// `Failed` matches `GOAL_FAILED` optionally followed by a reason.
/// Anything else, including an empty reply, returns `Continue`.
pub fn parse_terminal(reply_text: &str) -> TerminalSignal {
    let last = reply_text
        .lines()
        .rev()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("")
        .trim();
    if last == "GOAL_ACHIEVED" {
        return TerminalSignal::Achieved;
    }
    if let Some(rest) = last.strip_prefix("GOAL_FAILED") {
        return TerminalSignal::Failed(rest.trim().to_owned());
    }
    TerminalSignal::Continue
}

// ---------------------------------------------------------------------------
// Memory IO — read / set / clear / bump
// ---------------------------------------------------------------------------

/// Read the active goal for a session, if any.
pub async fn read(mem: &Arc<Mutex<MemoryStore>>, session_key: &str) -> Option<ActiveGoal> {
    let store = mem.lock().await;
    let docs = store.list_active();
    drop(store);
    docs.into_iter()
        .find(|d| d.scope == session_key && d.kind == GOAL_KIND)
        .map(|d| {
            let (iter, max_iter) = parse_iter_meta(d.abstract_text.as_deref().unwrap_or(""));
            ActiveGoal {
                condition: d.text,
                iter,
                max_iter,
                started_at: d.created_at,
            }
        })
}

/// Set (or replace) the goal for a session. Caps `max_iter` at
/// `HARD_MAX_ITER` so a runaway config can't break out.
pub async fn set(
    mem: &Arc<Mutex<MemoryStore>>,
    session_key: &str,
    condition: &str,
    max_iter: u32,
) -> Result<()> {
    let max_iter = max_iter.clamp(1, HARD_MAX_ITER);
    clear(mem, session_key).await?;
    let now = chrono::Utc::now().timestamp();
    let mut store = mem.lock().await;
    let doc = MemoryDoc {
        id: uuid::Uuid::new_v4().to_string(),
        scope: session_key.to_owned(),
        kind: GOAL_KIND.to_owned(),
        text: condition.to_owned(),
        vector: vec![],
        created_at: now,
        accessed_at: 0,
        access_count: 0,
        importance: 0.9,
        tier: Default::default(),
        abstract_text: Some(format!(r#"{{"iter":1,"max":{max_iter}}}"#)),
        overview_text: None,
        tags: vec!["goal".to_owned()],
        // Pinned so the crystallizer doesn't demote / decay the doc
        // while the goal is active.
        pinned: true,
    };
    store.add(doc).await?;
    Ok(())
}

/// Clear all goal docs in a session (idempotent — no-op if none).
pub async fn clear(mem: &Arc<Mutex<MemoryStore>>, session_key: &str) -> Result<()> {
    let store = mem.lock().await;
    let to_delete: Vec<String> = store
        .list_active()
        .into_iter()
        .filter(|d| d.scope == session_key && d.kind == GOAL_KIND)
        .map(|d| d.id)
        .collect();
    drop(store);
    if to_delete.is_empty() {
        return Ok(());
    }
    let mut store = mem.lock().await;
    for id in to_delete {
        if let Err(e) = store.delete(&id).await {
            tracing::warn!(goal_id = %id, err = %e, "goal: delete failed");
        }
    }
    Ok(())
}

/// Bump the iteration count by 1 (rewrites the doc — no in-place
/// mutation API on the memory store).
async fn bump_iter(
    mem: &Arc<Mutex<MemoryStore>>,
    session_key: &str,
    current: &ActiveGoal,
) -> Result<()> {
    let store = mem.lock().await;
    let existing_id: Option<String> = store
        .list_active()
        .into_iter()
        .find(|d| d.scope == session_key && d.kind == GOAL_KIND)
        .map(|d| d.id);
    drop(store);
    let Some(old_id) = existing_id else {
        return Ok(());
    };
    let mut store = mem.lock().await;
    if let Err(e) = store.delete(&old_id).await {
        tracing::warn!(old_goal_id = %old_id, err = %e, "goal: replace delete failed");
    }
    let doc = MemoryDoc {
        id: uuid::Uuid::new_v4().to_string(),
        scope: session_key.to_owned(),
        kind: GOAL_KIND.to_owned(),
        text: current.condition.clone(),
        vector: vec![],
        created_at: current.started_at,
        accessed_at: 0,
        access_count: 0,
        importance: 0.9,
        tier: Default::default(),
        abstract_text: Some(format!(
            r#"{{"iter":{},"max":{}}}"#,
            current.iter + 1,
            current.max_iter
        )),
        overview_text: None,
        tags: vec!["goal".to_owned()],
        pinned: true,
    };
    store.add(doc).await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Hook called from gateway/startup.rs after each turn
// ---------------------------------------------------------------------------

/// Inspect a finished turn for goal-relevant signals.
///
/// Returns:
///   * `None`        — no active goal for this session (no-op)
///   * `Some(Done)`  — goal terminated; surface the message to the user
///   * `Some(Continue)` — submit the included prompt as the next turn
pub async fn check_after_turn(session_key: &str, reply_text: &str) -> Option<Reaction> {
    let mem = crate::memory::global_store()?;
    let active = read(&mem, session_key).await?;
    let signal = parse_terminal(reply_text);
    match signal {
        TerminalSignal::Achieved => {
            let _ = clear(&mem, session_key).await;
            Some(Reaction::Done(format!(
                "✅ Goal achieved (iter {}/{}): {}",
                active.iter, active.max_iter, active.condition
            )))
        }
        TerminalSignal::Failed(reason) => {
            let _ = clear(&mem, session_key).await;
            let msg = if reason.is_empty() {
                format!(
                    "❌ Goal could not be achieved (iter {}/{}): {}",
                    active.iter, active.max_iter, active.condition
                )
            } else {
                format!(
                    "❌ Goal could not be achieved (iter {}/{}): {}\n原因: {}",
                    active.iter, active.max_iter, active.condition, reason
                )
            };
            Some(Reaction::Done(msg))
        }
        TerminalSignal::Continue => {
            if active.iter >= active.max_iter {
                let _ = clear(&mem, session_key).await;
                Some(Reaction::Done(format!(
                    "⚠ Goal hit iter cap ({}): {} — auto-stopped. Type `/goal {}` to restart.",
                    active.max_iter, active.condition, active.condition
                )))
            } else {
                let _ = bump_iter(&mem, session_key, &active).await;
                Some(Reaction::Continue(build_continuation_prompt(&active)))
            }
        }
    }
}

/// The prompt sent back through the task queue for the next iteration.
/// The instructions block is repeated each turn for two reasons:
///   * Stateless re-priming — the LLM might not have seen the initial /goal
///     turn in its context window after compaction.
///   * Anti-drift — the GOAL_ACHIEVED / GOAL_FAILED format must stay fresh;
///     without re-priming, the model tends to forget the exact marker spelling.
fn build_continuation_prompt(g: &ActiveGoal) -> String {
    format!(
        "目标: {} (iter {}/{})\n\n\
         你处于 /goal 模式。继续推进。\n\
         回复末行严格写其中一种 (区分大小写):\n\
         \u{0020}\u{0020}GOAL_ACHIEVED              → 已达成,我会停\n\
         \u{0020}\u{0020}GOAL_FAILED <理由>          → 放弃,我会停\n\
         \u{0020}\u{0020}(不写 GOAL_* 一行)         → 继续下一轮\n\n\
         如果上一轮已经达成,直接以 GOAL_ACHIEVED 收尾即可。",
        g.condition,
        g.iter + 1,
        g.max_iter
    )
}

/// First-turn prompt — the one fired immediately after `/goal <cond>`.
/// Distinct from continuation only by tone (introduces the mode).
pub fn build_initial_prompt(condition: &str, max_iter: u32) -> String {
    format!(
        "目标: {} (iter 1/{})\n\n\
         你处于 /goal 模式。开始干活推进这个目标。\n\
         回复末行严格写其中一种 (区分大小写):\n\
         \u{0020}\u{0020}GOAL_ACHIEVED              → 已达成,我会停\n\
         \u{0020}\u{0020}GOAL_FAILED <理由>          → 放弃,我会停\n\
         \u{0020}\u{0020}(不写 GOAL_* 一行)         → 继续下一轮\n\n\
         如果目标看起来已经达成 (例如静态条件已满足),直接 GOAL_ACHIEVED 即可。",
        condition, max_iter
    )
}

// ---------------------------------------------------------------------------
// Iter metadata serde — JSON-in-a-field
// ---------------------------------------------------------------------------

/// Parse the abstract_text payload (`{"iter":N,"max":M}`). Defaults
/// to (1, DEFAULT_MAX_ITER) on any error — safer than panic'ing
/// when the doc has been edited externally.
fn parse_iter_meta(raw: &str) -> (u32, u32) {
    let v: serde_json::Value = serde_json::from_str(raw).unwrap_or(serde_json::Value::Null);
    let iter = v.get("iter").and_then(|x| x.as_u64()).unwrap_or(1) as u32;
    let max = v
        .get("max")
        .and_then(|x| x.as_u64())
        .unwrap_or(DEFAULT_MAX_ITER as u64) as u32;
    (iter.max(1), max.clamp(1, HARD_MAX_ITER))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_terminal_recognises_achieved() {
        assert_eq!(
            parse_terminal("doing stuff\n\nGOAL_ACHIEVED"),
            TerminalSignal::Achieved
        );
        // Trailing whitespace + blank lines tolerated.
        assert_eq!(
            parse_terminal("doing stuff\n\nGOAL_ACHIEVED   \n\n"),
            TerminalSignal::Achieved
        );
    }

    #[test]
    fn parse_terminal_recognises_failed_with_reason() {
        assert_eq!(
            parse_terminal("explanation\n\nGOAL_FAILED no test runner"),
            TerminalSignal::Failed("no test runner".to_owned())
        );
        assert_eq!(
            parse_terminal("GOAL_FAILED"),
            TerminalSignal::Failed(String::new())
        );
    }

    #[test]
    fn parse_terminal_continue_when_no_marker() {
        assert_eq!(
            parse_terminal("just an analysis paragraph"),
            TerminalSignal::Continue
        );
        // Marker NOT on the last line — still continue.
        assert_eq!(
            parse_terminal("GOAL_ACHIEVED\nactually wait i'm not sure"),
            TerminalSignal::Continue
        );
    }

    #[test]
    fn parse_iter_meta_round_trip() {
        assert_eq!(parse_iter_meta(r#"{"iter":3,"max":50}"#), (3, 50));
        assert_eq!(parse_iter_meta(""), (1, DEFAULT_MAX_ITER));
        assert_eq!(parse_iter_meta("garbage"), (1, DEFAULT_MAX_ITER));
        // Hard cap is enforced even when parsing back from disk.
        assert_eq!(
            parse_iter_meta(r#"{"iter":1,"max":99999}"#).1,
            HARD_MAX_ITER
        );
    }
}
