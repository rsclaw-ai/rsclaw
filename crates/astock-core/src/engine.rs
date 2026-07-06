//! StockEngine + Builder
//!
//! 这是 astock-core 的总入口. 用户通过 builder 注入外部能力 (HTTP / CDP / LLM / 配置),
//! 得到 `StockEngine` 实例, 然后调用高层 API (`select` / `realtime` / `lhb` / `debate`).

use std::sync::Arc;

use anyhow::{Context, Result};

use crate::capability::{
    CdpCapability, ConfigProvider, HttpCapability, LlmCapability,
};
use crate::model::SelectionReport;

/// 股票引擎
///
/// 持有所有外部能力, 提供高层 API. 线程安全, 可共享 (`Arc<StockEngine>`).
pub struct StockEngine {
    pub(crate) http: Arc<dyn HttpCapability>,
    pub(crate) cdp: Arc<dyn CdpCapability>,
    pub(crate) llm: Arc<dyn LlmCapability>,
    pub(crate) config: Arc<dyn ConfigProvider>,
}

impl StockEngine {
    /// 创建 builder
    pub fn builder() -> StockEngineBuilder {
        StockEngineBuilder::default()
    }

    pub fn http(&self) -> &dyn HttpCapability { self.http.as_ref() }
    pub fn cdp(&self) -> &dyn CdpCapability { self.cdp.as_ref() }
    pub fn llm(&self) -> &dyn LlmCapability { self.llm.as_ref() }
    pub fn config(&self) -> &dyn ConfigProvider { self.config.as_ref() }
    pub fn tushare_token(&self) -> Option<String> { self.config.tushare_token() }

    /// 选股 (第 1 期)
    ///
    /// 流程:
    /// 1. 构造 tushare 数据源
    /// 2. 调用 `algo::selector::select`
    /// 3. 调用 `render::markdown::render_selection_report`
    /// 4. 返回 markdown 已预渲染的 `SelectionReport`
    pub async fn select(
        &self,
        strategy: &crate::algo::selector::SelectionStrategy,
    ) -> Result<SelectionReport> {
        let src = crate::source::tushare::TushareSource::from_config(
            Arc::clone(&self.http),
            self.config.as_ref(),
        )
        .context("failed to init tushare source (check tushare_token in config)")?;

        let resolved = strategy.with_defaults();
        let result = crate::algo::selector::select(&src, &resolved).await?;
        let mut report = result.report;
        report.markdown = crate::render::markdown::render_selection_report(&report);
        Ok(report)
    }

    /// 选股 (指定交易日, 用于测试/回放)
    pub async fn select_on_date(
        &self,
        strategy: &crate::algo::selector::SelectionStrategy,
        trade_date: &str,
    ) -> Result<SelectionReport> {
        let src = crate::source::tushare::TushareSource::from_config(
            Arc::clone(&self.http),
            self.config.as_ref(),
        )
        .context("failed to init tushare source")?;

        let resolved = strategy.with_defaults();
        let result = crate::algo::selector::select_on_date(&src, &resolved, trade_date).await?;
        let mut report = result.report;
        report.markdown = crate::render::markdown::render_selection_report(&report);
        Ok(report)
    }

    /// 实时行情 (第 2 期)
    ///
    /// 单只查询, 按数据源优先级 fallback (东财 → 腾讯 → 新浪).
    pub async fn realtime_quote(&self, ts_code: &str) -> Result<crate::model::Quote> {
        let query = crate::tool::realtime::RealtimeQuery::new(std::sync::Arc::clone(&self.http));
        query.quote(ts_code).await
    }

    /// 实时行情 (批量)
    ///
    /// 批量查询使用东财批量接口 (高效), 失败时 fallback 单只查询.
    pub async fn realtime_quotes(&self, ts_codes: &[&str]) -> Result<Vec<crate::model::Quote>> {
        let query = crate::tool::realtime::RealtimeQuery::new(std::sync::Arc::clone(&self.http));
        query.quotes(ts_codes).await
    }

    /// 实时行情 (兼容旧接口, deprecated)
    #[deprecated(note = "Use realtime_quotes instead")]
    pub async fn realtime(&self, codes: &[&str]) -> Result<Vec<crate::model::Quote>> {
        self.realtime_quotes(codes).await
    }

    /// 龙虎榜分析 (第 3 期)
    ///
    /// 查询指定日期的龙虎榜, 返回预渲染的 markdown 报告.
    pub async fn lhb(&self, date: &str) -> Result<crate::model::AnalysisReport> {
        let query = crate::tool::lhb::LonghubangQuery::new(std::sync::Arc::clone(&self.cdp));
        query.report(date).await
    }

