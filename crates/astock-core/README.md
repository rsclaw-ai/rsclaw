# astock-core

A 股数据获取、算法、渲染的独立核心库。

## 设计原则

- **不依赖 rsclaw**: 所有外部能力通过 trait 注入 (HTTP / CDP / LLM / 配置)
- **多数据源**: tushare / 东财 (HTTP+CDP) / 腾讯 / 新浪 / 问财 / pytdx
- **算法纯 Rust**: 选股 / 评分 / 技术指标
- **预渲染 markdown**: 避免 LLM hallucinate 股票名称和数字

## 使用

```rust
use astock_core::{StockEngine, capability::{DefaultHttp, NoCdp, NoLlm, StaticConfig}};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let engine = StockEngine::builder()
        .http(DefaultHttp::new()?)
        .cdp(NoCdp)
        .llm(NoLlm)
        .config(StaticConfig::new().with("tushare_token", "your-token"))
        .build()?;

    // 选股 (第 1 期)
    // let report = engine.select(&Default::default()).await?;
    // println!("{}", report.markdown);

    Ok(())
}
```

## Feature flags

- `http-reqwest` (默认): 启用基于 reqwest 的 `DefaultHttp`
- `ptdx-bridge` (预留): 启用 pytdx python 子进程桥接
- `wasm` (预留): WASM 编译时启用

## 模块结构

- `capability`: 外部能力抽象 (HTTP / CDP / LLM / 配置)
- `source`: 数据源 (tushare / 东财 / 腾讯 / 新浪 / 问财 / pytdx)
- `algo`: 算法 (选股 / 评分 / 技术指标)
- `analysis`: 分析 (龙虎榜 / 辩论 / 预测)
- `render`: 渲染 (markdown 报告)
- `model`: 数据模型 (Quote / Kline / Basic / Stock / Report)
- `tool`: 高层 API (`StockEngine` 方法)

## 路线图

详见 [`docs/astock-core-plan.md`](../../docs/astock-core-plan.md).

- [x] 第 0 期: crate 骨架 + trait 设计
- [ ] 第 1 期: 盘后选股 (tushare + 算法 + markdown)
- [ ] 第 2 期: HTTP 实时行情 (东财/腾讯/新浪)
- [ ] 第 3 期: CDP 抓取 (龙虎榜/公告/研报)
- [ ] 第 4 期: 问财 + debate

## License

MIT OR Apache-2.0
