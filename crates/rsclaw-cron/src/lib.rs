//! Cron job DATA / PERSISTENCE / PURE-COMPUTE layer.
//!
//! This crate holds the serialisable cron data types, the cron-expression
//! pure-compute helpers, and the redb/file persistence layer. The runtime
//! orchestrator (`CronRunner`) stays in the root crate because it is wired
//! to `agent`, `gateway`, `ws`, and channels — the root knot.
//!
//! Schedule format: standard 5-field cron "min hr dom mon dow".
//! Timezone: stored in schedule but currently executes in UTC.

use std::{
    path::PathBuf,
    sync::{Arc, OnceLock},
};

use anyhow::Context;
use chrono::{Datelike, TimeZone, Timelike, Utc};
use rsclaw_config::schema::{CronDelivery, CronJobConfig};
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;
use tracing::{debug, info, trace, warn};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Exponential backoff delays (ms) indexed by consecutive error count.
/// After the last entry the delay stays constant.
const ERROR_BACKOFF_MS: [u64; 5] = [
    30_000,    // 1st error  →  30 seconds
    60_000,    // 2nd error  →  1 minute
    300_000,   // 3rd error  →  5 minutes
    900_000,   // 4th error  →  15 minutes
    3_600_000, // 5th+ error →  60 minutes
];

/// Get backoff delay for consecutive error count.
pub fn error_backoff_ms(consecutive_errors: u32) -> u64 {
    let idx = (consecutive_errors.saturating_sub(1) as usize).min(ERROR_BACKOFF_MS.len() - 1);
    ERROR_BACKOFF_MS[idx]
}

// ---------------------------------------------------------------------------
// CronJob — serialisable description of a single scheduled task
// ---------------------------------------------------------------------------

/// Schedule descriptor — supports both rsclaw flat format and OpenClaw nested
/// format.
///
/// Uses `#[serde(untagged)]` at the top level to distinguish a plain string
/// (Flat) from an object. Object variants use `#[serde(tag = "kind")]`
/// (internally tagged) so that `{"kind": "once", "atMs": ...}` is not
/// accidentally matched by `Every`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum CronSchedule {
    /// Flat string: "*/30 9-11 * * 1-5" (rsclaw native).
    Flat(String),
    /// Object-based schedule with a "kind" discriminator.
    Tagged(CronScheduleTagged),
}

/// Internally tagged schedule object. Discriminated by the `kind` field.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind")]
pub enum CronScheduleTagged {
    /// Cron expression: { kind: "cron", expr: "...", tz: "Asia/Shanghai" }
    /// (OpenClaw compat).
    #[serde(rename = "cron")]
    Nested {
        expr: String,
        #[serde(default)]
        tz: Option<String>,
    },
    /// Interval-based schedule: { kind: "every", everyMs: 259200000, anchorMs:
    /// ... } (OpenClaw compat).
    #[serde(rename = "every")]
    Every {
        #[serde(default, alias = "everyMs")]
        every_ms: Option<u64>,
        #[serde(default, alias = "anchorMs")]
        anchor_ms: Option<u64>,
    },
    /// One-shot schedule: fires once then auto-removes.
    /// { kind: "once", atMs: 1713600000000 } — absolute timestamp
    /// { kind: "once", delayMs: 1200000 }   — relative delay from creation
    #[serde(rename = "once")]
    Once {
        #[serde(default, alias = "atMs")]
        at_ms: Option<u64>,
        #[serde(default, alias = "delayMs")]
        delay_ms: Option<u64>,
    },
}

impl CronSchedule {
    pub fn expr(&self) -> &str {
        match self {
            CronSchedule::Flat(s) => s,
            CronSchedule::Tagged(CronScheduleTagged::Nested { expr, .. }) => expr,
            CronSchedule::Tagged(CronScheduleTagged::Every { .. }) => "every",
            CronSchedule::Tagged(CronScheduleTagged::Once { .. }) => "once",
        }
    }

    /// Human-readable schedule string for display (e.g. "every 30s",
    /// "every 5m", "* * * * *").
    pub fn display(&self) -> String {
        match self {
            CronSchedule::Flat(s) => s.to_owned(),
            CronSchedule::Tagged(CronScheduleTagged::Nested { expr, .. }) => expr.clone(),
            CronSchedule::Tagged(CronScheduleTagged::Every { every_ms, .. }) => {
                if let Some(ms) = every_ms {
                    if *ms > 0 {
                        return format_every_ms(*ms);
                    }
                }
                "every".to_owned()
            }
            CronSchedule::Tagged(CronScheduleTagged::Once { at_ms, delay_ms }) => {
                if let Some(ms) = at_ms {
                    let dt = chrono::DateTime::from_timestamp_millis(*ms as i64)
                        .map(|dt| dt.to_rfc3339())
                        .unwrap_or_else(|| "?".to_owned());
                    return format!("once at {dt}");
                } else if let Some(ms) = delay_ms {
                    format_once_delay(*ms)
                } else {
                    "once".to_owned()
                }
            }
        }
    }

    pub fn tz(&self) -> Option<&str> {
        match self {
            CronSchedule::Flat(_) => None,
            CronSchedule::Tagged(CronScheduleTagged::Nested { tz, .. }) => tz.as_deref(),
            CronSchedule::Tagged(CronScheduleTagged::Every { .. }) => None,
            CronSchedule::Tagged(CronScheduleTagged::Once { .. }) => None,
        }
    }

    /// Whether this is a one-shot schedule (auto-remove after execution).
    pub fn is_once(&self) -> bool {
        matches!(self, CronSchedule::Tagged(CronScheduleTagged::Once { .. }))
    }

