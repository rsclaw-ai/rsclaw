//! 龙虎榜分析 tool
//!
//! 整合 CDP 数据源, 提供龙虎榜查询和分析:
//! - 按日期查询龙虎榜 (`longhubang`)
//! - 按股票查询龙虎榜历史 (`longhubang_by_code`)
//! - 龙虎榜统计分析 (`longhubang_stats`)
//!
//! 数据来源: 东方财富网站 (CDP 抓取)
//!
//! 使用示例:
//! ```rust,compile_fail
//! use astock_core::{StockEngine, capability::*};
//!
//! # async fn example() -> anyhow::Result<()> {
//! let engine = StockEngine::builder()
//!     .http(DefaultHttp::new()?)
//!     .cdp(RsclawCdp::new())  // 需要真实 CDP
//!     .config(StaticConfig::new())
//!     .build()?;
//!
//! // 查询今日龙虎榜
//! let lhb = engine.lhb("2026-07-01").await?;
//! for item in &lhb.items {
//!     println!("{} {} 净买:{:.2}亿 涨幅:{:.2}%",
//!         item.ts_code, item.name, item.net_buy_yi, item.change_pct);
//! }
//! # Ok(())
//! # }
//! ```

use std::sync::Arc;
use anyhow::Result;
use serde::Serialize;

use crate::capability::CdpCapability;
use crate::source::eastmoney_cdp::{EastmoneyCdpSource, LonghubangItem};
use crate::model::report::{AnalysisReport, ReportKind};

/// 龙虎榜查询器
pub struct LonghubangQuery {
    cdp: Arc<dyn CdpCapability>,
}

impl LonghubangQuery {
    pub fn new(cdp: Arc<dyn CdpCapability>) -> Self {
        Self { cdp }
    }

    /// 查询指定日期的龙虎榜
    ///
    /// 返回龙虎榜列表 + 统计信息
    pub async fn query(&self, date: &str) -> Result<LonghubangReport> {
        let source = EastmoneyCdpSource::new(Arc::clone(&self.cdp));
        let items = source.longhubang(date).await?;

        // 计算统计信息
        let stats = LonghubangStats::from_items(&items);

        Ok(LonghubangReport {
            date: date.to_string(),
            items,
            stats,
        })
    }

    /// 查询指定股票的龙虎榜历史 (最近 N 次)
    ///
    /// 需要在龙虎榜页面搜索股票代码
    pub async fn query_by_code(&self, _ts_code: &str, _limit: usize) -> Result<Vec<LonghubangItem>> {
        // TODO: 实现股票龙虎榜历史查询
        // 需要在龙虎榜页面搜索股票代码, 然后提取历史记录
        anyhow::bail!("longhubang_by_code not implemented yet")
    }

    /// 生成龙虎榜报告 (AnalysisReport)
    ///
    /// 包含 markdown 预渲染
    pub async fn report(&self, date: &str) -> Result<AnalysisReport> {
        let lhb = self.query(date).await?;
        let markdown = render_longhubang_report(&lhb);

        Ok(AnalysisReport {
            kind: ReportKind::Longhu,
            trade_date: date.to_string(),
            title: format!("龙虎榜分析 - {}", date),
            markdown,
            extra: Some(serde_json::to_value(&lhb)?),
        })
    }
}

/// 龙虎榜报告
#[derive(Debug, Clone, Serialize)]
pub struct LonghubangReport {
    pub date: String,
    pub items: Vec<LonghubangItem>,
    pub stats: LonghubangStats,
}

/// 龙虎榜统计
#[derive(Debug, Clone, Serialize)]
pub struct LonghubangStats {
    /// 上榜股票总数
    pub total_count: usize,
    /// 平均净买额 (亿元)
    pub avg_net_buy_yi: f64,
    /// 最大净买额 (亿元)
    pub max_net_buy_yi: f64,
    /// 最小净买额 (亿元)
    pub min_net_buy_yi: f64,
    /// 平均涨幅 (%)
    pub avg_change_pct: f64,
    /// 平均换手率 (%)
    pub avg_turnover_rate: f64,
}

