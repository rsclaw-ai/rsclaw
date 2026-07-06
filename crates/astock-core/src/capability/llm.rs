//! LLM 能力抽象 (用于 debate / 分析)

use std::fmt;
use anyhow::Result;
use async_trait::async_trait;

/// LLM 消息角色
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LlmRole {
    System,
    User,
    Assistant,
}

/// LLM 消息
#[derive(Debug, Clone)]
pub struct LlmMessage {
    pub role: LlmRole,
    pub content: String,
}

impl LlmMessage {
    pub fn system(content: impl Into<String>) -> Self {
        Self { role: LlmRole::System, content: content.into() }
    }
    pub fn user(content: impl Into<String>) -> Self {
        Self { role: LlmRole::User, content: content.into() }
    }
    pub fn assistant(content: impl Into<String>) -> Self {
        Self { role: LlmRole::Assistant, content: content.into() }
    }
}

/// LLM 能力 trait
///
/// 抽象对话接口, 用于:
/// - `analysis::debate`: 多空辩论 (BullAnalyst / BearAnalyst / ResearchManager)
/// - `analysis::forecast`: 未来走势预测
///
/// 默认实现: [`NoLlm`] (不可用占位).
/// rsclaw 端可接入现有的 LLM 层 (minimax / dashscope / openai 兼容).
#[async_trait]
pub trait LlmCapability: Send + Sync + 'static {
    /// 发起一次对话, 返回模型回复文本
    async fn chat(&self, messages: &[LlmMessage]) -> Result<String>;
}

/// LLM 不可用占位
pub struct NoLlm;

#[async_trait]
impl LlmCapability for NoLlm {
    async fn chat(&self, _messages: &[LlmMessage]) -> Result<String> {
        anyhow::bail!("LLM not available: NoLlm capability installed. Use a real LlmCapability implementation to enable debate/forecast features.")
    }
}

impl fmt::Debug for NoLlm {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("NoLlm")
    }
}