    /// Compute the next run timestamp (ms) from the given `from_ms`.
    /// For cron schedules: searches forward up to 1 year.
    /// For interval schedules (every): uses anchor + n*everyMs.
    pub fn compute_next_run(&self, from_ms: u64) -> Option<u64> {
        match self {
            CronSchedule::Flat(expr) => compute_next_run_from_expr(expr, from_ms, None),
            CronSchedule::Tagged(CronScheduleTagged::Nested { expr, tz, .. }) => {
                compute_next_run_from_expr(expr, from_ms, tz.as_deref())
            }
            CronSchedule::Tagged(CronScheduleTagged::Every {
                every_ms,
                anchor_ms,
            }) => {
                let every_ms = every_ms.unwrap_or(0);
                if every_ms == 0 {
                    return None;
                }
                let anchor = anchor_ms.unwrap_or(from_ms);
                // Find smallest n where anchor + n * every_ms > from_ms
                if anchor > from_ms {
                    Some(anchor)
                } else {
                    let elapsed = from_ms - anchor;
                    let n = (elapsed / every_ms) + 1;
                    Some(anchor + n * every_ms)
                }
            }
            CronSchedule::Tagged(CronScheduleTagged::Once { at_ms, delay_ms }) => {
                // Absolute timestamp takes priority over delay.
                if let Some(at) = at_ms {
                    if *at > from_ms { Some(*at) } else { None }
                } else if let Some(delay) = delay_ms {
                    // delay_ms is relative to creation, but compute_next_run
                    // is always called with current time.  The actual fire time
                    // is set in tool_cron when creating the job (createdAtMs + delayMs),
                    // stored as at_ms.  If we reach here, treat from_ms + delay as fallback.
                    let target = from_ms + delay;
                    if target > from_ms { Some(target) } else { None }
                } else {
                    None
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Human-readable schedule formatting
// ---------------------------------------------------------------------------

fn format_every_ms(ms: u64) -> String {
    if ms == 0 {
        return "every".to_owned();
    }
    if ms < 1000 {
        return format!("every {}ms", ms);
    }
    let secs = ms / 1000;
    if secs < 60 {
        return format!("every {}s", secs);
    }
    let mins = secs / 60;
    let rem_secs = secs % 60;
    if mins < 60 {
        if rem_secs == 0 {
            return format!("every {}m", mins);
        }
        return format!("every {}m{}s", mins, rem_secs);
    }
    let hours = mins / 60;
    let rem_mins = mins % 60;
    if hours < 24 {
        if rem_mins == 0 {
            return format!("every {}h", hours);
        }
        return format!("every {}h{}m", hours, rem_mins);
    }
    let days = hours / 24;
    let rem_hours = hours % 24;
    if rem_hours == 0 {
        return format!("every {}d", days);
    }
    format!("every {}d{}h", days, rem_hours)
}

fn format_once_delay(ms: u64) -> String {
    format!("once in {}", format_every_ms(ms).strip_prefix("every ").unwrap_or("?"))
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum CronPayload {
    /// Plain text message.
    Text(String),
    /// Structured payload (OpenClaw compat): { kind: "agentTurn", message:
    /// "...", timeoutSeconds: 1800 }
    Structured {
        #[serde(default, alias = "kind")]
        kind: Option<String>,
        /// Message text - serializes as "message" for openclaw compat, accepts
        /// "text" too
        #[serde(alias = "text", rename = "message", default)]
        text: Option<String>,
        #[serde(default, alias = "timeoutSeconds")]
        timeout_seconds: Option<u64>,
        /// For execCommand: if true, send output to agent for summarization.
        #[serde(default)]
        summarize: Option<bool>,
    },
}

impl CronPayload {
    pub fn text(&self) -> &str {
        match self {
            CronPayload::Text(s) => s,
            CronPayload::Structured { text, .. } => text.as_deref().unwrap_or(""),
        }
    }

    pub fn summarize(&self) -> bool {
        match self {
            CronPayload::Text(_) => false,
            CronPayload::Structured { summarize, .. } => summarize.unwrap_or(false),
        }
    }
}

/// Round-robin cursor for jobs that should iterate over a fixed list each
/// firing (e.g. "查询东京、曼谷、迪拜的天气，每次一个城市"). The cursor is
/// advanced and persisted on every dispatch — so a crash mid-run doesn't
/// repeat the previous item, and the LLM never has to remember progress.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CronIter {
    /// Items to cycle through.
    pub items: Vec<String>,
    /// 0-based index of the item to use on the NEXT firing. Wraps modulo
    /// `items.len()`.
    #[serde(default)]
    pub cursor: usize,
}

/// Persistent run state (OpenClaw compat).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CronJobState {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_run_at_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_run_status: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_status: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_duration_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_delivery_status: Option<String>,
    #[serde(default)]
    pub consecutive_errors: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_run_at_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub running_at_ms: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CronJob {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default)]
    pub agent_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_key: Option<String>,
    pub enabled: bool,
    pub schedule: CronSchedule,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payload: Option<CronPayload>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delivery: Option<CronDelivery>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_target: Option<String>,
    /// Optional runtime behavior. `plugin-preflight:<plugin>:<tool>` calls a
    /// plugin-owned preflight whose JSON result contains boolean `shouldRun`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wake_mode: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state: Option<CronJobState>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub iter: Option<CronIter>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_at_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_at_ms: Option<u64>,
}

impl CronJob {
    pub fn effective_message(&self) -> &str {
        if let Some(ref payload) = self.payload {
            return payload.text();
        }
        self.message.as_deref().unwrap_or("")
    }

    pub fn cron_expr(&self) -> &str {
        self.schedule.expr()
    }

    pub fn timezone(&self) -> Option<&str> {
        self.schedule.tz()
    }

    /// Render the message for THIS firing — substitutes `{current}`, `{next}`,
    /// `{index}` (1-based), and `{total}` from the iter state. Returns the raw
    /// message unchanged when no iter is configured or it's empty.
    pub fn render_message(&self) -> String {
        let raw = self.effective_message();
        let Some(iter) = self.iter.as_ref() else {
            return raw.to_owned();
        };
        if iter.items.is_empty() {
            return raw.to_owned();
        }
        let n = iter.items.len();
        let cur = iter.cursor % n;
        let nxt = (cur + 1) % n;
        raw.replace("{current}", &iter.items[cur])
            .replace("{next}", &iter.items[nxt])
            .replace("{index}", &(cur + 1).to_string())
            .replace("{total}", &n.to_string())
    }

    /// Advance the iter cursor by one (wrap-around). Returns the new cursor
    /// when an iter is configured, None otherwise. Caller persists the store.
    pub fn advance_iter(&mut self) -> Option<usize> {
        let iter = self.iter.as_mut()?;
        if iter.items.is_empty() {
            return None;
        }
        iter.cursor = (iter.cursor + 1) % iter.items.len();
        Some(iter.cursor)
    }

    /// Overwrite the message text for this firing — used after `render_message`
    /// to bake the resolved iter substitution into the dispatched job clone.
    pub fn bake_message(&mut self, text: String) {
        if let Some(payload) = self.payload.as_mut() {
            match payload {
                CronPayload::Text(s) => *s = text,
                CronPayload::Structured { text: t, .. } => *t = Some(text),
            }
        } else {
            self.message = Some(text);
        }
    }
}

impl From<&CronJobConfig> for CronJob {
    fn from(cfg: &CronJobConfig) -> Self {
        let session_key = cfg.session.as_ref().and_then(|v| {
            if let serde_json::Value::String(s) = v {
                Some(s.clone())
            } else {
                None
            }
        });
        let schedule = if let Some(ref tz) = cfg.tz {
            CronSchedule::Tagged(CronScheduleTagged::Nested {
                expr: cfg.schedule.clone(),
                tz: Some(tz.clone()),
            })
        } else {
            CronSchedule::Flat(cfg.schedule.clone())
        };
        Self {
            id: cfg.id.clone(),
            name: cfg.name.clone(),
            agent_id: cfg
                .agent_id
                .clone()
                .unwrap_or_else(|| "default".to_string()),
            session_key,
            enabled: cfg.enabled.unwrap_or(true),
            schedule,
            payload: None,
            message: Some(cfg.message.clone()),
            delivery: cfg.delivery.clone(),
            session_target: None,
            wake_mode: None,
            state: None,
            iter: None,
            created_at_ms: None,
            updated_at_ms: None,
        }
    }
}

// ---------------------------------------------------------------------------
// CronStore — persisted state
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CronStore {
    pub version: u32,
    pub jobs: Vec<CronJob>,
}

impl Default for CronStore {
    fn default() -> Self {
        Self {
            version: 1,
            jobs: Vec::new(),
        }
    }
}

// ---------------------------------------------------------------------------
// RunLogEntry
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RunLogEntry {
    pub id: String,
    pub job_id: String,
    pub started_at: chrono::DateTime<Utc>,
    pub finished_at: Option<chrono::DateTime<Utc>>,
    pub success: bool,
    pub reply_preview: Option<String>,
    pub error: Option<String>,
}

/// True when two CronJobs have identical user-facing configuration.
/// Compared via serde_json::Value so we don't have to derive PartialEq across
/// every nested type.  Strips fields that should NOT count as a meaningful
/// change:
///   - `state`: runtime-only execution state.
///   - `createdAtMs` / `updatedAtMs`: audit timestamps that don't affect
///     execution semantics (and `updatedAtMs` flips on every save).
pub fn cron_jobs_config_equal(a: &CronJob, b: &CronJob) -> bool {
    let mut a_v = match serde_json::to_value(a) {
        Ok(v) => v,
        Err(_) => return false,
    };
    let mut b_v = match serde_json::to_value(b) {
        Ok(v) => v,
        Err(_) => return false,
    };
    for v in [&mut a_v, &mut b_v] {
        if let Some(obj) = v.as_object_mut() {
            obj.remove("state");
            obj.remove("createdAtMs");
            obj.remove("updatedAtMs");
        }
    }
    a_v == b_v
}

// ---------------------------------------------------------------------------
// Cron expression parsing — next-run computation
// ---------------------------------------------------------------------------

/// Parse a cron field value (min/hr/dom/mon/dow) and check if a value matches.
/// Supports: * (any), */n (every n), n (specific), n,m (list).
/// Does NOT support: n-m (range), n/m (step with start).
fn field_matches(field: &str, value: u32) -> bool {
    if field == "*" {
        return true;
    }
    if let Some(step) = field.strip_prefix("*/") {
        if let Ok(n) = step.parse::<u32>() {
            return n > 0 && value % n == 0;
        }
    }
    // Handle comma-separated lists (each part may be a value, range, or step)
    if field.contains(',') {
        return field
            .split(',')
            .any(|part| field_matches(part.trim(), value));
    }
    // Handle range: "9-17" means 9 through 17 inclusive (standard cron semantics)
    if field.contains('-') {
        let parts: Vec<&str> = field.split('-').collect();
        if parts.len() == 2 {
            if let (Ok(start), Ok(end)) = (parts[0].parse::<u32>(), parts[1].parse::<u32>()) {
                return value >= start && value <= end;
            }
        }
    }
    field.parse::<u32>().map(|v| v == value).unwrap_or(false)
}

/// Check if a dow value matches a dow field.
/// Dow ranges use INCLUSIVE end (e.g., "1-5" = 1,2,3,4,5, where 1=Sunday).
/// Same inclusive-end semantics as field_matches.
fn dow_matches(field: &str, dow: u32) -> bool {
    if field == "*" {
        return true;
    }
    if let Some(step) = field.strip_prefix("*/") {
        if let Ok(n) = step.parse::<u32>() {
            return n > 0 && dow % n == 0;
        }
    }
    // Handle comma-separated lists
    if field.contains(',') {
        return field.split(',').any(|part| dow_matches(part.trim(), dow));
    }
    // Dow ranges: inclusive on end (e.g., "1-5" means 1 through 5 inclusive)
    if field.contains('-') {
        let parts: Vec<&str> = field.split('-').collect();
        if parts.len() == 2 {
            if let (Ok(start), Ok(end)) = (parts[0].parse::<u32>(), parts[1].parse::<u32>()) {
                return dow >= start && dow <= end;
            }
        }
    }
    field.parse::<u32>().map(|v| v == dow).unwrap_or(false)
}

/// Compute the next UTC timestamp (ms) when a cron expression should fire,
/// starting from `from_ms`. Returns None if parsing fails.
/// If `tz` is Some, the cron expression is evaluated in that timezone.
/// Otherwise, UTC is used.
pub fn compute_next_run_from_expr(cron_expr: &str, from_ms: u64, tz: Option<&str>) -> Option<u64> {
    let fields: Vec<&str> = cron_expr.split_whitespace().collect();
    if fields.len() != 5 {
        warn!(expr = %cron_expr, "cron: expression must have exactly 5 fields");
        return None;
    }
    let [min_f, hr_f, dom_f, mon_f, dow_f] = fields[..] else {
        return None;
    };

    // Parse from_ms as UTC DateTime
    let utc_dt = match chrono::DateTime::from_timestamp_millis(from_ms as i64) {
        Some(dt) => dt,
        None => return None,
    };

    // Determine timezone
    let tz_opt: Option<chrono_tz::Tz> = tz.and_then(|tz_str| tz_str.parse().ok());

    // Search in local time, always using a timezone-aware DateTime.
    // When no timezone is specified, use the system's local timezone (not UTC).
    let tz_for_search: chrono_tz::Tz = tz_opt.unwrap_or_else(rsclaw_config::system_tz);

    // Current minute in the target timezone
    let local_now = utc_dt.with_timezone(&tz_for_search);
    let mut cand = local_now
        .with_second(0)
        .expect("second 0 always valid")
        .with_nanosecond(0)
        .expect("nanosecond 0 always valid");
    cand += chrono::Duration::minutes(1);

    // Search up to 1 year ahead (in local time).
    let max_cand = cand + chrono::Duration::days(366);

    while cand < max_cand {
        // Use naive date's weekday to get the weekday in local time (not UTC)
        // chrono weekday IS compatible with openclaw dow (both use Sunday=0/1 as the
        // anchor)
        let dow = cand.date_naive().weekday().num_days_from_sunday();
        let m = field_matches(mon_f, cand.month());
        let d = field_matches(dom_f, cand.day());
        let w = dow_matches(dow_f, dow);
        // Optimization: if the date fields don't match, skip to next day midnight
        // instead of scanning minute-by-minute.  Reduces worst case from ~525K to ~1460
        // iterations per year.
        if !(m && d && w) {
            // Advance to 00:00 of the next day.
            cand = (cand.date_naive() + chrono::Days::new(1))
                .and_hms_opt(0, 0, 0)
                .and_then(|naive| cand.timezone().from_local_datetime(&naive).single())
                .unwrap_or_else(|| cand + chrono::Duration::days(1));
            continue;
        }
        let h = field_matches(hr_f, cand.hour());
        let mi = field_matches(min_f, cand.minute());
        trace!(expr=%cron_expr, dow, "searching: {} m={} d={} w={} h={} mi={}", cand.date_naive(), m, d, w, h, mi);
        if h && mi {
            // Convert the matched local time to UTC
            let utc_cand = cand.with_timezone(&chrono::Utc);
            debug!(expr=%cron_expr, "MATCH: {} (UTC: {})", cand, utc_cand);
            return Some(utc_cand.timestamp_millis() as u64);
        }
        cand += chrono::Duration::minutes(1);
    }

    warn!(expr = %cron_expr, "cron: no next run found within 1 year");
    None
}

pub fn current_timestamp_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system clock before UNIX epoch")
        .as_millis() as u64
}

pub fn build_run_log_entry(
    job: &CronJob,
    success: bool,
    error: Option<anyhow::Error>,
) -> RunLogEntry {
    RunLogEntry {
        id: uuid::Uuid::new_v4().to_string(),
        job_id: job.id.clone(),
        started_at: Utc::now(),
        finished_at: Some(Utc::now()),
        success,
        reply_preview: None,
        error: error.map(|e| e.to_string()),
    }
}

/// Extract saved file paths from command output and read their content.
/// Common patterns in Chinese/English:
/// - "报告已保存: /path/to/file.md"
/// - "saved to: /path/to/file"
/// - "文件已保存: /path/to/file"
/// - "output saved: /path/to/file"
pub fn extract_saved_files_content(output: &str) -> String {
    use std::collections::HashSet;

    // Pattern: "报告已保存: path" or "saved to: path" etc.
    let patterns = [
        r"报告已保存[:\s]+([^\n]+)",
        r"文件已保存[:\s]+([^\n]+)",
        r"saved to[:\s]+([^\n]+)",
        r"output saved[:\s]+([^\n]+)",
        r"保存到[:\s]+([^\n]+)",
    ];

    let mut seen_paths: HashSet<String> = HashSet::new();
    let mut contents: Vec<String> = Vec::new();

    for pattern in &patterns {
        let re = regex::Regex::new(pattern).unwrap();
        for cap in re.captures_iter(output) {
            let path = cap[1].trim();
            // Skip if already processed this path
            if seen_paths.contains(path) {
                continue;
            }
            seen_paths.insert(path.to_string());
            // Try to read the file
            if let Ok(content) = std::fs::read_to_string(path) {
                contents.push(format!("[FILE: {}]\n{}", path, content));
            }
        }
    }

    contents.join("\n\n---\n\n")
}

// ---------------------------------------------------------------------------
// Cron store file helpers (used by gateway API)
// ---------------------------------------------------------------------------

/// Returns the cron store file path.
/// Respects RSCLAW_BASE_DIR env var (same as other rsclaw data).
pub fn resolve_cron_store_path() -> PathBuf {
    let base = rsclaw_config::loader::base_dir();
    base.join("cron.json5")
}

/// Load all cron jobs.
///
/// **Authoritative source: redb** (since 2026-05). Falls back to the
/// legacy `cron.json5` file when `init_cron_store()` hasn't been
/// called yet (tests, ad-hoc tools). The redb path also auto-migrates
/// the cron.json5 contents on first read when redb is empty.
///
/// Returns `(jobs, parse_ok)` — `parse_ok=false` only when the legacy
/// file path is in use AND the file has syntax errors. The redb path
/// always returns `parse_ok=true`.
pub fn load_cron_jobs() -> (Vec<CronJob>, bool) {
    if let Some(store) = cron_store() {
        // The boot-time reconcile already ran inside `init_cron_store`,
        // so we just read what's in redb here.
        match store.cron_list() {
            Ok(entries) => {
                let jobs: Vec<CronJob> = entries
                    .into_iter()
                    .filter_map(|(_, json)| serde_json::from_str::<CronJob>(&json).ok())
                    .collect();
                return (jobs, true);
            }
            Err(e) => {
                warn!(err = %e, "cron: redb load failed; falling back to file");
                return load_cron_jobs_from_file();
            }
        }
    }
    load_cron_jobs_from_file()
}

/// Save all cron jobs.
///
/// **Authoritative target: redb**. The cron.json5 file is also
/// updated as a best-effort export so `cat` / `git diff` keep working.
/// When the redb store is uninitialised (tests / standalone tools),
/// falls back to file-only.
///
/// Note: this function performs a bulk replace of the entire job set.
/// For per-job updates use `RedbStore::cron_put` directly via
/// `cron_store()`.
pub fn save_cron_jobs(jobs: &[CronJob]) -> anyhow::Result<()> {
    if let Some(store) = cron_store() {
        let entries: Vec<(String, String)> = jobs
            .iter()
            .filter_map(|j| serde_json::to_string(j).ok().map(|s| (j.id.clone(), s)))
            .collect();
        store
            .cron_bulk_replace(&entries)
            .context("redb cron_bulk_replace failed")?;
        // Best-effort export so users can still read / git-diff / hand-edit.
        export_cron_jobs_to_file(jobs);
        return Ok(());
    }

    // Fallback: legacy file-only path (tests, standalone tools).
    let cron_file = resolve_cron_store_path();
    let store = serde_json::json!({ "version": 1, "jobs": jobs });
    let json =
        serde_json::to_string_pretty(&store).context("failed to serialize cron jobs to JSON")?;
    if let Some(parent) = cron_file.parent() {
        std::fs::create_dir_all(parent).context("failed to create cron directory")?;
    }
    let tmp = format!("{}.tmp", cron_file.display());
    std::fs::write(&tmp, json).context("failed to write cron jobs tmp file")?;
    std::fs::rename(&tmp, &cron_file).context("failed to rename cron jobs file")?;
    Ok(())
}

/// Global mutex that serializes read-modify-write on the cron store file.
/// Without this, concurrent `cron.add` calls (common when an LLM dispatches
/// multiple tool calls in one turn) race and silently lose writes.
pub static CRON_FILE_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

// ---------------------------------------------------------------------------
// Authoritative storage: redb (since 2026-05)
// ---------------------------------------------------------------------------

/// Process-wide handle to the redb instance backing cron storage.
/// Initialised by `gateway/startup.rs::start_gateway` (and CLI subcommands
/// that need cron access — see `cmd::cron`). When unset, `load_cron_jobs`
/// / `save_cron_jobs` fall back to the legacy file-based path so existing
/// tests and tools that don't go through full gateway boot keep working.
static CRON_STORE: std::sync::OnceLock<std::sync::Arc<rsclaw_store::RedbStore>> =
    std::sync::OnceLock::new();

/// Set the redb handle that future cron storage operations should use,
/// and run a one-time boot reconcile from `cron.json5` so user
/// hand-edits since the last shutdown take effect (file → redb merge,
/// preserving runtime state). Idempotent — a second call is silently
/// ignored (OnceLock semantics).
pub fn init_cron_store(store: std::sync::Arc<rsclaw_store::RedbStore>) {
    if CRON_STORE.set(Arc::clone(&store)).is_err() {
        return; // already initialised; do not re-reconcile
    }
    let count = reconcile_file_to_redb_on_boot(&store);
    info!(count, "cron: storage bound to redb (post-reconcile)");
}

/// Returns the configured redb handle, or `None` for callers running
/// without an initialised gateway (tests / standalone tools).
pub fn cron_store() -> Option<std::sync::Arc<rsclaw_store::RedbStore>> {
    CRON_STORE.get().cloned()
}

/// Reconcile `cron.json5` with redb on each gateway boot.
///
/// Called from `load_cron_jobs` (the first time) so the cycle is:
///   user hand-edits cron.json5 → user restarts gateway → file is
///   imported here → cron runner sees the new config.
///
/// Merge rules (file = user intent, redb = runtime state):
///   - **User-config fields** (enabled, schedule, payload, message, delivery,
///     agent_id, name, …) → take from FILE. If the file omits a field and redb
///     has it, keep redb's.
///   - **`state` sub-object** (next_run_at_ms, last_run_at_ms,
///     consecutive_errors, …) → take from REDB. Run statistics must not be
///     reset by an unrelated config edit.
///   - Job present in file but not in redb → add to redb.
///   - Job present in redb but not in file → user deleted it → remove from
///     redb.
///   - File parse failure → skip the merge (don't wipe redb based on a broken
///     file).
///
/// Returns the number of jobs in redb after the merge (for logging).
pub fn reconcile_file_to_redb_on_boot(store: &rsclaw_store::RedbStore) -> usize {
    let (file_jobs, file_ok) = load_cron_jobs_from_file();

    let redb_existing: std::collections::HashMap<String, CronJob> = match store.cron_list() {
        Ok(entries) => entries
            .into_iter()
            .filter_map(|(id, json)| serde_json::from_str::<CronJob>(&json).ok().map(|j| (id, j)))
            .collect(),
        Err(e) => {
            warn!(err = %e, "cron: redb cron_list failed during boot reconcile");
            std::collections::HashMap::new()
        }
    };

    if !file_ok {
        // File parse error — do NOT touch redb. Tell the user.
        warn!(
            "cron: cron.json5 parse failed at boot; redb left untouched ({} jobs)",
            redb_existing.len()
        );
        return redb_existing.len();
    }

    if file_jobs.is_empty() && redb_existing.is_empty() {
        return 0;
    }

    let mut merged: Vec<(String, String)> = Vec::with_capacity(file_jobs.len());
    for mut file_job in file_jobs {
        if let Some(redb_job) = redb_existing.get(&file_job.id) {
            // Merge: file owns user-config, redb owns state.
            // The struct fields we keep from redb are exactly `state`.
            file_job.state = redb_job.state.clone();
        }
        let json = match serde_json::to_string(&file_job) {
            Ok(s) => s,
            Err(e) => {
                warn!(err = %e, job_id = %file_job.id, "cron: serialize failed during reconcile");
                continue;
            }
        };
        merged.push((file_job.id.clone(), json));
    }

    if let Err(e) = store.cron_bulk_replace(&merged) {
        warn!(err = %e, "cron: reconcile bulk_replace failed");
        return redb_existing.len();
    }

    let added = merged
        .iter()
        .filter(|(id, _)| !redb_existing.contains_key(id))
        .count();
    let removed = redb_existing
        .keys()
        .filter(|id| !merged.iter().any(|(mid, _)| mid == *id))
        .count();
    if added > 0 || removed > 0 {
        info!(
            total = merged.len(),
            added, removed, "cron: boot reconcile cron.json5 -> redb"
        );
    }
    merged.len()
}

/// File-only loader (legacy path). Kept for tests and as a one-time
/// migration source. New code should call `load_cron_jobs()` which
/// uses redb when available.
pub fn load_cron_jobs_from_file() -> (Vec<CronJob>, bool) {
    let source = resolve_cron_store_path();

    // Auto-migrate legacy cron/jobs.json -> cron.json5
    if !source.exists() {
        let base = rsclaw_config::loader::base_dir();
        let legacy = base.join("cron").join("jobs.json");
        if legacy.exists() {
            info!(from = %legacy.display(), to = %source.display(), "migrating legacy cron/jobs.json to cron.json5");
            if let Err(e) = std::fs::copy(&legacy, &source) {
                warn!(err = %e, "failed to migrate legacy cron/jobs.json");
            } else {
                if let Err(e) = std::fs::remove_file(&legacy) {
                    tracing::debug!("failed to remove legacy cron file: {e}");
                }
                if let Err(e) = std::fs::remove_dir(base.join("cron")) {
                    tracing::debug!("failed to remove legacy cron dir: {e}");
                }
            }
        }
    }

    if !source.exists() {
        return (Vec::new(), true);
    }
    let raw = match std::fs::read_to_string(&source) {
        Ok(raw) => raw,
        Err(_) => return (Vec::new(), true),
    };
    if raw.trim().is_empty() {
        return (Vec::new(), true);
    }
    let parsed_result: Result<serde_json::Value, _> =
        json5::from_str(&raw).or_else(|_| serde_json::from_str(&raw));
    if parsed_result.is_err() {
        warn!(file = %source.display(), "cron.json5 parse failed - keeping original file");
        return (Vec::new(), false);
    }
    let parsed = parsed_result.unwrap();
    let jobs_array = if let Some(arr) = parsed.get("jobs").and_then(|v| v.as_array()) {
        arr.clone()
    } else if parsed.is_array() {
        parsed.as_array().cloned().unwrap_or_default()
    } else {
        Vec::new()
    };
    let total = jobs_array.len();
    let jobs: Vec<CronJob> = jobs_array
        .iter()
        .filter_map(|v| serde_json::from_value::<CronJob>(v.clone()).ok())
        .collect();
    let loaded = jobs.len();
    if loaded < total {
        return (jobs, false);
    }
    (jobs, true)
}

/// Best-effort export: write the redb-authoritative job list back to
/// `cron.json5` so users can `cat` / `git diff` / hand-edit the file.
/// On failure we log and continue — the redb write is what matters.
/// Hand-edits to the file get re-imported via the file watcher in
/// `gateway::startup` so this round-trip stays consistent.
pub fn export_cron_jobs_to_file(jobs: &[CronJob]) {
    let cron_file = resolve_cron_store_path();
    let store = serde_json::json!({ "version": 1, "jobs": jobs });
    let json = match serde_json::to_string_pretty(&store) {
        Ok(s) => s,
        Err(e) => {
            warn!(err = %e, "cron: export serialize failed");
            return;
        }
    };
    if let Some(parent) = cron_file.parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            tracing::warn!(dir=%parent.display(), error=%e, "failed to create cron directory");
        }
    }
    let tmp = format!("{}.tmp", cron_file.display());
    if let Err(e) = std::fs::write(&tmp, &json) {
        warn!(err = %e, "cron: export write failed");
        return;
    }
    if let Err(e) = std::fs::rename(&tmp, &cron_file) {
        warn!(err = %e, "cron: export rename failed");
    }
}

