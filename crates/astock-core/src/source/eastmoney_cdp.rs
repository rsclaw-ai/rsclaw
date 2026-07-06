//! 东方财富 CDP 数据源
//!
//! 用 CDP (Chrome DevTools Protocol) 抓取东财网站数据:
//! - 龙虎榜: https://data.eastmoney.com/stock/tradedetail.html
//! - 公告: https://data.eastmoney.com/notices/stock/{code}.html
//! - 研报: https://data.eastmoney.com/report/stock.jshtml
//!
//! # 为什么用 CDP 而不是 HTTP?
//!
//! 东财 HTTP 接口有反爬限制 (IP 封禁 / 登录墙), CDP 更稳定:
//! - 模拟真实浏览器行为
//! - 复用用户已登录的 Chrome (connect_existing)
//! - evaluate 直接在页面上下文提取 JSON (绕过反爬)
//!
//! # 使用方式
//!
//! ```rust,no_run
//! use astock_core::source::eastmoney_cdp::EastmoneyCdpSource;
//! use astock_core::capability::{CdpCapability, TabHandle};
//!
//! # async fn example(cdp: std::sync::Arc<dyn CdpCapability>) -> anyhow::Result<()> {
//! let source = EastmoneyCdpSource::new(cdp);
//! // 龙虎榜 (指定日期)
//! let lhb = source.longhubang("2026-07-01").await?;
//! for item in lhb {
//!     println!("{} {} 净买:{:.2}亿", item.ts_code, item.name, item.net_buy_yi);
//! }
//! # Ok(())
//! # }
//! ```

use std::sync::Arc;
use anyhow::{Result, Context};
use serde::{Deserialize, Serialize};
use tracing::debug;

use crate::capability::CdpCapability;

/// 东财 CDP 数据源
pub struct EastmoneyCdpSource {
    cdp: Arc<dyn CdpCapability>,
}

impl EastmoneyCdpSource {
    pub fn new(cdp: Arc<dyn CdpCapability>) -> Self {
        Self { cdp }
    }

    /// 龙虎榜 (指定日期)
    ///
    /// 页面: https://data.eastmoney.com/stock/tradedetail.html
    /// 数据: 股票代码、名称、净买额、买入金额、卖出金额、上榜原因等
    pub async fn longhubang(&self, date: &str) -> Result<Vec<LonghubangItem>> {
        let tab = self.cdp.acquire_tab().await?;

        // 东财龙虎榜 URL 格式: https://data.eastmoney.com/stock/tradedetail.html
        // 页面有日期筛选器, 默认显示最近交易日
        // 我们直接导航到页面, 然用 evaluate 提取表格
        let url = "https://data.eastmoney.com/stock/tradedetail.html";
        debug!(url, "eastmoney lhb navigate");

        tab.navigate(url).await?;

        // 等待表格加载
        tab.wait_for_selector("#dt_1", 5000).await?;

        // 用 evaluate 提取表格数据
        // 东财龙虎榜表格是动态加载的, 数据在 DOM 里
        let js = r#"
            // 提取龙虎榜表格数据
            const rows = document.querySelectorAll('#dt_1 tbody tr');
            const data = [];
            rows.forEach(row => {
                const cells = row.querySelectorAll('td');
                if (cells.length < 10) return;

                // 第 1 列: 股票代码 + 名称 (链接)
                const codeCell = cells[0];
                const codeLink = codeCell.querySelector('a');
                const code = codeLink ? codeLink.innerText.trim() : '';
                const name = cells[1].innerText.trim();

                // 第 2 列: 收盘价
                const close = parseFloat(cells[2].innerText) || 0;

                // 第 3 列: 涨跌幅
                const change_pct = parseFloat(cells[3].innerText) || 0;

                // 第 4 列: 换手率
                const turnover_rate = parseFloat(cells[4].innerText) || 0;

                // 第 5 列: 龙虎榜净买额 (万元)
                const net_buy_wan = parseFloat(cells[5].innerText) || 0;

                // 第 6 列: 买入金额 (万元)
                const buy_amount_wan = parseFloat(cells[6].innerText) || 0;

                // 第 7 列: 卖出金额 (万元)
                const sell_amount_wan = parseFloat(cells[7].innerText) || 0;

                // 第 8 列: 上榜原因
                const reason = cells[8].innerText.trim();

                data.push({
                    ts_code: code,
                    name: name,
                    close: close,
                    change_pct: change_pct,
                    turnover_rate: turnover_rate,
                    net_buy_wan: net_buy_wan,
                    buy_amount_wan: buy_amount_wan,
                    sell_amount_wan: sell_amount_wan,
                    reason: reason
                });
            });
            JSON.stringify(data);
        "#;

        debug!("eastmoney lhb evaluate");
        let value = tab.evaluate(js).await?;

        // 解析 JSON
        let items: Vec<RawLhbItem> = serde_json::from_value(value)
            .context("eastmoney lhb: parse JSON")?;

        // 转换为 LonghubangItem
        let mut result = Vec::with_capacity(items.len());
        for item in items {
            // 代码格式转换: 600519 -> 600519.SH
            let ts_code = infer_ts_code(&item.ts_code);

            result.push(LonghubangItem {
                ts_code,
                name: item.name,
                trade_date: date.to_string(),
                close: item.close,
                change_pct: item.change_pct,
                turnover_rate: item.turnover_rate,
                net_buy_wan: item.net_buy_wan,
                net_buy_yi: item.net_buy_wan / 10000.0,
                buy_amount_wan: item.buy_amount_wan,
                sell_amount_wan: item.sell_amount_wan,
                reason: item.reason,
            });
        }

        Ok(result)
    }

