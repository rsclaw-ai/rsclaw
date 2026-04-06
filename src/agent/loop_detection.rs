//! Tool call loop detection (AGENTS.md §20).
//!
//! Uses a sliding window over recent tool calls.
//! Per-tool thresholds allow different limits for different tools.
//!
//! Distinguishes between WARNING (model notified, execution continues)
//! and CRITICAL (execution blocked), matching OpenClaw behavior.

use std::collections::{HashMap, VecDeque};

/// Default sliding-window size.
const DEFAULT_WINDOW: usize = 25;
/// Default warning threshold — generic loops trigger warning at this count.
const DEFAULT_WARNING_THRESHOLD: usize = 10;
/// Default critical threshold — loops at this count are blocked.
const DEFAULT_CRITICAL_THRESHOLD: usize = 20;

/// Built-in per-tool threshold overrides.
fn builtin_overrides() -> HashMap<String, (usize, usize)> {
    HashMap::new()
}

/// Result of a loop detection check.
#[derive(Debug, Clone)]
pub enum LoopCheckResult {
    /// No loop detected — proceed normally.
    Ok,
    /// Generic repeat loop at warning level — model is notified, execution
    /// continues.
    Warning {
        tool_name: String,
        count: usize,
        message: String,
    },
    /// Critical loop detected — execution is blocked.
    Critical {
        tool_name: String,
        count: usize,
        message: String,
    },
}

impl LoopCheckResult {
    /// Returns true if this result blocks execution.
    pub fn is_critical(&self) -> bool {
        matches!(self, LoopCheckResult::Critical { .. })
    }

    /// Returns the warning message if this is a warning, None otherwise.
    pub fn warning_message(&self) -> Option<String> {
        match self {
            LoopCheckResult::Warning { message, .. } => Some(message.clone()),
            _ => None,
        }
    }

    /// Convert to a `Result<Option<String>>` for use with the `?` operator.
    /// - `Ok(None)` → no loop detected, proceed
    /// - `Ok(Some(msg))` → warning, proceed with warning logged
    /// - `Err(...)` → critical loop, block
    pub fn to_result(&self) -> anyhow::Result<Option<String>> {
        match self {
            LoopCheckResult::Ok => Ok(None),
            LoopCheckResult::Warning { message, .. } => Ok(Some(message.clone())),
            LoopCheckResult::Critical { message, .. } => Err(anyhow::anyhow!("{}", message)),
        }
    }
}

#[derive(Debug, Clone)]
pub struct LoopDetector {
    window: usize,
    warning_threshold: usize,
    critical_threshold: usize,
    overrides: HashMap<String, (usize, usize)>, // (warning, critical) per-tool
    history: VecDeque<String>,
}

impl LoopDetector {
    pub fn new(window: usize, default_threshold: usize) -> Self {
        // When constructed with a single threshold (backwards compat), use it as the
        // warning threshold; critical is set one step above.
        Self::with_dual_thresholds(
            window,
            default_threshold,
            default_threshold.saturating_add(1),
        )
    }

    /// Create with explicit dual thresholds (warning + critical).
    pub fn with_dual_thresholds(
        window: usize,
        warning_threshold: usize,
        critical_threshold: usize,
    ) -> Self {
        Self {
            window,
            warning_threshold,
            critical_threshold,
            overrides: builtin_overrides(),
            history: VecDeque::new(),
        }
    }

    pub fn with_overrides(
        window: usize,
        warning_threshold: usize,
        critical_threshold: usize,
        extra_overrides: HashMap<String, (usize, usize)>,
    ) -> Self {
        let mut overrides = builtin_overrides();
        overrides.extend(extra_overrides);
        Self {
            window,
            warning_threshold,
            critical_threshold,
            overrides,
            history: VecDeque::new(),
        }
    }

    /// Create a LoopDetector compatible with runtime.rs caller that passes
    /// a single threshold value. We treat that value as the WARNING threshold
    /// and set critical = warning + 10 (matching OpenClaw's
    /// DEFAULT_CRITICAL_THRESHOLD = WARNING_THRESHOLD + 10 pattern).
    pub fn from_single_threshold(window: usize, threshold: usize) -> Self {
        let critical = threshold.saturating_add(10).max(threshold + 1);
        Self::with_dual_thresholds(window, threshold, critical)
    }

    fn thresholds_for(&self, tool_name: &str) -> (usize, usize) {
        self.overrides
            .get(tool_name)
            .copied()
            .unwrap_or((self.warning_threshold, self.critical_threshold))
    }

    /// Record a tool call and check for loops.
    ///
    /// Returns `LoopCheckResult`:
    /// - `Ok` → proceed normally
    /// - `Warning` → model is notified, execution continues (generic repeat)
    /// - `Critical` → execution blocked (excessive repeats or circuit breaker)
    pub fn check(&mut self, tool_name: &str) -> LoopCheckResult {
        self.history.push_back(tool_name.to_owned());
        if self.history.len() > self.window {
            self.history.pop_front();
        }

        let count = self
            .history
            .iter()
            .filter(|n| n.as_str() == tool_name)
            .count();

        let (warning_threshold, critical_threshold) = self.thresholds_for(tool_name);

        // Critical threshold — blocks execution (matches OpenClaw globalCircuitBreaker
        // pattern)
        if count >= critical_threshold {
            return LoopCheckResult::Critical {
                tool_name: tool_name.to_owned(),
                count,
                message: format!(
                    "CRITICAL: tool `{tool_name}` called {count} times in the last {} calls. \
                     Session execution blocked to prevent runaway loops.",
                    self.history.len(),
                ),
            };
        }

        // Warning threshold — model is notified but execution continues
        // (matches OpenClaw genericRepeat warn-only behavior)
        if count >= warning_threshold {
            return LoopCheckResult::Warning {
                tool_name: tool_name.to_owned(),
                count,
                message: format!(
                    "WARNING: You have called `{tool_name}` {count} times in the last {} \
                     calls with identical arguments. If this is not making progress, \
                     stop retrying and report the task as failed.",
                    self.history.len(),
                ),
            };
        }

        LoopCheckResult::Ok
    }