// ---------------------------------------------------------------------------
// Cross-module reload signal
// ---------------------------------------------------------------------------
//
// Lets non-server code paths (e.g. fast preparse `/loop`) ask the cron runner
// to reload `cron.json5` after appending a new job. Populated once at gateway
// startup with the same broadcast sender wired into AppState.

static CRON_RELOAD_TX: OnceLock<broadcast::Sender<()>> = OnceLock::new();

/// Install the cron reload broadcast sender. Called once at gateway startup.
/// Subsequent installs are silently ignored (idempotent).
pub fn install_reload_sender(tx: broadcast::Sender<()>) {
    if CRON_RELOAD_TX.set(tx).is_err() {
        warn!("cron: reload sender already installed, ignoring duplicate install");
    }
}

/// Trigger a cron reload from anywhere in the crate. Returns `true` if the
/// signal was sent, `false` if no sender is installed yet (during early
/// startup) or if every receiver has been dropped.
pub fn trigger_reload() -> bool {
    match CRON_RELOAD_TX.get() {
        Some(tx) => tx.send(()).is_ok(),
        None => false,
    }
}

/// Validate a cron expression at save time. Returns a friendly error string
/// the LLM can act on, instead of silently accepting broken expressions and
/// failing later at scheduling time.
pub fn validate_cron_expr(expr: &str) -> Result<(), String> {
    let trimmed = expr.trim();
    if trimmed.is_empty() {
        return Err("cron expression is empty".to_owned());
    }
    let fields: Vec<&str> = trimmed.split_whitespace().collect();
    if fields.len() != 5 {
        // Build a hint that catches the common "forgot a space" mistake.
        // E.g. "017 * * *" → hint that "017" might be "0 17" (4 fields → 5).
        let hint = if fields.len() == 4
            && fields[0].len() >= 2
            && fields[0].chars().all(|c| c.is_ascii_digit())
        {
            let n = fields[0];
            format!(
                " — looks like a missing space: '{}' could be '{} {}' which makes 5 fields (e.g. '0 17 * * *' for 5pm daily)",
                n,
                &n[..1],
                &n[1..]
            )
        } else {
            String::new()
        };
        return Err(format!(
            "cron expression must have exactly 5 fields separated by spaces \
             (minute hour day month weekday), got {} field(s): '{}'{}",
            fields.len(),
            trimmed,
            hint
        ));
    }
    // Delegate range parsing to the existing scheduler. If it can compute a
    // next run, the expression is valid; otherwise reject.
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    if compute_next_run_from_expr(trimmed, now, None).is_none() {
        return Err(format!(
            "cron expression '{}' could not be parsed. Valid examples: \
             '*/5 * * * *' (every 5 min), '0 17 * * *' (5pm daily), \
             '0 9 * * 1' (9am Mondays)",
            trimmed
        ));
    }
    Ok(())
}

