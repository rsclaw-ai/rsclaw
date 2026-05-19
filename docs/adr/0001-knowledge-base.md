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

## Decision

新增 `src/kb/` 模块，复用 redb + tantivy + hnsw_rs 三件套但**独立 DB 文件 + 独立 lifecycle + 完全自包含目录**。设计组合了 RAG 领域的成熟模式与 rsclaw 独有创新。

### rsclaw 独有创新（差异化设计）

这些是基于 rsclaw 现有基础设施（GPU 集群 + agent memory + 多 channel）才有的能力，不是任何同类开源系统能简单复制的：

1. **Fleet-accelerated batch ingest** —— 千篇级 PDF 入库时，chunks 分发到 rsclaw-llm fleet（IDC GPU 集群）并行 embed + entity 抽取，单机数小时 → 集群数分钟。依赖 rsclaw-server `/v1/embed/batch` + `/v1/entity/batch` endpoint
2. **`kb_explain` 工具** —— agent 调 `kb_search` 拿 `trace_id`，再调 `kb_explain(trace_id)` 拿到完整检索推理 trace（BM25 命中 term / dense 维度激活 / entity_index 触发 / MMR 选择理由 / citation_confidence 因子分解）。让 agent 能解释"为什么引用这条"
3. **`citation_confidence` 评分** —— 独立于 relevance score：`f(quality × recency_decay × is_latest_version × entity_alignment × source_kind_trust)`。映射到三档 `citation_tier`（authoritative / supporting / indicative），agent 决定引用措辞强度
4. **Memory ↔ KB 双向流** —— 高稳定性 agent memory item 可经用户确认晋升为 KB doc；session 启动时按对话主题预热 KB 的 authoritative chunks 进 agent memory。利用 rsclaw "agent memory + 用户 KB" 双系统的独有结构，带防回路机制

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
| Embedding | BGE-M3 本地 (1024) 默认 + 远程 API 备路 + **Fleet 大批量路径**（独有） |
| **Jobs queue** | SQLite-backed in kb.redb，`dedupe_key` partial unique index + `claim_token` + `reclaim_stale_jobs`（参考 Sidekiq / RQ / Faktory 通用模式） |

### 检索

| 决策点 | 选择 |
|---|---|
| Retrieval | Tool-call (`kb_search` / `kb_fetch` / `kb_list_docs` / `kb_similar` / `kb_search_entities` / **`kb_explain`**)，不做 auto-RAG |
| Hybrid | Dense (BGE-M3) + Sparse (tantivy BM25) + RRF 融合 |
| Diversity | MMR 默认开 (λ=0.5) |
| **Entity inverted index** | `KbEntity` + `KbEntityIndex`，入库时 O(N) 一次建索引；查询时 O(1) 查 entity_alignment |
| 实体感知 | `require_entities` / `boost_entities` 参数 + RAG 引用纪律 prompt |
| **`citation_confidence`** | 独有：独立于 relevance；三档 tier 引导 agent 引用措辞 |
| Citation | agent 用 `[^kb:<chunk_id>]` 标记，前端 `<KbCitation>` 渲染 + 点击跳源 |
| Reranker | v1 不接，留 trait |

### 数据源同步

| 决策点 | 选择 |
|---|---|
| **Syncer trait** | `KbSourceSyncer`：trait-based provider 抽象，所有源（包括 ManualUpload）统一接口 |
| **SyncState** | KV 持久化（kb.redb），含 cursor / seen_index / daily_budget / last_seen_id / status / consecutive_failures / paused_until |
| **`seen_index` 实现** | `ScalableSeenSet`：Bloom filter (假阳性<0.1%) + LRU(10000) 精确集；假阳性由 chunk-level deterministic id 兜底 |
| Scheduler | 复用 `src/cron/`，5min tick + event-driven (`FsChangeEvent` / `ChannelMessageEvent`) |
| V1 syncers | ManualUpload + UrlSyncer + LocalFolderSyncer + ChannelHistorySyncer |
| **三层 dedup** | API/HTTP cursor → SyncState `seen_index` → chunk-level deterministic id |
| 退避 | 指数：1→0、2→1min、3→5min、6→1h、12→6h、>12 24h 封顶 |
| 错误隔离 | scheduler 永不 panic，单 syncer 失败不影响其他 |

