//! 多空辩论分析
//!
//! 用 LLM 对股票进行多空辩论:
//! - **多头观点**: 支持上涨的理由 (基本面/技术面/消息面)
//! - **空头观点**: 支持下跌的理由
//! - **综合判断**: 平衡观点, 给出最终结论
//!
//! # 使用方式
//!
//! ```rust,no_run
//! use astock_core::analysis::debate::{DebateAnalyzer, DebateConfig};
//! use astock_core::capability::{LlmCapability, LlmMessage, LlmRole};
//!
//! # async fn example(llm: std::sync::Arc<dyn LlmCapability>) -> anyhow::Result<()> {
//! let analyzer = DebateAnalyzer::new(llm);
//!
//! // 辩论茅台
//! let debate = analyzer.debate("600519.SH", "贵州茅台", 1500.0).await?;
//!
//! println!("多头观点:");
//! println!("{}", debate.bull_view);
//!
//! println!("\n空头观点:");
//! println!("{}", debate.bear_view);
//!
//! println!("\n综合判断:");
//! println!("{}", debate.summary);
//! println!("推荐: {}", debate.recommendation);
//! # Ok(())
//! # }
//! ```
//!
//! # 辩论框架
//!
//! 系统通过 3 次 LLM call 实现辩论:
//!
//! 1. **Call 1 - 多头**: 提供股票数据, 要求 LLM 列出上涨理由
//! 2. **Call 2 - 空头**: 提供股票数据, 要求 LLM 列出下跌理由
//! 3. **Call 3 - 综合**: 给出多空观点, 要求 LLM 平衡判断

use std::sync::Arc;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tracing::debug;

use crate::capability::{LlmCapability, LlmMessage, LlmRole};

/// 多空辩论分析器
pub struct DebateAnalyzer {
    llm: Arc<dyn LlmCapability>,
    config: DebateConfig,
}

/// 辩论配置
#[derive(Debug, Clone)]
pub struct DebateConfig {
    /// 多头 system prompt
    pub bull_prompt: String,
    /// 空头 system prompt
    pub bear_prompt: String,
    /// 综合 system prompt
    pub summary_prompt: String,
    /// 最大 token
    pub max_tokens: usize,
}

impl Default for DebateConfig {
    fn default() -> Self {
        Self {
            bull_prompt: BULL_SYSTEM_PROMPT.to_string(),
            bear_prompt: BEAR_SYSTEM_PROMPT.to_string(),
            summary_prompt: SUMMARY_SYSTEM_PROMPT.to_string(),
            max_tokens: 500,
        }
    }
}

impl DebateAnalyzer {
    pub fn new(llm: Arc<dyn LlmCapability>) -> Self {
        Self {
            llm,
            config: DebateConfig::default(),
        }
    }

    pub fn with_config(mut self, config: DebateConfig) -> Self {
        self.config = config;
        self
    }

    /// 多空辩论
    ///
    /// 输入股票代码、名称、当前价, 返回辩论结果.
    pub async fn debate(&self, ts_code: &str, name: &str, price: f64) -> Result<DebateResult> {
        debug!(ts_code, name, price, "debate start");

        // 准备股票数据上下文
        let stock_context = format!(
            "股票: {} ({})\n当前价: {:.2} 元\n",
            name, ts_code, price
        );

        // Call 1: 多头观点
        let bull_messages = vec![
            LlmMessage {
                role: LlmRole::System,
                content: self.config.bull_prompt.clone(),
            },
            LlmMessage {
                role: LlmRole::User,
                content: stock_context.clone(),
            },
        ];

        debug!("debate bull call");
        let bull_view = self.llm.chat(&bull_messages).await
            .context("LLM bull call failed")?;

        // Call 2: 空头观点
        let bear_messages = vec![
            LlmMessage {
                role: LlmRole::System,
                content: self.config.bear_prompt.clone(),
            },
            LlmMessage {
                role: LlmRole::User,
                content: stock_context.clone(),
            },
        ];

        debug!("debate bear call");
        let bear_view = self.llm.chat(&bear_messages).await
            .context("LLM bear call failed")?;

        // Call 3: 综合判断
        let summary_context = format!(
            "{}\n\n多头观点:\n{}\n\n空头观点:\n{}\n\n请给出综合判断。",
            stock_context, bull_view, bear_view
        );

        let summary_messages = vec![
            LlmMessage {
                role: LlmRole::System,
                content: self.config.summary_prompt.clone(),
            },
            LlmMessage {
                role: LlmRole::User,
                content: summary_context,
            },
        ];

        debug!("debate summary call");
        let summary = self.llm.chat(&summary_messages).await
            .context("LLM summary call failed")?;

        // 推断推荐 (从总结中提取)
        let recommendation = infer_recommendation(&summary);

        Ok(DebateResult {
            ts_code: ts_code.to_string(),
            name: name.to_string(),
            price,
            bull_view,
            bear_view,
            summary,
            recommendation,
        })
    }

