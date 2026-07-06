//! 外部能力抽象 + 默认实现
//!
//! 所有外部依赖 (HTTP / CDP / LLM / 配置) 都通过这里的 trait 注入,
//! 让 astock-core 完全独立于 rsclaw, 可被 WASM / 第三方集成复用.
//!
//! 默认实现:
//! - [`DefaultHttp`]: 基于 reqwest (feature `http-reqwest`)
//! - [`NoCdp`]: CDP 不可用占位
//! - [`NoLlm`]: LLM 不可用占位
//! - [`StaticConfig`]: 静态配置

mod http;
mod cdp;
mod llm;
mod config;

pub use http::{HttpCapability, DefaultHttp, NoHttp};
pub use cdp::{CdpCapability, TabHandle, NoCdp};
pub use llm::{LlmCapability, LlmMessage, LlmRole, NoLlm};
pub use config::{ConfigProvider, StaticConfig};
