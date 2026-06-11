//! Cron-driven daily A-share briefings to every peer with a
//! non-empty watchlist.
//!
//! Three fixed wallclock slots in `Asia/Shanghai`:
//!
//!   * 07:50 — pre-market heads-up
//!   * 12:05 — mid-day recap (right after the lunch break opens)
//!   * 18:30 — post-close summary + outlook
//!
//! Trading-day gate is a simple Mon-Fri check; A-share holidays
//! aren't observed (a smarter calendar can come via astock later).
//! Saturday/Sunday slots quietly skip — the scheduler still sleeps
//! the full week so the next fire is correct.
//!
//! How a briefing is delivered
//!
//! 1. The scheduler wakes up at slot time, calls `dispatch_for_slot`.
//! 2. We enumerate all `MemoryDoc`s with `kind == "watchlist"` and
//!    group them by their owning scope key
//!    `agent:{id}:watchlist:{channel}:{peer}`.
//! 3. For each (agent, channel, peer, codes) group, we synthesise an
//!    inbound message and drop it onto the task queue using the
//!    same `submit_to_queue` path real IM messages use. The agent
//!    runtime sees a regular user turn, calls `stock_quote` /
//!    `stock_kline` / `stock_chart` as it sees fit, and replies via
//!    the channel back to the user.
//!
//! Why this shape (not a cron-job entry, not a hardcoded handler)
//!
//! Routing the briefing through the normal task-queue path means
//! the LLM gets the same auto-recall, memory, and channel
//! plumbing it would for any inbound message — no parallel
//! reply path to maintain, no separate IM-send code, and the user
//! sees the briefing in the same thread their other rsclaw replies
//! land in (cap_resume continuity, etc.).

use std::sync::Arc;

use chrono::{Datelike, Duration as ChronoDuration, NaiveTime, TimeZone, Timelike, Weekday};
use chrono_tz::Asia::Shanghai;
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

use crate::agent::memory::MemoryStore;

/// Three briefing slots. Distinct types (not just a time) so the
/// prompt template can differ per slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum BriefingSlot {
    /// 07:50 — heads-up before the 09:30 open.
    PreMarket,
    /// 12:05 — five minutes into the lunch break, recap of the
    /// morning session and what to watch in the afternoon.
    MidDay,
    /// 18:30 — three hours after close, after most research notes
    /// drop. Recap + tomorrow outlook.
    PostMarket,
}

impl BriefingSlot {
    pub fn label(&self) -> &'static str {
        match self {
            BriefingSlot::PreMarket => "早盘前简报",
            BriefingSlot::MidDay => "午间简报",
            BriefingSlot::PostMarket => "收盘简报",
        }
    }

    /// Wall-clock time for this slot, in Asia/Shanghai. Reads
    /// `astock.briefing.slots.<slug>` from the live config — when
    /// absent or malformed, falls back to the hardcoded default
    /// (`07:50 / 12:05 / 18:30`).
    ///
    /// Each call re-reads the config (cheap — JSON5 file < 10 KB),
    /// but the scheduler captures the value once per loop iteration
    /// so changes only take effect after the scheduler wakes for the
    /// next slot. In practice that means: edit `rsclaw.json5`, wait
    /// for the next briefing to fire (or `cargo run -- gateway
    /// restart`), and the new time applies from then on.
    pub fn wallclock(&self) -> NaiveTime {
        self.config_wallclock().unwrap_or_else(|| self.default_wallclock())
    }

    fn default_wallclock(&self) -> NaiveTime {
        match self {
            BriefingSlot::PreMarket => NaiveTime::from_hms_opt(7, 50, 0).unwrap(),
            BriefingSlot::MidDay => NaiveTime::from_hms_opt(12, 5, 0).unwrap(),
            BriefingSlot::PostMarket => NaiveTime::from_hms_opt(18, 30, 0).unwrap(),
        }
    }

    fn config_wallclock(&self) -> Option<NaiveTime> {
        let cfg = crate::config::load().ok()?;
        let slots = cfg.raw.astock.as_ref()?.briefing.as_ref()?.slots.as_ref()?;
        let raw = slots.get(self.slug())?;
        match NaiveTime::parse_from_str(raw.trim(), "%H:%M") {
            Ok(t) => Some(t),
            Err(_) => {
                tracing::warn!(
                    slot = self.slug(),
                    raw = %raw,
                    "astock.briefing.slots: malformed HH:MM, using default"
                );
                None
            }
        }
    }

    /// Short slug used by `/astock briefing run <slot>` and any other
    /// callers that need a stable identifier (cleaner than `{:?}`).
    pub fn slug(&self) -> &'static str {
        match self {
            BriefingSlot::PreMarket => "premarket",
            BriefingSlot::MidDay => "midday",
            BriefingSlot::PostMarket => "postmarket",
        }
    }

    /// Parse a user-typed slug back into a slot. Accepts the canonical
    /// `premarket / midday / postmarket` plus common aliases so the
    /// slash command does the right thing for `/astock briefing run pre`
    /// or `... noon` or `... close` without typo-shaming the user.
    pub fn from_slug(s: &str) -> Option<Self> {
        match s.trim().to_lowercase().as_str() {
            "premarket" | "pre" | "pre-market" | "morning" => Some(Self::PreMarket),
            "midday" | "mid" | "mid-day" | "noon" | "lunch" => Some(Self::MidDay),
            "postmarket" | "post" | "post-market" | "close" | "evening" => Some(Self::PostMarket),
            _ => None,
        }
    }

    /// Ordered list of all slots in wallclock order — used to find
    /// the next slot from "now" and to enumerate the full schedule
    /// for `/astock briefing list`.
    pub fn all_in_order() -> [BriefingSlot; 3] {
        [
            BriefingSlot::PreMarket,
            BriefingSlot::MidDay,
            BriefingSlot::PostMarket,
        ]
    }
}

