//! Per-turn difficulty metrics — feeds workflow crystallization.
//!
//! A turn that took many tools, hit errors, looped on the same call, or ran
//! long is the kind of "stepped on a landmine" experience the agent should
//! codify into a SKILL.md so the next attempt is faster. This module
//! collects the raw counters during `agent_loop` and computes a normalized
//! difficulty score consumed by [`crate::skill::workflow_distill`].

use std::collections::HashSet;
use std::sync::{Mutex, OnceLock};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

/// Counters maintained for the current turn.
#[derive(Debug, Clone)]
pub struct TurnMetrics {
    pub started_at: Instant,
    pub tool_calls: usize,
    pub distinct_tools: HashSet<String>,
    pub tool_errors: usize,
    pub same_call_streak_max: usize,
    pub final_text_len: usize,
    /// Raw transcript of tool calls in execution order: (name, args_json,
    /// result_summary, is_error). Bounded length per result so we don't
    /// keep megabytes of screenshots in memory.
    pub tool_log: Vec<TurnToolEntry>,
}

#[derive(Debug, Clone)]
pub struct TurnToolEntry {
    pub name: String,
    pub args_summary: String,
    pub result_summary: String,
    pub is_error: bool,
}

impl Default for TurnMetrics {
    fn default() -> Self {
        Self::new()
    }
}

impl TurnMetrics {
    pub fn new() -> Self {
        Self {
            started_at: Instant::now(),
            tool_calls: 0,
            distinct_tools: HashSet::new(),
            tool_errors: 0,
            same_call_streak_max: 0,
            final_text_len: 0,
            tool_log: Vec::new(),
        }
    }

    pub fn record_tool(
        &mut self,
        name: &str,
        args_summary: String,
        result_summary: String,
        is_error: bool,
    ) {
        self.tool_calls += 1;
        self.distinct_tools.insert(name.to_owned());
        if is_error {
            self.tool_errors += 1;
        }
        self.tool_log.push(TurnToolEntry {
            name: name.to_owned(),
            args_summary,
            result_summary,
            is_error,
        });
    }

    pub fn duration_secs(&self) -> f32 {
        self.started_at.elapsed().as_secs_f32()
    }

    /// Composite difficulty in [0.0, 1.0]. Weighted average of normalized
    /// component metrics — tuned so a "moderately hard turn" (8 tool calls,
    /// 4 distinct tools, 1 error, no looping, 60s) lands near 0.5.
    pub fn difficulty_score(&self) -> f32 {
        let tc = (self.tool_calls as f32 / 20.0).min(1.0);
        let dt = (self.distinct_tools.len() as f32 / 6.0).min(1.0);
        let te = (self.tool_errors as f32 / 5.0).min(1.0);
        let scs = (self.same_call_streak_max as f32 / 3.0).min(1.0);
        let dur = (self.duration_secs() / 120.0).min(1.0);

        // Errors weight highest — "踩坑" is the strongest signal that this
        // turn carries hard-won knowledge worth codifying.
        0.25 * tc + 0.20 * dt + 0.30 * te + 0.15 * scs + 0.10 * dur
    }

    /// Order-invariant fingerprint over the set of tool names used in this
    /// turn. Two turns that hit the same tool palette dedup against each
    /// other so we don't crystallize the same workflow twice.
    pub fn signature(&self) -> u64 {
        use std::hash::{DefaultHasher, Hash, Hasher};
        let mut sorted: Vec<&str> = self.distinct_tools.iter().map(String::as_str).collect();
        sorted.sort_unstable();
        let mut hasher = DefaultHasher::new();
        for n in &sorted {
            n.hash(&mut hasher);
        }
        hasher.finish()
    }
}

// ---------------------------------------------------------------------------
// Process-wide rate limiting + dedup state
// ---------------------------------------------------------------------------

#[derive(Debug, Default)]
struct WorkflowState {
    /// Sliding window of distillation timestamps (Unix seconds) — used to
    /// enforce `max_per_hour` config cap.
    recent_distills: Vec<i64>,
    /// Signatures of workflows already distilled this run, to skip
    /// near-duplicate workflow patterns.
    seen_signatures: HashSet<u64>,
}

fn workflow_state() -> &'static Mutex<WorkflowState> {
    static STATE: OnceLock<Mutex<WorkflowState>> = OnceLock::new();
    STATE.get_or_init(|| Mutex::new(WorkflowState::default()))
}

/// Check whether a new workflow distillation is allowed right now. Side
/// effect: on success the timestamp is recorded and the signature is
/// inserted, so the caller is committed to running the distillation
/// (otherwise call [`release_signature`]).
pub fn try_admit_workflow(signature: u64, max_per_hour: usize) -> bool {
    let mut st = match workflow_state().lock() {
        Ok(s) => s,
        Err(_) => return false,
    };
    if st.seen_signatures.contains(&signature) {
        return false;
    }
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let one_hour_ago = now - 3600;
    st.recent_distills.retain(|t| *t >= one_hour_ago);
    if st.recent_distills.len() >= max_per_hour {
        return false;
    }
    st.recent_distills.push(now);
    st.seen_signatures.insert(signature);
    true
}

/// Roll back a previously-admitted signature when the distillation failed
/// before producing a SKILL.md. Lets a future retry succeed.
pub fn release_signature(signature: u64) {
    if let Ok(mut st) = workflow_state().lock() {
        st.seen_signatures.remove(&signature);
        if let Some(last) = st.recent_distills.pop() {
            // Best-effort: caller's failure shouldn't burn a slot.
            let _ = last;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn difficulty_increases_monotonically() {
        let mut easy = TurnMetrics::new();
        easy.tool_calls = 1;
        easy.distinct_tools.insert("t1".into());
        let easy_score = easy.difficulty_score();

        let mut hard = TurnMetrics::new();
        hard.tool_calls = 18;
        hard.distinct_tools.extend(["a", "b", "c", "d", "e"].iter().map(|s| s.to_string()));
        hard.tool_errors = 4;
        hard.same_call_streak_max = 3;
        let hard_score = hard.difficulty_score();

        assert!(hard_score > easy_score);
        assert!(hard_score <= 1.0);
        assert!(easy_score >= 0.0);
    }

    #[test]
    fn signature_is_order_invariant() {
        let mut a = TurnMetrics::new();
        a.distinct_tools.extend(["x", "y", "z"].iter().map(|s| s.to_string()));
        let mut b = TurnMetrics::new();
        b.distinct_tools.extend(["z", "x", "y"].iter().map(|s| s.to_string()));
        assert_eq!(a.signature(), b.signature());
    }

    #[test]
    fn signature_distinguishes_different_palettes() {
        let mut a = TurnMetrics::new();
        a.distinct_tools.extend(["x", "y"].iter().map(|s| s.to_string()));
        let mut b = TurnMetrics::new();
        b.distinct_tools.extend(["x", "z"].iter().map(|s| s.to_string()));
        assert_ne!(a.signature(), b.signature());
    }

    #[test]
    fn record_tool_increments_counters() {
        let mut m = TurnMetrics::new();
        m.record_tool("read_file", "{}".into(), "ok".into(), false);
        m.record_tool("read_file", "{}".into(), "ok".into(), false);
        m.record_tool("execute_command", "{}".into(), "err".into(), true);
        assert_eq!(m.tool_calls, 3);
        assert_eq!(m.distinct_tools.len(), 2);
        assert_eq!(m.tool_errors, 1);
        assert_eq!(m.tool_log.len(), 3);
    }
}