impl LonghubangStats {
    fn from_items(items: &[LonghubangItem]) -> Self {
        if items.is_empty() {
            return Self {
                total_count: 0,
                avg_net_buy_yi: 0.0,
                max_net_buy_yi: 0.0,
                min_net_buy_yi: 0.0,
                avg_change_pct: 0.0,
                avg_turnover_rate: 0.0,
            };
        }

        let net_buys: Vec<f64> = items.iter().map(|i| i.net_buy_yi).collect();
        let change_pcts: Vec<f64> = items.iter().map(|i| i.change_pct).collect();
        let turnovers: Vec<f64> = items.iter().map(|i| i.turnover_rate).collect();

        // f64 不实现 Ord, 用 reduce_by 比较
        let max_net_buy = net_buys.iter().reduce(|a, b| if a > b { a } else { b }).copied().unwrap_or(0.0);
        let min_net_buy = net_buys.iter().reduce(|a, b| if a < b { a } else { b }).copied().unwrap_or(0.0);

        Self {
            total_count: items.len(),
            avg_net_buy_yi: net_buys.iter().sum::<f64>() / net_buys.len() as f64,
            max_net_buy_yi: max_net_buy,
            min_net_buy_yi: min_net_buy,
            avg_change_pct: change_pcts.iter().sum::<f64>() / change_pcts.len() as f64,
            avg_turnover_rate: turnovers.iter().sum::<f64>() / turnovers.len() as f64,
        }
    }
}