#[cfg(test)]
mod cron_config_equal_tests {
    use super::*;

    fn job(id: &str, expr: &str, msg: &str) -> CronJob {
        CronJob {
            id: id.to_string(),
            name: Some(id.to_string()),
            agent_id: "default".to_string(),
            session_key: None,
            enabled: true,
            schedule: CronSchedule::Flat(expr.to_string()),
            payload: None,
            message: Some(msg.to_string()),
            delivery: None,
            session_target: None,
            wake_mode: None,
            state: None,
            iter: None,
            created_at_ms: Some(1_000),
            updated_at_ms: Some(1_000),
        }
    }

    #[test]
    fn identical_jobs_equal() {
        let a = job("j1", "*/5 * * * *", "ping");
        let b = job("j1", "*/5 * * * *", "ping");
        assert!(cron_jobs_config_equal(&a, &b));
    }

    #[test]
    fn different_message_not_equal() {
        let a = job("j1", "*/5 * * * *", "ping");
        let b = job("j1", "*/5 * * * *", "pong");
        assert!(!cron_jobs_config_equal(&a, &b));
    }

    #[test]
    fn different_schedule_not_equal() {
        let a = job("j1", "*/5 * * * *", "ping");
        let b = job("j1", "*/30 * * * *", "ping");
        assert!(!cron_jobs_config_equal(&a, &b));
    }

