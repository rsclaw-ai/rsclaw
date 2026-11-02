//! Compaction shaped by the model doing the work.
//!
//! One set of numbers doesn't fit every engine: a 32k local model needs to
//! compact early, keep fewer turns verbatim and get a short summary
//! template, while a 128k fleet model can carry five recent turns and a
//! full template. Each knob here derives from the model's window and
//! output limit (see `model_resolution::model_limits`); explicit
//! `compaction` config always wins over the derived value.

use crate::model_resolution::ModelLimits;

/// At or below this window a summary gets the short template and fewer
/// verbatim turns.
pub const SMALL_WINDOW: usize = 32_768;

/// Headroom kept below the window for the model's reply: its output limit
/// (else 5% of the window), at least 2k and at most a quarter of the
/// window.
pub fn reply_reserve(limits: ModelLimits, window: usize) -> usize {
    let want = limits.max_output.unwrap_or(window / 20).max(2_000);
    want.min((window / 4).max(2_000))
}

/// Recent user/assistant pairs kept verbatim when nothing is configured.
pub fn default_keep_recent_pairs(window: usize) -> usize {
    match window {
        w if w <= SMALL_WINDOW => 2,
        w if w <= 65_536 => 3,
        _ => 5,
    }
}

/// Transcript budget for a standalone summary call when nothing is
/// configured: most of the summarizing model's own window, the rest left
/// for the instruction and the summary.
pub fn default_transcript_tokens(summarizer_window: usize) -> usize {
    (summarizer_window * 6 / 10).max(2_000)
}

/// Output budget for the summary: the summarizing model's output limit,
/// 2k-8k, and never over a quarter of its window.
pub fn summary_max_tokens(limits: ModelLimits, summarizer_window: usize) -> u32 {
    let want = limits.max_output.unwrap_or(4_096).clamp(2_048, 8_192);
    want.min((summarizer_window / 4).max(1_024)) as u32
}

/// Whether the summary should use the short template.
pub fn uses_short_template(summarizer_window: usize) -> bool {
    summarizer_window <= SMALL_WINDOW
}

#[cfg(test)]
mod tests {
    use super::*;

    fn limits(window: Option<usize>, out: Option<usize>) -> ModelLimits {
        ModelLimits { context_window: window, max_output: out }
    }

    #[test]
    fn compaction_profile_follows_the_model() {
        // The fleet model (no catalog entry, 128k): today's numbers.
        let fleet = limits(None, None);
        assert_eq!(reply_reserve(fleet, 128_000), 6_400);
        assert_eq!(default_keep_recent_pairs(128_000), 5);
        assert_eq!(summary_max_tokens(fleet, 128_000), 4_096);
        assert!(!uses_short_template(128_000));
        // A 32k local model: tighter everywhere.
        let local = limits(Some(32_768), Some(8_192));
        assert_eq!(reply_reserve(local, 32_768), 8_192);
        assert_eq!(default_keep_recent_pairs(32_768), 2);
        assert_eq!(summary_max_tokens(local, 32_768), 8_192);
        assert!(uses_short_template(32_768));
        // An 8k model: the reserve and summary can't eat the window.
        let tiny = limits(Some(8_192), Some(8_192));
        assert_eq!(reply_reserve(tiny, 8_192), 2_048);
        assert_eq!(summary_max_tokens(tiny, 8_192), 2_048);
        assert_eq!(default_transcript_tokens(8_192), 4_915);
        // A big-output model gets a longer summary, capped.
        assert_eq!(summary_max_tokens(limits(None, Some(65_536)), 128_000), 8_192);
    }
}