    /// 快速辩论 (单次 call)
    ///
    /// 用于简化场景, 只生成综合观点.
    pub async fn quick_debate(&self, ts_code: &str, name: &str, price: f64) -> Result<String> {
        let stock_context = format!(
            "股票: {} ({})\n当前价: {:.2} 元\n\n请给出多空分析和综合判断。",
            name, ts_code, price
        );

        let messages = vec![
            LlmMessage {
                role: LlmRole::System,
                content: QUICK_DEBATE_PROMPT.to_string(),
            },
            LlmMessage {
                role: LlmRole::User,
                content: stock_context,
            },
        ];

        self.llm.chat(&messages).await
            .context("LLM quick debate failed")
    }
}

/// 辩论结果
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DebateResult {
    pub ts_code: String,
    pub name: String,
    pub price: f64,
    /// 多头观点 (上涨理由)
    pub bull_view: String,
    /// 空头观点 (下跌理由)
    pub bear_view: String,
    /// 综合判断
    pub summary: String,
    /// 推荐动作 (buy / hold / sell)
    pub recommendation: Recommendation,
}

/// 推荐动作
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Recommendation {
    Buy,
    Hold,
    Sell,
    Unknown,
}

impl std::fmt::Display for Recommendation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Buy => write!(f, "买入"),
            Self::Hold => write!(f, "观望"),
            Self::Sell => write!(f, "卖出"),
            Self::Unknown => write!(f, "不确定"),
        }
    }
}

// ============================================================================
// Prompt 模板 (公开, 可被 tool/debate.rs 等复用)
// ============================================================================

pub const BULL_SYSTEM_PROMPT: &str = r#"
你是股票多头分析师。你的任务是找出股票上涨的理由。

分析维度:
1. **基本面**: 行业景气度、业绩增长、估值水平、财务健康度
2. **技术面**: 价格形态、均线支撑、成交量、技术指标
3. **消息面**: 行业政策、公司公告、市场情绪

请用 2-3 条清晰的观点阐述为什么这只股票可能上涨。
每条观点控制在 50 字以内。
"#;

pub const BEAR_SYSTEM_PROMPT: &str = r#"
你是股票空头分析师。你的任务是找出股票下跌的风险。

分析维度:
1. **基本面**: 行业衰退、业绩下滑、估值过高、财务风险
2. **技术面**: 价格压力、均线阻力、量价背离、技术破位
3. **消息面**: 政策风险、负面公告、市场恐慌

请用 2-3 条清晰的观点阐述为什么这只股票可能下跌。
每条观点控制在 50 字以内。
"#;

pub const SUMMARY_SYSTEM_PROMPT: &str = r#"
你是股票综合分析师。你的任务是平衡多空观点, 给出最终判断。

请:
1. 总结多空双方的核心观点
2. 分析哪方观点更有说服力
3. 给出明确的操作建议 (买入/观望/卖出)
4. 提供风险提示

最后用一句话总结: "综合判断: [买入/观望/卖出]"
"#;

pub const QUICK_DEBATE_PROMPT: &str = r#"
你是股票分析师。请对股票给出简短的多空分析和综合判断。

格式:
- 多头观点: (1-2 条)
- 空头观点: (1-2 条)
- 综合判断: (买入/观望/卖出)
"#;

// ============================================================================
// 辅助函数
// ============================================================================

/// 从总结文本推断推荐动作
fn infer_recommendation(summary: &str) -> Recommendation {
    let lower = summary.to_lowercase();

    // 按关键词推断
    if lower.contains("买入") || lower.contains("buy") || lower.contains("建议买") {
        Recommendation::Buy
    } else if lower.contains("卖出") || lower.contains("sell") || lower.contains("建议卖") {
        Recommendation::Sell
    } else if lower.contains("观望") || lower.contains("hold") || lower.contains("等待") {
        Recommendation::Hold
    } else {
        Recommendation::Unknown
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_infer_recommendation() {
        assert_eq!(infer_recommendation("综合判断: 买入"), Recommendation::Buy);
        assert_eq!(infer_recommendation("建议卖出"), Recommendation::Sell);
        assert_eq!(infer_recommendation("观望为主"), Recommendation::Hold);
        assert_eq!(infer_recommendation("不确定"), Recommendation::Unknown);
    }

    #[test]
    fn test_recommendation_display() {
        assert_eq!(format!("{}", Recommendation::Buy), "买入");
        assert_eq!(format!("{}", Recommendation::Sell), "卖出");
        assert_eq!(format!("{}", Recommendation::Hold), "观望");
    }

    #[test]
    fn test_debate_config_default() {
        let config = DebateConfig::default();
        assert!(config.bull_prompt.contains("多头"));
        assert!(config.bear_prompt.contains("空头"));
        assert!(config.summary_prompt.contains("综合"));
    }
}