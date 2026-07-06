//! 数据源
//!
//! 每个数据源对应一个文件. 第 1 期先实现 `tushare`, 后续加 eastmoney_http /
//! eastmoney_cdp / tencent / sina / iwencai / ptdx.

pub mod tushare;
pub mod eastmoney_http;
pub mod eastmoney_cdp;
pub mod tencent;
pub mod sina;
pub mod iwencai;
pub mod ptdx;

/// 数据源 trait (预留, 第 1 期细化)
///
/// 不同数据源覆盖的数据类型不同 (实时行情 / 历史日线 / 基本面 / 龙虎榜 / 公告 等),
/// 这里只定义最小接口, 具体方法由各数据源自定义.
pub trait DataSource: Send + Sync {
    /// 数据源名称 (例如 "tushare", "eastmoney_http")
    fn name(&self) -> &'static str;
}

/// 数据源错误
#[derive(Debug, thiserror::Error)]
pub enum SourceError {
    #[error("data source `{name}` unavailable: {reason}")]
    Unavailable { name: String, reason: String },

    #[error("data source `{name}` forbidden (anti-crawl): {reason}")]
    Forbidden { name: String, reason: String },

    #[error("data source `{name}` parse error: {reason}")]
    Parse { name: String, reason: String },

    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

/// 数据源返回结果的通用封装
pub type SourceResult<T> = Result<T, SourceError>;
