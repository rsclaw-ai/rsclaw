//! Built-in `--template <name>` presets for `/watch`.
//!
//! A template bundles a default event-type filter + a default jq
//! formatter so common SSE sources work out of the box without the
//! user authoring jq expressions. User-supplied `--event` / `--jq` /
//! `--grep` flags override the template's defaults — the template
//! only fills slots the user left empty.
//!
//! New templates should target a *specific* upstream schema. Generic
//! "pretty-print any JSON" templates are not the goal; that's what
//! the bare `--jq` flag is for.

use anyhow::{anyhow, Result};

#[derive(Debug)]
pub struct Template {
    /// Default event-type filter (`!heartbeat` → drop heartbeats, etc).
    /// Applied only when the user did not pass `--event` themselves.
    pub event_filter: Option<&'static str>,
    /// Default jq expression. Applied only when the user did not pass
    /// `--jq` themselves. Must produce strings (one per output value);
    /// the rest of the watch pipeline treats each output line as a
    /// separate event for rate limiting and delivery.
    pub jq: Option<&'static str>,
}

/// Look up a template by name. Returns `Err` with a hint listing the
/// available names when the name is unknown.
pub fn lookup(name: &str) -> Result<&'static Template> {
    match name {
        "astock" => Ok(&ASTOCK),
        other => Err(anyhow!(
            "unknown template `{other}` — available: astock"
        )),
    }
}

// ---------------------------------------------------------------------------
// astock — ai-fast quick_stream live signals
// ---------------------------------------------------------------------------
//
// Schema reference (see ~/.claude/skills/quick-stream/SKILL.md):
//   event: snapshot        .data = {filter, codes:[{code,name,diag}], count, ts}
//   event: hit             .data = {filter, code, name, diag, ts}
//   event: drop            .data = {filter, code, name, current_close, current_pct, ts}
//   event: heartbeat       (suppressed)
//   event: stale_reset / error / reconnected  (passed through with [type] prefix)
//
// Per-filter hit formatting follows the skill doc — each match becomes
// one short, human-readable line. snapshot events surface a one-line
// summary "<filter> 命中 N 只: code1 name1, code2 name2 …" so the user
// sees the initial state without scrolling through every code.

