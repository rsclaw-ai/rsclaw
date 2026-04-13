pub mod schedule;
pub mod state;

use anyhow::{anyhow, bail, Result};
use chrono::NaiveTime;
use chrono_tz::Tz;
use std::time::Duration;

/// Parsed representation of a HEARTBEAT.md file.
#[derive(Debug, Clone)]
pub struct HeartbeatSpec {
    pub every: Duration,
    pub active_hours: Option<(NaiveTime, NaiveTime)>,
    pub timezone: Tz,
    pub content: String,
}

/// Parse a HEARTBEAT.md string (frontmatter + body) into a [`HeartbeatSpec`].
pub fn parse_heartbeat_md(raw: &str) -> Result<HeartbeatSpec> {
    // Must start with "---"
    let rest = raw
        .strip_prefix("---")
        .ok_or_else(|| anyhow!("HEARTBEAT.md must begin with a '---' frontmatter block"))?;

    // The first character after "---" must be a newline (or the line ends immediately)
    let rest = if rest.starts_with('\n') {
        &rest[1..]
    } else if rest.starts_with("\r\n") {
        &rest[2..]
    } else {
        bail!("HEARTBEAT.md must begin with a '---' frontmatter block");
    };

    // Find the closing "---"
    let closing = rest
        .find("\n---")
        .ok_or_else(|| anyhow!("HEARTBEAT.md frontmatter is not closed with '---'"))?;

    let fm_text = &rest[..closing];
    let after_closing = &rest[closing + 4..]; // skip "\n---"
    let content = if after_closing.starts_with('\n') {
        after_closing[1..].to_string()
    } else if after_closing.starts_with("\r\n") {
        after_closing[2..].to_string()
    } else {
        after_closing.to_string()
    };

    // Parse frontmatter key-value pairs (simple "key: value" lines)
    let mut every_raw: Option<String> = None;
    let mut active_hours_raw: Option<String> = None;
    let mut timezone_raw: Option<String> = None;

    for line in fm_text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some((key, val)) = line.split_once(':') {
            let key = key.trim();
            let val = val.trim().to_string();
            match key {
                "every" => every_raw = Some(val),
                "active_hours" => active_hours_raw = Some(val),
                "timezone" | "tz" => timezone_raw = Some(val),
                _ => {} // ignore unknown keys
            }
        }
    }

    let every_str = every_raw
        .ok_or_else(|| anyhow!("HEARTBEAT.md frontmatter is missing required field 'every'"))?;
    let every = parse_duration(&every_str);

    let active_hours = active_hours_raw
        .as_deref()
        .map(parse_time_range)
        .transpose()?;

    let timezone: Tz = match timezone_raw.as_deref() {
        Some(tz_str) => tz_str
            .parse()
            .map_err(|_| anyhow!("Unknown timezone: '{}'", tz_str))?,
        None => chrono_tz::Asia::Shanghai,
    };

    Ok(HeartbeatSpec {
        every,
        active_hours,
        timezone,
        content,
    })
}

/// Parse a human-readable duration string into [`std::time::Duration`].
///
/// Supported forms: `"5m"`, `"30m"`, `"1h"`, `"30s"`, bare integer (treated as minutes).
fn parse_duration(s: &str) -> Duration {
    let s = s.trim();
    if let Some(mins) = s.strip_suffix('m') {
        if let Ok(n) = mins.parse::<u64>() {
            return Duration::from_secs(n * 60);
        }
    }
    if let Some(hours) = s.strip_suffix('h') {
        if let Ok(n) = hours.parse::<u64>() {
            return Duration::from_secs(n * 3600);
        }
    }
    if let Some(secs) = s.strip_suffix('s') {
        if let Ok(n) = secs.parse::<u64>() {
            return Duration::from_secs(n);
        }
    }
    // Bare number → minutes
    if let Ok(n) = s.parse::<u64>() {
        return Duration::from_secs(n * 60);
    }
    // Fallback: zero (shouldn't happen in practice)
    Duration::ZERO
}

/// Parse a time range string of the form `"HH:MM-HH:MM"`.
fn parse_time_range(s: &str) -> Result<(NaiveTime, NaiveTime)> {
    let (start_str, end_str) = s
        .split_once('-')
        .ok_or_else(|| anyhow!("active_hours must be in 'HH:MM-HH:MM' format, got '{}'", s))?;

    let start = NaiveTime::parse_from_str(start_str.trim(), "%H:%M")
        .map_err(|e| anyhow!("Invalid start time '{}': {}", start_str.trim(), e))?;
    let end = NaiveTime::parse_from_str(end_str.trim(), "%H:%M")
        .map_err(|e| anyhow!("Invalid end time '{}': {}", end_str.trim(), e))?;

    Ok((start, end))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn parse_basic_frontmatter() {
        let input = "---\nevery: 30m\n---\nHello world\n";
        let spec = parse_heartbeat_md(input).unwrap();
        assert_eq!(spec.every, Duration::from_secs(30 * 60));
        assert!(spec.active_hours.is_none());
        assert_eq!(spec.timezone, chrono_tz::Asia::Shanghai);
        assert_eq!(spec.content.trim(), "Hello world");
    }

    #[test]
    fn parse_with_active_hours() {
        let input = "---\nevery: 1h\nactive_hours: 09:15-15:05\ntimezone: Asia/Tokyo\n---\nBody text\n";
        let spec = parse_heartbeat_md(input).unwrap();
        assert_eq!(spec.every, Duration::from_secs(3600));
        let (s, e) = spec.active_hours.unwrap();
        assert_eq!(s, NaiveTime::from_hms_opt(9, 15, 0).unwrap());
        assert_eq!(e, NaiveTime::from_hms_opt(15, 5, 0).unwrap());
        assert_eq!(spec.timezone, chrono_tz::Asia::Tokyo);
        assert_eq!(spec.content.trim(), "Body text");
    }

    #[test]
    fn parse_missing_every_fails() {
        let input = "---\nactive_hours: 09:00-17:00\n---\ncontent\n";
        let err = parse_heartbeat_md(input).unwrap_err();
        assert!(err.to_string().contains("every"));
    }

    #[test]
    fn parse_missing_frontmatter_fails() {
        let input = "No frontmatter here\n";
        let err = parse_heartbeat_md(input).unwrap_err();
        assert!(err.to_string().contains("---"));
    }

    #[test]
    fn parse_duration_variants() {
        assert_eq!(parse_duration("5m"), Duration::from_secs(5 * 60));
        assert_eq!(parse_duration("1h"), Duration::from_secs(3600));
        assert_eq!(parse_duration("30s"), Duration::from_secs(30));
        assert_eq!(parse_duration("30"), Duration::from_secs(30 * 60));
    }

    #[test]
    fn parse_time_range_valid() {
        let (s, e) = parse_time_range("09:00-17:30").unwrap();
        assert_eq!(s, NaiveTime::from_hms_opt(9, 0, 0).unwrap());
        assert_eq!(e, NaiveTime::from_hms_opt(17, 30, 0).unwrap());
    }

    #[test]
    fn parse_time_range_invalid() {
        assert!(parse_time_range("not-a-time").is_err());
        assert!(parse_time_range("25:00-26:00").is_err());
        assert!(parse_time_range("09:00").is_err());
    }
}
