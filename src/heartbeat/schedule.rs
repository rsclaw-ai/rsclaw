//! Adaptive scheduling utilities for the heartbeat engine.
//!
//! Provides:
//! - Exponential back-off on consecutive failures (capped at 8×)
//! - Active-hours window checking (normal and overnight windows)
//! - Restart-safe startup delay that respects the last-run timestamp

use chrono::{DateTime, NaiveTime, Timelike, Utc};
use chrono_tz::Tz;
use std::time::Duration;

/// Return the back-off interval for the given number of consecutive failures.
///
/// Formula: `base * 2^consecutive_failures`, capped at `base * 8`.
///
/// | failures | multiplier |
/// |----------|-----------|
/// | 0        | 1×        |
/// | 1        | 2×        |
/// | 2        | 4×        |
/// | 3+       | 8× (cap)  |
pub fn backoff_interval(base: Duration, consecutive_failures: u32) -> Duration {
    let exponent = consecutive_failures.min(3); // 2^3 = 8 — the cap
    base * (1u32 << exponent)
}

/// Check whether the current moment falls inside the configured active-hours window.
///
/// Returns:
/// - `None`  — proceed immediately (no window configured, or we are inside the window)
/// - `Some(sleep_duration)` — caller should sleep for this long before proceeding
///
/// Supports both normal windows (e.g. 09:15–15:05) and overnight windows
/// (e.g. 22:00–06:00 where end < start).
pub fn check_active_hours(
    active_hours: Option<(NaiveTime, NaiveTime)>,
    tz: Tz,
) -> Option<Duration> {
    let (window_start, window_end) = active_hours?;

    let now_tz = Utc::now().with_timezone(&tz);
    let now_time = now_tz.time();

    let inside = if window_start <= window_end {
        // Normal window: e.g. 09:15 – 15:05
        now_time >= window_start && now_time < window_end
    } else {
        // Overnight window: e.g. 22:00 – 06:00
        now_time >= window_start || now_time < window_end
    };

    if inside {
        return None; // We are active — proceed
    }

    // Calculate how long until the next window_start.
    let secs_until = if now_time < window_start {
        // Window starts later today
        let delta = window_start - now_time;
        delta.num_seconds()
    } else {
        // Window starts tomorrow
        let secs_left_today =
            86_400i64 - now_time.num_seconds_from_midnight() as i64;
        let secs_from_midnight = window_start.num_seconds_from_midnight() as i64;
        secs_left_today + secs_from_midnight
    };

    let secs_until = secs_until.max(0) as u64;
    Some(Duration::from_secs(secs_until))
}

/// Calculate a safe startup delay so that the heartbeat doesn't fire too soon
/// after a process restart.
///
/// Rules:
/// 1. No previous run recorded → return a 30 s warm-up delay.
/// 2. Last run was within one `interval` ago → sleep the remaining portion.
/// 3. Last run was more than one `interval` ago → return a 30 s warm-up delay
///    (we are already overdue; a short warm-up avoids a thundering herd on
///    cluster-wide restarts while still being prompt).
pub fn startup_delay(interval: Duration, last_run_at: Option<DateTime<Utc>>) -> Duration {
    const WARMUP: Duration = Duration::from_secs(30);

    let Some(last) = last_run_at else {
        return WARMUP;
    };

    let elapsed = Utc::now()
        .signed_duration_since(last)
        .to_std()
        .unwrap_or(Duration::ZERO);

    if elapsed < interval {
        interval - elapsed
    } else {
        WARMUP
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration as CDuration;

    // --- backoff_interval ---------------------------------------------------

    #[test]
    fn backoff_no_failures() {
        let base = Duration::from_secs(60);
        assert_eq!(backoff_interval(base, 0), Duration::from_secs(60));
    }

    #[test]
    fn backoff_one_failure_doubles() {
        let base = Duration::from_secs(60);
        assert_eq!(backoff_interval(base, 1), Duration::from_secs(120));
    }

    #[test]
    fn backoff_caps_at_8x() {
        let base = Duration::from_secs(60);
        // 3 failures → 8×, 10 failures → still 8×
        assert_eq!(backoff_interval(base, 3), Duration::from_secs(480));
        assert_eq!(backoff_interval(base, 10), Duration::from_secs(480));
    }

    // --- check_active_hours -------------------------------------------------

    #[test]
    fn active_hours_none_always_active() {
        // No window configured → always active → returns None
        let result = check_active_hours(None, chrono_tz::UTC);
        assert!(result.is_none());
    }

    // --- startup_delay ------------------------------------------------------

    #[test]
    fn startup_delay_no_last_run() {
        let interval = Duration::from_secs(300);
        assert_eq!(startup_delay(interval, None), Duration::from_secs(30));
    }

    #[test]
    fn startup_delay_recent_run() {
        let interval = Duration::from_secs(300);
        // Last run was 60 s ago → remaining = 300 - 60 = 240 s
        let last = Utc::now() - CDuration::seconds(60);
        let delay = startup_delay(interval, Some(last));
        // Allow ±2 s for test execution time
        assert!(
            delay >= Duration::from_secs(238) && delay <= Duration::from_secs(242),
            "expected ~240 s, got {delay:?}"
        );
    }

    #[test]
    fn startup_delay_old_run() {
        let interval = Duration::from_secs(300);
        // Last run was 600 s ago → overdue → warm-up
        let last = Utc::now() - CDuration::seconds(600);
        assert_eq!(startup_delay(interval, Some(last)), Duration::from_secs(30));
    }
}
