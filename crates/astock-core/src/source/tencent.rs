//! 腾讯财经 HTTP 数据源
//!
//! 接口说明:
//! - 实时行情: `https://web.sqt.gtimg.cn/q=${code}` (GBK 编码)
//! - 批量查询: `https://web.sqt.gtimg.cn/q=${code1},${code2},...`
//!
//! 特点:
//! - 免费, 无需 token
//! - 返回字段与新浪类似, 但格式不同
//! - 代码格式: `sh600519` 或 `sz000001`
//!
//! 单位注意:
//! - `volume`: 成交量, **手**
//! - `amount`: 成交额, **元** (与新浪一致)

use anyhow::{Context, Result, bail};
use tracing::debug;

use crate::capability::HttpCapability;
use crate::model::Quote;
use crate::source::sina::{convert_sina_code_to_tushare, convert_tushare_code_to_sina};

/// 腾讯数据源
pub struct TencentSource {
    http: std::sync::Arc<dyn HttpCapability>,
}

impl TencentSource {
    pub fn new(http: std::sync::Arc<dyn HttpCapability>) -> Self {
        Self { http }
    }

    /// 获取实时行情 (单只)
    ///
    /// 输入 tushare 代码 (如 "600519.SH"), 返回 Quote.
    pub async fn quote(&self, ts_code: &str) -> Result<Quote> {
        let sina_code = convert_tushare_code_to_sina(ts_code);
        let url = format!("https://web.sqt.gtimg.cn/q={sina_code}");
        let headers = [("Referer", "https://gu.qq.com")];

        debug!(ts_code, sina_code, "tencent quote");

        // 腾讯返回 GBK 编码
        let bytes = self.http.get_bytes(&url, &headers).await?;
        let (text, _, _) = encoding_rs::GBK.decode(&bytes);
        let text = text.to_string();

        // 解析: v_sh600519="51~茅台~600519~..."
        // 字段用 ~ 分隔
        let line = text.lines().next().context("tencent quote: empty response")?;
        let inner = line
            .split('"')
            .nth(1)
            .context("tencent quote: missing quoted content")?;

        let fields: Vec<&str> = inner.split('~').collect();
        if fields.len() < 45 {
            bail!("tencent quote: unexpected format (fields < 45)");
        }

        // 字段映射 (腾讯格式):
        // 1: name, 3: code, 4: price, 5: yesterday (pre_close),
        // 6: ? , 7: ? , 8: volume, 9: amount,
        // 30: high, 31: low, 32: open
        // 参考: https://gu.qq.com/resources/web/html/hq_detail.html
        let name = fields[1];
        let price: f64 = fields[4].parse().context("tencent quote: invalid price")?;
        let pre_close: Option<f64> = fields[5].parse().ok();
        let volume: Option<f64> = fields[8].parse().ok();
        let amount: Option<f64> = fields[9].parse().ok();
        let high: Option<f64> = fields[30].parse().ok();
        let low: Option<f64> = fields[31].parse().ok();
        let open: Option<f64> = fields[32].parse().ok();

        let change_pct = if let Some(pc) = pre_close {
            if pc > 0.0 { (price - pc) / pc * 100.0 } else { 0.0 }
        } else {
            0.0
        };

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
            turnover_rate: None,  // 腾讯不提供
            circ_mv: None,        // 腾讯不提供
            total_mv: None,       // 腾讯不提供
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

        let sina_codes: Vec<String> = ts_codes.iter().map(|c| convert_tushare_code_to_sina(c)).collect();
        let codes = sina_codes.join(",");
        let url = format!("https://web.sqt.gtimg.cn/q={codes}");
        let headers = [("Referer", "https://gu.qq.com")];

        debug!(codes, "tencent quotes batch");

        let bytes = self.http.get_bytes(&url, &headers).await?;
        let (text, _, _) = encoding_rs::GBK.decode(&bytes);
        let text = text.to_string();

        // 解析多行
        let mut quotes = Vec::with_capacity(ts_codes.len());
        for line in text.lines() {
            if line.trim().is_empty() {
                continue;
            }
            if let Ok(q) = self.parse_single_quote(line) {
                quotes.push(q);
            }
        }

        Ok(quotes)
    }

    /// 解析单条行情数据
    fn parse_single_quote(&self, line: &str) -> Result<Quote> {
        let inner = line
            .split('"')
            .nth(1)
            .context("missing quoted content")?;

        let fields: Vec<&str> = inner.split('~').collect();
        if fields.len() < 45 {
            bail!("unexpected format");
        }

        // 提取 tushare 代码
        let sina_code = line
            .split('_')
            .nth(1)
            .and_then(|s| s.split('=').next())
            .context("missing code in var name")?
            .trim();

        let ts_code = convert_sina_code_to_tushare(sina_code);
        let name = fields[1];
        let price: f64 = fields[4].parse()?;
        let pre_close: Option<f64> = fields[5].parse().ok();

        let change_pct = if let Some(pc) = pre_close {
            if pc > 0.0 { (price - pc) / pc * 100.0 } else { 0.0 }
        } else {
            0.0
        };

        Ok(Quote {
            ts_code,
            name: name.to_string(),
            price,
            open: fields[32].parse().ok(),
            high: fields[30].parse().ok(),
            low: fields[31].parse().ok(),
            pre_close,
            change: Some(price - pre_close.unwrap_or(0.0)),
            change_pct,
            volume: fields[8].parse().ok(),
            amount: fields[9].parse().ok(),
            turnover_rate: None,
            circ_mv: None,
            total_mv: None,
            ts: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tencent_source_new() {
        // 空测试, 仅验证编译
    }
}