### Memory ↔ KB 桥（独有）

| 决策点 | 选择 |
|---|---|
| **Memory → KB 晋升** | 满足 stability ≥ 0.85 + importance ≥ 0.7 + **用户手动确认** → 创建 KbDoc `source_kind=Doc`, `source_id=agent_memory:<mem_id>`，标 `promoted_to_kb_at` |
| **KB → Memory 预热** | session 启动时按对话主题 `kb_search(k=5, min_tier=authoritative)` → 注入 agent memory，标 `from_kb_at` |
| **防回路** | 晋升标记防止 memory 重新被晋升；注入标记防止 KB 内容回流晋升候选 |

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

6.5 phases + 1 收尾 / 总工期 **~13.5 周**：
1. MVP (3 周)：model + content_store + jobs queue + entity index + Hybrid+RRF + CLI 基础
2. 基础可用 (2 周)：UI 面板 + Citation 渲染 + entity_alignment + MMR + **citation_confidence + kb_explain**
3. OCR Fast (2 周)：RapidOCR + 预扫描路由
4. OCR Strong/Fleet (2 周)：PaddleOCR-VL + Qianfan vLLM sidecar
5. Syncer 框架 (2 周)：4 个 v1 syncer + 数据源 tab + sync CLI
5.5. **Fleet + Memory 桥** (1.5 周)：fleet_dispatch + rsclaw-server endpoints + memory_bridge + warm_session
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
- **Fleet 加速**：千篇级 ingest 在 IDC 集群上分钟级完成 —— 同类开源系统做不到
- **可解释 RAG**：`kb_explain` + `citation_confidence` 让 agent 能解释决策，降低幻觉

### 负面
- **hnsw_rs 不支持单点删** → tombstone + 后台 rebuild 复杂度（双 buffer 缓解）
- **OCR Fleet 层引入 vLLM/SGLang sidecar** → 多一个 Python 服务（复用数字人 sidecar 模式）
- **13.5 周工期** → 不是小投入；按 phase 灰度
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
**否决**：13.5 周 / 多模块项目级 feature，spec 必须可被 PR / review / implementation 引用。spec 入 `docs/specs/`，ADR 入 `docs/adr/`，AI brainstorming 草稿继续放 `docs/superpowers/` (ignored)。

### G. Chunk body 存 DB
**否决**：DB 臃肿；丧失 Obsidian / grep 兼容；备份只能 DB dump。**改为 content store on disk + DB 只存 byte_offset**。读 chunk 多一次文件 IO（~10μs），可忽略。

### H. Chunk ID 用 ULID
**否决**：再 ingest 同样内容会产生新 ID，索引爆膨胀。**改为 deterministic sha256(kind|source_id|seq|content) 截 32 hex**，完全幂等。

### I. 数据源各自实现，无统一 syncer 框架
**否决**：URL 周期重抓、目录监控、聊天增量这三类都需要 cursor + dedup + 退避 + 配额 + 错误隔离的同一套机制。**抽 `KbSourceSyncer` trait + `SyncState`**，每加一个源 = 一个 impl，零基础设施工作。

### J. 通过第三方 SaaS 中介做源 OAuth（Composio / Pipedream 等）
**否决**：rsclaw 主中国市场 + 私有部署，违背 desktop-first 和数据隐私默认（OAuth/HMAC 都过中介后端）。**直接对接原生 API**（飞书 / 企微 / IMAP / 等）。

### K. `seen_index` 用纯持久化 HashSet
**否决**：聊天历史几年下来百万级 ID，100MB+ 内存。**改 `ScalableSeenSet`：Bloom filter + LRU 精确集**；假阳性由 chunk-level deterministic id 兜底正确。

