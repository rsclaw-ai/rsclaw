//! astock-core: A 股数据获取、算法、渲染的独立核心库
//!
//! 设计原则:
//! - **不依赖 rsclaw**: 所有外部能力通过 trait 注入 (HTTP / CDP / LLM / 配置)
//! - **多数据源**: tushare / 东财 (HTTP+CDP) / 腾讯 / 新浪 / 问财 / pytdx
//! - **算法纯 Rust**: 选股 / 评分 / 技术指标
//! - **预渲染 markdown**: 避免 LLM hallucinate 股票名称和数字
//!
//! # 使用
//!
//! ```rust,no_run
//! use astock_core::{StockEngine, capability::{DefaultHttp, NoCdp, NoLlm, StaticConfig}};
//! use astock_core::algo::selector::SelectionStrategy;
//!
//! # async fn example() -> anyhow::Result<()> {
//! let engine = StockEngine::builder()
//!     .http(DefaultHttp::new()?)
//!     .cdp(NoCdp)
//!     .llm(NoLlm)
//!     .config(StaticConfig::new().with("tushare_token", "your-token"))
//!     .build()?;
//!
//! // 选股 (第 1 期)
//! let report = engine.select(&SelectionStrategy::default()).await?;
//! println!("{}", report.markdown); // 预渲染 markdown, 直接发给用户
//! # Ok(())
//! # }
//! ```
//!
//! # Feature flags
//!
//! - `http-reqwest` (默认): 启用基于 reqwest 的 `DefaultHttp`
//! - `ptdx-bridge` (预留): 启用 pytdx python 子进程桥接
//! - `wasm` (预留): WASM 编译时启用

pub mod capability;
pub mod source;
pub mod algo;
pub mod analysis;
pub mod render;
pub mod model;
pub mod tool;

mod engine;

pub use engine::{StockEngine, StockEngineBuilder};
pub use model::*;

/// Crate 版本
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
