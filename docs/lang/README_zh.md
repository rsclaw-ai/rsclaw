# RsClaw

> **会记住、会学习、能跨机路由的 AI Agent 引擎。**
> 一个 21MB 的 Rust 二进制 · A2A hub-spoke 集群 · 三层记忆 · 向量 + BM25 知识库 · 13 个通道 · 15 个 LLM 提供商 · OpenClaw drop-in 替换。

[![GitHub Stars](https://img.shields.io/github/stars/rsclaw-ai/rsclaw?style=flat&logo=github)](https://github.com/rsclaw-ai/rsclaw/stargazers)
[![Crates.io](https://img.shields.io/crates/v/rsclaw?style=flat&logo=rust)](https://crates.io/crates/rsclaw)
[![Release](https://img.shields.io/github/v/release/rsclaw-ai/rsclaw)](https://github.com/rsclaw-ai/rsclaw/releases)
[![Downloads](https://img.shields.io/crates/d/rsclaw?style=flat)](https://crates.io/crates/rsclaw)
[![License](https://img.shields.io/badge/License-MIT%20OR%20Apache--2.0-blue)](../../README.md#license)
[![Rust](https://img.shields.io/badge/Rust-1.91%2B-orange?logo=rust)](https://www.rust-lang.org/)

[🇺🇸 English](../../README.md) · **🇨🇳 中文** · [🇯🇵 日本語](README_ja.md) · [🇰🇷 한국어](README_ko.md) · [更多语言 ▾](.)

<p align="center">
  <img src="../images/en.gif" alt="RsClaw Preview" width="800" />
</p>

绝大多数 AI Agent 是绑在聊天框上的无状态进程。**RsClaw 是一个集群**：每个节点持久化结构化记忆、维护私有知识库、说 [Google A2A v1.0 协议](https://a2a-protocol.org/)——你在笔记本上敲的一句话可以同时扇出到 GPU spoke 出图、到大内存 spoke 跑 RAG、到第三方 partner agent 调专项工具，最后汇聚成一条流式回答送回来。

21 MB，~28 MB RAM，单文件二进制。纯 Rust，零 Node，零 Python。

💬 [加入社区](https://rsclaw.ai/zh/community) — WeChat / Feishu / QQ / Telegram

---

## 安装

### Homebrew（macOS / Linux）—— 推荐

```bash
brew tap rsclaw-ai/tap
brew install rsclaw            # CLI
brew install --cask rsclaw     # 桌面 app（macOS DMG）
```

### 其它方式

```bash
# Cargo
cargo install rsclaw

# 一键脚本（macOS / Linux）
curl -fsSL https://app.rsclaw.ai/scripts/install.sh | bash

# Windows
irm https://app.rsclaw.ai/scripts/install.ps1 | iex

# 或直接从 https://github.com/rsclaw-ai/rsclaw/releases 下载二进制
```

### 首次启动

```bash
rsclaw setup          # 初始化 ~/.rsclaw/
rsclaw onboard        # 交互式向导:provider、channel、记忆嵌入器
rsclaw start
```

首次启动会下载本地嵌入模型（BGE-small-zh，约 91 MB）到 `~/.rsclaw/models/`。断点续传;想跳过就预先放入 safetensors。桌面版预打包了该模型。

---

## A2A —— 集群级 Agent 互联

RsClaw 完整实现 [Google A2A v1.0 spec](https://a2a-protocol.org/latest/specification/) —— streaming、push 通知、任务持久化、cancel、INPUT_REQUIRED 中断，11 个 JSON-RPC 方法全部覆盖 —— 再叠加一个**一等公民的 hub-spoke relay**，把异构机器组成的集群变成一个逻辑 Agent。

### 为什么 A2A 是头牌特性

- **一个 gateway，背后多种后端**。Hub 按能力把请求路由到对应 spoke——GPU 机出图出视频、大内存机跑 RAG、partner 机调专有工具。
- **所有 spoke 在 LLM 眼里就是本地工具**（`agent_<peer-id>`），模型靠工具描述自然挑选，不需要写编排代码。
- **穿透 NAT、防火墙、国内网络环境**：relay 走一条持久 outbound WebSocket，spoke 不需要开任何 inbound 端口。

### 拓扑

```
        用户（chat / channel / curl）
              │
              ▼
       ┌──────────────┐
       │  Hub Agent   │  ← 公网,A2A v1.0 endpoint
       │   (router)   │
       └──────┬───────┘
        WS relay（每个
        spoke 一条持久
        连接）
              │
   ┌──────────┼──────────┐
   ▼          ▼          ▼
spoke-mac  spoke-aihub  spoke-partner
(你笔电)   (2×4090     (第三方
            GPU)        gateway)
```

每个 spoke 就是一个跑在 **relay spoke 模式**下的 `rsclaw gateway run`。Hub 配置里把 spoke 声明为 A2A peer，hub 上的 LLM 就把它们看作 `agent_spoke_mac`、`agent_spoke_aihub` 之类的工具，按能力描述自动路由。

Spoke 配置（一段——这就是全部）：

```json5
{
  gateway: {
    a2a: {
      relay: {
        mode: "spoke",
        nodeId: "spoke-aihub",
        relays: [
          "wss://hub.example.com/api/v1/a2a/relay/ws",
          "wss://backup.example.com/api/v1/a2a/relay/ws",   // 主备
        ],
        privateKey: "<keypair>",
      },
    },
  },
}
```

Hub 配置——声明 peer，LLM 看描述路由：

```json5
{
  agents: {
    a2a: [
      { id: "spoke_aihub",
        url: "http://localhost:18889",        // hub 调自己
        remoteAgentId: "spoke-aihub/main",
        description: "GPU 多媒体生成:文生图 / 图生视频 / 数字人 / TTS。\
                      触发:画 / 生图 / 视频 / 配音 / 数字人。" },
      { id: "spoke_mac",
        url: "http://localhost:18889",
        remoteAgentId: "spoke-mac/main",
        description: "通用对话 + 浏览器自动化 + 抖音 / 微信 / 飞书。" },
    ],
  },
}
```

用户说"**用 aihub 画一只猫**" → hub LLM 选中 `agent_spoke_aihub` → relay 转发 → aihub spoke 在 4090 上跑 `aihub-t2i` → 图片路径流式回到 hub，再回到用户。

### 不只 hub-spoke

直连模式也支持——`agents.a2a[].url` 直接指向任意 A2A v1.0 endpoint（rsclaw 或别的兼容实现）：

```bash
curl -X POST http://127.0.0.1:18888/api/v1/a2a \
  -H "Authorization: Bearer $TOKEN" \
  -d '{"jsonrpc":"2.0","id":"1","method":"SendStreamingMessage",
       "params":{"message":{"messageId":"m1","role":"user",
         "parts":[{"type":"text","text":"hi"}]}}}'
```

Spec 覆盖：`SendMessage` / `SendStreamingMessage` / `SubscribeToTask` / `GetTask` / `ListTasks` / `CancelTask` / push-config CRUD / `GetExtendedAgentCard`。INPUT_REQUIRED 挂起恢复走内置 `wait_input` 工具。任务持久化到 `var/data/a2a/tasks.redb`,`GetTask` 和 webhook 重启不丢。

公网暴露：Cloudflare Tunnel（境外），`frp` + 国内 VPS（国内），或 [`rsclaw-tunnel`](https://github.com/rsclaw-ai/rsclaw-tunnel) 做多租户。**上公网前一定要 set `RSCLAW_A2A_BEARER_TOKENS`**——空 = dev 模式 = 完全开放。

→ 完整协议面、hub-spoke 运维、identity & ACL、tunnel 配方：[docs/a2a.md](../a2a.md)。

---

## 记忆 —— 三层、带衰减、混合召回

不用你手工调 "save_memory" 的长期记忆。每个相关 turn,运行时会：

1. **抽取** 你这条消息里的耐久信号,蒸馏成结构化 doc（entity / preference / fact / procedure / relationship / lesson / failure）,用 flash 模型跑一遍——**原语言保留**（中文输入 → 中文存储,不翻译）。
2. **分层**：
   - **Core** —— 身份级（你的名字、联系方式、固化事实）。半衰减地板 0.9,永不降级。
   - **Working** —— 活跃上下文。标准指数衰减;从 Peripheral 升上来需要频繁被召回。
   - **Peripheral** —— 低信号。快速衰减;自动降级,定期淘汰。
3. **衰减** 每个 doc 走 **Weibull 拉伸指数**,每层有不同的 β —— 新近 + 频繁 + 重要的得分高;旧 + 被忽略的 doc 慢慢沉下去。
4. **召回** 走 **混合检索**：BM25 关键词（tantivy）+ 向量余弦（hnsw_rs），用 RRF 融合。每个相关 turn 自动注入到 LLM 上下文——不用手动 recall。

### 嵌入器分级

| 档位 | 嵌入器 | 延迟 | 何时用 |
|---|---|---|---|
| **本地** | BGE-small-zh-v1.5（Candle，91 MB） | ~5 ms / doc | 默认。桌面预打包;CLI 首次启动自动下载。 |
| **远程** | Qwen3-Embedding-0.6B（1024 维）on llama.cpp endpoint | ~30 ms / doc | 质量更高。配置 `memory.embedder.remote_url`。 |

### 工具 & CLI

```bash
# Chat 内（预解析,零 token）
/remember <文本>            # 存到长期记忆
/recall <query>             # 混合检索（BM25 + 向量 RRF）
/compact                    # 当前会话压缩 + 存摘要

# CLI / HTTP
rsclaw memory status        # 分层分布、scope 桶、pinned 数
rsclaw memory search <q>    # BM25 + 向量混合检索
curl http://127.0.0.1:18888/api/v1/memory/stats     # JSON 统计
curl http://127.0.0.1:18888/api/v1/memory/docs?q=…  # 列出 + 搜
```

scope 默认按 agent 分（`agent:main`）——子 agent 和 channel 可以划自己的 scope 隔离上下文。

→ 分层数学、抽取器 prompt 设计、嵌入器切换、HTTP API：[docs/memory.md](../memory.md)。重设计原始 rationale：[docs/memory-extraction-redesign.md](../memory-extraction-redesign.md)。

---

## 知识库 —— 受管 RAG，吃 OOXML，吐带引用的片段

跟会话记忆解耦的一等公民持久化知识库。用途：项目文档、参考资料、代码库、会议记录、合同条款——任何你希望 agent **引用而不是凭训练自由发挥** 的内容。

- **Collections** —— 单一 embedding 索引上的 tag veneer。桌面 UI 或 HTTP API 创建 / 列表 / 删除;没有 per-collection store 开销。
- **Ingest** —— 桌面 app 里拖拽,或 `POST /api/v1/knowledge/collections/<id>/docs`。支持纯文本、Markdown、PDF、**OOXML**（.docx / .xlsx / .pptx）、HTML。
- **Search** —— 跟记忆一样的混合 BM25 + 向量管线,按 collection scope。
- **默认带引用** —— agent 的 `knowledge_base` 工具返回的 snippet 带 doc-id + offset,回答可以引原文。

```bash
rsclaw knowledge ingest <path> --collection 会议记录
rsclaw knowledge search "Q3 营收预测" --collection 财报

# Chat 里——query 命中 collection 时 agent 自动用 knowledge_base 工具
"根据 Q3 财报,毛利率怎么样?"
```

→ Collections 模型、ingest 管线、检索（BM25 + 向量 + RRF + MMR）、CLI / HTTP API：[docs/kb.md](../kb.md)。工程实现：[src/kb/README.md](../../src/kb/README.md)。

---

## Agent —— 四种生命周期、四种后端

| 类型 | 创建者 | 持久化 | 杀者 |
|------|-----------|----------|-----------|
| **Main** | 系统 | 永久 | 没人——main 不可杀 |
| **Named** | 用户 / config | 重启幸存 | 仅用户 |
| **Sub** | LLM `agent_spawn` | 会话级 | 创建者 |
| **Task** | LLM `agent_task` | 一次性 | 返回时自动销毁 |

```
Main ──spawn──→ Named "pm"（持久化在 config）
                 └─spawn──→ Sub "analyst"（会话内）
                              ├─task──→ Task "search-jd"  ┐
                              └─task──→ Task "search-tb"  │ 并行
```

每个 agent 独立选后端：**Native Rust**（默认、最快）、**Claude Code**（Claude Agent SDK + ACP）、**OpenCode**（开源 coding agent）、**任何 ACP-compliant agent**。每个 agent 工具白名单可选 `minimal`（12）/ `web` / `code` / `standard`（16）/ `full`。委派**只能自上而下**——Sub 不能调 Main，平级不能互通。

---

## 通道（13 + 自定义）

| 通道 | 协议 | 备注 |
|---------|----------|-------|
| **微信 个人** | ilink long-poll | 扫码登录;语音 / 图片 / 文件 / 视频 |
| **飞书 / Lark** | WebSocket | OAuth 或 appId+secret |
| **企微 / WeCom** | AI Bot WebSocket | |
| **QQ Bot** | Gateway WebSocket | |
| **钉钉** | Stream Mode WS | |
| **Telegram** | HTTP long-poll | DM + group |
| **Discord** | Gateway WS | |
| **Slack** | Socket Mode | |
| **WhatsApp** | Cloud API webhook | |
| **Signal** | signal-cli JSON-RPC | |
| **LINE / Zalo** | Webhook | |
| **Matrix** | HTTP /sync | 可选 E2EE |
| **自定义** | `/hooks/{name}` | 入站 webhook |

每个通道都有：DM/群组 ACL、配对码（8 字符,1 小时）、健康监控、重试、流式、上传文件 confirmation gate。

---

## LLM Providers（15+）

Qwen · DeepSeek · Kimi · 智谱（GLM）· MiniMax · 豆包（字节）· SiliconFlow · GateRouter · OpenRouter · Anthropic · OpenAI · Gemini · xAI（Grok）· Groq · Ollama · 任意 OpenAI 兼容 endpoint。

特性：failover chain、指数退避、模型 fallback（`primary` → `flash` → `vision` → `fallbacks`）、thinking budget、Responses API、Ollama 原生、RsClaw 自有 fleet 的 KV prefix-cache（`rsclaw/*` namespace）。

---

## 工具 & 插件

**36 个内置工具**：文件读写搜、shell 执行（50+ 安全 deny 规则）、web 搜索 / fetch / 下载、浏览器自动化（CDP,50+ 动作,accessibility-tree snapshot）、记忆 CRUD、知识库 CRUD、文档抽取 / 创建（PDF / DOCX / XLSX / PPTX）、图像 / 视频生成、语音 STT（Whisper / SenseVoice）、TTS、computer_use、cron、multi-agent spawn/task、clarify（交互问询）、anycli（结构化 web 抽取）。

**40+ 预解析命令**（本地、零 token、亚毫秒）：`/run`、`/search`、`/help`、`/status`、`/clear`、`/compact`、`/ctx`、`/btw`、`/remember`、`/recall`、`/model`、`/cron`、`/plugin`、…

**插件** —— 双 runtime 设计：

| Runtime | Sandbox | 何时用 |
|---|---|---|
| **wasm** | runtime + `host.cli` 允许列表 | 不可信、受限宿主、跨平台 |
| **node / bun / deno** | install-time 允许列表 | OpenClaw 兼容,完整系统访问 |

`/plugin install <name>` 从 slash 命令或桌面 UI 装。每 agent 启用上限防止上下文溢出（`tools_tokens` 预算）。现有插件：jimeng（即梦图像 / 视频）、douyin（抖音）、wechat、xianyu（闲鱼）、travel。

---

## 配置

```json5
{
  gateway: { port: 18888 },
  models: {
    providers: {
      doubao:   { apiKey: "${DOUBAO_API_KEY}" },
      deepseek: { apiKey: "${DEEPSEEK_API_KEY}" },
      ollama:   { baseUrl: "http://localhost:11434" },
    },
  },
  agents: {
    defaults: {
      model: { primary: "doubao/doubao-seed-1-6-pro",
               flash: ["doubao/doubao-seed-2.0-lite"] },
    },
    list: [{ id: "main", default: true }],
  },
  channels: {
    telegram: { botToken: "${TELEGRAM_BOT_TOKEN}", dmPolicy: "pairing" },
  },
}
```

所有字符串支持 `${VAR}` 环境变量替换。优先级：CLI flag > `$RSCLAW_BASE_DIR/rsclaw.json5` > `~/.rsclaw/rsclaw.json5` > `./rsclaw.json5`。

---

## CLI 速查

```bash
rsclaw setup / onboard / configure        # 初始化 + 向导
rsclaw start / stop / restart / status    # gateway 控制
rsclaw doctor --fix                       # 健康检查
rsclaw update / upgrade                   # 自更新
rsclaw tools install chrome / ffmpeg / node / python / opencode
rsclaw channels login wechat              # 扫码登录
rsclaw memory status / search / docs      # 记忆操作
rsclaw knowledge ingest / search / list   # 知识库操作
rsclaw pairing pair / list / revoke       # 通道配对
rsclaw browser open / snapshot / click    # 无头 Chrome CLI
rsclaw anycli run / install / search      # 结构化 web 抽取
```

---

## 从 OpenClaw 迁移

```bash
openclaw gateway stop
rsclaw setup          # 检测到 ~/.openclaw/,提示一键导入
rsclaw start
```

导入对 `~/.openclaw/` 是只读;新数据写到 `~/.rsclaw/`。两边可以并行跑（端口 18888 vs 18789）——彼此不共享状态。

| | RsClaw | OpenClaw |
|---|---|---|
| 二进制体积 | ~21 MB 单文件 | ~300 MB + node_modules |
| 启动 | ~26 ms | 2–5 秒 |
| 闲置内存 | ~20 MB | ~1 GB |
| 长期记忆 | 三层 + Weibull 衰减 + 混合检索 | — |
| 知识库 | OOXML + RRF 检索,按 collection | — |
| A2A | Google v1.0 + hub-spoke relay | — |
| 浏览器 | 内置 CDP,50+ 动作 | — |
| 多后端 agent | Native / Claude Code / OpenCode / ACP | — |
| 执行安全 | 50+ deny 规则 | — |

---

## 开发

```bash
git clone https://github.com/rsclaw-ai/rsclaw.git
cd rsclaw
cargo build --profile release-dev          # 别名:`cargo brd`
RUST_LOG=rsclaw=debug cargo run -- gateway run
```

要求：Rust 1.91+,macOS / Linux / Windows。可选：ffmpeg、Chrome。

```
src/
  agent/       runtime、memory、tools、preparse
  channel/     13 个 channel adapter
  config/      JSON5 loader、schema、env 解析
  gateway/     启动、热重载、watch
  provider/    LLM provider、failover、prefix-cache
  server/      Axum HTTP、REST、OpenAI 兼容
  store/       redb + tantivy + hnsw_rs
  browser/     Chrome CDP 自动化
  a2a/         A2A v1.0 + hub-spoke relay
  acp/         ACP 客户端后端
  kb/          知识库 + collections
  plugin/      wasm + node/bun/deno 运行时
  skill/       skill 结晶化管线
```

---

## 支持

- ⭐ **Star** —— 帮助更多人发现 RsClaw
- 🐛 **Issues** —— [github.com/rsclaw-ai/rsclaw/issues](https://github.com/rsclaw-ai/rsclaw/issues)
- 💬 **社区** —— [WeChat / Feishu / QQ / Telegram](https://rsclaw.ai/zh/community)
- 🤝 **贡献** —— 见 [CONTRIBUTING.md](../../CONTRIBUTING.md)

## License

双协议 **MIT** ([LICENSE-MIT](../../LICENSE-MIT)) **OR** **Apache-2.0** ([LICENSE-APACHE](../../LICENSE-APACHE)),任选其一。

个人、商业、企业、SaaS、闭源产品都可以自由使用。可修改、可再分发,无 copyleft 义务。跟 Rust、Tokio、Serde、Axum 同一套许可。

除非另有说明,所有贡献按同一双许可协议授权。

---

🦀 用 Rust 构建。感谢 OpenClaw 社区的启发。