/// 渲染龙虎榜报告为 markdown
fn render_longhubang_report(lhb: &LonghubangReport) -> String {
    let mut out = String::with_capacity(4096);

    out.push_str("# 龙虎榜分析\n\n");
    out.push_str(&format!("**日期**: {}\n\n", lhb.date));

    // 统计摘要
    out.push_str("## 统计摘要\n\n");
    out.push_str(&format!("- 上榜股票数: {}\n", lhb.stats.total_count));
    out.push_str(&format!("- 平均净买额: {:.2} 亿\n", lhb.stats.avg_net_buy_yi));
    out.push_str(&format!("- 最大净买额: {:.2} 亿\n", lhb.stats.max_net_buy_yi));
    out.push_str(&format!("- 最小净买额: {:.2} 亿\n", lhb.stats.min_net_buy_yi));
    out.push_str(&format!("- 平均涨幅: {:.2}%\n", lhb.stats.avg_change_pct));
    out.push_str(&format!("- 平均换手率: {:.2}%\n\n", lhb.stats.avg_turnover_rate));

    // 龙虎榜表格 (按净买额排序, 最多显示 20 只)
    out.push_str("## 龙虎榜明细 (净买额 TOP 20)\n\n");

    if lhb.items.is_empty() {
        out.push_str("今日无龙虎榜数据。\n\n");
    } else {
        out.push_str("| 排名 | 代码 | 名称 | 收盘价 | 涨幅% | 换手率% | 净买(亿) | 买入(万) | 卖出(万) | 上榜原因 |\n");
        out.push_str("|---:|:---|:---|---:|---:|---:|---:|---:|---:|:---|\n");

        // 按净买额排序
        let mut sorted_items = lhb.items.clone();
        sorted_items.sort_by(|a, b| b.net_buy_yi.partial_cmp(&a.net_buy_yi).unwrap_or(std::cmp::Ordering::Equal));

        for (rank, item) in sorted_items.iter().take(20).enumerate() {
            out.push_str(&format!(
                "| {} | {} | {} | {:.2} | {:.2} | {:.2} | {:.2} | {:.0} | {:.0} | {} |\n",
                rank + 1,
                item.ts_code,
                item.name,
                item.close,
                item.change_pct,
                item.turnover_rate,
                item.net_buy_yi,
                item.buy_amount_wan,
                item.sell_amount_wan,
                item.reason,
            ));
        }
        out.push('\n');
    }

    // 涨幅分布
    out.push_str("## 涨幅分布\n\n");
    let up_count = lhb.items.iter().filter(|i| i.change_pct > 0.0).count();
    let down_count = lhb.items.iter().filter(|i| i.change_pct < 0.0).count();
    let flat_count = lhb.items.len() - up_count - down_count;
    out.push_str(&format!("- 上涨: {} 只 ({:.1}%)\n", up_count, up_count as f64 / lhb.items.len() as f64 * 100.0));
    out.push_str(&format!("- 下跌: {} 只 ({:.1}%)\n", down_count, down_count as f64 / lhb.items.len() as f64 * 100.0));
    out.push_str(&format!("- 平盘: {} 只\n\n", flat_count));

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_longhubang_stats_empty() {
        let stats = LonghubangStats::from_items(&[]);
        assert_eq!(stats.total_count, 0);
        assert_eq!(stats.avg_net_buy_yi, 0.0);
    }

    #[test]
    fn test_longhubang_stats_basic() {
        let items = vec![
            LonghubangItem {
                ts_code: "600519.SH".into(),
                name: "茅台".into(),
                trade_date: "2026-07-01".into(),
                close: 1500.0,
                change_pct: 3.0,
                turnover_rate: 2.0,
                net_buy_wan: 50000.0,
                net_buy_yi: 5.0,
                buy_amount_wan: 80000.0,
                sell_amount_wan: 30000.0,
                reason: "涨停".into(),
            },
            LonghubangItem {
                ts_code: "000001.SZ".into(),
                name: "平安银行".into(),
                trade_date: "2026-07-01".into(),
                close: 10.0,
                change_pct: -2.0,
                turnover_rate: 5.0,
                net_buy_wan: -10000.0,
                net_buy_yi: -1.0,
                buy_amount_wan: 20000.0,
                sell_amount_wan: 30000.0,
                reason: "跌幅异常".into(),
            },
        ];

        let stats = LonghubangStats::from_items(&items);
        assert_eq!(stats.total_count, 2);
        assert_eq!(stats.avg_net_buy_yi, 2.0); // (5 + (-1)) / 2
        assert_eq!(stats.max_net_buy_yi, 5.0);
        assert_eq!(stats.min_net_buy_yi, -1.0);
        assert_eq!(stats.avg_change_pct, 0.5); // (3 + (-2)) / 2
        assert_eq!(stats.avg_turnover_rate, 3.5); // (2 + 5) / 2
    }

    #[test]
    fn test_render_longhubang_report_empty() {
        let lhb = LonghubangReport {
            date: "2026-07-01".into(),
            items: vec![],
            stats: LonghubangStats::from_items(&[]),
        };
        let md = render_longhubang_report(&lhb);
        assert!(md.contains("# 龙虎榜分析"));
        assert!(md.contains("今日无龙虎榜数据"));
    }

    #[test]
    fn test_render_longhubang_report_with_data() {
        let items = vec![
            LonghubangItem {
                ts_code: "600519.SH".into(),
                name: "贵州茅台".into(),
                trade_date: "2026-07-01".into(),
                close: 1500.0,
                change_pct: 3.77,
                turnover_rate: 2.5,
                net_buy_wan: 50000.0,
                net_buy_yi: 5.0,
                buy_amount_wan: 80000.0,
                sell_amount_wan: 30000.0,
                reason: "涨停".into(),
            },
        ];

        let stats = LonghubangStats::from_items(&items);
        let lhb = LonghubangReport {
            date: "2026-07-01".into(),
            items,
            stats,
        };

        let md = render_longhubang_report(&lhb);
        assert!(md.contains("贵州茅台"));
        assert!(md.contains("5.0"));
        assert!(md.contains("涨幅分布"));
    }
}