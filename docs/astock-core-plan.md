# astock-core 规划文档

> 创建日期：2026-07-01
> 状态：第 4 期完成 (问财 + LLM 辩论) - **全部完成！**

## 1. 背景

当前 rsclaw 的股票相关能力分散在 Python 脚本里（`workspace-multi-agent/skills/trading/`），存在以下问题：

1. **LLM hallucinate 股票名称**：agent 读 JSON 后自己"翻译"报告，用训练数据里的旧公司名替换真实名称
2. **cron 调度僵硬**：只能定时跑，用户无法按需调用（如"帮我辩论 600519"）
3. **数据源分散**：tushare / 东财 / 腾讯 / 新浪 / CDP / pytdx 各自为政
4. **不能复用**：Python 脚本无法给 rsclaw 内置 tool、WASM 浏览器版、第三方集成使用

## 2. 目标

把股票数据获取、算法、渲染**全部 Rust 化**，封装成**独立 crate `astock-core`**：

- ✅ 独立发布（不依赖 rsclaw 内部 crate）
- ✅ 所有外部能力通过 trait 注入（HTTP / CDP / LLM / 配置）
- ✅ rsclaw 内置 tool 接入后：解决 hallucinate + 支持按需调用
- ✅ 未来可编译为 WASM（浏览器版）
- ✅ 未来可给第三方集成

## 3. 核心设计

### 3.1 trait 抽象（不依赖 rsclaw）

```rust
// HTTP 能力 - 默认 reqwest，WASM 可换 web_sys
#[async_trait]
pub trait HttpCapability: Send + Sync {
    async fn get(&self, url: &str, headers: &[(&str, &str)]) -> Result<String>;
    async fn post_json(&self, url: &str, body: &Value) -> Result<Value>;
}

// CDP 能力 - rsclaw 用 rsclaw-browser 实现，WASM 用 NoCdp
#[async_trait]
pub trait CdpCapability: Send + Sync {
    async fn acquire_tab(&self) -> Result<Box<dyn TabHandle>>;
}

// LLM 能力 - debate 用
#[async_trait]
pub trait LlmCapability: Send + Sync {
    async fn chat(&self, messages: &[Message]) -> Result<String>;
}

// 配置
pub trait ConfigProvider: Send + Sync {
    fn tushare_token(&self) -> Option<&str>;
    // ...
}
```

### 3.2 模块结构

```
crates/astock-core/
├── Cargo.toml
├── src/
│   ├── lib.rs
│   ├── capability/         # 外部能力抽象 + 默认实现
│   │   ├── mod.rs
│   │   ├── http.rs         # reqwest 实现
│   │   └── noop.rs
│   ├── source/             # 数据源（每个一个文件）
│   │   ├── mod.rs          # DataSource trait
│   │   ├── tushare.rs
│   │   ├── eastmoney_http.rs
│   │   ├── eastmoney_cdp.rs
│   │   ├── tencent.rs
│   │   ├── sina.rs
│   │   ├── iwencai.rs
│   │   └── ptdx.rs
│   ├── algo/               # 算法（纯 Rust）
│   │   ├── mod.rs
│   │   ├── selector_v2.rs
│   │   ├── scoring.rs
│   │   ├── filter.rs
│   │   └── indicators.rs
│   ├── analysis/           # 分析
│   │   ├── mod.rs
│   │   ├── longhu.rs
│   │   ├── debate.rs
│   │   └── forecast.rs
│   ├── render/             # 渲染
│   │   ├── mod.rs
│   │   └── markdown.rs
│   ├── model/              # 数据结构
│   │   ├── mod.rs
│   │   ├── quote.rs
│   │   ├── kline.rs
│   │   ├── basic.rs
│   │   ├── stock.rs
│   │   └── report.rs
│   └── tool/               # 高层 API
│       ├── mod.rs
│       ├── select.rs
│       ├── realtime.rs
│       ├── lhb.rs
│       └── debate.rs
```

### 3.3 数据源优先级

**能用 HTTP 就用 HTTP，CDP 是 fallback**（CDP 资源开销大）：

```rust
async fn fetch_longhu(date: &str) -> Result<Vec<Stock>> {
    // 1. 优先 HTTP：东财 datacenter API 直接返回 JSON
    match eastmoney_http_longhu(date).await {
        Ok(data) => return Ok(data),
        Err(Forbidden) => { /* fallback CDP */ }
        Err(e) => return Err(e),
    }
    // 2. Fallback CDP：rsclaw-browser evaluate 提取表格
    let tab = cdp.acquire_tab().await?;
    tab.navigate("https://data.eastmoney.com/stock/tradedetail.html").await?;
    let json = tab.evaluate(EXTRACT_LONGHU_JS).await?;
    Ok(serde_json::from_value(json)?)
}
```

## 4. 关键调研发现

### rsclaw-browser 已具备的能力（5600 行 CDP 实现）

- 50+ 个 CDP action：`navigate`/`snapshot`/`click`/`fill`/`evaluate`/`screenshot`/`network sniff`/`state save/load`/`pick`/`search` 等
- `evaluate`：执行任意 JS，返回任意 JSON（**抓表格的王炸**）
- `network sniff`：发现页面上所有 XHR URL（找 SPA 背后真实接口）
- `state save/load`：跨重启保持登录态
- `connect_existing_reuse`：复用用户真实 Chrome（不暴露 CDP 特征）
- `BrowserPool`：8 tab/Chrome，idle 10 分钟回收，崩溃自动 restart