/// One-row snapshot of the next future fire time per slot. Returned in
/// wallclock order (PreMarket → MidDay → PostMarket). Each entry's
/// `next_local` is the next Asia/Shanghai datetime at which the
/// scheduler will dispatch that slot (skipping weekends — see
/// `is_trading_day`).
///
/// Used by `/astock briefing list` and `/astock briefing next` so the
/// user can see at a glance when each slot is next due, without
/// having to grep gateway.log. The internal scheduler still owns
/// firing; this is a read-only projection of its calendar.
pub fn schedule_snapshot() -> Vec<(BriefingSlot, chrono::DateTime<chrono_tz::Tz>)> {
    let now_utc = chrono::Utc::now();
    let now_local = now_utc.with_timezone(&Shanghai);
    BriefingSlot::all_in_order()
        .iter()
        .copied()
        .map(|slot| {
            let mut day = now_local.date_naive();
            for _ in 0..14 {
                if is_trading_day(day) {
                    let candidate = Shanghai
                        .from_local_datetime(&day.and_time(slot.wallclock()))
                        .single()
                        .unwrap_or(now_local);
                    if candidate > now_local {
                        return (slot, candidate);
                    }
                }
                day += ChronoDuration::days(1);
            }
            // Defensive fallback so the slash command never panics on a
            // broken calendar helper — same shape as `next_slot`'s
            // 1-hour escape hatch.
            (slot, now_local + ChronoDuration::hours(1))
        })
        .collect()
}

/// Manual one-shot dispatch — mirrors what the scheduler does on its
/// own at the wallclock time. Used by `/astock briefing run <slot>`
/// so the user can re-fire (or test) a briefing on demand.
///
/// Does NOT bypass the trading-day check, but DOES ignore weekday —
/// the caller asked for it, fire it. If you wanted a no-op
/// weekend run you'd call this from a weekend, which is rare and
/// usually intentional (testing).
pub async fn dispatch_one(slot: BriefingSlot) {
    dispatch_for_slot(slot).await;
}

/// Compute the next briefing slot to fire, in `Asia/Shanghai`.
///
/// Returns `(wallclock_utc, slot)`. Skips Saturday and Sunday by
/// rolling the date forward — Friday's 18:30 hands off to Monday's
/// 07:50, not Saturday's. The slot AFTER Sunday's "phantom" 18:30 is
/// Monday's 07:50, etc.
pub fn next_slot(now_utc: chrono::DateTime<chrono::Utc>) -> (chrono::DateTime<chrono::Utc>, BriefingSlot) {
    let now_local = now_utc.with_timezone(&Shanghai);
    // Walk forward day-by-day until we land on a weekday whose slot
    // is still in the future. Bounded loop (max 8 iterations) so a
    // broken `is_trading_day` never deadlocks.
    let mut day = now_local.date_naive();
    for _ in 0..8 {
        if is_trading_day(day) {
            for slot in BriefingSlot::all_in_order() {
                let candidate = Shanghai
                    .from_local_datetime(&day.and_time(slot.wallclock()))
                    .single()
                    .unwrap_or_else(|| now_local + ChronoDuration::days(1));
                if candidate > now_local {
                    return (candidate.with_timezone(&chrono::Utc), slot);
                }
            }
        }
        day += ChronoDuration::days(1);
    }
    // Unreachable in practice; fail safely by scheduling 1h from
    // now so the loop keeps making progress even if the calendar
    // helper goes haywire.
    (now_utc + ChronoDuration::hours(1), BriefingSlot::PreMarket)
}

