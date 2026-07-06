//! 多空辩论工具
//!
//! 整合 LLM 分析器, 提供多空辩论:
//! - 按股票辩论 (`debate`)
//! - 快速辩论 (`quick_debate`)
//! - 生成辩论报告 (`debate_report`)
//!
//! 使用示例:
//! ```rust,compile_fail
//! use astock_core::{StockEngine, capability::*};
//!
//! # async fn example() -> anyhow::Result<()> {
//! let engine = StockEngine::builder()
//!     .http(DefaultHttp::new()?)
//!     .llm(RsclawLlm::new())  // 需要真实 LLM
//!     .config(StaticConfig::new())
//!     .build()?;
//!
//! // 辩论茅台
//! let report = engine.debate("600519.SH").await?;
//! println!("{}", report.markdown);
//! # Ok(())
//! # }
//! ```

use std::sync::Arc;
use anyhow::{Result, Context};

use crate::capability::{LlmCapability, HttpCapability, CdpCapability};
use crate::analysis::debate::{DebateAnalyzer, DebateResult, DebateConfig};
use crate::model::report::{AnalysisReport, ReportKind};

/// 辩论查询器
pub struct DebateQuery {
    llm: Arc<dyn LlmCapability>,
    http: Arc<dyn HttpCapability>,
    cdp: Arc<dyn CdpCapability>,
}

impl DebateQuery {
    pub fn new(
        llm: Arc<dyn LlmCapability>,
        http: Arc<dyn HttpCapability>,
        cdp: Arc<dyn CdpCapability>,
    ) -> Self {
        Self { llm, http, cdp }
    }

    /// 多空辩论 (指定股票)
    ///
    /// 自动获取股票名称和当前价, 然后进行辩论.
    pub async fn debate(&self, ts_code: &str) -> Result<DebateResult> {
        // 1. 获取实时行情 (用于获取名称和当前价)
        let realtime = crate::tool::realtime::RealtimeQuery::new(Arc::clone(&self.http));
        let quote = realtime.quote(ts_code).await
            .context("debate: failed to fetch realtime quote")?;

        // 2. 进行辩论
        let analyzer = DebateAnalyzer::new(Arc::clone(&self.llm));
        analyzer.debate(ts_code, &quote.name, quote.price).await
    }

    /// 快速辩论 (单次 LLM call)
    pub async fn quick_debate(&self, ts_code: &str) -> Result<String> {
        let realtime = crate::tool::realtime::RealtimeQuery::new(Arc::clone(&self.http));
        let quote = realtime.quote(ts_code).await?;

        let analyzer = DebateAnalyzer::new(Arc::clone(&self.llm));
        analyzer.quick_debate(ts_code, &quote.name, quote.price).await
    }

    /// 生成辩论报告 (AnalysisReport)
    ///
    /// 包含 markdown 预渲染.
    pub async fn debate_report(&self, ts_code: &str) -> Result<AnalysisReport> {
        let result = self.debate(ts_code).await?;
        let markdown = render_debate_report(&result);

        Ok(AnalysisReport {
            kind: ReportKind::Debate,
            trade_date: chrono::Utc::now().format("%Y%m%d").to_string(),
            title: format!("{} ({}) 多空辩论", result.name, result.ts_code),
            markdown,
            extra: Some(serde_json::to_value(&result)?),
        })
    }

    /// 辩论 + 问财增强
    ///
    /// 在辩论前先用问财查询相关信息.
    #[allow(dead_code)]
    pub async fn debate_with_iwencai(&self, ts_code: &str, question: &str) -> Result<DebateResult> {
        // 1. 问财查询
        let iwencai = crate::source::iwencai::IwencaiSource::new(Arc::clone(&self.cdp));
        let iwencai_result = iwencai.query(question).await?;

        // 2. 获取实时行情
        let realtime = crate::tool::realtime::RealtimeQuery::new(Arc::clone(&self.http));
        let quote = realtime.quote(ts_code).await?;

        // 3. 辩论 (带问财上下文)
        let analyzer = DebateAnalyzer::new(Arc::clone(&self.llm))
            .with_config(DebateConfig {
                bull_prompt: crate::analysis::debate::BULL_SYSTEM_PROMPT.to_string() +
                    &format!("\n\n问财相关信息:\n{}", iwencai_result.summary),
                bear_prompt: crate::analysis::debate::BEAR_SYSTEM_PROMPT.to_string() +
                    &format!("\n\n问财相关信息:\n{}", iwencai_result.summary),
                summary_prompt: crate::analysis::debate::SUMMARY_SYSTEM_PROMPT.to_string(),
                max_tokens: 500,
            });

        analyzer.debate(ts_code, &quote.name, quote.price).await
    }
}

/// 渲染辩论报告为 markdown
fn render_debate_report(result: &DebateResult) -> String {
    let mut out = String::with_capacity(2048);

    out.push_str("# 多空辩论分析\n\n");
    out.push_str(&format!("**股票**: {} ({})\n", result.name, result.ts_code));
    out.push_str(&format!("**当前价**: {:.2} 元\n\n", result.price));

    // 多头观点
    out.push_str("## 🔴 多头观点 (上涨理由)\n\n");
    out.push_str(&result.bull_view);
    out.push_str("\n\n");

    // 空头观点
    out.push_str("## 🔵 空头观点 (下跌风险)\n\n");
    out.push_str(&result.bear_view);
    out.push_str("\n\n");

    // 综合判断
    out.push_str("## ⚖️ 综合判断\n\n");
    out.push_str(&result.summary);
    out.push_str("\n\n");

    // 推荐动作
    out.push_str(&format!("**推荐**: {}\n\n", result.recommendation));

    // 风险提示
    out.push_str("---\n\n");
    out.push_str("> ⚠️ **风险提示**: 本分析仅供参考, 不构成投资建议. ");
    out.push_str("股市有风险, 投资需谨慎.\n");

    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analysis::debate::Recommendation;

    #[test]
    fn test_render_debate_report() {
        let result = DebateResult {
            ts_code: "600519.SH".into(),
            name: "贵州茅台".into(),
            price: 1500.0,
            bull_view: "1. 行业龙头, 品牌价值高\n2. 业绩稳定增长".into(),
            bear_view: "1. 估值偏高\n2. 市场竞争加剧".into(),
            summary: "综合判断: 观望".into(),
            recommendation: Recommendation::Hold,
        };

        let md = render_debate_report(&result);
        assert!(md.contains("# 多空辩论分析"));
        assert!(md.contains("贵州茅台"));
        assert!(md.contains("多头观点"));
        assert!(md.contains("空头观点"));
        assert!(md.contains("综合判断"));
        assert!(md.contains("观望"));
        assert!(md.contains("风险提示"));
    }
}