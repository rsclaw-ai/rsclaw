//! 实时行情查询 tool
//!
//! 整合三个 HTTP 数据源 (东财/腾讯/新浪), 提供:
//! - 单只查询 (`quote`)
//! - 批量查询 (`quotes`)
//! - 自动 fallback (东财失败 → 腾讯 → 新浪)
//!
//! 数据源特点:
//! - **东财**: 提供 **换手率** + **市值**, 最完整
//! - **腾讯**: 字段较少, 但稳定
//! - **新浪**: 字段最少, 但搜索 API 可用 (名称搜索)
//!
//! 使用示例:
//! ```rust,no_run
//! use astock_core::{StockEngine, capability::*};
//!
//! # async fn example() -> anyhow::Result<()> {
//! let engine = StockEngine::builder()
//!     .http(DefaultHttp::new()?)
//!     .config(StaticConfig::new())
//!     .build()?;
//!
//! // 查询茅台
//! let quote = engine.realtime_quote("600519.SH").await?;
//! println!("{}: {} ({:.2}%)", quote.name, quote.price, quote.change_pct);
//!
//! // 批量查询
//! let quotes = engine.realtime_quotes(&["600519.SH", "000001.SZ"]).await?;
//! # Ok(())
//! # }
//! ```

use std::sync::Arc;
use anyhow::Result;

use crate::capability::HttpCapability;
use crate::model::Quote;
use crate::source::{eastmoney_http::EastmoneyHttpSource, tencent::TencentSource, sina::SinaSource};

/// 实时行情数据源优先级
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RealtimeSource {
    /// 东财 HTTP (最完整, 有换手率/市值)
    Eastmoney,
    /// 腾讯财经
    Tencent,
    /// 新浪财经
    Sina,
}

impl Default for RealtimeSource {
    fn default() -> Self {
        Self::Eastmoney
    }
}

/// 实时行情查询器
///
/// 按优先级顺序尝试数据源, 失败时自动 fallback.
pub struct RealtimeQuery {
    http: Arc<dyn HttpCapability>,
    /// 优先级顺序
    priority: Vec<RealtimeSource>,
}

impl RealtimeQuery {
    /// 创建查询器 (默认优先级: 东财 → 腾讯 → 新浪)
    pub fn new(http: Arc<dyn HttpCapability>) -> Self {
        Self {
            http,
            priority: vec![RealtimeSource::Eastmoney, RealtimeSource::Tencent, RealtimeSource::Sina],
        }
    }

    /// 自定义优先级
    pub fn with_priority(mut self, priority: Vec<RealtimeSource>) -> Self {
        self.priority = priority;
        self
    }

    /// 查询单只股票 (按优先级 fallback)
    pub async fn quote(&self, ts_code: &str) -> Result<Quote> {
        for source in &self.priority {
            let result = match source {
                RealtimeSource::Eastmoney => {
                    let src = EastmoneyHttpSource::new(Arc::clone(&self.http));
                    src.quote(ts_code).await
                }
                RealtimeSource::Tencent => {
                    let src = TencentSource::new(Arc::clone(&self.http));
                    src.quote(ts_code).await
                }
                RealtimeSource::Sina => {
                    let src = SinaSource::new(Arc::clone(&self.http));
                    let sina_code = crate::source::sina::convert_tushare_code_to_sina(ts_code);
                    src.quote(&sina_code).await
                }
            };

            match result {
                Ok(q) => return Ok(q),
                Err(e) => {
                    tracing::debug!(source = source.name(), ts_code, error = %e, "failed, fallback");
                    continue;
                }
            }
        }

        anyhow::bail!("all realtime sources failed for {}", ts_code)
    }

    /// 查询多只股票 (按优先级 fallback)
    ///
    /// 批量查询使用东财/腾讯的批量接口 (更高效).
    pub async fn quotes(&self, ts_codes: &[&str]) -> Result<Vec<Quote>> {
        if ts_codes.is_empty() {
            return Ok(vec![]);
        }

        // 批量查询优先东财 (有换手率/市值)
        let src = EastmoneyHttpSource::new(Arc::clone(&self.http));
        match src.quotes(ts_codes).await {
            Ok(quotes) => return Ok(quotes),
            Err(e) => {
                tracing::warn!(error = %e, "eastmoney batch failed, fallback to individual queries");
            }
        }

        // Fallback: 单只查询
        let mut quotes = Vec::with_capacity(ts_codes.len());
        for code in ts_codes {
            match self.quote(code).await {
                Ok(q) => quotes.push(q),
                Err(e) => {
                    tracing::warn!(code, error = %e, "failed to fetch quote");
                }
            }
        }

        if quotes.is_empty() {
            anyhow::bail!("all realtime sources failed for all codes");
        }

        Ok(quotes)
    }
}

impl RealtimeSource {
    fn name(&self) -> &'static str {
        match self {
            Self::Eastmoney => "eastmoney",
            Self::Tencent => "tencent",
            Self::Sina => "sina",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_realtime_source_default() {
        let src = RealtimeSource::default();
        assert_eq!(src, RealtimeSource::Eastmoney);
    }

    #[test]
    fn test_realtime_source_name() {
        assert_eq!(RealtimeSource::Eastmoney.name(), "eastmoney");
        assert_eq!(RealtimeSource::Tencent.name(), "tencent");
        assert_eq!(RealtimeSource::Sina.name(), "sina");
    }

    #[test]
    fn test_realtime_query_priority_default() {
        let query = RealtimeQuery::new(Arc::new(crate::capability::NoHttp));
        assert_eq!(query.priority, vec![RealtimeSource::Eastmoney, RealtimeSource::Tencent, RealtimeSource::Sina]);
    }
}