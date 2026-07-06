//! 报告模型

use serde::{Deserialize, Serialize};

/// 报告类型
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReportKind {
    /// 选股报告 (stock_select tool 输出)
    Selection,
    /// 龙虎榜分析
    Longhu,
    /// 公告/研报
    News,
    /// 多空辩论
    Debate,
    /// 走势预测
    Forecast,
}

/// 选股报告
///
/// 对应 `tool/select.rs` 的输出. 包含:
/// - 最终选出的股票列表
/// - 过滤统计 (每步过滤后剩余数量)
/// - 预渲染 markdown (直接发给用户, 不经 LLM 重写)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SelectionReport {
    /// 交易日期 (YYYYMMDD)
    pub trade_date: String,
    /// 策略版本 (例如 "v2.0 final")
    pub strategy: String,
    /// 选出的股票
    pub stocks: Vec<SelectedStock>,
    /// 过滤统计
    pub filter_stats: Option<FilterStats>,
    /// 预渲染 markdown (直接发给用户)
    pub markdown: String,
}

/// 选出的单只股票
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SelectedStock {
    /// 排名 (1-based)
    pub rank: usize,
    pub ts_code: String,
    /// 名称 (**必须来自数据源**)
    pub name: String,
    pub industry: Option<String>,
    /// 收盘价
    pub close: f64,
    /// 涨跌幅 (%)
    pub pct_chg: f64,
    /// 换手率 (%)
    pub turnover_rate: f64,
    /// 流通市值 (亿元)
    pub circ_mv: f64,
    /// 成交额 (万元)
    pub amount: f64,
    /// 综合分数 (0-1)
    pub score: f64,
    /// boost 命中 (sector_hot / turnover_high / ann:xxx 等)
    pub boost_hits: Vec<String>,
    /// boost 后分数
    pub boosted_score: Option<f64>,
}

/// 过滤统计 (用于报告摘要)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FilterStats {
    /// 初始股票数
    pub initial: usize,
    /// 排除 ST 后
    pub after_exclude_st: usize,
    /// 排除科创板/北交所后
    pub after_exclude_star: usize,
    /// 排除新股后
    pub after_exclude_new: usize,
    /// 涨幅过滤后
    pub after_change_pct: usize,
    /// 换手率过滤后
    pub after_turnover: usize,
    /// 流通市值过滤后
    pub after_circ_mv: usize,
    /// 成交额过滤后
    pub after_amount: usize,
    /// 最终选出
    pub final_count: usize,
}

/// 分析报告 (龙虎榜 / 公告 / 辩论 / 预测)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AnalysisReport {
    pub kind: ReportKind,
    pub trade_date: String,
    /// 报告主题 (例如 "600519 多空辩论")
    pub title: String,
    /// 报告正文 (预渲染 markdown)
    pub markdown: String,
    /// 额外数据 (不同 kind 结构不同)
    pub extra: Option<serde_json::Value>,
}