const ASTOCK_JQ: &str = r#"
if .event == "heartbeat" then empty
elif .event == "snapshot" then
  .data as $d
  | if ($d.count // 0) > 0 then
      "[snapshot] \($d.filter) 命中 \($d.count) 只: " +
      ([$d.codes[] | "\(.code) \(.name)"] | join(", "))
    else
      "[snapshot] \($d.filter) 暂无命中"
    end
elif .event == "hit" then
  .data as $d
  # Unit conventions observed in the wire schema:
  #   quick_rally.diag.pct_change    : already in percent (e.g. 7.5 means +7.5%)
  #   quick_deadtogold.prev_day_pct  : decimal fraction (0.075 means +7.5%) — *100
  #   quick_cow_catch.today_pct      : decimal fraction (same as above)        — *100
  # The `(x * 100 | floor) / 100` idiom truncates to 2 decimal places.
  | if $d.filter == "quick_rally" then
      "🔔 \($d.code) \($d.name) 拉升 \(($d.diag.pct_change * 100 | floor) / 100)% (close \($d.diag.close), 10min前 \($d.diag.ref_price))"
    elif $d.filter == "quick_goldcross" then
      "🔔 \($d.code) \($d.name) 第3次金叉 (DIF=\($d.diag.current_macd), DEA=\($d.diag.current_signal))"
    elif $d.filter == "quick_deadtogold" then
      "🔔 \($d.code) \($d.name) 死叉后金叉 (昨涨 \((($d.diag.prev_day_pct // 0) * 100 * 100 | floor) / 100)%)"
    elif $d.filter == "quick_cow_catch" then
      "🔔 \($d.code) \($d.name) 牛股捕手 (今 \((($d.diag.today_pct // 0) * 100 * 100 | floor) / 100)%, MACD柱新高)"
    else
      "🔔 \($d.code) \($d.name) [\($d.filter)]"
    end
elif .event == "drop" then
  .data as $d
  # current_pct also arrives in percent units (consistent with rally's
  # pct_change, where it originated).
  | "📉 \($d.code) \($d.name) 退出 (now \((($d.current_pct // 0) * 100 | floor) / 100)%)"
elif .event == "stale_reset" then
  "♻️ stream resync: 错过部分事件，下面是最新状态"
elif .event == "reconnected" then
  "✅ stream 恢复 (suppressed \(.data.suppressed_errors // 0) errors)"
elif .event == "error" then
  "⚠️ \(.data.msg // "stream error")"
else
  "[\(.event)] \(.data)"
end
"#;

static ASTOCK: Template = Template {
    // Defensive: even though the jq expression drops heartbeats with
    // `empty`, the upstream event filter saves the cost of parsing
    // every heartbeat through the full jq program.
    event_filter: Some("!heartbeat"),
    jq: Some(ASTOCK_JQ),
};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gateway::watch::jq::CompiledJq;
    use serde_json::json;

    fn run_astock(input: serde_json::Value) -> Vec<String> {
        let f = CompiledJq::compile(ASTOCK_JQ).expect("astock jq compiles");
        f.run(&input)
    }

    #[test]
    fn astock_hit_quick_rally() {
        // Real wire format from quick_stream: pct_change arrives in
        // percent units (7.5 = +7.5%), NOT as a decimal fraction.
        let out = run_astock(json!({
            "event": "hit",
            "data": {
                "code": "600192",
                "name": "长城电工",
                "filter": "quick_rally",
                "diag": {
                    "close": 9.6,
                    "pct_change": 7.502799552071668,
                    "ref_price": 8.93,
                    "ref_ts": 1778723340
                },
                "ts": 1778723945
            }
        }));
        assert_eq!(out.len(), 1);
        assert!(out[0].contains("600192"));
        assert!(out[0].contains("长城电工"));
        assert!(out[0].contains("拉升 7.5"), "got: {}", out[0]);
        assert!(out[0].contains("close 9.6"));
    }

    #[test]
    fn astock_drop_event() {
        let out = run_astock(json!({
            "event": "drop",
            "data": {
                "code": "600192",
                "name": "长城电工",
                "current_close": 9.4,
                "current_pct": 5.23,
                "ts": 1778724000
            }
        }));
        assert_eq!(out.len(), 1);
        assert!(out[0].contains("📉"));
        assert!(out[0].contains("600192"));
        // current_pct is already in percent units — verify the 2-decimal
        // truncation works as advertised.
        assert!(out[0].contains("now 5.23%"), "got: {}", out[0]);
    }

    #[test]
    fn astock_hit_goldcross() {
        let out = run_astock(json!({
            "event": "hit",
            "data": {
                "code": "601225",
                "name": "陕西煤业",
                "filter": "quick_goldcross",
                "diag": {
                    "current_macd": -0.0938,
                    "current_signal": -0.0955
                },
                "ts": 1778730188
            }
        }));
        assert_eq!(out.len(), 1);
        assert!(out[0].contains("第3次金叉"), "got: {}", out[0]);
        assert!(out[0].contains("601225"));
    }

    #[test]
    fn astock_snapshot_with_hits() {
        let out = run_astock(json!({
            "event": "snapshot",
            "data": {
                "filter": "quick_goldcross",
                "codes": [
                    {"code": "601225", "name": "陕西煤业", "diag": {}},
                    {"code": "002327", "name": "富安娜", "diag": {}}
                ],
                "count": 2,
                "ts": 1778730188
            }
        }));
        assert_eq!(out.len(), 1);
        assert!(out[0].contains("命中 2 只"), "got: {}", out[0]);
        assert!(out[0].contains("陕西煤业"));
        assert!(out[0].contains("富安娜"));
    }

    #[test]
    fn astock_snapshot_empty() {
        let out = run_astock(json!({
            "event": "snapshot",
            "data": {"filter": "quick_rally", "codes": [], "count": 0, "ts": 1}
        }));
        assert_eq!(out, vec!["[snapshot] quick_rally 暂无命中".to_owned()]);
    }

    #[test]
    fn astock_heartbeat_dropped() {
        let out = run_astock(json!({
            "event": "heartbeat",
            "data": {"ts": 1, "active": 4}
        }));
        assert!(out.is_empty(), "heartbeat must drop, got: {out:?}");
    }

    #[test]
    fn astock_unknown_event_falls_back() {
        let out = run_astock(json!({
            "event": "weird",
            "data": {"a": 1}
        }));
        assert_eq!(out.len(), 1);
        assert!(out[0].starts_with("[weird]"), "got: {}", out[0]);
    }

    #[test]
    fn lookup_unknown_template_errors() {
        let err = lookup("does-not-exist").unwrap_err();
        assert!(err.to_string().contains("unknown template"));
    }

    #[test]
    fn lookup_astock_works() {
        let t = lookup("astock").unwrap();
        assert_eq!(t.event_filter, Some("!heartbeat"));
        assert!(t.jq.is_some());
    }
}