### L. 不做 fleet 路径，纯本地 ingest
**否决**：rsclaw 已经有 IDC GPU 集群，**不利用就是浪费独有基础设施**。本地路径作为 fallback，大批量自动走 fleet 让用户感受到 rsclaw 的硬件优势。

### M. `citation_confidence` 合并到 `score` 一个字段
**否决**：relevance（语义相关）和 trust（可信度）是两个独立维度。同一 score 的两个 chunk，一个是上周的官方 PRD，一个是 1 年前的群聊截图，应当区别对待。**分两个字段 + 三档 tier 明示给 agent**。

### N. KB ↔ Memory 自动晋升（无用户确认）
**否决**：agent memory 包含很多噪音（误判、上下文性临时事实），自动晋升会污染 KB。**必须用户手动 promote**，KB 是"用户主动维护"的 source of truth。

## References

### 算法 / 模式（公开发表的方法）

- **RRF 融合**：Cormack et al., "Reciprocal Rank Fusion outperforms Condorcet and individual Rank Learning Methods" (SIGIR 2009)
- **MMR 多样性**：Carbonell & Goldstein, "The Use of MMR, Diversity-Based Reranking for Reordering Documents and Producing Summaries" (SIGIR 1998)
- **SimHash**：Charikar, "Similarity estimation techniques from rounding algorithms" (STOC 2002)
- **BM25**：Robertson & Walker, "Some Simple Effective Approximations to the 2-Poisson Model for Probabilistic Weighted Retrieval" (SIGIR 1994)
- **HNSW**：Malkov & Yashunin, "Efficient and robust approximate nearest neighbor search using Hierarchical Navigable Small World graphs" (2016)
- **Bloom Filter**：Bloom, "Space/Time Trade-offs in Hash Coding with Allowable Errors" (CACM 1970)
- **BGE-M3**：BAAI, "BGE M3-Embedding: Multi-Lingual, Multi-Functionality, Multi-Granularity Text Embeddings Through Self-Knowledge Distillation"
- **Job queue dedupe + claim_token**：通用 production 模式，参考 Sidekiq / RQ / Faktory / GoodJob 设计

### 工具 / 模型（all permissive license）

- **RapidOCR**：PP-OCRv4 蒸馏 ONNX 实现（Apache 2.0）
- **PaddleOCR-VL 1.5**：Apache 2.0, OmniDocBench v1.5 SOTA pipeline
- **Qianfan-OCR 4B**：Apache 2.0, end-to-end SOTA + KIE，via vLLM/SGLang serve
- **jieba-rs**：中文分词（MIT）
- **ort**：ONNX Runtime Rust binding（Apache 2.0 / MIT）
- **tantivy**：full-text search engine（MIT）
- **hnsw_rs**：HNSW Rust impl（Apache 2.0）
- **redb**：embedded KV store（Apache 2.0 / MIT）

### rsclaw 内部依赖

- `src/agent/memory.rs` —— lifecycle 区别参照 + Memory↔KB 桥的对侧
- `src/store/` —— redb + tantivy + hnsw_rs 基础设施
- `src/cron/` —— syncer scheduler 集成点
- `src/channel/` —— ChannelHistorySyncer 复用 `fetch_messages`
- `src/browser/` —— UrlSyncer 复用渲染
- `src/agent/prompt_builder.rs` —— RAG 引用纪律 prompt 注入点
- `project_rsclaw_llm_rollout.md`（auto-memory）—— Fleet 部署上下文
- `project_context_mgmt_v2.md`（auto-memory）—— KV cache 优化路线

### 设计灵感

- Notion AI / Perplexity —— citation 渲染 UX
- Obsidian —— `.md` 文件本地优先的 PKM 模型
- Anthropic Claude Projects / OpenAI Custom GPTs —— 用户主动 curate 知识库的产品形态
- Glean / Hebbia —— enterprise RAG 的 citation 严谨度参考
