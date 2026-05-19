# ADR 0001: Knowledge Base — 用户级 RAG 知识库 + 数据源同步框架

- **Status**: Accepted
- **Date**: 2026-05-19
- **Spec**: [docs/specs/2026-05-19-knowledge-base.md](../specs/2026-05-19-knowledge-base.md)

## Context

rsclaw 当前没有用户主动管理的知识库。`src/agent/memory.rs` 提供的是 agent 自学/会衰减的长期记忆，不能满足以下需求：

- 用户喂入产品文档 / PDF / URL / 聊天历史 / 邮件，agent 在回答时引用
- 引用必须可溯源（点击跳转到原文）
- 内容不会被 agent 自然遗忘 / 衰减
- 多 agent 共享同一份知识
- 数据源支持**周期/事件触发增量同步**（URL 重抓、目录监控、聊天历史增量）

memory 系统的衰减、importance、tier transition 等机制对知识库场景是反模式（用户不希望昨天上传的合同今天被"忘了"）。

经研究 OpenHuman (`~/git/openhuman/src/openhuman/memory/tree/` 和 `composio/providers/`) 之后，借鉴了多项 production-grade 模式：canonicalize-first / deterministic chunk_id / content store on disk / jobs queue / entity inverted index / `ComposioProvider` → `KbSourceSyncer` trait 改造。

## Decision

新增 `src/kb/` 模块，复用 redb + tantivy + hnsw_rs 三件套但**独立 DB 文件 + 独立 lifecycle + 完全自包含目录**。核心选择：

### 数据 & 存储

| 决策点 | 选择 |
|---|---|
| 用户边界 | 全局一个库（所有 agent 共享） |
| 文档源 v1 | 本地文件 + URL（单页）+ 聊天历史 + 图片 + 邮件 (.eml/.mbox 手动上传) |
| **目录布局** | `~/.rsclaw/kb/{md,raw,db,idx,hnsw,state}/` —— 完全自包含，可整目录搬运 |
| **Content store on disk** | canonicalized markdown 作为 `.md` 文件落 `md/<kind>/`，DB 只存 path + sha256 + byte_offset |
| **Raw cache 默认开** | `raw/<doc_id>.<ext>` 存原始字节；用户可关 (`kb.keep_raw=false`) |
| 存储后端 | redb (`db/kb.redb`) + tantivy (`idx/`) + hnsw_rs (`hnsw/kb_v1024_<id>.hnsw`)，独立 DB 文件 |
| **chunk_id deterministic** | `sha256(kind\|source_id\|seq\|content)` 截 32 hex，幂等 upsert（替代 ULID） |
| Source kind 短化 | `Doc / Chat / Url / Img / Mail`（on-wire 短字符串，对齐目录名） |

### Pipeline

| 决策点 | 选择 |
|---|---|
| **Canonicalize-first** | 所有源 → CanonicalizedSource { markdown, metadata } → 下游统一 |
| Chunker | 512/64 token，**强制 `heading_path` 前缀注入 indexed text**，SimHash 去重 |
| OCR 三层 | Fast=RapidOCR (PP-OCRv4 ONNX) / Strong=PaddleOCR-VL 1.5 / Fleet=Qianfan-OCR 4B via rsclaw-server :8444 vLLM sidecar |
| OCR 路由 | 按文档特征预扫描（图表 → Fleet / KIE → Fleet / 表格公式 → Strong / 纯文本 → Fast） |
| Embedding | BGE-M3 本地 (1024) 默认 + 远程 API 备路 |
| **Jobs queue** | SQLite-backed in kb.redb，`dedupe_key` partial unique index + `claim_token` + `recover_stale_locks`（OpenHuman 模式） |

### 检索

| 决策点 | 选择 |
|---|---|
| Retrieval | Tool-call (`kb_search` / `kb_fetch` / `kb_list_docs` / `kb_similar` / **`kb_search_entities`**)，不做 auto-RAG |
| Hybrid | Dense (BGE-M3) + Sparse (tantivy BM25) + RRF 融合 |
| Diversity | MMR 默认开 (λ=0.5) |
| **Entity inverted index** | `KbEntity` + `KbEntityIndex`，入库时 O(N) 一次建索引；查询时 O(1) 查 entity_alignment |
| 实体感知 | `require_entities` / `boost_entities` 参数 + RAG 引用纪律 prompt |
| Citation | agent 用 `[^kb:<chunk_id>]` 标记，前端 `<KbCitation>` 渲染 + 点击跳源 |
| Reranker | v1 不接，留 trait |

### 数据源同步

