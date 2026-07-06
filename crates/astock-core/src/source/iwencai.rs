//! 问财（iWenCai）智能选股数据源
//!
//! 问财是东方财富的 AI 选股工具:
//! - URL: https://www.iwencai.com/
//! - 输入自然语言查询, 返回股票列表
//! - 需要 **登录态** (用 CDP 复用用户已登录的 Chrome)
//!
//! # 使用方式
//!
//! ```rust,no_run
//! use astock_core::source::iwencai::IwencaiSource;
//! use astock_core::capability::CdpCapability;
//!
//! # async fn example(cdp: std::sync::Arc<dyn CdpCapability>) -> anyhow::Result<()> {
//! let source = IwencaiSource::new(cdp);
//!
//! // 问财查询: "今天涨停的股票"
//! let result = source.query("今天涨停的股票").await?;
//! println!("查询: {}", result.query);
//! for stock in result.stocks {
//!     println!("{} {} 涨幅:{:.2}%", stock.ts_code, stock.name, stock.change_pct);
//! }
//! # Ok(())
//! # }
//! ```
//!
//! # 技术细节
//!
//! 问财页面是 SPA, 数据通过 XHR 加载. 两种抓取方式:
//!
//! 1. **CDP evaluate** (推荐): 在页面上下文提取 DOM 表格
//! 2. **network sniff** (高级): 监听 XHR 请求, 获取原始 JSON
//!
//! 本实现用 evaluate (更简单稳定).

use std::sync::Arc;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tracing::debug;

use crate::capability::CdpCapability;

/// 问财数据源
pub struct IwencaiSource {
    cdp: Arc<dyn CdpCapability>,
}

impl IwencaiSource {
    pub fn new(cdp: Arc<dyn CdpCapability>) -> Self {
        Self { cdp }
    }

    /// 问财查询
    ///
    /// 输入自然语言问题 (如 "今天涨停的股票", "市值小于50亿的半导体"),
    /// 返回股票列表 + 问财的回答摘要.
    pub async fn query(&self, question: &str) -> Result<IwencaiResult> {
        let tab = self.cdp.acquire_tab().await?;

        // 问财首页
        let url = "https://www.iwencai.com/";
        debug!(url, question, "iwencai navigate");

        tab.navigate(url).await?;

        // 等待搜索框出现
        tab.wait_for_selector("#auto-complete-input", 5000).await?;

        // 输入问题并搜索
        // 问财的搜索框是 input, 需要填值后触发搜索
        // 注意: format! 宏会把 { 当作占位符, JS 里的 { 需要转义为 {{
        let search_js = format!(
            r#"
            (async () => {{
                const input = document.querySelector('#auto-complete-input');
                if (!input) return {{ error: 'input not found' }};

                // 填入问题
                input.value = '{q}';
                input.dispatchEvent(new Event('input', {{ bubbles: true }}));

                // 点击搜索按钮
                const btn = document.querySelector('.search-btn') || document.querySelector('button[type="submit"]');
                if (btn) btn.click();

                // 等待结果加载 (最多 10 秒)
                await new Promise(resolve => setTimeout(resolve, 3000));

                // 提取股票列表表格
                const table = document.querySelector('.stock-list-table') || document.querySelector('table');
                if (!table) return {{ stocks: [], summary: '未找到结果表格' }};

                const rows = table.querySelectorAll('tr');
                const stocks = [];
                rows.forEach((row, idx) => {{
                    if (idx === 0) return; // 跳过表头

                    const cells = row.querySelectorAll('td');
                    if (cells.length < 3) return;

                    const codeCell = cells[0];
                    const codeLink = codeCell.querySelector('a');
                    const code = codeLink ? codeLink.innerText.trim() : cells[0].innerText.trim();
                    const name = cells[1].innerText.trim();
                    const price = parseFloat(cells[2].innerText) || 0;
                    const change_pct = parseFloat(cells[3].innerText) || 0;

                    stocks.push({{
                        ts_code: code,
                        name: name,
                        price: price,
                        change_pct: change_pct
                    }});
                }});

                // 提取问财回答摘要
                const summaryEl = document.querySelector('.answer-summary') || document.querySelector('.result-summary');
                const summary = summaryEl ? summaryEl.innerText.trim() : '查询成功';

                return {{ stocks, summary }};
            }})();
            "#,
            q = question
        );

        debug!("iwencai evaluate");
        let value = tab.evaluate(&search_js).await?;

        // 解析结果
        let raw: RawIwencaiResult = serde_json::from_value(value)
            .context("iwencai: parse JSON")?;

        // 转换代码格式
        let stocks = raw.stocks.into_iter().map(|s| {
            let ts_code = infer_ts_code(&s.ts_code);
            IwencaiStock {
                ts_code,
                name: s.name,
                price: s.price,
                change_pct: s.change_pct,
            }
        }).collect();

        Ok(IwencaiResult {
            query: question.to_string(),
            stocks,
            summary: raw.summary,
        })
    }

    /// 问财查询 (返回原始 JSON)
    ///
    /// 用于调试或高级场景.
    pub async fn query_raw(&self, question: &str) -> Result<serde_json::Value> {
        let tab = self.cdp.acquire_tab().await?;
        let url = "https://www.iwencai.com/";

        tab.navigate(url).await?;
        tab.wait_for_selector("#auto-complete-input", 5000).await?;

        let search_js = format!(
            "document.querySelector('#auto-complete-input').value = '{}'; document.querySelector('.search-btn').click();",
            question
        );

        tab.evaluate(&search_js).await?;

        // 等待结果
        tab.wait_for_selector(".stock-list-table", 10000).await?;

        // 返回页面 HTML (用于调试)
        tab.content().await.map(|s| serde_json::json!({ "html": s }))
    }
}

/// 问财查询结果
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IwencaiResult {
    /// 用户查询
    pub query: String,
    /// 问财选出的股票
    pub stocks: Vec<IwencaiStock>,
    /// 问财回答摘要
    pub summary: String,
}

/// 问财选出的股票
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IwencaiStock {
    pub ts_code: String,
    pub name: String,
    pub price: f64,
    pub change_pct: f64,
}

// ============================================================================
// 内部类型
// ============================================================================

#[derive(Debug, Deserialize)]
struct RawIwencaiResult {
    stocks: Vec<RawIwencaiStock>,
    summary: String,
}

#[derive(Debug, Deserialize)]
struct RawIwencaiStock {
    ts_code: String,
    name: String,
    price: f64,
    change_pct: f64,
}

// ============================================================================
// 辅助函数
// ============================================================================

/// 推断完整 ts_code (复用 eastmoney_cdp 的逻辑)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_infer_ts_code() {
        assert_eq!(infer_ts_code("600519"), "600519.SH");
        assert_eq!(infer_ts_code("000001"), "000001.SZ");
        assert_eq!(infer_ts_code("300613"), "300613.SZ");
    }
}