/// Mon-Fri only. A-share market holidays are NOT observed in this
/// version — the briefing will fire on e.g. May 1st even though the
/// market is closed. Live with it for v1; a calendar-aware version
/// can plug in via astock's `Calendar` later.
fn is_trading_day(date: chrono::NaiveDate) -> bool {
    !matches!(date.weekday(), Weekday::Sat | Weekday::Sun)
}

/// Spawn the long-running scheduler task. Fire-and-forget — the
/// task lives until the process exits. Idempotent? No — calling
/// this twice spawns two schedulers. Gateway startup calls it once.
pub fn spawn_scheduler() {
    tokio::spawn(async move {
        loop {
            let (next_utc, slot) = next_slot(chrono::Utc::now());
            let now = chrono::Utc::now();
            let sleep = (next_utc - now)
                .to_std()
                .unwrap_or_else(|_| std::time::Duration::from_secs(60));
            tracing::info!(
                    next = %next_utc.with_timezone(&Shanghai).format("%Y-%m-%d %H:%M %Z"),
                slot = ?slot,
                sleep_secs = sleep.as_secs(),
                "next briefing slot scheduled"
            );
            tokio::time::sleep(sleep).await;
            // Re-check trading day at fire time — handles the
            // "scheduled on Friday for Monday" → "Monday is a
            // declared holiday" upgrade path when we add calendar
            // support later. Weekday-only is already covered by
            // `next_slot`.
            let fire_date = chrono::Utc::now().with_timezone(&Shanghai).date_naive();
            if !is_trading_day(fire_date) {
                tracing::info!(
                            date = %fire_date,
                    "briefing skipped — not a trading day"
                );
                continue;
            }
            dispatch_for_slot(slot).await;
        }
    });
}

/// Enumerate all (agent, channel, peer, codes) groups currently
/// holding a non-empty watchlist, then submit one synthetic user
/// turn per group. Failure of any one group is logged and the rest
/// continue — a misconfigured peer should not block briefings to
/// everyone else.
async fn dispatch_for_slot(slot: BriefingSlot) {
    let Some(mem) = crate::agent::memory::global_store() else {
        tracing::warn!("no memory store; skipping briefing");
        return;
    };
    let groups = enumerate_watchlists(&mem).await;
    if groups.is_empty() {
        tracing::info!(
            slot = ?slot,
            "no peers with watchlists; skipping briefing"
        );
        return;
    }
    let Some(tq) = crate::gateway::task_queue::get_task_queue() else {
        tracing::warn!("task queue not installed; skipping briefing");
        return;
    };
    tracing::info!(
        slot = ?slot,
        peers = groups.len(),
        "dispatching briefings"
    );
    for group in groups {
        let prompt = build_prompt(slot, &group);
        let session_key = format!(
            "agent:{}:{}:direct:{}",
            group.agent_id, group.channel, group.peer_id
        );
        match crate::gateway::task_queue::submit_to_queue(
            &tq,
            &session_key,
            &prompt,
            &group.channel,
            &group.peer_id,
            &group.peer_id,
            false,
            crate::gateway::task_queue::Priority::Cron,
        ) {
            Ok((task_id, merged)) => {
                tracing::info!(
                            slot = ?slot,
                    agent = %group.agent_id,
                    channel = %group.channel,
                    peer = %group.peer_id,
                    codes = group.codes.len(),
                    task_id = %task_id,
                    merged,
                    "briefing submitted"
                );
            }
            Err(e) => {
                tracing::warn!(
                            slot = ?slot,
                    agent = %group.agent_id,
                    channel = %group.channel,
                    peer = %group.peer_id,
                    error = %e,
                    "briefing submit failed"
                );
            }
        }
    }
}

#[derive(Debug, Clone)]
struct WatchlistGroup {
    agent_id: String,
    channel: String,
    peer_id: String,
    codes: Vec<String>,
}

