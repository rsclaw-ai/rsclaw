//! 新浪财经 HTTP 数据源
//!
//! 接口说明:
//! - 搜索代码: `https://suggest3.sinajs.cn/suggest/type=&key={query}&name=suggestdata`
//! - 实时行情: `https://hq.sinajs.cn/list={code}` (GBK 编码)
//!
//! 特点:
//! - 免费, 无需 token
//! - 返回 32 字段 (name, open, prev_close, price, high, low, ...)
//! - 需要带 `Referer: https://finance.sina.com.cn` 请求头
//!
//! 单位注意:
//! - `volume`: 成交量, **手**
//! - `amount`: 成交额, **元** (注意与 tushare 不同!)

use anyhow::{Context, Result, bail};
use tracing::debug;

use crate::capability::HttpCapability;
use crate::model::Quote;

/// 新浪数据源
pub struct SinaSource {
    http: std::sync::Arc<dyn HttpCapability>,
}

impl SinaSource {
    pub fn new(http: std::sync::Arc<dyn HttpCapability>) -> Self {
        Self { http }
    }

    /// 搜索股票代码
    ///
    /// 输入股票名称或代码片段, 返回完整代码 (如 "sh600519").
    pub async fn search_code(&self, query: &str) -> Result<String> {
        let url = format!(
            "https://suggest3.sinajs.cn/suggest/type=&key={}&name=suggestdata",
            urlencoding::encode(query),
        );
        let headers = [("Referer", "https://finance.sina.com.cn")];

        debug!(query, "sina search");

        let text = self.http.get(&url, &headers).await?;

        // 解析: var suggestdata="code,name,...;code,name,...";
        let inner = text
            .split('"')
            .nth(1)
            .context("sina suggest: missing quoted content")?;

        // 取第一条结果
        let first = inner.split(';').next().context("sina suggest: empty result")?;

        let parts: Vec<&str> = first.split(',').collect();
        if parts.len() < 4 {
            bail!("sina suggest: unexpected format (parts < 4)");
        }

        // parts[3] 是 market+code, 例如 "sh600519"
        let code = parts[3];
        if code.is_empty() {
            bail!("sina suggest: empty code");
        }

        Ok(code.to_string())
    }

    /// 获取实时行情 (单只)
    ///
    /// 输入新浪代码 (如 "sh600519"), 返回 Quote.
    pub async fn quote(&self, sina_code: &str) -> Result<Quote> {
        let url = format!("https://hq.sinajs.cn/list={sina_code}");
        let headers = [("Referer", "https://finance.sina.com.cn")];

        debug!(sina_code, "sina quote");

        // 新浪返回 GBK 编码, 用 get_bytes 获取原始字节
        let bytes = self.http.get_bytes(&url, &headers).await?;
        let (text, _, _) = encoding_rs::GBK.decode(&bytes);
        let text = text.to_string();

        // 解析: var hq_str_sh600519="name,open,prev_close,...";
        let inner = text
            .split('"')
            .nth(1)
            .context("sina quote: missing quoted content")?;

        let fields: Vec<&str> = inner.split(',').collect();
        if fields.len() < 32 {
            bail!("sina quote: unexpected format (fields < 32)");
        }

        // 字段映射 (参考 tools_web.rs:3137):
        // 0: name, 1: open, 2: prev_close, 3: price, 4: high, 5: low,
        // 8: volume, 9: amount, 30: date, 31: time
        let name = fields[0];
        let open: Option<f64> = fields[1].parse().ok();
        let pre_close: Option<f64> = fields[2].parse().ok();
        let price: f64 = fields[3].parse().context("sina quote: invalid price")?;
        let high: Option<f64> = fields[4].parse().ok();
        let low: Option<f64> = fields[5].parse().ok();
        let volume: Option<f64> = fields[8].parse().ok();
        let amount: Option<f64> = fields[9].parse().ok();

        // 计算涨跌幅
        let change_pct = if let Some(pc) = pre_close {
            if pc > 0.0 {
                (price - pc) / pc * 100.0
            } else {
                0.0
            }
        } else {
            0.0
        };

        // 转换 ts_code: sh600519 -> 600519.SH
        let ts_code = convert_sina_code_to_tushare(sina_code);

        Ok(Quote {
            ts_code,
            name: name.to_string(),
            price,
            open,
            high,
            low,
            pre_close,
            change: Some(price - pre_close.unwrap_or(0.0)),
            change_pct,
            volume,
            amount, // 单位: 元 (注意!)
            turnover_rate: None,  // 新浪不提供
            circ_mv: None,        // 新浪不提供
            total_mv: None,       // 新浪不提供
            ts: None,             // 新浪不提供时间戳
        })
    }