    #[test]
    fn state_diff_still_equal() {
        // State is runtime-only; two configs that differ only in state must
        // be treated as equal so a state update doesn't trip cancellation.
        let mut a = job("j1", "*/5 * * * *", "ping");
        let mut b = job("j1", "*/5 * * * *", "ping");
        a.state = Some(CronJobState {
            consecutive_errors: 0,
            ..Default::default()
        });
        b.state = Some(CronJobState {
            consecutive_errors: 7,
            last_error: Some("boom".to_string()),
            next_run_at_ms: Some(99_999),
            ..Default::default()
        });
        assert!(cron_jobs_config_equal(&a, &b));
    }

    #[test]
    fn updated_at_diff_still_equal() {
        // updated_at_ms flips on every save; treating it as a config change
        // would cause spurious cancellations.
        let mut a = job("j1", "*/5 * * * *", "ping");
        let mut b = job("j1", "*/5 * * * *", "ping");
        a.updated_at_ms = Some(1_000);
        b.updated_at_ms = Some(2_000);
        assert!(cron_jobs_config_equal(&a, &b));
    }

    #[test]
    fn enabled_diff_not_equal() {
        // Toggling enabled IS a meaningful change — but the cancellation
        // path for disabled jobs goes through the active_unchanged filter
        // (a disabled job is not in active_unchanged), so it'd be cancelled
        // either way.  This just documents that enabled is part of config.
        let a = job("j1", "*/5 * * * *", "ping");
        let mut b = job("j1", "*/5 * * * *", "ping");
        b.enabled = false;
        assert!(!cron_jobs_config_equal(&a, &b));
    }
}

