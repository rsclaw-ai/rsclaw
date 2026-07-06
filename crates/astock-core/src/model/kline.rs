//! K 线数据

use serde::{Deserialize, Serialize};

/// K 线周期
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum KlinePeriod {
    /// 日线
    Day,
    /// 周线
    Week,
    /// 月线
    Month,
    /// 1 分钟
    Min1,
    /// 5 分钟
    Min5,
    /// 15 分钟
    Min15,
    /// 30 分钟
    Min30,
    /// 60 分钟
    Min60,
}

impl KlinePeriod {
    /// tushare 的 freq 参数
    pub fn as_tushare_freq(&self) -> &'static str {
        match self {
            KlinePeriod::Day => "D",
            KlinePeriod::Week => "W",
            KlinePeriod::Month => "M",
            KlinePeriod::Min1 => "1min",
            KlinePeriod::Min5 => "5min",
            KlinePeriod::Min15 => "15min",
            KlinePeriod::Min30 => "30min",
            KlinePeriod::Min60 => "60min",
        }
    }
}

impl std::fmt::Display for KlinePeriod {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            KlinePeriod::Day => f.write_str("day"),
            KlinePeriod::Week => f.write_str("week"),
            KlinePeriod::Month => f.write_str("month"),
            KlinePeriod::Min1 => f.write_str("1min"),
            KlinePeriod::Min5 => f.write_str("5min"),
            KlinePeriod::Min15 => f.write_str("15min"),
            KlinePeriod::Min30 => f.write_str("30min"),
            KlinePeriod::Min60 => f.write_str("60min"),
        }
    }
}

/// 单根 K 线
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Kline {
    /// 交易日期 (YYYYMMDD)
    pub trade_date: String,
    pub open: f64,
    pub high: f64,
    pub low: f64,
    pub close: f64,
    /// 成交量 (手)
    pub volume: f64,
    /// 成交额 (万元)
    pub amount: f64,
    /// 涨跌幅 (%)
    pub change_pct: Option<f64>,
    /// 复权类型 (qfq/hfq/none)
    pub adjust: Option<String>,
}

impl Kline {
    /// 收盘价高于开盘价 (阳线)
    pub fn is_bullish(&self) -> bool {
        self.close > self.open
    }

    /// 振幅 (%)
    pub fn amplitude(&self) -> f64 {
        if self.open == 0.0 { return 0.0; }
        (self.high - self.low) / self.open * 100.0
    }
}
