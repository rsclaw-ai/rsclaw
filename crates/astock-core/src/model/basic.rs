//! 基本面数据 (市盈率/市净率/换手率等)
//!
//! 预留, 第 1 期启用.

use serde::{Deserialize, Serialize};

/// 每日基本面指标
///
/// 对应 tushare `daily_basic` 接口.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Basic {
    pub ts_code: String,
    pub trade_date: String,
    /// 换手率 (%)
    pub turnover_rate: Option<f64>,
    /// 量比
    pub volume_ratio: Option<f64>,
    /// 市盈率 (动态)
    pub pe: Option<f64>,
    /// 市净率
    pub pb: Option<f64>,
    /// 总市值 (万元)
    pub total_mv: Option<f64>,
    /// 流通市值 (万元)
    pub circ_mv: Option<f64>,
}
