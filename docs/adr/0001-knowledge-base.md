# ADR 0001: Knowledge Base — 用户级 RAG 知识库

- **Status**: Proposed
- **Date**: 2026-05-19 (revised after architectural review)
- **Spec**: [docs/specs/2026-05-19-knowledge-base.md](../specs/2026-05-19-knowledge-base.md)

## Context

rsclaw 当前没有用户主动管理的知识库。`src/agent/memory.rs` 提供的是 agent 自学/会衰减的长期记忆，不能满足以下需求：

- 用户喂入产品文档 / PDF / URL / 聊天历史 / 邮件，agent 在回答时引用
- 引用必须可溯源（点击跳转到原文）
- 内容不会被 agent 自然遗忘 / 衰减
- 多 agent 共享同一份知识，**但需要权限边界**（聊天 / 邮件 / 私有内容不能裸泳）
- 数据源支持周期 / 事件触发增量同步（URL 重抓、目录监控、聊天历史增量）

memory 系统的衰减、importance、tier transition 等机制对知识库场景是反模式。

### 设计审查（第一版的关键缺陷）

第一版 spec (Accepted, 后改为 Proposed) 经审查发现 9 个架构问题：

1. Jobs queue 用 SQL 语言（partial unique index / UPDATE...RETURNING）描述 redb 后端 — 不兼容
2. Writer 不是原子的（先写文件再写 DB 再删旧文件）— 崩溃导致孤儿 / 漂移
3. Bloom filter 假阳性会让 syncer **跳过** ingest，chunk-level dedup 救不了 — 静默漏数据
4. doc_id (ULID) 与 source_id 耦合，重传同文件产生新 chunk_id — 破坏幂等
5. HNSW 文件双 buffer 太轻 — 没解决运行时正确性，缓存 vs source of truth 边界不清
6. citation_confidence 一刀切 recency decay — 误伤合同 / API spec / 制度文档
7. kb_explain 承诺 "dense 维度激活" — embedding 维度对人不可读，是假可解释性
8. ChannelHistorySyncer 假设 Channel trait 有 `fetch_messages` — 不存在
9. 全局共享 + 聊天历史 + raw 默认开 — 多 agent 数据互窜风险

本 ADR 反映**修订后的设计**，引入四个新的核心架构机制：SourceIdentity + VersionGraph、IngestLedger + Outbox、PermissionScope、Index Rebuild Contract。

## Decision

### MVP 范围（4 周，单人全职）

| 包含 | 不包含 (移 v2) |
|---|---|
| Doc + Url 两 source | Chat / Img / Mail source |
| ManualUploadSyncer + UrlSyncer | LocalFolderSyncer / ChannelHistorySyncer / MailSyncer |
| canonicalize → IngestLedger → Outbox 异步 chunks+embed | Fleet-accelerated batch ingest |
| HybridRetrieval (Dense+BM25+RRF) + MMR + entity_alignment | kb_explain / citation_confidence / recency_policy |
| visibility (Global + Private + Channel + Agent) | Memory ↔ KB bridge |
| CLI (kb add / ls / rm / search / show / compact / stats / export) | Tauri UI 面板 |
| BGE-M3 local embedder | Remote API embedder / Reranker |
| pdf-extract 文本层 | OCR Fast / Strong / Fleet |

### 数据 & 身份

| 决策点 | 选择 |
|---|---|
| 用户边界 | 全局 KB pool + per-doc visibility |
| 目录布局 | `~/.rsclaw/kb/{md,raw,db,idx,hnsw,state}/` 自包含 |
| Content store | canonicalized markdown 作为 `.md` 文件落 `md/<kind>/`；DB 只存 path + sha256 + byte_offset |
| Raw cache | 默认开 (`kb.keep_raw=true`)；用户可关 |
| **logical_source_id** | `file:sha256:<hash>` / `url:<normalized>` / `chat:<channel>:<window>` / `mail:<msg_id>` — 幂等 key，与 doc_id (ULID 实例) 分离 |
| **VersionGraph** | KbDoc.version + `kb_doc_latest_version` 表 + 老版本保留 30 天；支持回滚 + time-travel 查询 (v2) |
| **chunk_id** | deterministic `sha256(logical_source_id\|seq\|content)` 截 32 hex — 真幂等 |
| Source kind 短化 | `Doc / Chat / Url / Img / Mail`（MVP 仅 Doc/Url） |

### Atomicity (IngestLedger + Outbox)