| 决策点 | 选择 |
|---|---|
| **Syncer trait** | `KbSourceSyncer` (参考 OpenHuman `ComposioProvider`)，所有源（包括 ManualUpload）统一接口 |
| **SyncState** | KV 持久化（kb.redb），含 cursor / synced_ids / daily_budget / last_seen_id / status / consecutive_failures / paused_until |
| **synced_ids 实现** | Bloom filter (假阳性<0.1%) + LRU(10000) 精确集；假阳性由 chunk-level deterministic id 兜底 |
| Scheduler | 复用 `src/cron/`，5min tick + event-driven (`FsChangeEvent` / `ChannelMessageEvent`) |
| V1 syncers | ManualUpload + UrlSyncer + LocalFolderSyncer + ChannelHistorySyncer |
| **三层 dedup** | API/HTTP cursor → SyncState `synced_ids` → chunk-level deterministic id |
| 退避 | 指数：1→0、2→1min、3→5min、6→1h、12→6h、>12 24h 封顶 |
| 错误隔离 | scheduler 永不 panic，单 syncer 失败不影响其他 |

### Lifecycle / Security / 隐私

| 决策点 | 选择 |
|---|---|
| 删除机制 | Tombstone + filter + 后台 compactor，30 天恢复期 |
| Compactor | 1h tick + 03:00 强制 + 残骸率 >15% 触发；HNSW 双 buffer μs 级原子切换 |
| 模型迁移 | 双写 + 渐进重建 + 7 天回滚 |
| **PII redaction** | 日志全栈走 `src/kb/util/redact.rs`，source_id / 内容预览永远哈希 |
| 远程开关 | 默认本地全栈；启 remote 弹一次确认 |
| 聊天历史隐私 | 默认 `self_messages_only=true`（只入用户消息+@自己） |

### 实施分期

5 phases + 1 收尾 phase / 总工期 **~12 周**：
1. MVP (3 周)：model + content_store + jobs queue + entity index + Hybrid+RRF + CLI 基础
2. 基础可用 (2 周)：UI 面板 + Citation 渲染 + entity_alignment + MMR
3. OCR Fast (2 周)：RapidOCR + 预扫描路由
4. OCR Strong/Fleet (2 周)：PaddleOCR-VL + Qianfan vLLM sidecar
5. Syncer 框架 (2 周)：4 个 v1 syncer + 数据源 tab + sync CLI
6. Compactor / 迁移 / 收尾 (1 周)

## Consequences

### 正面
- **零新依赖**（除 `ort` for OCR ONNX），rsclaw 二进制体积可控
- **和 KV cache / context-mgmt v2 完全兼容**：tool-call 路径 + 确定性 chunk 排序
- **自包含目录**：`cp -r ~/.rsclaw/kb/` 即完整备份
- **Obsidian / grep / ripgrep 兼容**：canonicalized markdown 在磁盘可被任何工具打开
- **多 agent 共享天然成立**
- **可溯源 + 点击跳源**：UI 体验贴近 Notion AI / Perplexity
- **幂等入库**：deterministic chunk_id + 三层 dedup
- **数据源框架可演进**：v2 加 MailSyncer / 整站爬等只需 impl trait，不改架构

### 负面
- **hnsw_rs 不支持单点删** → tombstone + 后台 rebuild 复杂度（双 buffer 缓解）
- **OCR Fleet 层引入 vLLM/SGLang sidecar** → 多一个 Python 服务（复用数字人 sidecar 模式）
- **12 周工期** → 不是小投入；按 phase 灰度
- **content store on disk** → DB 与 md/ 目录必须一起备份（loose coupling 但有依赖）
- **raw/ 默认开** → 100 PDF ≈ 几百 MB 磁盘（用户可关）

### 中性
- KB 体积可能 GB 级，但独立目录隔离了对 hot KV 路径的影响
- BGE-M3 模型 ~2GB，首次启动需下载

## Alternatives Considered

### A. Auto-RAG（每轮自动检索注入 system prompt）
**否决**：每轮 top-K 变 → system prompt 变 → KV cache 全废。与 rsclaw 刚做完的 context-mgmt v2 + KV 缓存优化路线冲突。

### B. 独立向量库 (sqlite-vec / lancedb)
**否决**：多一套存储依赖；lancedb 二进制 +50MB；和现有 memory 检索逻辑割裂。等真到千万级文档再切换。

### C. 外接 qdrant / milvus
**否决**：违背 desktop-first；用户需额外部署 service；KB 是端侧概念，不该跑去 GPU 机房。

### D. OCR 选 Tesseract
**否决**：中文准确率显著低于 RapidOCR；ONNX 路径接入成本相当。

### E. Fleet OCR 走百度千帆云 API
**否决**：违背"chunk 文本不出本机"的隐私默认。改 rsclaw-llm fleet 自部署 vLLM sidecar。

### F. KB spec 不入 git
**否决**：12 周 / 多模块项目级 feature，spec 必须可被 PR / review / implementation 引用。spec 入 `docs/specs/`，ADR 入 `docs/adr/`，AI brainstorming 草稿继续放 `docs/superpowers/` (ignored)。

