//! Tool call loop detection (AGENTS.md §20).
//!
//! Uses a sliding window over recent tool calls.
//! Per-tool thresholds allow different limits for different tools.
//!
//! Distinguishes between WARNING (model notified, execution continues)
//! and CRITICAL (execution blocked), matching OpenClaw behavior.
//!
//! Hashes tool name + params (like OpenClaw's hashToolCall) so
//! different arguments are treated as different calls.

use std::collections::{HashMap, VecDeque};

/// Default sliding-window size.
const DEFAULT_WINDOW: usize = 25;
/// Default warning threshold — generic loops trigger warning at this count.
const DEFAULT_WARNING_THRESHOLD: usize = 5;
/// Default critical threshold — loops at this count are blocked.
const DEFAULT_CRITICAL_THRESHOLD: usize = 10;

/// Built-in per-tool threshold overrides.
fn builtin_overrides() -> HashMap<String, (usize, usize)> {
    HashMap::new()
}

/// Hash tool name + params for loop detection (matches OpenClaw's hashToolCall).
pub fn hash_tool_call(tool_name: &str, params: &serde_json::Value) -> String {
    let stable = stable_stringify(params);
    // Use a simple hash (not SHA256) for speed - we only need uniqueness within a session
    let hash = simple_hash(&stable);
    format!("{tool_name}:{hash}")
}

/// Stable JSON stringify with sorted keys (matches OpenClaw's stableStringify).
fn stable_stringify(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::Null => "null".to_string(),
        serde_json::Value::Bool(b) => b.to_string(),
        serde_json::Value::Number(n) => n.to_string(),
        serde_json::Value::String(s) => format!("\"{}\"", escape_json_string(s)),
        serde_json::Value::Array(arr) => {
            format!("[{}]", arr.iter().map(stable_stringify).collect::<Vec<_>>().join(","))
        }
        serde_json::Value::Object(obj) => {
            let keys: Vec<_> = obj.keys().collect();
            let sorted_keys = sort_keys(&keys);
            let entries: Vec<String> = sorted_keys
                .iter()
                .map(|k| {
                    let v = obj.get(*k).unwrap_or(&serde_json::Value::Null);
                    format!("\"{}\":{}", escape_json_string(k), stable_stringify(v))
                })
                .collect();
            format!("{{{}}}", entries.join(","))
        }
    }
}

fn escape_json_string(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
        .replace('\r', "\\r")
        .replace('\t', "\\t")
}

fn sort_keys<'a>(keys: &[&'a String]) -> Vec<&'a String> {
    let mut sorted = keys.to_vec();
    sorted.sort();
    sorted
}