    /// 龙虎榜明细 (不含报告包装)
    pub async fn lhb_items(&self, date: &str) -> Result<Vec<crate::source::eastmoney_cdp::LonghubangItem>> {
        let query = crate::tool::lhb::LonghubangQuery::new(std::sync::Arc::clone(&self.cdp));
        let report = query.query(date).await?;
        Ok(report.items)
    }

    /// 多空辩论 (第 4 期)
    ///
    /// 对股票进行多空辩论分析, 返回预渲染的 markdown 报告.
    pub async fn debate(&self, ts_code: &str) -> Result<crate::model::AnalysisReport> {
        let query = crate::tool::debate::DebateQuery::new(
            std::sync::Arc::clone(&self.llm),
            std::sync::Arc::clone(&self.http),
            std::sync::Arc::clone(&self.cdp),
        );
        query.debate_report(ts_code).await
    }

    /// 快速辩论 (单次 LLM call)
    pub async fn quick_debate(&self, ts_code: &str) -> Result<String> {
        let query = crate::tool::debate::DebateQuery::new(
            std::sync::Arc::clone(&self.llm),
            std::sync::Arc::clone(&self.http),
            std::sync::Arc::clone(&self.cdp),
        );
        query.quick_debate(ts_code).await
    }

    /// 问财查询
    pub async fn iwencai(&self, question: &str) -> Result<crate::source::iwencai::IwencaiResult> {
        let source = crate::source::iwencai::IwencaiSource::new(std::sync::Arc::clone(&self.cdp));
        source.query(question).await
    }
}

/// StockEngine 构建器
#[derive(Default)]
pub struct StockEngineBuilder {
    http: Option<Arc<dyn HttpCapability>>,
    cdp: Option<Arc<dyn CdpCapability>>,
    llm: Option<Arc<dyn LlmCapability>>,
    config: Option<Arc<dyn ConfigProvider>>,
}

impl StockEngineBuilder {
    pub fn http(mut self, h: impl HttpCapability) -> Self {
        self.http = Some(Arc::new(h)); self
    }
    pub fn cdp(mut self, c: impl CdpCapability) -> Self {
        self.cdp = Some(Arc::new(c)); self
    }
    pub fn llm(mut self, l: impl LlmCapability) -> Self {
        self.llm = Some(Arc::new(l)); self
    }
    pub fn config(mut self, c: impl ConfigProvider) -> Self {
        self.config = Some(Arc::new(c)); self
    }

    /// 构建 StockEngine
    ///
    /// # Errors
    ///
    /// 缺少必需的能力 (http / config) 时返回错误.
    /// cdp / llm 可选, 缺失时用占位实现 (NoCdp / NoLlm).
    pub fn build(self) -> Result<StockEngine> {
        let http = self.http.ok_or_else(|| {
            anyhow::anyhow!("StockEngine requires HTTP capability. Call .http(...) on the builder.")
        })?;
        let config = self.config.ok_or_else(|| {
            anyhow::anyhow!("StockEngine requires config. Call .config(...) on the builder.")
        })?;

        Ok(StockEngine {
            http,
            cdp: self.cdp.unwrap_or_else(|| Arc::new(crate::capability::NoCdp)),
            llm: self.llm.unwrap_or_else(|| Arc::new(crate::capability::NoLlm)),
            config,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capability::{DefaultHttp, StaticConfig};

    #[cfg(feature = "http-reqwest")]
    #[test]
    fn test_builder_default() {
        let engine = StockEngine::builder()
            .http(DefaultHttp::new().unwrap())
            .config(StaticConfig::new())
            .build()
            .unwrap();
        assert!(engine.tushare_token().is_none());
    }

    #[test]
    fn test_builder_missing_http() {
        let err = StockEngine::builder()
            .config(StaticConfig::new())
            .build()
            .err()
            .expect("should fail without HTTP");
        assert!(err.to_string().contains("HTTP"));
    }

    #[cfg(feature = "http-reqwest")]
    #[test]
    fn test_builder_missing_config() {
        let err = StockEngine::builder()
            .http(DefaultHttp::new().unwrap())
            .build()
            .err()
            .expect("should fail without config");
        assert!(err.to_string().contains("config"));
    }

    #[cfg(feature = "http-reqwest")]
    #[tokio::test]
    async fn test_select_without_token() {
        let engine = StockEngine::builder()
            .http(DefaultHttp::new().unwrap())
            .config(StaticConfig::new()) // 没设 token
            .build()
            .unwrap();
        let err = engine
            .select(&Default::default())
            .await
            .err()
            .expect("should fail without tushare token");
        // 错误信息应该友好
        let s = err.to_string();
        assert!(
            s.contains("tushare") || s.contains("token"),
            "unexpected error: {s}",
        );
    }
}
