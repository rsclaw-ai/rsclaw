//! 东方财富 HTTP 数据源
//!
//! 接口说明:
//! - 实时行情: `https://push2.eastmoney.com/api/qt/stock/get?secid=${market}.${code}&fields=...`
//! - 批量行情: `https://push2.eastmoney.com/api/qt/stock/list?secids=...&fields=...`
//!
//! 特点:
//! - 免费, 无需 token
//! - 返回 JSON 格式 (比新浪/腾讯更易解析)
//! - 代码格式: `1.600519` (1=沪市) 或 `0.000001` (0=深市)
//! - 提供 **换手率** (turnoverRate) 和 **市值** (f140/f141)
//!
//! 单位注意:
//! - `volume`: 成交量, **手**
//! - `amount`: 成交额, **元**
//! - `f140`: 流通市值, **元** (需 ÷1e8 得亿元)
//! - `f141`: 总市值, **元**

use anyhow::{Context, Result};
use serde::Deserialize;
use tracing::debug;

use crate::capability::HttpCapability;
use crate::model::Quote;

/// 东财 HTTP 数据源
pub struct EastmoneyHttpSource {
    http: std::sync::Arc<dyn HttpCapability>,
}

impl EastmoneyHttpSource {
    pub fn new(http: std::sync::Arc<dyn HttpCapability>) -> Self {
        Self { http }
    }

    /// 获取实时行情 (单只)
    ///
    /// 输入 tushare 代码 (如 "600519.SH"), 返回 Quote.
    pub async fn quote(&self, ts_code: &str) -> Result<Quote> {
        let secid = convert_tushare_code_to_secid(ts_code);
        // 字段说明: f43=最新价, f44=最高价, f45=最低价, f46=开盘价,
        // f47=成交量, f48=成交额, f49=昨收, f50=涨跌, f51=涨幅,
        // f52=换手率, f55=名称, f57=代码, f58=市场, f140=流通市值, f141=总市值
        let fields = "f43,f44,f45,f46,f47,f48,f49,f50,f51,f52,f55,f57,f58,f140,f141";
        let url = format!(
            "https://push2.eastmoney.com/api/qt/stock/get?secid={secid}&fields={fields}",
        );

        debug!(ts_code, secid, "eastmoney quote");

        let value = self.http.get_json(&url).await?;
        let resp: EastmoneyQuoteResp = serde_json::from_value(value)
            .context("eastmoney quote: parse response")?;

        let data = resp.data.context("eastmoney quote: empty response")?;

        // f55 是名称
        let name = data.f55.as_str().context("missing name")?;

        // f43 是最新价 (需要 ÷100, 因为东财返回的是整数形式)
        // 参考: https://quote.eastmoney.com/concept/
        let price = data.f43.map(|v| v as f64 / 100.0).context("missing price")?;
        let high = data.f44.map(|v| v as f64 / 100.0);
        let low = data.f45.map(|v| v as f64 / 100.0);
        let open = data.f46.map(|v| v as f64 / 100.0);
        let pre_close = data.f49.map(|v| v as f64 / 100.0);

        // f51 是涨幅 (直接是百分比数值, 例如 3.77)
        let change_pct = data.f51.unwrap_or(0.0);

        // f47 是成交量 (手), f48 是成交额 (元)
        let volume = data.f47.map(|v| v as f64);
        let amount = data.f48.map(|v| v as f64);

        // f52 是换手率 (%)
        let turnover_rate = data.f52;

        // f140/f141 是市值 (元), 转换为亿元
        let circ_mv = data.f140.map(|v| v as f64 / 1e8);
        let total_mv = data.f141.map(|v| v as f64 / 1e8);

        Ok(Quote {
            ts_code: ts_code.to_string(),
            name: name.to_string(),
            price,
            open,
            high,
            low,
            pre_close,
            change: Some(price - pre_close.unwrap_or(0.0)),
            change_pct,
            volume,
            amount,
            turnover_rate,
            circ_mv,
            total_mv,
            ts: None,
        })
    }

