//! 股票基础信息

use serde::{Deserialize, Serialize};

/// 股票基础信息
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Stock {
    /// tushare 格式代码, 例如 "000001.SZ"
    pub ts_code: String,
    /// 股票名称
    pub name: String,
    /// 所属行业
    pub industry: Option<String>,
    /// 市场: SH / SZ / BJ
    pub market: Option<String>,
    /// 上市日期 (YYYYMMDD)
    pub list_date: Option<String>,
    /// 是否 ST
    pub is_st: bool,
    /// 是否退市
    pub is_delisted: bool,
}

impl Stock {
    /// 是否是科创板 (688 开头)
    pub fn is_star(&self) -> bool {
        self.ts_code.starts_with("688")
    }

    /// 是否是北交所 (920 / 8 开头)
    pub fn is_bse(&self) -> bool {
        self.ts_code.starts_with("920") || self.ts_code.starts_with('8')
    }

    /// 是否创业板 (300 / 301 开头)
    pub fn is_chinext(&self) -> bool {
        self.ts_code.starts_with("300") || self.ts_code.starts_with("301")
    }

    /// 从 ts_code 推断市场
    pub fn infer_market(ts_code: &str) -> &'static str {
        if ts_code.ends_with(".SH") {
            "SH"
        } else if ts_code.ends_with(".BJ") {
            "BJ"
        } else {
            "SZ"
        }
    }
}