    /// 获取实时行情 (多只)
    ///
    /// 输入新浪代码列表, 返回 Quote 列表.
    /// 新浪支持批量查询: `list=sh600519,sz000001`
    pub async fn quotes(&self, sina_codes: &[&str]) -> Result<Vec<Quote>> {
        if sina_codes.is_empty() {
            return Ok(vec![]);
        }

        let codes = sina_codes.join(",");
        let url = format!("https://hq.sinajs.cn/list={codes}");
        let headers = [("Referer", "https://finance.sina.com.cn")];

        debug!(codes, "sina quotes batch");

        let bytes = self.http.get_bytes(&url, &headers).await?;
        let (text, _, _) = encoding_rs::GBK.decode(&bytes);
        let text = text.to_string();

        // 解析多行: var hq_str_sh600519="..."; var hq_str_sz000001="...";
        let mut quotes = Vec::with_capacity(sina_codes.len());
        for line in text.split(';') {
            if line.trim().is_empty() {
                continue;
            }
            // 提取代码: var hq_str_sh600519="..."
            let code = line
                .split('_')
                .nth(3)
                .and_then(|s| s.split('=').next())
                .map(|s| s.trim());
            if let Some(_code) = code {
                if let Ok(q) = self.parse_single_quote(line) {
                    quotes.push(q);
                }
            }
        }

        Ok(quotes)
    }

    /// 解析单条行情数据
    fn parse_single_quote(&self, line: &str) -> Result<Quote> {
        let inner = line
            .split('"')
            .nth(1)
            .context("sina quote: missing quoted content")?;

        let fields: Vec<&str> = inner.split(',').collect();
        if fields.len() < 32 {
            bail!("sina quote: unexpected format");
        }

        // 提取代码
        let sina_code = line
            .split('_')
            .nth(3)
            .and_then(|s| s.split('=').next())
            .context("sina quote: missing code in var name")?
            .trim();

        let name = fields[0];
        let price: f64 = fields[3].parse().context("invalid price")?;
        let pre_close: Option<f64> = fields[2].parse().ok();

        let change_pct = if let Some(pc) = pre_close {
            if pc > 0.0 { (price - pc) / pc * 100.0 } else { 0.0 }
        } else {
            0.0
        };

        Ok(Quote {
            ts_code: convert_sina_code_to_tushare(sina_code),
            name: name.to_string(),
            price,
            open: fields[1].parse().ok(),
            high: fields[4].parse().ok(),
            low: fields[5].parse().ok(),
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

/// 转换新浪代码 -> tushare 代码
///
/// sh600519 -> 600519.SH
/// sz000001 -> 000001.SZ
pub fn convert_sina_code_to_tushare(sina_code: &str) -> String {
    if let Some(market) = sina_code.get(0..2) {
        let code = &sina_code[2..];
        match market {
            "sh" => format!("{}.SH", code),
            "sz" => format!("{}.SZ", code),
            _ => sina_code.to_string(),
        }
    } else {
        sina_code.to_string()
    }
}

/// 转换 tushare 代码 -> 新浪代码
///
/// 600519.SH -> sh600519
/// 000001.SZ -> sz000001
pub fn convert_tushare_code_to_sina(ts_code: &str) -> String {
    if let Some(pos) = ts_code.find('.') {
        let code = &ts_code[..pos];
        let market = &ts_code[pos + 1..];
        match market {
            "SH" => format!("sh{}", code),
            "SZ" => format!("sz{}", code),
            _ => ts_code.to_string(),
        }
    } else {
        ts_code.to_string()
    }
}

// urlencoding 在 astock-core 依赖中? 检查 Cargo.toml
// 如果没有, 使用简单编码
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_convert_sina_to_tushare() {
        assert_eq!(convert_sina_code_to_tushare("sh600519"), "600519.SH");
        assert_eq!(convert_sina_code_to_tushare("sz000001"), "000001.SZ");
        assert_eq!(convert_sina_code_to_tushare("bj430001"), "bj430001"); // 北交所不支持
    }

    #[test]
    fn test_convert_tushare_to_sina() {
        assert_eq!(convert_tushare_code_to_sina("600519.SH"), "sh600519");
        assert_eq!(convert_tushare_code_to_sina("000001.SZ"), "sz000001");
    }
}