| 决策点 | 选择 |
|---|---|
| **IngestLedger** | 每次 ingest 写一条 LedgerEntry，记录 `op / new_paths / old_paths / status`；状态机：Pending → IndexingComplete → CleanupPending → Done |
| **Outbox** | Job 在 ingest tx 内写到 redb (`kb_jobs_*`)，worker 异步轮询。chunk+embed+index 完全异步，崩溃可恢复 |
| **文件 stage 永不直接删** | 文件原子写后，不被 ingest 路径删除；compactor 按 ledger 推进物理清理（grace 1h 防进行中 ingest） |
| **崩溃恢复** | 矩阵化设计：stage 后崩 / commit 后崩 / worker 中途崩，分别由 compactor + worker pool + reclaim_stale_jobs 兜底 |

### Jobs Queue (redb-native，非 SQLite)

| 决策点 | 选择 |
|---|---|
| 4 个 redb 索引表 | `jobs_by_id` (job_id → Job) / `jobs_by_dedupe_active` (dedupe_key → job_id, 仅 Ready+Running) / `jobs_by_status_priority` (composite key for claim order) / `job_claims` (claim_token with expiry) |
| 原子操作 | 所有改动一个 `begin_write` 事务，redb 单写者天然原子 |
| Stale recovery | `reclaim_stale_jobs` 扫 expires_at 过期的 claim → 重置 status=Ready |
| Handler 幂等 | 所有 job handler 必须幂等（chunk_id deterministic，重写无副作用） |

### Dedup (两层，不是 Bloom)

| 决策点 | 选择 |
|---|---|
| Layer 1: 持久 seen_items 表 | redb 表 `seen_items: (source_id, item_id) → SeenRecord`；B-tree lookup μs 级 |
| Layer 2: chunk-level deterministic id | logical_source_id 稳定 → chunk_id 稳定 → upsert no-op |
| **不用 Bloom** | Bloom 假阳性会让 syncer 在 chunking 前跳过 ingest，下游 dedup 救不了 |

### PermissionScope

| 决策点 | 选择 |
|---|---|
| **Visibility enum** | `Global / Agent { id } / Channel { id } / Private` |
| **CallerScope** | agent runtime 注入 `(agent_id, channel_id, user_id)`，agent 不能伪造 |
| Filter | retrieval pipeline 在 filter 阶段按 visibility 硬过滤 |
| 默认 (per source_kind) | Doc/Url/Img: Global · Mail: **Private** · Chat: **Channel** |
| 多 agent 跨问 | A 调 B 时 caller_scope 透传 A 的 scope；A 看不见的 B 也看不见（静默 mask） |

### Index Rebuild Contract

| 决策点 | 选择 |
|---|---|
| Source of truth | **redb** (KbDoc / KbChunk / vector / entity_index) |
| HNSW 角色 | 进程内可重建缓存；`ArcSwap<Hnsw>` 原子切换；disk snapshot 仅启动加速 |
| Tantivy 角色 | 同样可重建缓存；删 `idx/` 启动重建 |
| HNSW snapshot | 每 1h dump 到 `hnsw/*.snap.next` 原子改名 |
| 损坏恢复 | snapshot 损坏 / 落后 → 从 redb 重建（百万 chunk 几分钟） |
| 重建期间双写 | 罕见场景：rebuild 进行中的 ingest → push 到 pending_writes，重建完应用再 swap |

### Pipeline & Retrieval

| 决策点 | 选择 |
|---|---|
| Canonicalize-first | 所有源 → CanonicalizedSource { markdown, metadata }，下游零分支 |
| Chunker | 512/64 token，**heading_path 强制前缀**注入 indexed_text，SimHash-64 去重 |
| Embedding | BGE-M3 local (1024) 默认；远程 API v2 备路 |
| Entity index | `KbEntity` + `KbEntityIndex`，入库 O(N) 建索引；查询 O(1) entity_alignment |
| Hybrid | Dense (BGE-M3+hnsw) + Sparse (tantivy BM25) + RRF + MMR (λ=0.5) |
| Tool surface (MVP) | `kb_search / kb_fetch / kb_list_docs / kb_similar / kb_search_entities` |
| Tool (v2) | `kb_explain`（不含 dense 维度激活）；`kb_search --as_of` time-travel |
| Citation | agent 用 `[^kb:<chunk_id>]` 标记；前端 `<KbCitation>` 渲染 (v2)；CLI plain text (v1) |
| 配额 | search ≤8KB / fetch_full ≤32KB / ≤5次 search 每轮 |
| KV cache 友好 | chunk 严格按 (score desc, chunk_id asc) 排序；不带 timestamp / uuid |

