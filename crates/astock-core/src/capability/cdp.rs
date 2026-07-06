//! CDP (浏览器自动化) 能力抽象
//!
//! rsclaw 已自带完善的 CDP 实现 (`rsclaw-browser` crate, 5600 行),
//! 这里通过 trait 注入, 让 astock-core 能接入 rsclaw 的 CDP,
//! 也能接入第三方实现 (playwright, selenium, 或 WASM 版).

use std::fmt;
use anyhow::Result;
use async_trait::async_trait;
use serde_json::Value;

/// 浏览器 tab 句柄
///
/// 抽象 CDP 会话, 提供导航 / JS 执行 / 内容获取等基本操作.
/// Drop 时自动关闭 tab (具体行为由实现决定).
///
/// # 关键操作
///
/// - `evaluate`: 抓表格数据的**王炸**. 直接在页面里跑 JS 提取 JSON,
///   比解析 snapshot/DOM 更稳更快. 例如:
///   ```js
///   JSON.stringify(
///     Array.from(document.querySelectorAll('table tr'))
///       .map(r => Array.from(r.cells).map(c => c.innerText))
///   )
///   ```
#[async_trait]
pub trait TabHandle: Send {
    /// 导航到 URL, 等待页面加载
    async fn navigate(&self, url: &str) -> Result<()>;

    /// 在页面上下文执行 JS, 返回任意 JSON 值
    ///
    /// - 支持 await promise
    /// - 无沙箱, 可访问 document/window/fetch/localStorage 等
    /// - 超时由实现决定 (rsclaw-browser 默认 180s)
    async fn evaluate(&self, js: &str) -> Result<Value>;

    /// 获取渲染后的完整 HTML
    async fn content(&self) -> Result<String>;

    /// 等待 CSS selector 出现
    async fn wait_for_selector(&self, selector: &str, timeout_ms: u64) -> Result<()>;

    /// 获取当前 URL
    async fn get_url(&self) -> Result<String>;
}

/// CDP 能力 trait
///
/// 负责获取浏览器 tab. 实际使用模式:
/// ```rust,ignore
/// let tab = cdp.acquire_tab().await?;
/// tab.navigate(url).await?;
/// let data = tab.evaluate(EXTRACT_JS).await?;
/// drop(tab); // 自动关闭
/// ```
#[async_trait]
pub trait CdpCapability: Send + Sync + 'static {
    /// 获取一个可用的 tab.
    ///
    /// 实现应负责:
    /// - 连接池 / 复用 (例如 rsclaw-browser 的 `BrowserPool`)
    /// - 生命周期管理 (Drop tab 时自动关闭)
    /// - idle 回收
    async fn acquire_tab(&self) -> Result<Box<dyn TabHandle + Send>>;
}

/// CDP 不可用占位
///
/// 所有操作返回错误. 用于:
/// - 不需要 CDP 的数据源 (例如纯 HTTP 的 tushare)
/// - WASM 编译 (浏览器没 CDP)
/// - 测试
pub struct NoCdp;

#[async_trait]
impl CdpCapability for NoCdp {
    async fn acquire_tab(&self) -> Result<Box<dyn TabHandle + Send>> {
        anyhow::bail!("CDP not available: NoCdp capability installed. Use a real CdpCapability implementation (e.g. rsclaw-browser) to enable browser-based data sources.")
    }
}

impl fmt::Debug for NoCdp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("NoCdp")
    }
}