#[cfg(test)]
mod cron_iter_tests {
    use super::*;

    fn bare_job(msg: &str) -> CronJob {
        CronJob {
            id: "rot".into(),
            name: None,
            agent_id: "default".into(),
            session_key: None,
            enabled: true,
            schedule: CronSchedule::Flat("* * * * *".into()),
            payload: None,
            message: Some(msg.into()),
            delivery: None,
            session_target: None,
            wake_mode: None,
            state: None,
            iter: None,
            created_at_ms: None,
            updated_at_ms: None,
        }
    }

    fn iter_job(items: &[&str], cursor: usize, msg: &str) -> CronJob {
        let mut j = bare_job(msg);
        j.iter = Some(CronIter {
            items: items.iter().map(|s| s.to_string()).collect(),
            cursor,
        });
        j
    }

    #[test]
    fn render_substitutes_current_and_next() {
        let j = iter_job(
            &["东京", "曼谷", "迪拜"],
            0,
            "查询{current}天气，下一次：{next}",
        );
        assert_eq!(j.render_message(), "查询东京天气，下一次：曼谷");
    }

    #[test]
    fn render_index_and_total_one_based() {
        let j = iter_job(&["a", "b", "c"], 1, "{index}/{total}: {current}");
        assert_eq!(j.render_message(), "2/3: b");
    }