### V2 增项（不在 MVP）

- **kb_explain** 工具：BM25 命中 term / Dense rank+score / RRF 贡献 / entity hit/miss / MMR 决策 / citation factors（**不含 embedding 维度**）
- **citation_confidence** + 三档 `citation_tier`（authoritative / supporting / indicative）
- **recency_policy** per doc：Evergreen / Versioned / TimeSensitive — 替代一刀切 decay
- **HistoryProvider** trait：channel adapter capability，首发 Feishu impl
- ChannelHistorySyncer / LocalFolderSyncer / MailSyncer
- Fleet-accelerated batch ingest（rsclaw-llm fleet `/v1/embed/batch` + `/v1/entity/batch`）
- Memory ↔ KB bidirectional bridge
- OCR Fast (RapidOCR) / Strong (PaddleOCR-VL) / Fleet (Qianfan-OCR)
- Tauri UI 控制台知识库面板
- Reranker (BGE-Reranker-v2-m3)
- Summary tree (per-source / per-topic / global)
- AGE 加密 raw/

### Lifecycle / Security

| 决策点 | 选择 |
|---|---|
| 删除机制 | Tombstone + filter + 后台 compactor，30 天恢复期 |
| Compactor | 1h tick + 03:00 强制；并行：孤儿文件清理 + Ledger 状态推进 + HNSW snapshot dump |
| PII redaction | 日志全栈走 `src/kb/util/redact.rs`，source_id / logical_source_id / 内容预览永远哈希 |
| 远程开关 | 默认本地全栈；启 remote 弹一次确认 |
| 聊天历史隐私 (v2) | 默认 `self_messages_only=true` + visibility=Channel |
| Caller scope 防伪造 | runtime 注入，agent 不能传 |

## Consequences

### 正面

- **零新依赖**（除 v2 OCR 引入 `ort`），rsclaw 二进制体积可控
- **崩溃安全**：IngestLedger + Outbox 解决 FS+DB 跨事务问题；任意步骤崩溃可恢复
- **真幂等**：logical_source_id + deterministic chunk_id；重传同物 NOOP
- **版本可回滚**：KbDoc.version + latest_version 指针；改一个指针就回退
- **权限边界清晰**：visibility + caller_scope，多 agent 互不窜
- **缓存可重建**：HNSW / tantivy 损坏不丢数据，redb 是 source of truth
- **自包含目录**：`cp -r ~/.rsclaw/kb/` 即完整备份
- **Obsidian / grep / ripgrep 兼容**
- **MVP 4 周可交付**（vs 原 13.5 周），快速上线 + 学习

### 负面

- **hnsw_rs 不支持单点删** → tombstone + 后台 rebuild（ArcSwap 缓解）
- **IngestLedger 增加 ~150 行 + 一张 redb 表 + compactor 周期任务** — production grade 必要复杂度
- **没 UI / 没 OCR / 单 worker** — MVP 只能 CLI 操作 + 文本类源
- **content store on disk** → DB 与 `md/` 目录必须一起备份
- **raw/ 默认开** → 100 PDF ≈ 几百 MB（用户可关）

### 中性

- KB 体积可能 GB 级，但独立目录隔离了对 hot KV 路径的影响
- BGE-M3 模型 ~2GB，首次启动需下载

## Alternatives Considered

### A. Auto-RAG（每轮自动检索注入 system prompt）
**否决**：每轮 top-K 变 → system prompt 变 → KV cache 全废。

### B. 独立向量库 (sqlite-vec / lancedb)
**否决**：多一套存储依赖；和现有 memory 检索逻辑割裂。

### C. 外接 qdrant / milvus
**否决**：违背 desktop-first；用户需额外部署 service。

### D. OCR 选 Tesseract
**否决**：中文准确率显著低于 RapidOCR。

### E. Fleet OCR 走百度千帆云 API
**否决**：违背"chunk 文本不出本机"的隐私默认。

### F. KB spec 不入 git
**否决**：项目级 feature，spec 必须可被 PR / review / implementation 引用。

### G. Chunk body 存 DB
**否决**：DB 臃肿；丧失 Obsidian / grep 兼容。**改为 content store on disk + DB 只存 byte_offset**。

### H. Chunk ID 用 ULID
**否决**：再 ingest 同样内容产生新 ID，索引爆膨胀。**改为 deterministic sha256(logical_source_id|seq|content)**。