    /// Reset the history (e.g. after a tool successfully produces new output).
    pub fn reset(&mut self) {
        self.history.clear();
    }
}

impl Default for LoopDetector {
    fn default() -> Self {
        Self::with_dual_thresholds(
            DEFAULT_WINDOW,
            DEFAULT_WARNING_THRESHOLD,
            DEFAULT_CRITICAL_THRESHOLD,
        )
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn is_ok(r: &LoopCheckResult) -> bool {
        matches!(r, LoopCheckResult::Ok)
    }
    fn is_warning(r: &LoopCheckResult) -> bool {
        matches!(r, LoopCheckResult::Warning { .. })
    }
    fn is_critical(r: &LoopCheckResult) -> bool {
        matches!(r, LoopCheckResult::Critical { .. })
    }

    #[test]
    fn no_loop_for_varied_tools() {
        let mut d = LoopDetector::default();
        assert!(is_ok(&d.check("read")));
        assert!(is_ok(&d.check("write")));
        assert!(is_ok(&d.check("exec")));
        assert!(is_ok(&d.check("read")));
    }

    #[test]
    fn detects_warning_before_critical() {
        // With dual thresholds: warn=3, crit=5
        // count >= warning_threshold triggers Warning, count >= critical triggers Critical
        let mut d = LoopDetector::with_dual_thresholds(10, 3, 5);
        assert!(is_ok(&d.check("read")));     // count=1
        assert!(is_ok(&d.check("read")));     // count=2
        assert!(is_warning(&d.check("read"))); // count=3 >= warn(3)
        assert!(is_warning(&d.check("read"))); // count=4
        assert!(is_critical(&d.check("read"))); // count=5 >= crit(5)
    }

    #[test]
    fn single_threshold_constructor_sets_critical_above() {
        // LoopDetector::new with threshold=3 sets warning=3, critical=4
        let mut d = LoopDetector::new(10, 3);
        assert!(is_ok(&d.check("exec")));      // count=1
        assert!(is_ok(&d.check("exec")));      // count=2
        assert!(is_warning(&d.check("exec"))); // count=3 >= warn(3)
        assert!(is_critical(&d.check("exec"))); // count=4 >= crit(4)
    }

    #[test]
    fn default_has_warning_at_10_critical_at_20() {
        let mut d = LoopDetector::default();
        // 10th call hits warning threshold
        for i in 0..9 {
            assert!(is_ok(&d.check("exec")), "call {} should be ok", i + 1);
        }
        assert!(is_warning(&d.check("exec")), "10th call should be warning");
        // 20th call hits critical threshold
        for i in 10..19 {
            assert!(
                is_warning(&d.check("exec")),
                "call {} should be warning",
                i + 1
            );
        }
        assert!(
            is_critical(&d.check("exec")),
            "20th call should be critical"
        );
    }

    #[test]
    fn custom_override_takes_priority() {
        let mut overrides = HashMap::new();
        overrides.insert("my_tool".into(), (2, 3)); // warn=2, crit=3
        let mut d = LoopDetector::with_overrides(10, 10, 20, overrides);
        assert!(is_ok(&d.check("my_tool")));
        assert!(is_warning(&d.check("my_tool"))); // 2nd = warn
        assert!(is_critical(&d.check("my_tool"))); // 3rd = crit
    }

    #[test]
    fn window_slides_correctly() {
        let mut d = LoopDetector::with_dual_thresholds(4, 3, 5);
        assert!(is_ok(&d.check("a")));
        assert!(is_ok(&d.check("b")));
        assert!(is_ok(&d.check("a")));
        assert!(is_ok(&d.check("b")));
        // window=[a,b,a,b]. "a" appears 2 times, not 3.
        assert!(is_ok(&d.check("a")));
        // window=[a,b,a,a] -> a appears 3 times -> warning.
        assert!(is_warning(&d.check("a")));
    }

    #[test]
    fn reset_clears_loop_state() {
        let mut d = LoopDetector::with_dual_thresholds(10, 3, 5);
        assert!(is_ok(&d.check("read")));
        assert!(is_ok(&d.check("read")));
        assert!(is_warning(&d.check("read")));
        d.reset();
        assert!(is_ok(&d.check("read")), "after reset, should be ok");
    }

    #[test]
    fn warning_message_contains_info() {
        let mut d = LoopDetector::with_dual_thresholds(10, 3, 5);
        for _ in 0..3 {
            d.check("exec");
        }
        let result = d.check("exec");
        if let LoopCheckResult::Warning {
            tool_name,
            count,
            message,
        } = result
        {
            assert_eq!(tool_name, "exec");
            assert_eq!(count, 4);
            assert!(message.contains("exec"));
        } else {
            panic!("expected Warning, got {:?}", result);
        }
    }
}
