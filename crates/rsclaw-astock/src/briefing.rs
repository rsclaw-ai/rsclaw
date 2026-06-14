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

use chrono::{Datelike, Duration as ChronoDuration, NaiveTime, TimeZone, Weekday};
#[cfg(test)]
use chrono::Timelike;
use chrono_tz::Asia::Shanghai;
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

use rsclaw_memory::MemoryStore;

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
        let cfg = rsclaw_config::load().ok()?;
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
/// holding a non-empty watchlist, pre-fetch their market data from
/// astock, then submit one synthetic user turn per group with the
/// data embedded in the prompt. Failure of any one group is logged
/// and the rest continue.
///
/// Why pre-fetch in gateway instead of letting the LLM call
/// `stock_quote` itself
///
/// The LLM-tool path (the original v1) had a latent honesty bug:
/// when a peer received their second briefing of the day (e.g.
/// midday after premarket), the LLM saw the premarket reply in
/// session history (`msg_count` jumped from 30 → 70) and decided it
/// already had enough data to write the recap — `tool_call_count=0`,
/// reply produced from training memory + premarket prices. The
/// "noon" prices in the midday briefing were therefore the SAME
/// numbers as the premarket briefing.
///
/// Pre-fetching here makes the data deterministic: gateway calls
/// astock directly, embeds the fresh JSON-shaped block in the
/// prompt with a strict "use exactly these numbers, do not call any
/// stock_* tool" instruction. The LLM can still call `web_search`
/// for soft context (隔夜美股 / 板块情绪) when the prompt allows,
/// but the price/snapshot/kline numbers are now 100% from astock at
/// fire time.
async fn dispatch_for_slot(slot: BriefingSlot) {
    let Some(mem) = rsclaw_memory::global_store() else {
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
    let Some(sink) = crate::global_briefing_sink() else {
        tracing::warn!("briefing sink not installed; skipping briefing");
        return;
    };
    let arc = match crate::global_client() {
        Some(c) => c,
        None => {
            tracing::warn!(
                slot = ?slot,
                "astock client not initialized; skipping briefing (would have no data to embed)"
            );
            return;
        }
    };
    tracing::info!(
        slot = ?slot,
        peers = groups.len(),
        "dispatching briefings"
    );
    for group in groups {
        let prefetch = prefetch_market_data(&arc, slot, &group.codes).await;
        tracing::info!(
            slot = ?slot,
            peer = %group.peer_id,
            quotes = prefetch.quotes.len(),
            snapshot_rows = prefetch.snapshot.as_ref().map(|v| v.len()).unwrap_or(0),
            klines = prefetch.klines.len(),
            "briefing data prefetched"
        );
        let prompt = build_prompt(slot, &group, &prefetch);
        let session_key = format!(
            "agent:{}:{}:direct:{}",
            group.agent_id, group.channel, group.peer_id
        );
        match sink.submit_briefing(
            &session_key,
            &prompt,
            &group.channel,
            &group.peer_id,
            &group.peer_id,
            false,
            rsclaw_types::Priority::Cron,
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

/// Gateway-prefetched data block carried to the LLM in the prompt.
/// Filled by `prefetch_market_data` per slot — different slots
/// surface different data shapes.
struct PrefetchedData {
    /// Always populated when astock is reachable. Empty Vec when
    /// `quote_batch` failed (logged, briefing still fires).
    quotes: Vec<crate::Quote>,
    /// MidDay only — full snapshot row including amount / turnover.
    snapshot: Option<Vec<crate::SnapshotRow>>,
    /// PostMarket only — last 20 daily K-line bars per code (qfq).
    klines: std::collections::HashMap<String, crate::KlineResp>,
}

/// Pre-fetch the market data this slot wants the LLM to talk about.
/// Each slot has a different data shape — see `format_data_block`
/// for what ends up in the prompt:
///   * PreMarket  — just `quote_batch` (latest tick before open)
///   * MidDay     — `quote_batch` + `snapshot` (amount/turnover)
///   * PostMarket — `quote_batch` + per-code 20-day K-line (qfq)
///
/// Errors are swallowed and logged — a flaky upstream upstream
/// shouldn't kill the briefing. Empty data → the LLM gets the
/// empty data block + a hint that "(数据拉取失败)" so it can write
/// a graceful "data unavailable" reply.
async fn prefetch_market_data(
    arc: &crate::AstockClient,
    slot: BriefingSlot,
    codes: &[String],
) -> PrefetchedData {
    let quotes = match arc.quote_batch(codes).await {
        Ok(q) => q,
        Err(e) => {
            tracing::warn!(error = %e, "briefing prefetch: quote_batch failed");
            Vec::new()
        }
    };
    let snapshot = match slot {
        BriefingSlot::MidDay => match arc.snapshot(None, None, codes, None).await {
            Ok(r) => Some(r),
            Err(e) => {
                tracing::warn!(error = %e, "briefing prefetch: snapshot failed");
                None
            }
        },
        _ => None,
    };
    let klines = match slot {
        BriefingSlot::PostMarket => {
            let mut m = std::collections::HashMap::new();
            for code in codes {
                match arc
                    .kline(code, Some("1d"), Some(20), Some(0), Some("qfq"))
                    .await
                {
                    Ok(r) => {
                        m.insert(code.clone(), r);
                    }
                    Err(e) => {
                        tracing::warn!(
                            code = %code,
                            error = %e,
                            "briefing prefetch: kline failed"
                        );
                    }
                }
            }
            m
        }
        _ => std::collections::HashMap::new(),
    };
    PrefetchedData {
        quotes,
        snapshot,
        klines,
    }
}

/// Render the prefetched data into a markdown-ish block the LLM can
/// read verbatim. Kept compact so the prompt token count stays under
/// 1 KB per peer even with 50-code watchlists.
fn format_data_block(data: &PrefetchedData) -> String {
    let mut out = String::new();
    if data.quotes.is_empty()
        && data.snapshot.as_ref().map(|v| v.is_empty()).unwrap_or(true)
        && data.klines.is_empty()
    {
        out.push_str("_(数据拉取失败,请在简报里如实说明)_\n");
        return out;
    }
    if !data.quotes.is_empty() {
        out.push_str("**实时报价 (quote):**\n");
        for q in &data.quotes {
            let pct = q.change_pct();
            let sign = if pct >= 0.0 { "+" } else { "" };
            out.push_str(&format!(
                "- {} 现价 ¥{:.2} 涨跌 {}{:.2}% | 开 {:.2} 高 {:.2} 低 {:.2} 昨收 {:.2} | 成交 {} 手 / ¥{:.0}万\n",
                q.code,
                q.price,
                sign,
                pct,
                q.open,
                q.high,
                q.low,
                q.pre_close,
                q.volume / 100,
                q.amount / 10_000.0,
            ));
        }
    }
    if let Some(rows) = &data.snapshot
        && !rows.is_empty()
    {
        out.push_str("\n**快照 (snapshot):**\n");
        for r in rows {
            let pct = if r.pre_close.abs() > f64::EPSILON {
                (r.price - r.pre_close) / r.pre_close * 100.0
            } else {
                0.0
            };
            let sign = if pct >= 0.0 { "+" } else { "" };
            out.push_str(&format!(
                "- {} 价 {:.2} 涨跌 {}{:.2}% 量 {} 额 ¥{:.0}万\n",
                r.code,
                r.price,
                sign,
                pct,
                r.volume / 100,
                r.amount / 10_000.0,
            ));
        }
    }
    if !data.klines.is_empty() {
        out.push_str("\n**近 20 日 K 线 (1d, qfq):**\n");
        // Stable iteration order so the same prompt is reproducible
        // across briefings — HashMap is unordered.
        let mut codes: Vec<&String> = data.klines.keys().collect();
        codes.sort();
        for code in codes {
            let Some(kr) = data.klines.get(code) else {
                continue;
            };
            // Last 5 bars in chronological order (oldest → newest).
            let bars: Vec<String> = kr
                .klines
                .iter()
                .rev()
                .take(5)
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .map(|b| {
                    let d = chrono::DateTime::from_timestamp(b.timestamp, 0)
                        .map(|d| d.format("%m-%d").to_string())
                        .unwrap_or_else(|| "?".to_owned());
                    format!("{} 收{:.2}", d, b.close)
                })
                .collect();
            // Range = first bar's open → last bar's close, plus high/low across the window.
            let (lo, hi) = kr
                .klines
                .iter()
                .fold((f64::INFINITY, f64::NEG_INFINITY), |(lo, hi), b| {
                    (lo.min(b.low), hi.max(b.high))
                });
            out.push_str(&format!(
                "- {} 区间 {:.2}–{:.2} | 近5日: {}\n",
                code,
                lo,
                hi,
                bars.join(" → ")
            ));
        }
    }
    out
}

/// Build the synthetic user message that becomes the LLM's briefing
/// turn. The prompt embeds gateway-pre-fetched data and tells the
/// LLM to USE IT VERBATIM — no second-trip to `stock_*` tools.
fn build_prompt(slot: BriefingSlot, group: &WatchlistGroup, data: &PrefetchedData) -> String {
    let label = slot.label();
    let codes_list = group.codes.join(", ");
    let data_block = format_data_block(data);
    let body = match slot {
        BriefingSlot::PreMarket => "任务:\n\
            1. 简短列出每只股票的最新价 / 涨跌幅(数据用上面的报价区,不要自己调 stock_quote)\n\
            2. 给一句话开盘看点(隔夜美股 / 重要事件 / 板块情绪 — 这一点允许调 web_search 补充)\n\
            3. 全篇控制在 200 字以内",
        BriefingSlot::MidDay => "任务:\n\
            1. 列出每只股票上午的表现(用上面的报价 + 快照区,不要自己调 stock_quote 或 stock_snapshot)\n\
            2. 评一句话下午看点 / 关键支撑或压力位\n\
            3. 200 字以内",
        BriefingSlot::PostMarket => "任务:\n\
            1. 当日每只股票收盘价 / 涨跌幅 / 关键变动(数据用上面的报价 + K 线区,不要自己调 stock_quote 或 stock_kline)\n\
            2. 综合一句话总结今日盘面 + 明日值得关注的点\n\
            3. 如果有研报或公告,可以调 web_search 简单核查后提一下\n\
            4. 250 字以内",
    };
    format!(
        "[{label}] 现在是 {ts}。\n\n\
         你管理的关注列表: {codes_list}。\n\n\
         === gateway 已为你拉好实时数据 (start) ===\n\
         {data_block}\
         === gateway 已为你拉好实时数据 (end) ===\n\n\
         {body}\n\n\
         **严格要求**:\n\
         - 所有价格、涨跌幅、成交量、K 线数字必须直接用上面 'gateway 已为你拉好' 块里的数,**严禁编造任何数字**。\n\
         - 严禁再调任何 stock_* 工具拉一遍同样的数据 — 浪费 token 也不会更新。\n\
         - 只有 web_search 在允许的子任务里可以用。\n\n\
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
        let empty = PrefetchedData {
            quotes: vec![],
            snapshot: None,
            klines: std::collections::HashMap::new(),
        };
        let p = build_prompt(BriefingSlot::PreMarket, &g, &empty);
        assert!(p.contains("早盘前简报"));
        assert!(p.contains("600519"));
        assert!(p.contains("000001"));
        assert!(p.contains("不构成投资建议"));
        // Empty prefetch should surface a graceful "数据拉取失败" line so the
        // LLM doesn't silently produce a number-free reply.
        assert!(p.contains("数据拉取失败"));
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