### 已有股票相关能力

- `tools_web.rs:3063` 已实现 `fetch_stock_sina`（Sina HTTP API 拿实时行情）
- `BrowserPool` 已被 `web_browser`/`browser_get_article`/`browser_search` 使用，模式成熟

### 不需要做的事

- ❌ 不需要"Python 抓数据写 duckdb → Rust 读"的迂回架构
- ❌ 不需要新写 CDP（rsclaw-browser 已完备）
- ❌ 不需要重新实现 Sina 行情（已有 `fetch_stock_sina`）

## 5. 实施节奏

### 第 0 期（2-3 天）：建 crate 骨架 ✅ 当前
- 创建 `crates/astock-core/`
- 定义所有 trait（HttpCapability / CdpCapability / LlmCapability / ConfigProvider）
- 实现 `DefaultHttp`（reqwest）
- 定义数据结构（Quote / Kline / Basic / Stock / Report）
- 单元测试验证 trait 设计
- **不写业务逻辑**

### 第 1 期（1 周）：盘后选股
- `source/tushare.rs`
- `algo/selector_v2.rs` + `scoring.rs`（翻译 `selector_tushare_final.py`）
- `render/markdown.rs`（预渲染 markdown，解决 hallucinate）
- `tool/select.rs`
- rsclaw 端接入：加 `stock_select` tool
- 发版 `0.1.0`

### 第 2 期（1 周）：HTTP 实时行情
- `source/eastmoney_http.rs` / `tencent.rs` / `sina.rs`
- 复用已有的 `fetch_stock_sina` 逻辑
- `tool/realtime.rs`
- 发版 `0.2.0`

### 第 3 期（1 周）：CDP 抓取
- `source/eastmoney_cdp.rs`（龙虎榜/公告/研报）
- rsclaw 端实现 `RsclawCdp` 适配层
- `tool/lhb.rs` / `news.rs`
- 发版 `0.3.0`

### 第 4 期（1 周）：问财 + debate
- `source/iwencai.rs`（CDP + 复用登录态）
- `analysis/debate.rs`（接 LLM trait）
- `tool/ask.rs` / `debate.rs`
- 发版 `1.0.0`

**总计 5-6 周**，每周独立产出。

## 6. Cargo.toml features

```toml
[features]
default = ["http-reqwest"]
http-reqwest = ["reqwest"]       # 默认启用 reqwest
ptdx-bridge = []                  # pytdx python 子进程桥接
wasm = []                         # WASM 编译时启用
```

## 7. 依赖原则

| 依赖 | 是否允许 | 说明 |
|------|---------|------|
| `tokio` | ✅ | 异步 runtime |
| `reqwest` (可选 feature) | ✅ | 默认 HTTP 实现 |
| `serde` / `serde_json` | ✅ | 序列化 |
| `anyhow` / `thiserror` | ✅ | 错误处理 |
| `tracing` | ✅ | 日志 |
| `rsclaw-browser` | ❌ | 通过 `CdpCapability` trait 注入 |
| `rsclaw-*` 其他 | ❌ | 完全独立 |
| `python` subprocess | ⚠️ | 通过 `PtdxBridge` feature 控制，可选 |

## 8. rsclaw 端接入示例

```rust
// rsclaw-agent 里
use astock_core::{StockEngine, capability::*};
use rsclaw_browser::pool::BrowserPool;

struct RsclawCdp;  // 把 BrowserPool 适配成 CdpCapability

#[async_trait]
impl CdpCapability for RsclawCdp {
    async fn acquire_tab(&self) -> Result<Box<dyn TabHandle>> {
        let tab = BrowserPool::global().acquire_tab().await?;
        Ok(Box::new(RsclawTab(tab)))
    }
}

let engine = StockEngine::builder()
    .http(DefaultHttp::default())
    .cdp(RsclawCdp)
    .llm(RsclawLlm::new(config))
    .config(RsclawConfig::from_global())
    .build()?;

let report = engine.select(&SelectConfig::default()).await?;
// report 是预渲染的 markdown，直接发给 agent
```

## 9. 风险与缓解

| 风险 | 缓解 |
|------|------|
| 东财 HTTP 接口被反爬 | CDP fallback（`evaluate` 提取表格） |
| 问财需要登录态 | `connect_existing_reuse` 复用用户 Chrome |
| pytdx 没有 Rust 实现 | 短期 spawn python 子进程，长期重写协议 |
| rsclaw-browser 反 stealth 不完整 | `connect_existing_reuse` 兜底 |
| 算法翻译错误 | 写单元测试，对比 Python 输出 |

## 10. 参考

- 现有 Python 脚本：`workspace-multi-agent/skills/trading/stock-selector/`
- 现有 CDP 实现：`crates/rsclaw-browser/`
- 现有 Sina 行情：`crates/rsclaw-agent/src/tools_web.rs:3063`
- 现有 astock 配置：`rsclaw.json5` 的 `astock` 字段