async fn enumerate_watchlists(mem: &Arc<Mutex<MemoryStore>>) -> Vec<WatchlistGroup> {
    let store = mem.lock().await;
    let docs = store.list_active();
    drop(store);
    let mut groups: std::collections::HashMap<(String, String, String), Vec<String>> =
        std::collections::HashMap::new();
    for d in docs {
        if d.kind != "watchlist" {
            continue;
        }
        // Scope shape: `agent:{id}:watchlist:{channel}:{peer}`
        let Some((agent_id, channel, peer_id)) = parse_watchlist_scope(&d.scope) else {
            continue;
        };
        groups
            .entry((agent_id, channel, peer_id))
            .or_default()
            .push(d.text);
    }
    groups
        .into_iter()
        .map(|((agent_id, channel, peer_id), mut codes)| {
            codes.sort();
            codes.dedup();
            WatchlistGroup {
                agent_id,
                channel,
                peer_id,
                codes,
            }
        })
        .filter(|g| !g.codes.is_empty())
        .collect()
}

/// Reverse of `tools_stock::watchlist_scope`. Returns
/// `(agent_id, channel, peer_id)` when the scope matches the
/// expected shape, or `None` otherwise. peer_id can contain colons
/// (some IM channels embed structure there), so we only split the
/// first four segments.
fn parse_watchlist_scope(scope: &str) -> Option<(String, String, String)> {
    let parts: Vec<&str> = scope.splitn(5, ':').collect();
    if parts.len() < 5 {
        return None;
    }
    if parts[0] != "agent" || parts[2] != "watchlist" {
        return None;
    }
    Some((parts[1].to_owned(), parts[3].to_owned(), parts[4].to_owned()))
}

