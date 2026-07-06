//! 算法 (纯 Rust, 无 I/O)
//!
//! - `selector`: 选股算法 (第 1 期)
//! - `scoring`: 综合评分 (第 1 期)
//! - `filter`: 通用过滤 (第 1 期)
//! - `indicators`: 技术指标 (MA / MACD / RSI 等)

pub mod selector;
pub mod scoring;
pub mod filter;
pub mod indicators;