    /// 公告 (指定股票)
    ///
    /// 页面: https://data.eastmoney.com/notices/stock/{code}.html
    /// 数据: 公告标题、发布日期、类型等
    pub async fn notices(&self, ts_code: &str) -> Result<Vec<NoticeItem>> {
        let tab = self.cdp.acquire_tab().await?;

        // 转换代码格式: 600519.SH -> 600519
        let code = extract_code(ts_code);
        let url = format!("https://data.eastmoney.com/notices/stock/{}.html", code);

        debug!(url, "eastmoney notices navigate");
        tab.navigate(&url).await?;

        // 等待公告列表加载
        tab.wait_for_selector(".news_list", 5000).await?;

        // 提取公告列表
        let js = r#"
            const items = document.querySelectorAll('.news_list li');
            const data = [];
            items.forEach(item => {
                const link = item.querySelector('a');
                const title = link ? link.innerText.trim() : '';
                const href = link ? link.href : '';
                const dateSpan = item.querySelector('.time');
                const date = dateSpan ? dateSpan.innerText.trim() : '';
                const typeSpan = item.querySelector('.type');
                const type = typeSpan ? typeSpan.innerText.trim() : '';

                if (title) {
                    data.push({
                        title: title,
                        url: href,
                        date: date,
                        type: type
                    });
                }
            });
            JSON.stringify(data);
        "#;

        debug!("eastmoney notices evaluate");
        let value = tab.evaluate(js).await?;

        let items: Vec<RawNoticeItem> = serde_json::from_value(value)
            .context("eastmoney notices: parse JSON")?;

        Ok(items.into_iter().map(|i| NoticeItem {
            ts_code: ts_code.to_string(),
            title: i.title,
            url: i.url,
            date: i.date,
            type_: i.type_,
        }).collect())
    }

    /// 研报 (指定股票)
    ///
    /// 页面: https://data.eastmoney.com/report/stock.jshtml
    /// 数据: 研报标题、机构、评级、目标价等
    pub async fn research_reports(&self, ts_code: &str) -> Result<Vec<ResearchItem>> {
        let tab = self.cdp.acquire_tab().await?;

        // 东财研报页面需要先搜索股票
        let url = "https://data.eastmoney.com/report/stock.jshtml";
        debug!(url, "eastmoney research navigate");

        tab.navigate(url).await?;

        // 等待搜索框出现
        tab.wait_for_selector("#search_input", 5000).await?;

        // 输入股票代码搜索
        let code = extract_code(ts_code);
        let search_js = format!(
            r#"
            document.querySelector('#search_input').value = '{}';
            document.querySelector('#search_input').dispatchEvent(new Event('input'));
            document.querySelector('#search_btn').click();
            "#,
            code
        );

        tab.evaluate(&search_js).await?;

        // 等待研报列表加载
        tab.wait_for_selector("#dt_1", 10000).await?;

        // 提取研报列表
        let js = r#"
            const rows = document.querySelectorAll('#dt_1 tbody tr');
            const data = [];
            rows.forEach(row => {
                const cells = row.querySelectorAll('td');
                if (cells.length < 6) return;

                const title = cells[0].innerText.trim();
                const org = cells[1].innerText.trim();
                const rating = cells[2].innerText.trim();
                const target_price = parseFloat(cells[3].innerText) || 0;
                const date = cells[4].innerText.trim();

                data.push({
                    title: title,
                    organization: org,
                    rating: rating,
                    target_price: target_price,
                    date: date
                });
            });
            JSON.stringify(data);
        "#;

        debug!("eastmoney research evaluate");
        let value = tab.evaluate(js).await?;

        let items: Vec<RawResearchItem> = serde_json::from_value(value)
            .context("eastmoney research: parse JSON")?;

        Ok(items.into_iter().map(|i| ResearchItem {
            ts_code: ts_code.to_string(),
            title: i.title,
            organization: i.organization,
            rating: i.rating,
            target_price: i.target_price,
            date: i.date,
        }).collect())
    }
}

