//! 实时行情

use serde::{Deserialize, Serialize};

/// 实时行情快照
///
/// 字段说明:
/// - `ts_code`: tushare 格式代码, 例如 "000001.SZ"
/// - `name`: 股票名称 (**必须来自数据源, 禁止 LLM 重写**)
/// - `price`: 当前价 (元)
/// - `change_pct`: 涨跌幅 (%)
/// - `volume`: 成交量 (手)
/// - `amount`: 成交额 (万元)
/// - `turnover_rate`: 换手率 (%)
/// - `circ_mv`: 流通市值 (亿元)
/// - `total_mv`: 总市值 (亿元)
///
/// 注意: 不同数据源返回的字段集合不同, 缺失字段为 `None`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Quote {
    pub ts_code: String,
    pub name: String,
    pub price: f64,
    pub open: Option<f64>,
    pub high: Option<f64>,
    pub low: Option<f64>,
    pub pre_close: Option<f64>,
    pub change: Option<f64>,
    pub change_pct: f64,
    pub volume: Option<f64>,
    pub amount: Option<f64>,
    pub turnover_rate: Option<f64>,
    pub circ_mv: Option<f64>,
    pub total_mv: Option<f64>,
    /// 时间戳 (秒, Unix epoch)
    pub ts: Option<i64>,
}

impl Quote {
    /// 创建一个最小化的 Quote (只填必填字段)
    pub fn new(ts_code: impl Into<String>, name: impl Into<String>, price: f64, change_pct: f64) -> Self {
        Self {
            ts_code: ts_code.into(),
            name: name.into(),
            price,
            change_pct,
            open: None,
            high: None,
            low: None,
            pre_close: None,
            change: None,
            volume: None,
            amount: None,
            turnover_rate: None,
            circ_mv: None,
            total_mv: None,
            ts: None,
        }
    }
}