    /// 获取实时行情 (多只)
    ///
    /// 输入 tushare 代码列表, 返回 Quote 列表.
    pub async fn quotes(&self, ts_codes: &[&str]) -> Result<Vec<Quote>> {
        if ts_codes.is_empty() {
            return Ok(vec![]);
        }

        let secids: Vec<String> = ts_codes.iter().map(|c| convert_tushare_code_to_secid(c)).collect();
        let secids_str = secids.join(",");
        let fields = "f43,f44,f45,f46,f47,f48,f49,f50,f51,f52,f55,f57,f58,f140,f141";
        let url = format!(
            "https://push2.eastmoney.com/api/qt/stock/list?secids={secids_str}&fields={fields}",
        );

        debug!(codes = ts_codes.len(), "eastmoney quotes batch");

        let value = self.http.get_json(&url).await?;
        let resp: EastmoneyQuoteListResp = serde_json::from_value(value)
            .context("eastmoney quotes: parse response")?;

        let data = resp.data.context("eastmoney quotes: empty response")?;

        let mut quotes = Vec::with_capacity(data.len());
        for item in data {
            if let Ok(q) = self.parse_quote_item(&item) {
                quotes.push(q);
            }
        }

        Ok(quotes)
    }

    /// 解析单个行情项
    fn parse_quote_item(&self, item: &EastmoneyQuoteItem) -> Result<Quote> {
        let name = item.f55.as_str().context("missing name")?;
        let code = item.f57.as_str().context("missing code")?;

        // 找到对应的 ts_code
        let secid_market = item.f58.as_i64().context("missing market")?;
        let market_suffix = if secid_market == 1 { "SH" } else { "SZ" };
        let ts_code = format!("{}.{}", code, market_suffix);

        let price = item.f43.map(|v| v as f64 / 100.0).context("missing price")?;
        let pre_close = item.f49.map(|v| v as f64 / 100.0);
        let change_pct = item.f51.unwrap_or(0.0);

        Ok(Quote {
            ts_code,
            name: name.to_string(),
            price,
            open: item.f46.map(|v| v as f64 / 100.0),
            high: item.f44.map(|v| v as f64 / 100.0),
            low: item.f45.map(|v| v as f64 / 100.0),
            pre_close,
            change: Some(price - pre_close.unwrap_or(0.0)),
            change_pct,
            volume: item.f47.map(|v| v as f64),
            amount: item.f48.map(|v| v as f64),
            turnover_rate: item.f52,
            circ_mv: item.f140.map(|v| v as f64 / 1e8),
            total_mv: item.f141.map(|v| v as f64 / 1e8),
            ts: None,
        })
    }
}

/// 东财 API 响应 (单只)
#[derive(Debug, Deserialize)]
struct EastmoneyQuoteResp {
    data: Option<EastmoneyQuoteItem>,
}

/// 东财 API 响应 (多只)
#[derive(Debug, Deserialize)]
struct EastmoneyQuoteListResp {
    data: Option<Vec<EastmoneyQuoteItem>>,
}

/// 东财行情字段
///
/// 字段编号对应东财内部定义, 参考:
/// https://quote.eastmoney.com/sh600519.html
#[derive(Debug, Deserialize)]
struct EastmoneyQuoteItem {
    /// 最新价 (需 ÷100)
    f43: Option<i64>,
    /// 最高价 (需 ÷100)
    f44: Option<i64>,
    /// 最低价 (需 ÷100)
    f45: Option<i64>,
    /// 开盘价 (需 ÷100)
    f46: Option<i64>,
    /// 成交量 (手)
    f47: Option<i64>,
    /// 成交额 (元)
    f48: Option<i64>,
    /// 昨收价 (需 ÷100)
    f49: Option<i64>,
    /// 涨跌 (元)
    #[allow(dead_code)]
    f50: Option<i64>,
    /// 涨幅 (%)
    f51: Option<f64>,
    /// 换手率 (%)
    f52: Option<f64>,
    /// 股票名称
    f55: serde_json::Value,
    /// 股票代码
    f57: serde_json::Value,
    /// 市场 (1=沪市, 0=深市)
    f58: serde_json::Value,
    /// 流通市值 (元)
    f140: Option<i64>,
    /// 总市值 (元)
    f141: Option<i64>,
}

/// 转换 tushare 代码 -> 东财 secid
///
/// 600519.SH -> 1.600519
/// 000001.SZ -> 0.000001
fn convert_tushare_code_to_secid(ts_code: &str) -> String {
    if let Some(pos) = ts_code.find('.') {
        let code = &ts_code[..pos];
        let market = &ts_code[pos + 1..];
        match market {
            "SH" => format!("1.{}", code),
            "SZ" => format!("0.{}", code),
            "BJ" => format!("0.{}", code), // 北交所也用 0
            _ => ts_code.to_string(),
        }
    } else {
        ts_code.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_convert_tushare_to_secid() {
        assert_eq!(convert_tushare_code_to_secid("600519.SH"), "1.600519");
        assert_eq!(convert_tushare_code_to_secid("000001.SZ"), "0.000001");
        assert_eq!(convert_tushare_code_to_secid("430001.BJ"), "0.430001");
    }
}