// ============================================================================
// 数据结构
// ============================================================================

/// 龙虎榜项
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LonghubangItem {
    pub ts_code: String,
    pub name: String,
    pub trade_date: String,
    pub close: f64,
    pub change_pct: f64,
    pub turnover_rate: f64,
    /// 净买额 (万元)
    pub net_buy_wan: f64,
    /// 净买额 (亿元)
    pub net_buy_yi: f64,
    /// 买入金额 (万元)
    pub buy_amount_wan: f64,
    /// 卖出金额 (万元)
    pub sell_amount_wan: f64,
    /// 上榜原因
    pub reason: String,
}

/// 公告项
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NoticeItem {
    pub ts_code: String,
    pub title: String,
    pub url: String,
    pub date: String,
    #[serde(rename = "type")]
    pub type_: String,
}

/// 研报项
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResearchItem {
    pub ts_code: String,
    pub title: String,
    pub organization: String,
    pub rating: String,
    pub target_price: f64,
    pub date: String,
}

// ============================================================================
// 内部类型 (用于 JSON 解析)
// ============================================================================

#[derive(Debug, Deserialize)]
struct RawLhbItem {
    ts_code: String,
    name: String,
    close: f64,
    change_pct: f64,
    turnover_rate: f64,
    net_buy_wan: f64,
    buy_amount_wan: f64,
    sell_amount_wan: f64,
    reason: String,
}

#[derive(Debug, Deserialize)]
struct RawNoticeItem {
    title: String,
    url: String,
    date: String,
    #[serde(rename = "type")]
    type_: String,
}

#[derive(Debug, Deserialize)]
struct RawResearchItem {
    title: String,
    organization: String,
    rating: String,
    target_price: f64,
    date: String,
}

// ============================================================================
// 辅助函数
// ============================================================================

/// 推断完整 ts_code
///
/// 东财返回的代码可能是 "600519" 或 "000001",
/// 根据代码前缀推断市场:
/// - 6xx: 沪市 -> .SH
/// - 0xx/3xx: 深市 -> .SZ
/// - 4xx/8xx: 北交所 -> .BJ
fn infer_ts_code(code: &str) -> String {
    if code.contains('.') {
        return code.to_string();
    }

    let prefix = code.get(0..1).unwrap_or("");
    match prefix {
        "6" => format!("{}.SH", code),
        "0" | "3" => format!("{}.SZ", code),
        "4" | "8" => format!("{}.BJ", code),
        _ => code.to_string(),
    }
}

/// 从 ts_code 提取纯代码
///
/// 600519.SH -> 600519
fn extract_code(ts_code: &str) -> String {
    if let Some(pos) = ts_code.find('.') {
        ts_code[..pos].to_string()
    } else {
        ts_code.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_infer_ts_code() {
        assert_eq!(infer_ts_code("600519"), "600519.SH");
        assert_eq!(infer_ts_code("000001"), "000001.SZ");
        assert_eq!(infer_ts_code("300613"), "300613.SZ");
        assert_eq!(infer_ts_code("430001"), "430001.BJ");
        assert_eq!(infer_ts_code("600519.SH"), "600519.SH");
    }

    #[test]
    fn test_extract_code() {
        assert_eq!(extract_code("600519.SH"), "600519");
        assert_eq!(extract_code("000001.SZ"), "000001");
        assert_eq!(extract_code("600519"), "600519");
    }
}