    #[test]
    fn next_wraps_around_at_end() {
        let j = iter_job(&["a", "b", "c"], 2, "{current}->{next}");
        assert_eq!(j.render_message(), "c->a");
    }

    #[test]
    fn advance_wraps_and_reports_new_cursor() {
        let mut j = iter_job(&["x", "y"], 1, "{current}");
        assert_eq!(j.advance_iter(), Some(0));
        assert_eq!(j.iter.as_ref().unwrap().cursor, 0);
    }

    #[test]
    fn render_without_iter_returns_raw() {
        let mut j = bare_job("hello {current}");
        assert!(j.iter.is_none());
        assert_eq!(j.render_message(), "hello {current}");
        assert_eq!(j.advance_iter(), None);
    }

    #[test]
    fn empty_items_falls_back_to_raw() {
        let j = iter_job(&[], 0, "x={current}");
        assert_eq!(j.render_message(), "x={current}");
    }

    #[test]
    fn bake_overwrites_payload_then_message() {
        let mut j = iter_job(&["a", "b"], 0, "ignored");
        j.payload = Some(CronPayload::Structured {
            kind: Some("agentTurn".into()),
            text: Some("查询{current}".into()),
            timeout_seconds: None,
            summarize: None,
        });
        let rendered = j.render_message();
        assert_eq!(rendered, "查询a");
        j.bake_message(rendered);
        assert_eq!(j.effective_message(), "查询a");
    }

