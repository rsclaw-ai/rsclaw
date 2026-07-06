//! 核心数据模型
//!
//! 所有模型都实现 `Serialize + Deserialize + Clone + Debug`,
//! 方便跨模块传递、缓存、序列化到 JSON.

pub mod quote;
pub mod kline;
pub mod basic;
pub mod stock;
pub mod report;

pub use quote::Quote;
pub use kline::Kline;
pub use basic::Basic;
pub use stock::Stock;
pub use report::{
    SelectionReport, SelectedStock, FilterStats,
    AnalysisReport, ReportKind,
};