/// Build the synthetic user message that will become the LLM's
/// briefing turn. Goal: tell the LLM (1) it's a briefing call, not
/// a user question, (2) the timeframe, (3) the watchlist, (4) the
/// expected output shape. The LLM then calls stock_* tools as
/// needed.
fn build_prompt(slot: BriefingSlot, group: &WatchlistGroup) -> String {
    let label = slot.label();
    let codes_list = group.codes.join(", ");
    let body = match slot {
        BriefingSlot::PreMarket => "请用 stock_quote / stock_ask 拉数据,然后:\n\
            1. 简短列出每只股票的最新价 / 涨跌幅\n\
            2. 给一句话开盘看点(隔夜美股 / 重要事件 / 板块情绪,允许调 web_search 补充)\n\
            3. 全篇控制在 200 字以内",
        BriefingSlot::MidDay => "请用 stock_quote + stock_snapshot use_watchlist=true 拉数据,然后:\n\
            1. 列出每只股票上午的表现(开盘价 → 上午收盘价 / 涨跌幅)\n\
            2. 评一句话下午看点 / 关键支撑或压力位\n\
            3. 200 字以内",
        BriefingSlot::PostMarket => "请用 stock_quote + stock_kline period=1d count=20 拉数据,然后:\n\
            1. 当日每只股票收盘价 / 涨跌幅 / 关键变动\n\
            2. 综合一句话总结今日盘面 + 明日值得关注的点\n\
            3. 如果有研报或公告,提一下\n\
            4. 250 字以内",
    };
    format!(
        "[{label}] 现在是 {ts}。\n\n\
         你管理的关注列表: {codes_list}。\n\n\
         {body}\n\n\
         结尾追加一行免责声明:信息来自公开 A 股数据接口,仅供参考,不构成投资建议。",
        ts = chrono::Utc::now()
            .with_timezone(&Shanghai)
            .format("%Y-%m-%d %H:%M"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn briefing_slot_labels() {
        assert_eq!(BriefingSlot::PreMarket.label(), "早盘前简报");
        assert_eq!(BriefingSlot::MidDay.label(), "午间简报");
        assert_eq!(BriefingSlot::PostMarket.label(), "收盘简报");
    }

    #[test]
    fn next_slot_picks_premarket_when_before_750() {
        // Wednesday 2026-06-10, 07:00 +08:00 should land on the same
        // day's PreMarket slot at 07:50.
        let t = Shanghai
            .with_ymd_and_hms(2026, 6, 10, 7, 0, 0)
            .unwrap()
            .with_timezone(&chrono::Utc);
        let (when, slot) = next_slot(t);
        assert!(matches!(slot, BriefingSlot::PreMarket));
        let local = when.with_timezone(&Shanghai);
        assert_eq!(local.hour(), 7);
        assert_eq!(local.minute(), 50);
        assert_eq!(local.day(), 10);
    }

    #[test]
    fn next_slot_picks_midday_when_between_750_and_1205() {
        let t = Shanghai
            .with_ymd_and_hms(2026, 6, 10, 9, 30, 0)
            .unwrap()
            .with_timezone(&chrono::Utc);
        let (when, slot) = next_slot(t);
        assert!(matches!(slot, BriefingSlot::MidDay));
        let local = when.with_timezone(&Shanghai);
        assert_eq!(local.hour(), 12);
        assert_eq!(local.minute(), 5);
    }

    #[test]
    fn next_slot_rolls_to_next_weekday_after_friday_evening() {
        // Friday 2026-06-12, 19:00 +08:00. Saturday is not a trading
        // day; next slot must be Monday 2026-06-15 07:50.
        let t = Shanghai
            .with_ymd_and_hms(2026, 6, 12, 19, 0, 0)
            .unwrap()
            .with_timezone(&chrono::Utc);
        let (when, slot) = next_slot(t);
        assert!(matches!(slot, BriefingSlot::PreMarket));
        let local = when.with_timezone(&Shanghai);
        assert_eq!(local.weekday(), Weekday::Mon);
        assert_eq!(local.day(), 15);
        assert_eq!(local.hour(), 7);
        assert_eq!(local.minute(), 50);
    }

    #[test]
    fn next_slot_from_saturday_jumps_to_monday() {
        // Saturday 2026-06-13 10:00 → Monday 07:50.
        let t = Shanghai
            .with_ymd_and_hms(2026, 6, 13, 10, 0, 0)
            .unwrap()
            .with_timezone(&chrono::Utc);
        let (when, slot) = next_slot(t);
        assert!(matches!(slot, BriefingSlot::PreMarket));
        let local = when.with_timezone(&Shanghai);
        assert_eq!(local.weekday(), Weekday::Mon);
    }

    #[test]
    fn parse_watchlist_scope_round_trip() {
        let scope = "agent:main:watchlist:feishu:ou_abc123";
        let (a, c, p) = parse_watchlist_scope(scope).expect("must parse");
        assert_eq!(a, "main");
        assert_eq!(c, "feishu");
        assert_eq!(p, "ou_abc123");
    }

    #[test]
    fn parse_watchlist_scope_rejects_bad_shape() {
        assert!(parse_watchlist_scope("agent:main").is_none());
        assert!(parse_watchlist_scope("not:agent:watchlist:x:y").is_none());
        assert!(parse_watchlist_scope("agent:main:facts:x:y").is_none());
    }

    #[test]
    fn parse_watchlist_scope_allows_colons_in_peer_id() {
        // Some IM channels embed extra structure in the peer id —
        // we should keep the FULL trailing string as peer_id.
        let scope = "agent:main:watchlist:wechat:u:wxid:123";
        let (a, c, p) = parse_watchlist_scope(scope).expect("must parse");
        assert_eq!(a, "main");
        assert_eq!(c, "wechat");
        assert_eq!(p, "u:wxid:123");
    }

    #[test]
    fn build_prompt_carries_codes_and_label() {
        let g = WatchlistGroup {
            agent_id: "main".into(),
            channel: "feishu".into(),
            peer_id: "ou_x".into(),
            codes: vec!["600519".into(), "000001".into()],
        };
        let p = build_prompt(BriefingSlot::PreMarket, &g);
        assert!(p.contains("早盘前简报"));
        assert!(p.contains("600519"));
        assert!(p.contains("000001"));
        assert!(p.contains("不构成投资建议"));
    }

    #[test]
    fn is_trading_day_weekdays_only() {
        // 2026-06-08 Mon .. 2026-06-14 Sun
        let mon = chrono::NaiveDate::from_ymd_opt(2026, 6, 8).unwrap();
        let fri = chrono::NaiveDate::from_ymd_opt(2026, 6, 12).unwrap();
        let sat = chrono::NaiveDate::from_ymd_opt(2026, 6, 13).unwrap();
        let sun = chrono::NaiveDate::from_ymd_opt(2026, 6, 14).unwrap();
        assert!(is_trading_day(mon));
        assert!(is_trading_day(fri));
        assert!(!is_trading_day(sat));
        assert!(!is_trading_day(sun));
    }
}