### G. Chunk body 存 DB（原 spec v1 设计）
**否决**：DB 臃肿；丧失 Obsidian / grep 兼容；备份只能 DB dump。**改为 content store on disk + DB 只存 byte_offset**（OpenHuman 同款）。读 chunk 多一次文件 IO（~10μs），可忽略。

### H. Chunk ID 用 ULID
**否决**：再 ingest 同样内容会产生新 ID，索引爆膨胀。**改为 deterministic sha256(kind|source_id|seq|content) 截 32 hex**（OpenHuman 同款），完全幂等。

### I. 数据源各自实现，无统一 syncer 框架
**否决**：URL 周期重抓、目录监控、聊天增量这三类都需要 cursor + dedup + 退避 + 配额 + 错误隔离的同一套机制。**抽 `KbSourceSyncer` trait + `SyncState`（OpenHuman `ComposioProvider` 模式）**，每加一个源 = 一个 impl，零基础设施工作。

### J. 走 Composio 做第三方源 OAuth（Gmail / Slack / Notion 等）
**否决**：rsclaw 主中国市场 + 私有部署，Composio 是境外 SaaS（OAuth/HMAC/socket.io 都过它后端），违背 desktop-first 和数据隐私默认。**保留 Composio 的 trait 设计模式，但 impl 直接对接原生 API**。

### K. `synced_ids` 用纯持久化 HashSet
**否决**：聊天历史几年下来百万级 ID，100MB+ 内存。**改 Bloom filter (假阳性<0.1%) + LRU(10000) 精确集**；假阳性由 chunk-level deterministic id 兜底正确。

## Open follow-up（不进 KB spec，但记录给 rsclaw 主线借鉴）

### 1. OpenHuman `tokenjuice/` → rsclaw agent tool output 压缩

[`~/git/openhuman/src/openhuman/tokenjuice/`](file:///Users/oopos/git/openhuman/src/openhuman/tokenjuice/) 是 [vincentkoc/tokenjuice](https://github.com/vincentkoc/tokenjuice) 的 Rust port，把 verbose tool 输出（git status / cargo build / docker logs）按 JSON 规则压缩，pass-through safe。**直接 vendor 进 rsclaw agent loop → 每次 tool turn 省 30-60% token，几乎零工程量**。和 KB 完全平行，但 ROI 极高。

### 2. OpenHuman `learning/` → rsclaw evolution / meditation

[`~/git/openhuman/src/openhuman/learning/`](file:///Users/oopos/git/openhuman/src/openhuman/learning/) 是 agent 自学子系统。值得 rsclaw 主线借鉴的：

- `reflection.rs` —— 四类结构化 LLM 反思输出（observations / patterns / preferences / user_reflections），喂给 meditation
- `tool_tracker.rs` —— 工具有效性追踪
- `stability_detector.rs` —— "同一观察被多次确认才晋升"的稳定性评分
- `prompt_sections.rs` —— 学到的东西分段注入下次 prompt
- `transcript_ingest/` —— heuristic-only 设计（不强依赖 LLM），先 heuristic 跑通再 LLM 增强的工程模式

**KB 不抄它**（物种不同），但 rsclaw 的 heartbeat / meditation / evolution 路线可借鉴。

## References

### 完整设计
- [docs/specs/2026-05-19-knowledge-base.md](../specs/2026-05-19-knowledge-base.md)

### 现有代码
- `src/agent/memory.rs` —— lifecycle 区别参照
- `src/store/` —— redb + tantivy + hnsw_rs 基础设施
- `src/cron/` —— syncer scheduler 集成点
- `src/channel/` —— ChannelHistorySyncer 复用 fetch_messages
- `src/browser/` —— UrlSyncer 复用渲染

### OpenHuman 借鉴（具体路径）
- `~/git/openhuman/src/openhuman/memory/tree/` —— canonicalize-first / chunk_id / content_store / jobs queue / entity index
- `~/git/openhuman/src/openhuman/memory/tree/canonicalize/` —— 源适配模式
- `~/git/openhuman/src/openhuman/composio/providers/` —— sync trait + SyncState 模式
- `~/git/openhuman/src/openhuman/composio/providers/sync_state.rs` —— SyncState 字段设计参考
- `~/git/openhuman/src/openhuman/composio/providers/gmail/sync.rs` —— Gmail incremental sync 模式（v2 MailSyncer 参考）
- `~/git/openhuman/src/openhuman/composio/periodic.rs` —— 周期 scheduler 模式

### Memory（auto-memory）
- `project_rsclaw_llm_rollout.md` —— Fleet 部署上下文
- `project_context_mgmt_v2.md` —— KV cache 优化路线
- `project_three_repo_topology.md` —— rsclaw / rsclaw-server / rsclaw-llm 拓扑