    /// The dispatcher persists the advanced cursor BEFORE handing the rendered
    /// job to the agent. Verify that the iter struct round-trips through the
    /// same JSON form `save_store` writes — if cursor doesn't survive the
    /// serde dance, a crash mid-fire would replay the same item next start.
    #[test]
    fn iter_cursor_survives_serde_roundtrip() {
        let mut j = iter_job(&["东京", "曼谷", "迪拜"], 0, "查询{current}");
        // Simulate one dispatch's mutations: render captures item 0 ("东京"),
        // advance moves cursor to 1, persistence writes the new state.
        let rendered = j.render_message();
        assert_eq!(rendered, "查询东京");
        assert_eq!(j.advance_iter(), Some(1));

        let json = serde_json::to_string(&j).expect("serialize");
        let restored: CronJob = serde_json::from_str(&json).expect("deserialize");
        let iter = restored.iter.as_ref().expect("iter must round-trip");
        assert_eq!(iter.cursor, 1, "cursor must survive serde roundtrip");
        assert_eq!(iter.items, vec!["东京", "曼谷", "迪拜"]);
        // Next dispatch (post-restart) picks up at the new cursor → "曼谷".
        assert_eq!(restored.render_message(), "查询曼谷");
    }

    /// Full on-disk persist test using the same `CronStore { version, jobs }`
    /// envelope `save_store` writes. Mirrors a kill -9 scenario: the
    /// dispatcher renders + advances + saves, then crashes BEFORE the agent
    /// dispatch returns. After restart, the file on disk must reflect the
    /// advanced cursor so the next fire picks up the next item — no replay,
    /// no skip.
    #[tokio::test]
    async fn iter_cursor_persists_to_disk_before_dispatch() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("cron.json5");

        // Build the store the way the dispatcher does: a job with iter,
        // mutate as if one fire had just begun.
        let mut job = iter_job(&["a", "b", "c"], 0, "do {current}");
        let rendered = job.render_message();
        assert_eq!(rendered, "do a");
        assert_eq!(job.advance_iter(), Some(1));

        // Mimic the exact write path of `CronRunner::save_store` — JSON
        // serialise the store envelope and atomic-rename via .tmp.
        #[derive(serde::Serialize, serde::Deserialize)]
        struct Store {
            version: u32,
            jobs: Vec<CronJob>,
        }
        let store = Store {
            version: 1,
            jobs: vec![job],
        };
        let json = serde_json::to_string_pretty(&store).expect("serialize");
        let tmp_path = format!("{}.tmp", path.display());
        tokio::fs::write(&tmp_path, &json).await.expect("write tmp");
        tokio::fs::rename(&tmp_path, &path).await.expect("rename");

        // Simulate post-restart: read the file fresh, verify cursor advanced.
        let bytes = tokio::fs::read(&path).await.expect("read");
        let restored: Store = serde_json::from_slice(&bytes).expect("deserialize");
        assert_eq!(restored.jobs.len(), 1);
        let iter = restored.jobs[0].iter.as_ref().expect("iter present");
        assert_eq!(
            iter.cursor, 1,
            "cursor must persist before dispatch returns"
        );
        assert_eq!(
            restored.jobs[0].render_message(),
            "do b",
            "next fire post-restart must pick the next item, not replay 'a'"
        );
    }
}

#[cfg(test)]
mod cron_validate_tests {
    use super::validate_cron_expr;

    #[test]
    fn accepts_common_patterns() {
        for ok in ["*/5 * * * *", "0 17 * * *", "30 8 * * 1-5", "0 9 1 * *"] {
            assert!(validate_cron_expr(ok).is_ok(), "should accept '{}'", ok);
        }
    }

    #[test]
    fn rejects_empty() {
        assert!(validate_cron_expr("").is_err());
        assert!(validate_cron_expr("   ").is_err());
    }

    #[test]
    fn rejects_four_fields_with_hint() {
        let err = validate_cron_expr("017 * * *").unwrap_err();
        assert!(err.contains("5 fields"), "err = {err}");
        assert!(err.contains("0 17"), "should hint at '0 17': {err}");
    }

    #[test]
    fn rejects_garbage() {
        assert!(validate_cron_expr("not a cron").is_err());
    }
}
