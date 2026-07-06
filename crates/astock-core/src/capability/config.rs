//! 配置能力抽象 + 静态配置实现

use std::collections::HashMap;
use std::sync::RwLock;

/// 配置提供 trait
///
/// 抽象配置访问, 让 astock-core 能对接:
/// - rsclaw 全局配置 (从 `rsclaw.json5` 的 `astock` 字段)
/// - 独立配置文件
/// - 环境变量
pub trait ConfigProvider: Send + Sync + 'static {
    /// tushare API token
    fn tushare_token(&self) -> Option<String>;

    /// 通用配置查询
    fn get(&self, key: &str) -> Option<String>;
}

/// 静态内存配置 (线程安全)
///
/// 用 `RwLock<HashMap>` 包装, 支持并发读写.
///
/// # 示例
///
/// ```rust
/// use astock_core::capability::{StaticConfig, ConfigProvider};
///
/// let cfg = StaticConfig::new()
///     .with("tushare_token", "your-token")
///     .with("eastmoney.timeout_ms", "5000");
///
/// assert_eq!(cfg.tushare_token(), Some("your-token".to_string()));
/// ```
#[derive(Debug, Default)]
pub struct StaticConfig {
    inner: RwLock<HashMap<String, String>>,
}

impl StaticConfig {
    pub fn new() -> Self {
        Self::default()
    }

    /// 链式添加配置项
    pub fn with(self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.set(key, value);
        self
    }

    /// 添加配置项 (可变引用版本)
    pub fn set(&self, key: impl Into<String>, value: impl Into<String>) {
        self.inner.write().unwrap().insert(key.into(), value.into());
    }

    /// 批量添加
    pub fn with_map(self, map: HashMap<String, String>) -> Self {
        *self.inner.write().unwrap() = map;
        self
    }
}

impl ConfigProvider for StaticConfig {
    fn tushare_token(&self) -> Option<String> {
        self.get("tushare_token")
            .or_else(|| self.get("tushare.token"))
    }

    fn get(&self, key: &str) -> Option<String> {
        self.inner.read().unwrap().get(key).cloned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_static_config_basic() {
        let cfg = StaticConfig::new()
            .with("tushare_token", "abc123")
            .with("other", "value");
        assert_eq!(cfg.tushare_token(), Some("abc123".to_string()));
        assert_eq!(cfg.get("other"), Some("value".to_string()));
        assert_eq!(cfg.get("missing"), None);
    }

    #[test]
    fn test_static_config_fallback_key() {
        let cfg = StaticConfig::new().with("tushare.token", "fallback");
        assert_eq!(cfg.tushare_token(), Some("fallback".to_string()));
    }

    #[test]
    fn test_static_config_concurrent() {
        use std::sync::Arc;
        let cfg = Arc::new(StaticConfig::new());
        let mut handles = vec![];
        for i in 0..8 {
            let c = cfg.clone();
            handles.push(std::thread::spawn(move || {
                c.set(format!("key{i}"), format!("val{i}"));
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        for i in 0..8 {
            assert_eq!(cfg.get(&format!("key{i}")), Some(format!("val{i}")));
        }
    }
}
