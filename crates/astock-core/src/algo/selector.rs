//! 选股算法 v2.0 final
//!
//! 翻译自 `workspace-multi-agent/skills/trading/stock-selector/selector_tushare_final.py`.
//!
//! 第 1 期实现核心公式 (不含历史数据修正项, 如 duckdb 的 3 日回撤/5 日缩量等):
//! - 基础过滤: ST / 科创板 / 北交所 / 新股 / 涨幅 / 换手率 / 流通市值 / 成交额
//! - 归一化评分: score_pct * 0.4 + score_turnover * 0.3 + score_amount * 0.3
//! - Top N 排序
//!
//! v2.2+ 的板块加分、历史修正等依赖 duckdb 历史数据, 留给第 2 期.

use std::collections::HashMap;
use anyhow::{Result, bail};

use crate::model::report::{FilterStats, SelectedStock, SelectionReport};
use crate::model::stock::Stock;
use crate::source::tushare::TushareSource;

use super::filter::{Candidate, exclude_new, exclude_st, exclude_star, filter_amount,
                    filter_change_pct, filter_circ_mv, filter_turnover, merge_into_candidates};
use super::scoring::score_candidates;

/// 选股策略配置 (v2.0 final)
///
/// 所有字段可选, `None` 表示使用默认值.
#[derive(Debug, Clone, Default)]
pub struct SelectionStrategy {
    /// 涨幅下限 (%)
    pub change_pct_min: Option<f64>,
    /// 涨幅上限 (%)
    pub change_pct_max: Option<f64>,
    /// 换手率下限 (%)
    pub turnover_rate_min: Option<f64>,
    /// 换手率上限 (%)
    pub turnover_rate_max: Option<f64>,
    /// 流通市值下限 (亿元)
    pub market_cap_min_yi: Option<f64>,
    /// 流通市值上限 (亿元)
    pub market_cap_max_yi: Option<f64>,
    /// 成交额下限 (万元)
    pub amount_min_wan: Option<f64>,
    /// 排除 ST
    pub exclude_st: Option<bool>,
    /// 排除科创板/北交所
    pub exclude_star: Option<bool>,
    /// 排除新股 (上市不足 N 天)
    pub exclude_new_days: Option<u32>,
    /// 选出数量
    pub select_count: Option<usize>,
    /// 策略标签 (用于报告展示)
    pub strategy_label: Option<String>,
}

impl SelectionStrategy {
    /// 全部填充默认值
    pub fn with_defaults(&self) -> ResolvedStrategy {
        ResolvedStrategy {
            change_pct_min: self.change_pct_min.unwrap_or(2.0),
            change_pct_max: self.change_pct_max.unwrap_or(4.0),
            turnover_rate_min: self.turnover_rate_min.unwrap_or(5.0),
            turnover_rate_max: self.turnover_rate_max.unwrap_or(10.0),
            market_cap_min_yi: self.market_cap_min_yi.unwrap_or(50.0),
            market_cap_max_yi: self.market_cap_max_yi.unwrap_or(200.0),
            amount_min_wan: self.amount_min_wan.unwrap_or(3000.0),
            exclude_st: self.exclude_st.unwrap_or(true),
            exclude_star: self.exclude_star.unwrap_or(true),
            exclude_new_days: self.exclude_new_days.unwrap_or(60),
            select_count: self.select_count.unwrap_or(10),
            strategy_label: self.strategy_label.clone().unwrap_or_else(|| "v2.0 final".into()),
        }
    }
}

/// 已解析 (无 None) 的策略
#[derive(Debug, Clone)]
pub struct ResolvedStrategy {
    pub change_pct_min: f64,
    pub change_pct_max: f64,
    pub turnover_rate_min: f64,
    pub turnover_rate_max: f64,
    pub market_cap_min_yi: f64,
    pub market_cap_max_yi: f64,
    pub amount_min_wan: f64,
    pub exclude_st: bool,
    pub exclude_star: bool,
    pub exclude_new_days: u32,
    pub select_count: usize,
    pub strategy_label: String,
}