### I. 数据源各自实现，无统一 syncer 框架
**否决**：所有 source 都需要 cursor + dedup + 退避 + 配额 + 错误隔离的同一套机制。**抽 trait + SyncState**。

### J. 通过第三方 SaaS 中介做源 OAuth（Composio 等）
**否决**：rsclaw 主中国市场 + 私有部署，违背 desktop-first 和数据隐私默认。

### K. `seen_index` 用 Bloom + LRU
**否决（架构 review 中改的）**：Bloom 假阳性会让 syncer 在 chunking 前 skip ingest，**下游 dedup 救不了**，静默漏数据。**改 redb 表 `seen_items`** 精确 lookup，B-tree 百万级 μs 级，CPU/IO 都不是瓶颈。

### L. Jobs queue 用 SQL 语言描述 redb 后端
**否决（架构 review 中改的）**：redb 不是 SQLite，没 partial unique index 也没 UPDATE...RETURNING。原 spec 表面用了 SQL 术语。**改 redb-native 4 表设计 + 单写事务**，符合 redb 实际能力。

### M. Writer "先写文件再写 DB 再删旧文件"
**否决（架构 review 中改的）**：任意步崩溃 → 孤儿文件 / 悬空指针 / 漏 job。**改 IngestLedger + Outbox + 文件 stage 永不删 + compactor 按 ledger 清理**。这是文件系统 + DB 跨事务的标准解。

### N. doc_id (ULID) 当 source_id 派生 chunk_id
**否决（架构 review 中改的）**：重传同文件产生新 doc_id → 新 source_id → 新 chunk_ids → 破坏幂等。**引入 logical_source_id** 作为内容稳定 key + KbDoc.version 形成版本链。

### O. HNSW 文件级 .next 改名做双 buffer
**否决（架构 review 中改的）**：文件级改名只解决 startup 加速，不解决运行时正确性 + 缓存/source of truth 边界不清。**改 redb 为 source of truth + 进程内 ArcSwap 切换 + snapshot 仅启动加速**。

### P. citation_confidence 一刀切 recency decay
**否决（架构 review 中改的）**：exp(-days/90) 对合同 / API spec / 制度文档全错。**改 recency_policy per doc：Evergreen / Versioned / TimeSensitive**（v2 落地，MVP 先不上 citation_confidence）。

### Q. kb_explain 解释 dense 维度激活
**否决（架构 review 中改的）**：embedding 维度对人不可读，承诺会卖假可解释性。**改解释 BM25 term / Dense rank+score / RRF 贡献 / entity hit/miss / MMR 决策 / citation factors**（v2）。

### R. ChannelHistorySyncer 假设 Channel trait 有 fetch_messages
**否决（架构 review 中改的）**：Channel trait 现在只有 name/send/run。**抽 `HistoryProvider` capability trait**（v1 定义），**impl 留 v2 由 channel adapter 各自贡献**，首发 Feishu。

### S. 全局共享 KB 无 visibility 标签
**否决（架构 review 中改的）**：聊天 / 邮件 / 合同 / 图片 OCR 混在 global pool 多 agent 读写 = 数据互窜。**引入 PermissionScope (visibility + caller_scope)**，聊天 / 邮件 / Private 不默认 Global。

## Open Questions (实施前需对齐)

**A. Jobs queue：redb 显式索引表 ✅ (已 confirm)**
- 待验证：高并发 worker 抢任务的实测延迟（目标 < 5ms p99）
- 🟢 **已落地**：4 表设计 + 单写事务 + fencing claim_token；单 worker 在 redb 单写者下足以满足 v1 throughput（多 worker 待 v1.x 压测后再决）

**B. logical_source_id schema 边界**
- URL canonicalize：除了 utm/fbclid/gclid 还要剥哪些 tracker？
- Chat bucket window：5min idle vs 6h 固定？
- 邮件无 Message-ID 极少见场景的 fallback 策略
- 🟡 **部分**：URL canonicalize 已 ship 通用 tracker stripping + 参数排序；Chat / Mail 不在 v1 范围

**C. recency_policy 默认表（v2）**
- 按 source_kind vs 按 tag vs 按用户全局策略
- 🔵 **V2**

**D. HistoryProvider 首发 channel**
- Feishu / Slack / Telegram / Matrix 哪个先？倾向 Feishu
- 🔵 **V2**

**E. PermissionScope 多 agent 跨问行为**
- A 调 B 时 A 看不见的 B 是否应静默 mask（倾向）vs 显式拒绝
- 🟢 **已落地**：retrieval pipeline 的 `keep_doc` 默认静默 mask（`doc.visible_to(scope)` 不通过 → chunk 被 skip，不在结果中暴露存在性）

