//! Integration tests for HEARTBEAT.md parsing.

use rsclaw::heartbeat::parse_heartbeat_md;
use chrono::Timelike;
use std::time::Duration;

#[test]
fn parse_openclaw_compatible_format() {
    let raw = r#"---
every: 30m
active_hours: 09:15-15:05
timezone: Asia/Shanghai
---

# 心跳任务 —— 后台主动检查清单

## 市场异动监控
- 持仓股是否有涨跌幅 > 5% 的异动？
- 自选池是否有放量突破（成交量 > 5日均量 2倍）？

## 风控警报
- 持仓股是否触及止损位？
- 当日累计亏损是否超过 3%？

## 返回格式
- **无异常**：返回 `HEARTBEAT_OK`
- **有警报**：直接返回警报内容
"#;

    let spec = parse_heartbeat_md(raw).unwrap();
    assert_eq!(spec.every, Duration::from_secs(1800));
    assert!(spec.active_hours.is_some());
    let (start, end) = spec.active_hours.unwrap();
    assert_eq!(start.hour(), 9);
    assert_eq!(start.minute(), 15);
    assert_eq!(end.hour(), 15);
    assert_eq!(end.minute(), 5);
    assert!(spec.content.contains("市场异动监控"));
    assert!(spec.content.contains("HEARTBEAT_OK"));
}

#[test]
fn parse_minimal_heartbeat() {
    let raw = "---\nevery: 1h\n---\n- Check system health";
    let spec = parse_heartbeat_md(raw).unwrap();
    assert_eq!(spec.every, Duration::from_secs(3600));
    assert!(spec.active_hours.is_none());
    assert!(spec.content.contains("Check system health"));
}

#[test]
fn parse_without_frontmatter_fails() {
    let raw = "# Just a checklist\n- Item 1\n- Item 2";
    assert!(parse_heartbeat_md(raw).is_err());
}