/// 选股结果 (含中间统计)
pub struct SelectionResult {
    pub report: SelectionReport,
    pub stats: FilterStats,
}

/// 执行选股
///
/// 流程:
/// 1. tushare 拉: stock_basic / daily / daily_basic
/// 2. 合并 + 过滤
/// 3. 归一化评分
/// 4. Top N
/// 5. 构造 SelectionReport
pub async fn select(
    src: &TushareSource,
    strategy: &ResolvedStrategy,
) -> Result<SelectionResult> {
    // 1. 最近交易日
    let trade_date = src.latest_trade_date().await?;

    select_on_date(src, strategy, &trade_date).await
}

/// 在指定交易日执行选股 (用于测试或回放)
pub async fn select_on_date(
    src: &TushareSource,
    strategy: &ResolvedStrategy,
    trade_date: &str,
) -> Result<SelectionResult> {
    // 2. 拉数据 (三次 HTTP, 并行或串行都 OK, 串行简单)
    let stock_basic = src.stock_basic().await?;
    let dailies = src.daily(trade_date).await?;
    let basics = src.daily_basic(trade_date).await?;

    if dailies.is_empty() {
        bail!("no daily data for trade_date {trade_date}");
    }

    // 构造 ts_code -> Stock 索引 (带 name/list_date/industry)
    let stock_map: HashMap<String, Stock> = stock_basic
        .into_iter()
        .map(|s| {
            let stock = Stock {
                ts_code: s.ts_code.clone(),
                name: s.name.clone(),
                industry: s.industry.clone(),
                market: s.market.clone().or_else(|| {
                    Some(Stock::infer_market(&s.ts_code).to_string())
                }),
                list_date: s.list_date.clone(),
                is_st: s.name.contains("ST"),
                is_delisted: false,
            };
            (s.ts_code, stock)
        })
        .collect();

    let initial = basics.len();
    let mut stats = FilterStats {
        initial,
        after_exclude_st: 0,
        after_exclude_star: 0,
        after_exclude_new: 0,
        after_change_pct: 0,
        after_turnover: 0,
        after_circ_mv: 0,
        after_amount: 0,
        final_count: 0,
    };

    // 3. 合并日线 + 基本面 + stock_basic
    let cands = merge_into_candidates(&dailies, &basics, &stock_map);

    // 4. 过滤链 (与 Python 版顺序一致)
    let (cands, n) = exclude_st(cands);
    stats.after_exclude_st = n;

    let (cands, n) = exclude_star(cands);
    stats.after_exclude_star = n;

    let (cands, n) = exclude_new(cands, strategy.exclude_new_days);
    stats.after_exclude_new = n;

    let (cands, n) = filter_change_pct(cands, strategy.change_pct_min, strategy.change_pct_max);
    stats.after_change_pct = n;

    let (cands, n) = filter_turnover(cands, strategy.turnover_rate_min, strategy.turnover_rate_max);
    stats.after_turnover = n;

    let (cands, n) = filter_circ_mv(cands, strategy.market_cap_min_yi, strategy.market_cap_max_yi);
    stats.after_circ_mv = n;

    let (mut cands, n) = filter_amount(cands, strategy.amount_min_wan);
    stats.after_amount = n;

    // 5. 评分
    score_candidates(&mut cands);

    // 6. 排序 (降序)
    cands.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));

    // 7. Top N
    let top: Vec<Candidate> = cands.into_iter().take(strategy.select_count).collect();
    stats.final_count = top.len();

    // 8. 构造 SelectionReport
    let stocks: Vec<SelectedStock> = top
        .into_iter()
        .enumerate()
        .map(|(i, c)| SelectedStock {
            rank: i + 1,
            ts_code: c.ts_code,
            name: c.name,
            industry: c.industry,
            close: c.close,
            pct_chg: c.pct_chg,
            turnover_rate: c.turnover_rate,
            circ_mv: c.circ_mv_yi,
            amount: c.amount_wan,
            score: c.score,
            boost_hits: vec![],
            boosted_score: None,
        })
        .collect();

    let report = SelectionReport {
        trade_date: trade_date.to_string(),
        strategy: strategy.strategy_label.clone(),
        stocks,
        filter_stats: Some(stats.clone()),
        markdown: String::new(), // 由 engine 调用 render 填充
    };

    Ok(SelectionResult { report, stats })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_strategy_defaults() {
        let s = SelectionStrategy::default().with_defaults();
        assert_eq!(s.change_pct_min, 2.0);
        assert_eq!(s.change_pct_max, 4.0);
        assert_eq!(s.turnover_rate_min, 5.0);
        assert_eq!(s.turnover_rate_max, 10.0);
        assert_eq!(s.market_cap_min_yi, 50.0);
        assert_eq!(s.market_cap_max_yi, 200.0);
        assert_eq!(s.amount_min_wan, 3000.0);
        assert_eq!(s.exclude_st, true);
        assert_eq!(s.exclude_star, true);
        assert_eq!(s.exclude_new_days, 60);
        assert_eq!(s.select_count, 10);
        assert_eq!(s.strategy_label, "v2.0 final");
    }

    #[test]
    fn test_strategy_override() {
        let s = SelectionStrategy {
            change_pct_min: Some(1.0),
            select_count: Some(5),
            ..Default::default()
        }
        .with_defaults();
        assert_eq!(s.change_pct_min, 1.0);
        assert_eq!(s.change_pct_max, 4.0); // default
        assert_eq!(s.select_count, 5);
    }

    /// 构造一批候选, 跑 filter+score 链路, 验证 Top N 排序正确.
    #[test]
    fn test_filter_and_score_pipeline() {
        let strategy = SelectionStrategy::default().with_defaults();
        // 10 只候选, 都满足基础过滤, 但分数不同
        let mut cands: Vec<Candidate> = (0..10)
            .map(|i| Candidate {
                ts_code: format!("00000{i}.SZ"),
                name: format!("股票{i}"),
                industry: None,
                list_date: Some("20200101".into()),
                pct_chg: 2.0 + (i as f64) * 0.2,         // 2.0, 2.2, ... 3.8
                turnover_rate: 5.0 + (i as f64) * 0.5,   // 5.0, 5.5, ... 9.5
                circ_mv_yi: 100.0,
                amount_wan: 10000.0 + (i as f64) * 5000.0, // 10k, 15k, ... 55k
                close: 10.0,
                score: 0.0,
            })
            .collect();

        let initial = cands.len();
        let (c, n) = exclude_st(cands); assert_eq!(n, initial);
        let (c, n) = exclude_star(c); assert_eq!(n, initial);
        let (c, n) = exclude_new(c, 60); assert_eq!(n, initial);
        let (c, n) = filter_change_pct(c, strategy.change_pct_min, strategy.change_pct_max);
        assert_eq!(n, initial);
        let (c, n) = filter_turnover(c, strategy.turnover_rate_min, strategy.turnover_rate_max);
        assert_eq!(n, initial);
        let (c, n) = filter_circ_mv(c, strategy.market_cap_min_yi, strategy.market_cap_max_yi);
        assert_eq!(n, initial);
        let (mut c, n) = filter_amount(c, strategy.amount_min_wan);
        assert_eq!(n, initial);

        score_candidates(&mut c);

        c.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));
        let top3: Vec<_> = c.into_iter().take(3).collect();

        // 涨幅/换手/成交额都按索引递增, 所以分数也递增, top3 应该是最大的 3 个
        assert_eq!(top3[0].ts_code, "000009.SZ");
        assert_eq!(top3[1].ts_code, "000008.SZ");
        assert_eq!(top3[2].ts_code, "000007.SZ");
    }
}