**F. URL canonicalization 测试套件**
- v1 MVP 需要覆盖：Google 搜索结果、知乎、GitHub、b 站、微博、wikipedia
- 🟡 **部分**：Week 1 e2e 覆盖 `?utm_source=…&b=2&a=1` → 排序 + tracker 剥除；fixture-driven 多站测试套件未 ship

**G. HNSW snapshot 周期 + 重启重建阈值**
- snapshot 每 1h vs 每 N 个新 chunk
- 启动 snapshot 落后多少 chunks 直接重建
- 🟢 **已落地**：`kb compact` 触发 `hnsw_rs::file_dump` + JSON sidecar；`KbIndex::open_and_rebuild` 先尝试 restore，失败/缺文件 fall back rebuild。自动周期 snapshot 留给 gateway-resident scheduler（v1.x）

## Status

**v1.0 MVP ✅ shipped** (Weeks 1–5, branch `worktree-feat+knowledge-base`):

- 13 redb tables + per-table accessors
- `ingest_canonicalized` atomic single-tx pipeline with race-safe NOOP re-check
- `WorkerPool` with fencing-token transitions + bounded shutdown
- `KbIndex` composite (HNSW + tantivy) with snapshot persistence + CJK tokenizer
- 5 MCP tools: `kb_search`, `kb_fetch`, `kb_list_docs`, `kb_similar`, `kb_search_entities`
- Hybrid retrieval: dense + sparse → RRF → MMR → lazy text fetch
- Visibility filter (`Global / Agent / Channel / Private`) at retrieval boundary
- `ManualUploadSyncer` + `UrlSyncer` (conditional GET via ETag/Last-Modified)
- Compactor (orphan scan + ledger advancement)
- Regex entity extraction + `require_entities` / `boost_entities` filters
- CLI: `rsclaw kb add | ls | rm | search | show | visibility | compact | stats | export | sync-all`
- Test surface: 215 unit + 25 integration, 0 ignored, 0 failed

**Deferred to v1.x:**

- BGE-M3 real embedder (today: `StubEmbedder`)
- Gateway-resident syncer scheduler (today: `kb sync-all` CLI tick)

**V2 (post-MVP):** see "V2 增项" above.

## References

### 算法 / 模式（公开发表）

- **RRF 融合**：Cormack et al., "Reciprocal Rank Fusion outperforms Condorcet and individual Rank Learning Methods" (SIGIR 2009)
- **MMR 多样性**：Carbonell & Goldstein (SIGIR 1998)
- **SimHash**：Charikar (STOC 2002)
- **BM25**：Robertson & Walker (SIGIR 1994)
- **HNSW**：Malkov & Yashunin (2016)
- **BGE-M3**：BAAI Multi-Lingual / Multi-Functionality embeddings
- **Outbox pattern**：Chris Richardson "Microservices Patterns" Ch.3
- **Job queue dedupe + claim_token**：通用 production 模式（Sidekiq / RQ / Faktory / GoodJob 系列）

### 工具 / 模型（permissive license）

- **RapidOCR** (Apache 2.0)
- **PaddleOCR-VL 1.5** (Apache 2.0)
- **Qianfan-OCR 4B** (Apache 2.0)
- **jieba-rs** (MIT)
- **ort** (Apache 2.0 / MIT)
- **tantivy** (MIT)
- **hnsw_rs** (Apache 2.0)
- **redb** (Apache 2.0 / MIT)
- **arc-swap** (Apache 2.0 / MIT)
- **url** crate (Apache 2.0 / MIT)

### rsclaw 内部依赖

- `src/agent/memory.rs` — lifecycle 区别参照
- `src/store/` — 基础设施
- `src/cron/` — syncer scheduler
- `src/channel/` — HistoryProvider 适配点 (v2)
- `src/browser/` — UrlSyncer 渲染
- `src/agent/prompt_builder.rs` — RAG 引用纪律 prompt
- `project_rsclaw_llm_rollout.md`（auto-memory）— Fleet 部署上下文
- `project_context_mgmt_v2.md`（auto-memory）— KV cache 优化路线

### 设计灵感

- Notion AI / Perplexity — citation 渲染 UX
- Obsidian — `.md` 文件本地优先的 PKM 模型
- Anthropic Claude Projects / OpenAI Custom GPTs — 用户主动 curate 知识库