/// Simple hash function for loop detection (fast, in-memory).
fn simple_hash(s: &str) -> u64 {
    // FNV-1a hash with wrapping multiplication to avoid overflow
    let mut hash: u64 = 0xcbf29ce484222325;
    for byte in s.bytes() {
        hash ^= byte as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
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

/// A record of a tool call in history.
#[derive(Debug, Clone)]
pub struct ToolCallRecord {
    pub tool_name: String,
    pub args_hash: String,
    /// Hash of the result (for no-progress detection).
    pub result_hash: Option<String>,
}

/// Max consecutive failures with IDENTICAL error for the same tool before blocking.
/// Gives LLM enough room for iterative debugging (5 "fix and retry" cycles) while
/// still catching genuine dead-ends.
const MAX_SAME_ERROR_STREAK: usize = 5;

/// Fallback: max consecutive failures of ANY kind for the same tool before blocking.
/// Covers superficially-varying errors that still mean "stuck". A bit more lenient
/// than same-error since errors do genuinely differ during normal debugging.
const MAX_ANY_FAILURE_STREAK: usize = 8;

/// Normalize an error message so superficial differences (line:col, timestamps,
/// line numbers) don't produce different hashes, WITHOUT collapsing short
/// meaningful numbers like exit codes ("exit 1" vs "exit 127" must stay distinct).
///
/// Rule: only digit-runs of length ≥ 3 are replaced with "N". Line/column
/// numbers almost always hit that threshold once messages include ~3 digits
/// somewhere; single- and two-digit numbers (exit codes, version majors) are
/// preserved.
fn normalize_error(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut run = String::new();
    let flush = |run: &mut String, out: &mut String| {
        if run.len() >= 3 {
            out.push('N');
        } else {
            out.push_str(run);
        }
        run.clear();
    };
    for c in s.chars() {
        if c.is_ascii_digit() {
            run.push(c);
        } else {
            flush(&mut run, &mut out);
            out.push(c);
        }
    }
    flush(&mut run, &mut out);
    // Collapse whitespace.
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[derive(Debug, Clone)]
pub struct LoopDetector {
    window: usize,
    warning_threshold: usize,
    critical_threshold: usize,
    overrides: HashMap<String, (usize, usize)>, // (warning, critical) per-tool
    /// History of tool call records with args_hash and result_hash.
    history: VecDeque<ToolCallRecord>,
    /// Per-tool streak of identical errors. Keyed by tool_name.
    /// Value: (error_hash, count). Reset when a different error OR success appears.
    error_streak: HashMap<String, (String, usize)>,
    /// Per-tool streak of ANY failures (regardless of error). Catches superficially
    /// varying errors that still mean the same thing. Reset on success.
    any_failure_streak: HashMap<String, usize>,
}

/// Inspect a tool result value and decide if it represents a failure.
fn is_result_failure(result: &serde_json::Value) -> bool {
    // exec-style: exit_code != 0
    if let Some(code) = result.get("exit_code").and_then(|v| v.as_i64()) {
        if code != 0 {
            return true;
        }
    }
    // Error field with non-empty string
    if let Some(err) = result.get("error").and_then(|v| v.as_str()) {
        if !err.is_empty() {
            return true;
        }
    }
    // Explicit success=false / ok=false
    if result.get("success").and_then(|v| v.as_bool()) == Some(false) {
        return true;
    }
    if result.get("ok").and_then(|v| v.as_bool()) == Some(false) {
        return true;
    }
    false
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
            error_streak: HashMap::new(),
            any_failure_streak: HashMap::new(),
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
            error_streak: HashMap::new(),
            any_failure_streak: HashMap::new(),
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

    /// Record a tool call with full params hash (OpenClaw-compatible).
    ///
    /// Returns `LoopCheckResult`:
    /// - `Ok` → proceed normally
    /// - `Warning` → model is notified, execution continues (generic repeat)
    /// - `Critical` → execution blocked (excessive repeats or circuit breaker)
    ///
    /// Progress detection: same args + different results = making progress.
    /// Only count as "loop" when same args AND same results (no progress).
    pub fn check_with_params(&mut self, tool_name: &str, params: &serde_json::Value) -> LoopCheckResult {
        let args_hash = hash_tool_call(tool_name, params);

        // Add to history (result_hash will be set later via record_result)
        self.history.push_back(ToolCallRecord {
            tool_name: tool_name.to_owned(),
            args_hash: args_hash.clone(),
            result_hash: None,
        });
        if self.history.len() > self.window {
            self.history.pop_front();
        }

        // Progress-aware loop detection:
        // Count only calls where same args AND same result (no progress).
        // Different results = making progress, don't count as loop.
        let same_args_records: Vec<_> = self
            .history
            .iter()
            .filter(|r| r.args_hash == args_hash)
            .collect();

        // Check if there's progress: different result_hash values among same args calls.
        let result_hashes: Vec<_> = same_args_records
            .iter()
            .filter_map(|r| r.result_hash.as_ref())
            .collect();

        let has_progress = result_hashes.len() >= 2 && {
            // If we have at least 2 different result_hash values, there's progress.
            let first = result_hashes.first();
            result_hashes.iter().any(|h| h != first.unwrap())
        };

        // Count for loop detection:
        // - If progress detected (different results), only count calls with no result_hash yet
        //   (these are pending calls that haven't finished, might be making progress)
        // - If no progress detected (same results or all pending), count all same args calls
        let count = if has_progress {
            // Making progress: only count pending calls (result_hash = None)
            same_args_records
                .iter()
                .filter(|r| r.result_hash.is_none())
                .count()
        } else {
            // No progress detected: count all same args calls
            same_args_records.len()
        };

        // Second axis: same tool repeatedly producing the same (normalized) error.
        // Catches "LLM retrying syntactically-different-but-equally-broken variants".
        if let Some((err_hash, streak)) = self.error_streak.get(tool_name) {
            if *streak >= MAX_SAME_ERROR_STREAK {
                return LoopCheckResult::Critical {
                    tool_name: tool_name.to_owned(),
                    count: *streak,
                    message: format!(
                        "CRITICAL: tool `{tool_name}` returned the same (normalized) error {streak} times in a row \
                         (error hash {err_hash}). Different arguments, same failure — the approach \
                         is wrong. Stop and report the problem to the user.",
                    ),
                };
            }
        }
        // Third axis: any-failure streak fallback — catches errors that differ
        // in surface form but are still repeated failures on the same tool.
        if let Some(streak) = self.any_failure_streak.get(tool_name) {
            if *streak >= MAX_ANY_FAILURE_STREAK {
                return LoopCheckResult::Critical {
                    tool_name: tool_name.to_owned(),
                    count: *streak,
                    message: format!(
                        "CRITICAL: tool `{tool_name}` failed {streak} times consecutively with no success. \
                         The approach is stuck. Stop and report the problem to the user.",
                    ),
                };
            }
        }

        let (warning_threshold, critical_threshold) = self.thresholds_for(tool_name);

        // Critical threshold — blocks execution
        if count >= critical_threshold {
            return LoopCheckResult::Critical {
                tool_name: tool_name.to_owned(),
                count,
                message: format!(
                    "CRITICAL: tool `{tool_name}` called {count} times in the last {} calls with identical arguments and results. \
                     No progress detected. Session execution blocked to prevent runaway loops.",
                    self.history.len(),
                ),
            };
        }

        // Warning threshold — model is notified but execution continues
        if count >= warning_threshold {
            return LoopCheckResult::Warning {
                tool_name: tool_name.to_owned(),
                count,
                message: format!(
                    "WARNING: You have called `{tool_name}` {count} times in the last {} \
                     calls with identical arguments and results. If this is not making progress, \
                     stop retrying and report the task as failed.",
                    self.history.len(),
                ),
            };
        }

        LoopCheckResult::Ok
    }

    /// Record a tool call and check for loops (legacy API - only uses tool_name).
    ///
    /// This is a backwards-compat wrapper that constructs an empty params value.
    /// Prefer `check_with_params` for proper argument hashing.
    pub fn check(&mut self, tool_name: &str) -> LoopCheckResult {
        self.check_with_params(tool_name, &serde_json::Value::Object(serde_json::Map::new()))
    }

    /// Record the result hash for the most recent tool call.
    /// Used for no-progress detection (same call, same result = stuck).
    /// Also maintains the per-tool error_streak for the second-axis loop check.
    pub fn record_result(&mut self, result: &serde_json::Value) {
        // Capture the tool name before the mutable borrow below.
        let tool_name = self.history.back().map(|r| r.tool_name.clone());

        if let Some(last) = self.history.back_mut() {
            let result_str = stable_stringify(result);
            last.result_hash = Some(format!("{}", simple_hash(&result_str)));
        }

        let Some(name) = tool_name else { return };

        let failure = is_result_failure(result);
        if failure {
            // Normalize the error signature — strip line:col, numeric suffixes.
            let raw_sig = result
                .get("error")
                .and_then(|v| v.as_str())
                .map(String::from)
                .or_else(|| result.get("stderr").and_then(|v| v.as_str()).map(String::from))
                .unwrap_or_else(|| stable_stringify(result));
            let err_sig = normalize_error(&raw_sig);
            let err_hash = format!("{}", simple_hash(&err_sig));

            self.error_streak
                .entry(name.clone())
                .and_modify(|(h, c)| {
                    if *h == err_hash {
                        *c += 1;
                    } else {
                        *h = err_hash.clone();
                        *c = 1;
                    }
                })
                .or_insert((err_hash, 1));
            // Increment any-failure streak too.
            *self.any_failure_streak.entry(name.clone()).or_insert(0) += 1;

        } else {
            // Success clears both streaks for this tool.
            self.error_streak.remove(&name);
            self.any_failure_streak.remove(&name);
        }
    }

    /// Reset the history (e.g. after a tool successfully produces new output).
    pub fn reset(&mut self) {
        self.history.clear();
        self.error_streak.clear();
        self.any_failure_streak.clear();
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
    fn default_has_warning_at_5_critical_at_10() {
        let mut d = LoopDetector::default();
        // 5th call hits warning threshold
        for i in 0..4 {
            assert!(is_ok(&d.check("exec")), "call {} should be ok", i + 1);
        }
        assert!(is_warning(&d.check("exec")), "5th call should be warning");
        // 10th call hits critical threshold
        for i in 5..9 {
            assert!(
                is_warning(&d.check("exec")),
                "call {} should be warning",
                i + 1
            );
        }
        assert!(
            is_critical(&d.check("exec")),
            "10th call should be critical"
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

    #[test]
    fn different_params_count_as_different_calls() {
        let mut d = LoopDetector::with_dual_thresholds(10, 3, 5);
        let params_a = serde_json::json!({"command": "ls"});
        let params_b = serde_json::json!({"command": "pwd"});

        // These should be counted separately since params differ
        assert!(is_ok(&d.check_with_params("exec", &params_a)));
        assert!(is_ok(&d.check_with_params("exec", &params_b)));
        assert!(is_ok(&d.check_with_params("exec", &params_a)));
        // params_a appears 2 times, params_b appears 1 time - no warning
        assert!(is_ok(&d.check_with_params("exec", &params_b)));
    }

    #[test]
    fn same_params_trigger_warning() {
        let mut d = LoopDetector::with_dual_thresholds(10, 3, 5);
        let params = serde_json::json!({"command": "ls -la"});

        assert!(is_ok(&d.check_with_params("exec", &params)));       // count=1
        assert!(is_ok(&d.check_with_params("exec", &params)));       // count=2
        assert!(is_warning(&d.check_with_params("exec", &params)));  // count=3 >= warn(3)
        assert!(is_warning(&d.check_with_params("exec", &params)));  // count=4
        assert!(is_critical(&d.check_with_params("exec", &params))); // count=5 >= crit(5)
    }

    #[test]
    fn hash_tool_call_includes_params() {
        let params_a = serde_json::json!({"command": "ls"});
        let params_b = serde_json::json!({"command": "pwd"});
        let hash_a = hash_tool_call("exec", &params_a);
        let hash_b = hash_tool_call("exec", &params_b);
        // Different params should produce different hashes
        assert_ne!(hash_a, hash_b);
        // Same params should produce same hash
        let hash_a2 = hash_tool_call("exec", &params_a);
        assert_eq!(hash_a, hash_a2);
    }

    #[test]
    fn stable_stringify_sorts_keys() {
        let obj1 = serde_json::json!({"b": 2, "a": 1});
        let obj2 = serde_json::json!({"a": 1, "b": 2});
        // Different key order should produce same hash
        let hash1 = simple_hash(&stable_stringify(&obj1));
        let hash2 = simple_hash(&stable_stringify(&obj2));
        assert_eq!(hash1, hash2);
    }

    // ---------------------------------------------------------------------------
    // Progress detection tests
    // ---------------------------------------------------------------------------

    #[test]
    fn different_results_means_progress() {
        // Same params but different results = making progress, should NOT trigger loop.
        let mut d = LoopDetector::with_dual_thresholds(10, 3, 5);
        let params = serde_json::json!({"command": "ls"});

        // Call 1: check, then record result
        assert!(is_ok(&d.check_with_params("exec", &params)));
        d.record_result(&serde_json::json!({"stdout": "file1.txt"}));

        // Call 2: same params, different result = progress
        assert!(is_ok(&d.check_with_params("exec", &params)));
        d.record_result(&serde_json::json!({"stdout": "file1.txt file2.txt"}));

        // Call 3: still progressing
        assert!(is_ok(&d.check_with_params("exec", &params)));
        d.record_result(&serde_json::json!({"stdout": "file1.txt file2.txt file3.txt"}));

        // Call 4: even after many calls with same params, different results mean progress
        assert!(is_ok(&d.check_with_params("exec", &params)));
        d.record_result(&serde_json::json!({"stdout": "file1.txt file2.txt file3.txt file4.txt"}));

        // Should still be OK - no loop detected because results are changing
        assert!(is_ok(&d.check_with_params("exec", &params)));
    }

    #[test]
    fn same_results_means_no_progress() {
        // Same params AND same results = no progress, should trigger loop.
        let mut d = LoopDetector::with_dual_thresholds(10, 3, 5);
        let params = serde_json::json!({"command": "ls"});

        // Call 1-2: same params, same result = stuck
        assert!(is_ok(&d.check_with_params("exec", &params)));       // count=1
        d.record_result(&serde_json::json!({"stdout": "same_output"}));

        assert!(is_ok(&d.check_with_params("exec", &params)));       // count=2
        d.record_result(&serde_json::json!({"stdout": "same_output"}));

        // Call 3: count=3 >= warn(3) = warning
        assert!(is_warning(&d.check_with_params("exec", &params)));
        d.record_result(&serde_json::json!({"stdout": "same_output"}));

        // Call 4: still warning
        assert!(is_warning(&d.check_with_params("exec", &params)));
        d.record_result(&serde_json::json!({"stdout": "same_output"}));

        // Call 5: count=5 >= crit(5) = critical
        assert!(is_critical(&d.check_with_params("exec", &params)));
    }

    #[test]
    fn mixed_results_progres_detection() {
        // Some same results, some different = still considered progress.
        // Use warn=4, crit=6 to allow enough calls before progress kicks in.
        let mut d = LoopDetector::with_dual_thresholds(10, 4, 6);
        let params = serde_json::json!({"command": "ls"});

        // Call 1: initial
        assert!(is_ok(&d.check_with_params("exec", &params)));
        d.record_result(&serde_json::json!({"stdout": "a"}));

        // Call 2: same result as call 1
        assert!(is_ok(&d.check_with_params("exec", &params)));
        d.record_result(&serde_json::json!({"stdout": "a"}));

        // Call 3: different result = progress detected
        assert!(is_ok(&d.check_with_params("exec", &params)));
        d.record_result(&serde_json::json!({"stdout": "b"}));

        // Call 4: after progress detected, should not count as loop
        // (only pending calls with result_hash=None are counted)
        assert!(is_ok(&d.check_with_params("exec", &params)));
        d.record_result(&serde_json::json!({"stdout": "c"}));

        // Many more calls with different results - no loop
        for i in 0..20 {
            assert!(is_ok(&d.check_with_params("exec", &params)));
            d.record_result(&serde_json::json!({"stdout": format!("result_{}", i)}));
        }
    }

    #[test]
    fn no_result_hash_yet_counts_as_potential_loop() {
        // When result_hash is None (call hasn't finished), count it as potential loop.
        let mut d = LoopDetector::with_dual_thresholds(10, 3, 5);
        let params = serde_json::json!({"command": "ls"});

        // Call without recording result
        assert!(is_ok(&d.check_with_params("exec", &params)));
        // Don't call record_result

        // Another call (previous still has result_hash=None)
        assert!(is_ok(&d.check_with_params("exec", &params)));       // count=2
        assert!(is_warning(&d.check_with_params("exec", &params)));  // count=3 >= warn(3)
        assert!(is_warning(&d.check_with_params("exec", &params)));  // count=4
        assert!(is_critical(&d.check_with_params("exec", &params))); // count=5 >= crit(5)
    }
}
