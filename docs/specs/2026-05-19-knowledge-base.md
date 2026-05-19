# Knowledge Base — Design Spec

## Overview

给 rsclaw 加**用户级知识库**：用户主动喂入 PDF / DOCX / Markdown / TXT / URL / 聊天历史 / 图片 / 邮件（.eml），agent 通过工具调用检索，回答里带可点击的出处引用。同时支持周期性数据同步（URL 重抓、本地目录监控、聊天历史增量入库）。

**和现有 `src/agent/memory.rs` 的区别**：memory 是 agent 自学/会衰减/会淘汰；KB 是用户喂入/不衰减/可溯源/有版本。共底层存储但完全独立的 lifecycle 与 DB 文件。

**核心定位：**

- **全局一个库**，所有 agent 共享读写
- **Tool-call retrieval**（agent 主动调），不做 auto-RAG
- **Canonicalize-first**：所有源类型先转 Markdown 字符串，下游统一处理
- **Content store on disk**：canonicalized markdown 整文档存磁盘 `.md` 文件（可被 Obsidian / grep / 编辑器直接打开），DB 只存指针 + offset + metadata + vector
- **三层 OCR 路由**：RapidOCR (Fast) / PaddleOCR-VL 1.5 (Strong) / Qianfan-OCR 4B via rsclaw-llm fleet (Fleet)
- **Embedding**：BGE-M3 本地默认 + 远程 API 备路
- **Citation 必带**：UI 渲染统一风格 + 点击跳源
- **Deterministic chunk_id**：`sha256(source_kind|source_id|seq|content)`，幂等 upsert
- **Entity inverted index**：解决"伊利问题"，支持 entity-based 检索
- **Source syncer 框架**：URL/本地目录/聊天历史可周期或事件触发增量同步

**采用的业界成熟模式：** canonicalize-first pipeline / deterministic chunk_id / on-disk content store / 带 `dedupe_key` + `claim_token` + `reclaim_stale_jobs` 的 jobs queue / entity inverted index / PII redaction / provider-style sync trait + KV-persisted `SyncState`。

**rsclaw 独有创新（差异化设计）：**

1. **Fleet-accelerated batch ingest** —— 千篇级 PDF 入库时，chunks 分发到 rsclaw-llm fleet（IDC GPU 集群）并行 embed + entity 抽取，单机几小时 → 集群几分钟。
2. **`kb_explain` 工具** —— 返回检索推理 trace（BM25 term / dense 维度激活 / entity_index 触发 / MMR 选择理由），agent 能解释"为什么引这条"。
3. **`citation_confidence` 评分** —— 独立于 relevance：`f(quality × recency_decay × is_latest_version × entity_alignment × source_kind_trust)`。Agent 用它决定"引用"还是"仅参考"，避免高 score 但低可信度（过期 PRD）被当权威引。
4. **Memory ↔ KB 双向流** —— 高稳定性 agent memory item 可晋升为 KB doc；session 启动时 KB 喂 context 预热 agent memory。利用 rsclaw "agent memory + 用户 KB" 双系统的独有结构。

## 设计决策

| 决策点 | 选择 | 原因 |
|---|---|---|
| 用户边界 | 全局一个库 | YAGNI；多租户 v2 再说 |
| Retrieval 方式 | Tool-call（agent 主动） | KV cache 友好；与 agent-loop 哲学一致 |
| 文档源 v1 | 本地文件 + URL（单页）+ 聊天历史 + 图片 + 邮件（.eml 手动上传） | 覆盖 80% 场景；代码 repo / 整站爬 / IMAP-Gmail 直连留 v2 |
| 数据同步 | `KbSourceSyncer` trait + KV-persisted `SyncState` | URL 周期、目录监控、聊天增量都走同一套 trait |
| Scheduler | 复用 `src/cron/`，5min tick + event-driven 触发 | 不重复造轮子；channel/fs 事件实时响应 |
| 存储后端 | redb + tantivy + hnsw_rs，**独立 DB 文件 + 独立目录** | 零新依赖；与现有 store 完全平级隔离 |
| **目录布局** | `~/.rsclaw/kb/{md,raw,db,idx,hnsw,state}/` | 全部短化；md=canonicalized markdown / raw=原始字节 / db=redb / idx=tantivy / hnsw=向量 / state=syncer state |
| Content store | canonicalized markdown 作为 `.md` 文件落磁盘，DB 只存路径+offset | Obsidian 兼容 / grep 友好 / DB 不臃肿 / 备份=copy 文件夹 / 可重新 canonicalize |
| Raw cache | 默认开（`kb.keep_raw = true`） | KB 自包含：备份/迁移完整可用；可重新 canonicalize；用户可关 |
| **chunk_id** | **deterministic `sha256(kind\|source_id\|seq\|content)` 截 32 hex** | 同内容重复入库 → 同 ID → upsert no-op，完全幂等（替代 ULID） |
| Canonicalize-first | 所有源先转 Markdown 字符串 + Metadata，再走统一 chunker / embedder | 下游零分支；source-specific 复杂度只在 canonicalize 层 |
| 删除机制 | Tombstone + 查询 filter + 后台 compactor | hnsw_rs 不支持 true delete 的标准解法 |
| Hybrid 检索 | Dense (BGE-M3) + Sparse (BM25) + **RRF 融合** | 不调权重，对分数尺度不敏感 |
| Reranker | v1 不接，留 trait | 质量提升显著但单独算时间最长，留 hook |
| Citation | agent 用 `[^kb:<chunk_id>]` 标记，前端组件渲染 | 不让 agent 拼 URL，避免幻觉 |
| Locator 设计 | enum (PdfPage/MdSection/UrlAnchor/ChatMsgs/Image/Offset) | UI 能跳到具体位置（PDF 翻页、bbox 高亮） |
| Chunking | 默认 512/64 token，**heading_path 强制前缀** | 保护"主语+属性"完整性，防"伊利问题" |
| **Entity inverted index** | `KbEntity` + `KbEntityIndex` 表，入库时一次性建索引 | 替代查询时 jieba 检查；O(1) 查"X 在哪些 chunk 出现"；驱动 `kb_search_entities` 工具 |
| 实体感知 | `entity_alignment` 返回字段 + `require_entities`/`boost_entities` 参数 + RAG 引用纪律 prompt | 防"query 含库里没有的实体"翻车 |
| 多样性 | MMR 默认开 (λ=0.5) | RAG "5 chunk 说同一件事"是最常见失败模式 |
| 入库去重 | 三层防护：API cursor + SyncState `seen_index` + chunk-level deterministic id | 任一层漏掉下层兜底 |
| OCR 引擎 | Fast=RapidOCR / Strong=PaddleOCR-VL 1.5 / Fleet=Qianfan-OCR 4B | RapidOCR 中文显著高于 tesseract；PaddleOCR-VL OmniDocBench SOTA pipeline；Qianfan end-to-end SOTA + KIE |
| OCR 路由 | 按文档特征预扫描自动路由 | 资源等级 ≠ 任务类型；图表必须 Vision LLM |
| Fleet 部署 | rsclaw-server :8444 vLLM/SGLang sidecar | llama.cpp 不支持 InternVL vision encoder；不挂境外云 |
| Embedding 默认 | BGE-M3 本地 (1024) + 远程 API 备路 | desktop-first；几千 chunk 走 API 太贵 |
| 模型迁移 | 双写 + 渐进重建 + 7 天回滚窗口 | 不能让旧 vector 立刻失效 |
| 配额 | search ≤8KB / fetch_full ≤32KB / ≤5次 search 每轮 | 防 context 爆 / search-spam |
| KV cache 友好 | chunk 严格按 (score, chunk_id) 字典序，不带 timestamp/uuid | 同 query 命中同组 chunks → tool result 完全一致 → cache hit |
| Lifecycle 隔离 | KB 不衰减；删除 30 天恢复期 | 跟 MemoryDoc 区分开 |
| Jobs queue | SQLite-backed (in kb.redb) + `dedupe_key` 唯一索引 + `claim_token` 防 stale worker + `reclaim_stale_jobs` 重启续传 | production-grade async pipeline 通用模式（参考 Sidekiq / RQ / Faktory） |
| Compactor | 1h tick + 03:00 强制 + 残骸率 >15% 触发；HNSW 双 buffer μs 级原子切换 | 重建期间老 index 服务查询，新入库写双份 |
| PII redaction | 日志全栈走 `util/redact.rs`，source_id / 内容预览永远是哈希 | 从 day 1 强制，避免后期反向加固 |
| Security 默认 | 本地全栈；远程开关显式确认 | chunk 文本不出本机 |
| 聊天历史隐私 | 默认 `self_messages_only = true`（只入用户消息+@自己） | 不把他人发言入库；UI 可关 |
| **`seen_index` 实现** | `ScalableSeenSet`：Bloom filter (假阳性 <0.1%) + LRU 精确集合 (最近 10000 条) | 百万级 ID 不能全内存 HashSet；假阳性由 chunk-level deterministic id 兜底 |
| 删除检测（folder syncer）| 周期全扫识别 orphan + tombstone（30 天恢复期） | 用户临时移走再放回有 30 天窗口 |
| 退避策略 | 指数：1次→0、2次→1min、3次→5min、6次→1h、12次→6h、>12次 24h 封顶 | 平衡敏感性与噪音 |

## 模块布局

```
src/kb/
  mod.rs              # KbStore facade
  model.rs            # KbDoc / KbChunk / KbEntity / KbEntityIndex / KbSource / KbLocator
  canonicalize/       # 源 → CanonicalizedSource { markdown, metadata }
    mod.rs            # Canonicalizer trait + dispatch
    document.rs       # 本地文件总入口（按 mime 分派）
    chat.rs           # 聊天 batch → markdown
    url.rs            # URL → markdown
    image.rs          # 图片 → OCR → markdown
    mail.rs           # .eml/.mbox → thread 结构化 markdown
    pdf.rs            # PDF 文本层抽取 + 扫描页转 OCR
    docx.rs           # docx-rs，按 paragraph + heading_path
    md.rs             # passthrough + heading_path 抽取
    html.rs           # lol-html 剥脚本 → markdown
    text.rs           # 兜底
  ocr/
    mod.rs            # OcrEngine trait + 三层路由
    rapidocr.rs       # Tier Fast：PP-OCRv4 ONNX via `ort`
    paddleocr_vl.rs   # Tier Strong：PaddleOCR-VL 1.5 via `ort`
    qianfan.rs        # Tier Fleet：HTTP 到 rsclaw-server :8444
    prescan.rs        # 预扫描特征检测
  chunker.rs          # 512/64 + heading_path 前缀 + deterministic chunk_id + SimHash
  embedder.rs         # KbEmbedder trait + LocalBgeM3 主 / RemoteApi 备
  content_store/      # 磁盘 markdown + raw 文件管理
    mod.rs            # 公共 API: stage_doc / read_doc_range / verify_sha
    atomic.rs         # 原子写：tempfile + fsync + rename + parent fsync
    compose.rs        # YAML front-matter + body 组装，tags 单独可重写
    paths.rs          # path 生成器：md/doc/<slug>.md / md/chat/<source_slug>_<date>.md / raw/<doc_id>.<ext>
    raw.rs            # raw/ 目录管理（按 doc_id 存原始字节）
    read.rs           # read_doc_body / read_doc_range，SHA 校验
    tags.rs           # 重写 YAML front-matter tags 块（保持 body 不可变）
  entity/
    mod.rs            # 实体抽取 + 倒排索引
    extractor.rs      # EntityExtractor trait
    regex.rs          # RegexEntityExtractor（email/URL/handle/hashtag）
    jieba.rs          # 中文实体抽取（jieba 分词 + 命名实体过滤）
    llm.rs            # v2：LlmEntityExtractor（NER + 重要性）
    resolver.rs       # 实体规范化（大小写、变体合并）
    index.rs          # 倒排索引读写
  retrieval/
    mod.rs            # kb_search / kb_fetch / kb_list_docs / kb_similar / kb_search_entities / kb_explain
    hybrid.rs         # Dense + BM25 + RRF
    mmr.rs            # MMR 多样性
    alignment.rs      # entity_alignment 返回字段（查倒排索引）
    confidence.rs     # citation_confidence 评分（独有：独立于 relevance）
    explain.rs        # 检索 trace 收集（独有：kb_explain 工具用）
    memory_bridge.rs  # KB ↔ agent memory 双向流（独有）
  syncer/             # 数据源同步框架
    mod.rs            # KbSourceSyncer trait + SyncState + SyncContext / Reason / Outcome / Error
    registry.rs       # source 注册表 + 启动 wiring
    state.rs          # SyncState 持久化（kb.redb）
    scheduler.rs      # 复用 src/cron/ 的 5min tick + 退避
    bloom_lru.rs      # synced_ids 的 Bloom+LRU 实现
    impls/
      manual.rs       # ManualUploadSyncer（统一入口，不周期跑）
      url.rs          # UrlSyncer（ETag/Last-Modified + content-hash 兜底）
      folder.rs       # LocalFolderSyncer（notify 实时 + 周期全扫）
      chat_history.rs # ChannelHistorySyncer（backfill + event-driven 增量）
  jobs/               # async job queue
    mod.rs            # 公共 API
    types.rs          # JobKind / JobStatus / Job / NewJob + 每 kind 的 Payload
    store.rs          # SQLite（kb.redb）持久化：dedupe_key + claim_token + reclaim_stale_jobs
    worker.rs         # worker pool（3 task），LLM-bound 用 semaphore 限流
    handlers/         # 每 JobKind 一个 handler：canonicalize / chunk / embed / index / extract_entities
    fleet_dispatch.rs # 大批量任务分发到 rsclaw-llm fleet（独有：batch embed / entity 走集群）
  compactor.rs        # 后台 tokio task：tombstone 清理 + HNSW 双 buffer 重建
  migrator.rs         # embedding 模型迁移流程
  util/
    redact.rs         # 日志 PII redaction（source_id / 内容预览哈希化）

src/cmd/
  kb_add.rs           # 兼容 manual upload
  kb_ls.rs            # 列文档
  kb_rm.rs            # 删文档（tombstone）
  kb_search.rs        # 命令行检索
  kb_show.rs          # 查 doc/chunk
  kb_reindex.rs       # embedding 模型迁移
  kb_compact.rs       # 手动 compactor
  kb_stats.rs         # 统计
  kb_export.rs        # 导出
  kb_sync_add.rs      # 注册 syncer
  kb_sync_ls.rs       # 列 syncer + 状态
  kb_sync_show.rs     # 单 syncer 详情
  kb_sync_pause.rs    # 暂停
  kb_sync_resume.rs   # 恢复
  kb_sync_run.rs      # run-now
  kb_sync_rm.rs       # 移除 syncer
  kb_sync_logs.rs     # 最近日志

src/agent/tools_kb.rs # tool 注册 + JSON schema

ui/app/components/kb/
  panel.tsx           # 主面板
  upload.tsx          # 上传弹窗
  doc-list.tsx        # 文档列表
  search-preview.tsx  # 检索预览
  sync-tab.tsx        # 数据源 tab
  settings.tsx        # 设置
  citation.tsx        # <KbCitation> 渲染组件
  citation-cache.ts   # frontend chunk meta 缓存
```

## 存储布局

```
~/.rsclaw/kb/
  md/                       # canonicalize 后整文档 (Obsidian/grep 友好)
    doc/蒙牛奶粉冲泡指南.md
    chat/feishu_pm_group_2026-05.md
    url/example-com-changelog.md
    img/合同扫描件-001.md
    mail/alice_at_x__bob_at_y__2026-05.md
  raw/                      # 原始字节（kb.keep_raw=true 时存）
    01HXY...abc.pdf
    01HXX...def.docx
    01HXZ...ghi.png
  db/
    kb.redb                 # KbDoc / KbChunk / KbEntity / KbEntityIndex / SyncState / Jobs
  idx/                      # tantivy FTS 索引目录
  hnsw/
    kb_v1024_bgem3.hnsw     # 当前活跃向量索引（按 embedder_id 命名）
    kb_v1024_bgem3.next     # 迁移/重建中的下一份（双 buffer）
  state/
    triggers/YYYY-MM-DD.jsonl  # webhook/event 归档
    logs/                    # syncer 最近运行日志
```

**关键约束：**

- `~/.rsclaw/kb/` 整个目录**自包含**：备份/迁移 = `cp -r kb/ backup/`，DB 路径指向都是相对 `kb/` 的，移走后仍可用
- `md/` 永远是 source of truth：DB 损坏时可从 `md/` 重建 metadata（不含 vector）；`md/` 损坏时 DB 检测 SHA 失败拒绝服务
- `raw/` 可关闭以节省空间；关闭后失去"重新 canonicalize"能力，可"打开原文"需依赖原 `KbSource::Doc.path`

## §1 数据模型

```rust
// src/kb/model.rs

pub struct KbDoc {
    pub id: String,                  // ulid
    pub source: KbSource,
    pub source_kind: KbSourceKind,
    pub source_id: String,           // 同 syncer source_id 语义；ManualUpload 是 "manual:<doc_id>"
    pub title: String,
    pub mime: String,
    pub hash: String,                // sha256(原始 bytes)，doc-level dedup
    pub markdown_path: String,       // 相对 ~/.rsclaw/kb/，如 "md/doc/蒙牛奶粉冲泡指南.md"
    pub markdown_sha256: String,     // body bytes only (不含 front-matter)
    pub raw_path: Option<String>,    // 相对 ~/.rsclaw/kb/，如 "raw/01HXY...abc.pdf"；keep_raw=false 时 None
    pub created_at: i64,
    pub updated_at: i64,
    pub version: u32,
    pub status: KbStatus,
    pub tags: Vec<String>,
    pub meta: serde_json::Value,
}

pub enum KbSourceKind {
    Doc,    // on-wire "doc"  — 本地文件
    Chat,   // on-wire "chat" — 聊天历史
    Url,    // on-wire "url"  — 网页
    Img,    // on-wire "img"  — 图片
    Mail,   // on-wire "mail" — 邮件（v1 .eml 手动上传；v2 IMAP/Gmail）
}

pub enum KbSource {
    Doc  { path: PathBuf },
    Url  { url: String, fetched_at: i64 },
    Chat { channel: String, range: (i64, i64) },
    Img  { path: PathBuf },
    Mail { source: MailSource },
}

pub enum MailSource {
    EmlFile  { path: PathBuf },                                    // v1
    MboxFile { path: PathBuf },                                    // v1
    Imap     { account: String, folder: String, uid: u64 },         // v2
    Gmail    { account: String, thread_id: String, msg_id: String },// v2
}

pub enum KbStatus { Active, Tombstoned, Updating }

pub struct KbChunk {
    /// Deterministic: sha256(source_kind|source_id|seq|content) 截 32 hex chars
    /// 同内容重复入库 → 同 ID → upsert no-op
    pub id: String,
    pub doc_id: String,
    pub doc_version: u32,            // 必须等于 KbDoc.version 才参与召回
    pub seq: u32,
    pub heading_path: Vec<String>,   // ["蒙牛奶粉冲泡指南", "建议比例"]
    pub byte_offset: (u64, u64),     // 在 markdown_path 文件内的 body 字节范围
    pub indexed_text: String,        // heading_path.join(" > ") + "\n\n" + body_text，用于 embed/BM25
    pub vector: Vec<f32>,            // 1024（BGE-M3）
    pub simhash: u64,                // chunk-level near-dup（hamming ≤ 3 视重复）
    pub locator: KbLocator,
    pub status: KbStatus,
    pub source_quality: f32,         // OCR confidence 或 1.0
    pub embedder_id: String,         // "bge-m3@v1"，用于模型迁移
    // 注意：没有 text 字段！按需 read_doc_range(markdown_path, byte_offset) 拿原文
}

pub enum KbLocator {
    PdfPage   { page: u32, bbox: Option<(f32,f32,f32,f32)> },
    MdSection { heading_path: Vec<String> },
    UrlAnchor { fragment: Option<String> },
    ChatMsgs  { first_ts: i64, last_ts: i64 },
    Image     { bbox: Option<(f32,f32,f32,f32)> },
    Offset    { start: usize, end: usize },
}

/// 实体倒排索引（解决"伊利问题"）
pub struct KbEntity {
    pub canonical_id: String,        // "ent_yili" / "ent_email_alice_at_x"
    pub surface_forms: Vec<String>,  // ["伊利", "Yili", "伊利股份"]
    pub kind: EntityKind,            // Brand / Person / Email / Url / Hashtag / Custom
    pub created_at: i64,
}

pub enum EntityKind { Brand, Person, Org, Email, Url, Hashtag, Other }

pub struct KbEntityIndex {
    pub entity_id: String,           // 指向 KbEntity.canonical_id
    pub chunk_id: String,
    pub doc_id: String,
    pub mention_count: u32,
    pub score: f32,                  // tf-idf 风格权重
}
```

### 存储映射

| 表 | 后端 | 内容 |
|---|---|---|
| `kb_docs` | redb | id → KbDoc |
| `kb_chunks` | redb | id → KbChunk（不含 text body） |
| `kb_entities` | redb | canonical_id → KbEntity |
| `kb_entity_index` | redb | (entity_id, chunk_id) → KbEntityIndex |
| `kb_sync_state` | redb | source_id → SyncState |
| `kb_jobs` | redb | job_id → Job（含 dedupe_key partial unique index） |
| tantivy idx | tantivy | full-text on `indexed_text`，doc_id/tags/status/source_kind facet |
| hnsw | hnsw_rs | `kb_v1024_<embedder_id>` 实例 |

### Chunk ID 决定论

```rust
pub fn chunk_id(kind: KbSourceKind, source_id: &str, seq: u32, content: &str) -> String {
    let mut h = Sha256::new();
    h.update(kind.as_str().as_bytes());
    h.update([0u8]);
    h.update(source_id.as_bytes());
    h.update([0u8]);
    h.update(&seq.to_be_bytes());
    h.update([0u8]);
    h.update(content.as_bytes());
    hex::encode(&h.finalize())[..32].to_string()
}
```

content 进 hash 是为了：同 `(source_id, seq)` 但内容不同（聊天历史一段 bucket 后又有更新）也产生不同 ID。再 ingest 完全相同内容才命中同 ID → 真幂等。

## §2 Canonicalize-first Ingestion

```
syncer 拉到 raw bytes / 用户上传文件
        │
        ▼
canonicalize/<kind>.rs ── CanonicalizedSource { markdown, metadata }
        │
        ▼
content_store::stage_doc(markdown) ── 原子写 md/<kind>/<slug>.md
        │
        ▼
KbDoc 写 db（含 markdown_path + sha256 + raw_path?）
        │
        ▼
enqueue Job: ChunkAndEmbed { doc_id, doc_version }
        │
        ▼ (后台 worker)
chunker.chunk_markdown ── deterministic chunk_id + heading_path 前缀 + SimHash
        │
        ├──▶ entity extractor ── 写 KbEntityIndex
        ├──▶ embedder ──────── 写 KbChunk.vector
        └──▶ tantivy add ───── 写 FTS index
```

### canonicalize 阶段

```rust
pub struct CanonicalizedSource {
    pub markdown: String,             // 整个文档的 canonical markdown（YAML front-matter + body）
    pub metadata: CanonicalMetadata,
}

pub struct CanonicalMetadata {
    pub source_kind: KbSourceKind,
    pub source_id: String,
    pub owner: String,
    pub timestamp: DateTime<Utc>,
    pub time_range: (DateTime<Utc>, DateTime<Utc>),
    pub tags: Vec<String>,
    pub source_ref: Option<SourceRef>,
}

#[async_trait]
pub trait Canonicalizer: Send + Sync {
    fn source_kind(&self) -> KbSourceKind;
    async fn canonicalize(
        &self,
        raw: CanonicalizeInput,
        ctx: &CanonicalizeContext,
    ) -> Result<Option<CanonicalizedSource>>;  // None = 空输入 noop
}
```

每个 source kind 一个 Canonicalizer impl：

- `DocCanonicalizer`：按 mime 内部分派（PDF → `pdf.rs`，DOCX → `docx.rs`，MD → `md.rs`，HTML → `html.rs`，TXT → `text.rs`）
  - 例外：mime `message/rfc822` / `application/mbox` → 委托给 `MailCanonicalizer`（即使是用户拖 `.eml` 上传，最终也归 `KbSourceKind::Mail`，写 `md/mail/`）
- `ChatCanonicalizer`：消息按时间排序 → `## <ts> — <author>\n<body>` block
- `UrlCanonicalizer`：lol-html 剥脚本 → markdown 转换
- `ImgCanonicalizer`：OCR → text → 简单 markdown wrap
- `MailCanonicalizer`：thread 解析 → 按 `---\nFrom: ...\nSubject: ...\nDate: ...\n\n<cleaned-body>` 切块（剥回复链 / footer / legal boilerplate）；source_id 用 `mail:{participants}` 形式（`from ∪ to` 去重排序，CC 不入 bucket key，避免会话碎片化）

**OCR 三层路由（PDF/Img）：**

| 检测到 | 路由 |
|---|---|
| 文本层 PDF | Skip OCR |
| 收据/发票/证书/病历/身份证 (KIE) | Fleet (Qianfan) |
| 图表密集 | Fleet (Qianfan) |
| 手写体 | Fleet (Qianfan) |
| 公式密集 (∫∑∂√) | Strong (PaddleOCR-VL) |
| 复杂表格（合并/旋转） | Strong (PaddleOCR-VL) |
| 多栏排版 | Strong (PaddleOCR-VL) |
| 纯文本扫描 / 简单单栏 | Fast (RapidOCR) |
| 场景文本 | Fast → 质量不够升 Strong |
| 多语言（非中英） | Strong / Fleet |

预扫描跑 RapidOCR 抽 1-2 页特征（边/直线密度、文本块分布、特殊字符），10-30ms 一页。

**Fleet 部署**：rsclaw-server 加 `/v1/ocr/parse` endpoint 转发到 4090 节点的 vLLM sidecar：

```bash
# scripts/deploy-ocr-sidecar.sh
vllm serve baidu/Qianfan-OCR --trust-remote-code --port 8444
```

### content_store

```rust
// src/kb/content_store/mod.rs

pub struct StagedDoc {
    pub markdown_path: String,        // 相对 kb_root
    pub markdown_sha256: String,
    pub raw_path: Option<String>,
}

pub async fn stage_doc(
    canonical: &CanonicalizedSource,
    raw_bytes: Option<&[u8]>,
    raw_ext: Option<&str>,
) -> Result<StagedDoc>;

pub async fn read_doc_body(markdown_path: &str) -> Result<String>;
pub async fn read_doc_range(markdown_path: &str, range: (u64, u64)) -> Result<String>;
pub async fn verify_doc_sha(markdown_path: &str, expected: &str) -> Result<()>;
pub async fn rewrite_tags(markdown_path: &str, new_tags: &[String]) -> Result<()>;  // body 不变
pub async fn delete_doc_files(markdown_path: &str, raw_path: Option<&str>) -> Result<()>;
```

**原子写实现：** tempfile + fsync(file) + rename + fsync(parent dir on Unix)。POSIX 标准的崩溃安全写文件模式。

**Body 不可变 + tags 可变：** `markdown_sha256` 只 hash body 部分。重写 tags 不影响 SHA。

**路径示例：**
```
md/doc/<slug>.md            # slug 是 title 经 slugify
md/chat/<channel_slug>_<YYYY-MM>.md
md/url/<host>_<path_hash>.md
md/img/<title_slug>.md
md/mail/<participants_slug>_<YYYY-MM>.md  # 同一参与者组同月的邮件合并到一个 md
raw/<doc_id>.<ext>          # 按 ulid 存，避免 slug 冲突
```

### chunker

- 目标 chunk size：**~512 token**，overlap **~64 token**（BGE-M3 tokenizer 计）
- **优先尊重 semantic_unit 边界**：原生段落/标题/对话整块保留
- 超过 target 按 sentence 边界切
- 太小（<50 token）相邻同 section chunk 合并
- **强制 `heading_path` 前缀**到 `indexed_text`：`heading_path.join(" > ") + "\n\n" + body`
- 每 chunk 算 SimHash，入库前查 hamming ≤ 3，命中即去重（不写 chunk，记录引用关系到 `meta.dedup_of`）
- chunk body 不存 DB，只存 `byte_offset` 指向 `markdown_path` 内位置

### embedder

```rust
pub trait KbEmbedder: Send + Sync {
    fn embed_batch(&self, texts: &[String]) -> Result<Vec<Vec<f32>>>;
    fn dimension(&self) -> usize;
    fn embedder_id(&self) -> &str;
}

// 默认 LocalBgeM3 (1024)；备路 RemoteApiEmbedder（走 ProviderRegistry）；
// 大批量走 FleetEmbedder（独有）
```

batch：local 16、remote 64；fleet **每节点 128，并发 fan-out**。`embedder_id` 落 `KbChunk.embedder_id`，模型变更触发迁移流程（§6）。

### Fleet-accelerated batch ingest（独有）

当 jobs queue 检测到**单次入库 ≥100 chunks** 或 **整库 reindex** 任务时，`jobs/fleet_dispatch.rs` 自动分发到 rsclaw-llm fleet：

```
local 触发 (用户拖 1000 PDF)
        │
        ▼
canonicalize + chunk (单机，快)
        │
        ▼ N chunks
fleet_dispatch.rs 切片：N / fleet_size 每节点
        │
        ▼
rsclaw-server `/v1/embed/batch`     ──┐
rsclaw-server `/v1/entity/batch`    ──┤  (并行)
                                       │
                                       ▼
                              rsclaw-llm fleet (IDC 1000 节点)
                                       │
                                       ▼ vectors + entities
                              本地 writer 合并 → redb + tantivy + hnsw
```

**规模收益示例：** 1000 PDF (~30 万 chunks)
- 纯本地 (单 GPU 4090)：~4-6 小时
- Fleet (200 节点并发，仅占用 20%)：**~3-5 分钟**

**Fleet 不可用时自动 fallback 本地**，UI 显示路径。配置：`kb.embedding.use_fleet_threshold = 100`（chunks 数阈值）。

**这是 rsclaw 独有能力**：不是任何同类系统能简单复制的，依赖完整的 GPU 集群 + rsclaw-server 协议栈。

### entity extraction

每 chunk 在 embed 后跑：

```rust
#[async_trait]
pub trait EntityExtractor: Send + Sync {
    async fn extract(&self, text: &str) -> Result<Vec<ExtractedEntity>>;
}

pub struct ExtractedEntity {
    pub canonical_id: String,
    pub surface: String,
    pub kind: EntityKind,
}

// v1: CompositeExtractor = RegexEntityExtractor + JiebaEntityExtractor
// v2: + LlmEntityExtractor（NER + 重要性评分）
```

**RegexEntityExtractor：** email / URL / `@handle` / `#hashtag` 等机械标识符。
**JiebaEntityExtractor：** 中文分词 + 大写词 / 专有名词 boost，提取候选实体。
**resolver：** 大小写规范化、去 @/#、合并变体（"伊利" / "Yili" / "伊利股份" → 同 canonical_id）。

写入 `kb_entities` (upsert) + `kb_entity_index` (chunk-level rows)。

### Writer（事务）

```rust
async fn upsert_doc(doc: KbDoc, raw_bytes: Option<Vec<u8>>) -> Result<KbDocId> {
    // 1. content_store stage
    let staged = stage_doc(&canonical, raw_bytes.as_deref(), ...).await?;
    
    // 2. redb 事务
    let mut wtx = redb.begin_write()?;
    if let Some(old) = wtx.get_doc(&doc.id)? {
        wtx.tombstone_chunks_for(&doc.id)?;
        delete_doc_files(&old.markdown_path, old.raw_path.as_deref()).await?;
    }
    wtx.put_doc(&doc_with_paths)?;
    wtx.commit()?;
    
    // 3. enqueue ChunkAndEmbed job（异步）
    jobs::enqueue(JobKind::ChunkAndEmbed { doc_id, doc_version }, dedupe_key).await?;
    
    Ok(doc_id)
}
```

后台 worker 异步处理 chunk + embed + entity + tantivy + hnsw。失败任一步，job 重试；hnsw 写失败不回滚 redb，启动时校验 hnsw vs redb 缺的补。

### Jobs queue

```rust
pub enum JobKind {
    ChunkAndEmbed { doc_id: String, doc_version: u32 },
    ExtractEntities { chunk_id: String },
    RebuildHnsw,
    PurgeTombstones,
    SyncerRun { source_id: String, reason: SyncReason },
}

pub enum JobStatus { Ready, Running, Done, Failed }

pub struct Job {
    pub id: String,
    pub kind: JobKind,
    pub status: JobStatus,
    pub claim_token: Option<String>,
    pub claimed_at: Option<i64>,
    pub dedupe_key: String,
    pub priority: u8,
    pub retries: u32,
    pub error: Option<String>,
    pub created_at: i64,
}
```

**关键模式：**

1. **`dedupe_key` partial unique index** (`WHERE status IN ('Ready','Running')`)：飞行中重复任务静默抑制（同一 doc 重复 enqueue chunk-embed → 第二次 no-op）
2. **`enqueue_tx`**：side-effect + follow-up job 同一 redb 事务原子提交
3. **`claim_next` = 单条 UPDATE...RETURNING**：worker 抢任务无竞争
4. **`mark_done` / `mark_failed` 走 `claim_token` 校验**：stale worker 已被替换则写回 no-op
5. **`recover_stale_locks`**：worker 重启时自动接管 >5min 没心跳的任务
6. **`scheduler_gate::wait_for_capacity()`**：throttle 模式下不占 lease

worker pool：3 个 task，LLM-bound 走 3-permit semaphore（embedder / entity / OCR）。

## §3 Retrieval

### Tool Surface

```jsonc
// kb_search
{
  "query": "string",                              // 必填
  "k": 8,                                         // 默认 8，最多 20
  "filter": {
    "tags": ["string"],
    "source_kind": "doc|chat|url|img|mail",
    "doc_ids": ["string"],
    "entity_ids": ["string"],                     // 限定含某实体的 chunks
    "min_quality": 0.6
  },
  "mode": "auto|dense|bm25|hybrid",               // 默认 hybrid
  "diversity": "off|mmr",                         // 默认 mmr
  "mmr_lambda": 0.5,
  "require_entities": ["string"],                 // 硬约束：必须含此 entity
  "boost_entities":  ["string"]                   // 软约束：含此 entity score ×1.5
}

// 返回
{
  "results": [
    {
      "chunk_id": "01HXY...",
      "doc_id": "01HXX...",
      "doc_title": "蒙牛奶粉冲泡指南.pdf",
      "text": "蒙牛奶粉建议100g兑100ml温水",     // 按需 read_doc_range 读
      "heading_path": ["蒙牛奶粉冲泡指南", "建议比例"],
      "score": 0.83,                              // 检索相关性
      "citation_confidence": 0.91,                // 独有：是否值得作为引用源
      "citation_tier": "authoritative",           // 独有：authoritative | supporting | indicative
      "citation": {
        "source": "file:///Users/x/docs/...",
        "locator_human": "p.12 §建议比例",
        "locator_machine": { /* KbLocator enum */ }
      },
      "quality": 0.95,
      "entities": ["ent_mengniu", "ent_milkpowder"]
    }
  ],
  "entity_alignment": [
    { "entity_surface": "伊利", "canonical_id": "ent_yili", "matched_chunks": 0, "total": 5 }
  ],
  "warnings": [
    "query 含关键词 [伊利]，召回 chunks 中 0/5 包含此词，可能存在实体不匹配"
  ],
  "trace_id": "trc_01HXY..."                      // 独有：用 kb_explain(trace_id) 拿推理细节
}

// kb_explain  (独有：检索推理 trace)
{ "trace_id": "trc_01HXY..." }
// 返回：每 chunk 的 bm25 命中 terms / dense 激活维度 top-N / entity_index 触发关系 /
//      MMR 入选/拒绝理由 / citation_confidence 各因子分解

// kb_fetch
{ "chunk_id": "...", "expand": "none|neighbor|full_doc" }

// kb_list_docs
{ "tags": [...], "source_kind": "...", "limit": 50, "cursor": "..." }

// kb_similar (chunk → chunk)
{
  "chunk_id": "01HXY...",
  "k": 8,
  "scope": "any|same_doc|other_docs",
  "min_score": 0.7,
  "exclude_neighbors": true
}

// kb_search_entities  (新：实体倒排索引查询)
{
  "query": "伊利",                                // 模糊匹配 surface_forms
  "kind": "Brand|Person|Org|...|any",
  "limit": 20
}
// 返回：[{ canonical_id, surface_forms, kind, mention_count, doc_count }]
```

### Pipeline

```
query
  │
  ├──▶ dense: BGE-M3 embed → hnsw.search(k*3)        ┐
  │                                                    │
  ├──▶ sparse: tantivy BM25(k*3)                       │ 全程
  │                                                    │ 收集 trace
  └──▶ [filter: tags / source_kind / doc_ids /         │ (独有)
                entity_ids / status≠Tombstoned /       │
                quality / require_entities]            │
              │                                        │
              ▼                                        │
       RRF fusion (k=60)                               │
              │                                        │
              ▼                                        │
       boost_entities apply (×1.5 if hit)              │
              │                                        │
              ▼                                        │
       MMR diversity (λ=0.5)                           │
              │                                        │
              ▼                                        │
       [optional rerank] —— v1 noop trait              │
              │                                        │
              ▼                                        │
       entity_alignment 计算（查倒排索引）              │
              │                                        │
              ▼                                        │
       citation_confidence 评分（独有）                  │
              │                                        │
              ▼                                        │
       lazy read body via content_store.read_doc_range │
              │                                        │
              ▼                                        ▼
       top-k 截断 → 返回 + trace_id 入 explain_cache
```

### Citation Confidence（独有）

`citation_confidence ∈ [0.0, 1.0]`，公式：

```
confidence = quality
           × recency_decay(doc.updated_at)        // exp(-Δdays / 90)
           × is_latest_version_flag               // 0.5 if not latest, 1.0 if latest
           × max(0.3, entity_alignment_match)     // entity 全不匹配下限 0.3
           × source_kind_trust                    // 默认表，用户可调
```

**`source_kind_trust` 默认表：**

| source_kind | 默认 trust |
|---|---|
| Doc | 1.0 |
| Mail | 1.0 |
| Url | 0.85 |
| Chat | 0.65 |
| Img (OCR) | 0.7 |

**`citation_tier` 分档**（给 agent 用）：

| confidence 区间 | tier | 含义 |
|---|---|---|
| ≥ 0.8 | `authoritative` | 直接引用 |
| 0.5–0.8 | `supporting` | 可引但建议措辞缓和（"根据..."） |
| < 0.5 | `indicative` | 仅作参考，不应作为权威来源 |

system prompt 教 agent：低 tier 内容必须明确措辞标识，不能装权威。

### Filter 时机

- 硬过滤（status / tags / source_kind / doc_ids / entity_ids / require_entities）：召回时直接 skip
- 软过滤（quality / boost_entities）：召回时降权 (×0.7) 或升权 (×1.5)，不 skip

### Entity Alignment

**入库时**做一次 entity extraction，把 `chunk_id → entity_ids` 关系写 `kb_entity_index`。

**查询时**：
1. 提取 query 关键词（jieba 分词）
2. 对每个关键词查 `kb_entities.surface_forms` 找 canonical_id
3. 对每个 canonical_id 查 `kb_entity_index` 找出现的 chunks
4. 比对 top-k 召回结果，算 `matched_chunks / total`
5. `matched_chunks == 0` 时生成 warning

vs spec v1 的"查询时 jieba 检查 per chunk"——这版**入库一次 O(N)，查询 O(1)**，规模大显著领先。

### Citation 格式

- `locator_human`：Rust 端格式化（"p.12 §建议比例"），给 agent 用
- `locator_machine`：enum 序列化，只回 UI 用
- agent 不能自己拼 locator，避免幻觉

### 配额限流

- `kb_search` 单次返回 chunks 总字数 ≤ 8KB
- `kb_fetch expand=full_doc` ≤ 32KB
- agent 单 turn 内 `kb_search` ≤ 5 次

### KV cache 友好

chunk 排序严格 (score 降序, chunk_id 字典序)，不带 timestamp / uuid / request_id。

### RAG 引用纪律 Prompt

加进 `src/agent/prompt_builder.rs`：

```
使用 kb_search 时：
- 返回的 chunk 是语义相关而非精确匹配
- 引用前必须验证 chunk 中的实体/品牌/数值与用户问题一致
- 若 entity_alignment 显示某关键词 matched_chunks=0，必须明确告知用户「知识库未找到 X 的相关数据」，不得套用其他实体的数据
- 引用时必须用 [^kb:<chunk_id>] 标记，由 UI 渲染为可点击引用
- 关注每个 chunk 的 citation_tier：
  · authoritative —— 可直接引用
  · supporting —— 引用需措辞缓和（"根据 X，可能..."）
  · indicative —— 仅作参考，不应作为权威来源
- 拿不准时调 kb_explain(trace_id) 查清楚为什么这条命中，再决定是否引用
```

## §4 Citation 渲染（UI）

### Agent 输出格式

```
根据《蒙牛奶粉冲泡指南》[^kb:01HXY...]，建议比例是 100g 兑 100ml 温水。
```

### 渲染管线

```
agent stream → message store
        │
        ▼
markdown renderer (NextChat-derived)
        │
        ▼
kb-citation plugin：
- 扫描 [^kb:<id>] 标记
- 查 frontend kb-cache（无则调 kb_fetch）
- 替换为 <KbCitation> 组件
        │
        ▼
UI 显示：[1] 上标 + 悬浮卡片 + 点击跳源
```

### `<KbCitation>` 组件

- 内联上标 `[N]`（按出现顺序编号）
- 悬浮卡：doc title + locator + 50 字 snippet（从 `read_doc_range` 拿）
- 点击行为按 `KbLocator` 类型分派：
  - `PdfPage` → 右侧打开 PDF.js viewer 跳 page + bbox 高亮
  - `MdSection` → 右侧打开 markdown 渲染（直接读 `md/...` 文件），滚动到 heading
  - `UrlAnchor` → 系统浏览器打开 URL + fragment
  - `ChatMsgs` → 跳到对应 channel + 时间范围
  - `Image` → 右侧打开图片 viewer + bbox 高亮

### 消息底部「参考资料」区

```
─────────────────────────
📚 参考资料 (2)
  [1] 蒙牛奶粉冲泡指南.pdf · p.12 §建议比例    (打开)
  [2] 客服 FAQ.md · §奶粉冲泡常见问题            (打开)
─────────────────────────
```

去重 by chunk_id；同文档多次引用合并显示 `× N`。

### 前端缓存

```ts
// ui/app/store/kb-cache.ts
export const kbCache = new Map<string, KbChunkMeta>();

// rsclaw-ws.ts 收到 kb_search/kb_fetch tool_result 时：
result.results.forEach(c => kbCache.set(c.chunk_id, {
  doc_title, heading_path, locator_human, locator_machine, source
}));
```

## §5 管理 UI + CLI

### Tauri 控制台「知识库」面板

主导航 `📚 知识库`，三个 tab：

- **文档** tab：左侧文档列表（按 source_kind / tag 过滤）+ 右侧搜索预览 / 单文档详情
- **数据源** tab：syncer 列表 + 状态 + 配额 + 暂停/恢复/立即同步
- **设置** tab：默认 OCR engine / embedding backend / chunk size / tombstone 天数 / compactor 频率 / entity_alignment 开关 / `keep_raw` 开关

「+ 添加」三 tab：
- **文件**：拖拽多选 → 走 ManualUploadSyncer 一次性入库
- **URL**：粘贴 URL + 「定期重抓」开关 → 注册 UrlSyncer
- **聊天**：channel + 时间范围 → ChannelHistorySyncer backfill 模式
- **目录**：选目录 + glob 过滤 + 「实时监控」开关 → LocalFolderSyncer

### 聊天窗口轻交互

- 拖拽文件入聊天 → 弹「加入知识库 / 仅本轮使用」
- agent 回答的 citation 悬浮卡有「打开知识库」按钮跳面板

### CLI

```bash
# 文档管理（一次性）
rsclaw kb add <path|url> [--tags=...] [--ocr=auto|fast|strong|fleet]
rsclaw kb ls [--tag=...] [--source-kind=doc|chat|url|img|mail] [--limit=N]
rsclaw kb rm <doc_id|--tag=...> [--yes]
rsclaw kb search <query> [-k 8] [--filter='{...}']
rsclaw kb show <doc_id|chunk_id>
rsclaw kb reindex [--doc=<id>] [--all]
rsclaw kb compact
rsclaw kb stats
rsclaw kb export <doc_id> --to <path>

# 数据源同步
rsclaw kb sync add url <URL> [--interval=1h] [--browser]
rsclaw kb sync add folder <PATH> [--include='**/*.{pdf,md}'] [--exclude='**/.*'] [--realtime]
rsclaw kb sync add chat <CHANNEL_ID> [--backfill-until=2025-01-01] [--all-messages]
rsclaw kb sync ls
rsclaw kb sync show <SOURCE_ID>
rsclaw kb sync pause <SOURCE_ID>
rsclaw kb sync resume <SOURCE_ID>
rsclaw kb sync run-now <SOURCE_ID>
rsclaw kb sync rm <SOURCE_ID> [--purge-docs]
rsclaw kb sync logs <SOURCE_ID> [--tail=50]
```

CLI 子命令 `src/cmd/kb_*.rs`，跟现有 `gateway`/`provider` 子命令风格一致。

### i18n

- UI 文案进 `ui/app/locales/`（10 语言）
- CLI 帮助走现有 `src/i18n.rs`

## §6 Lifecycle / Compactor / Migrator / Config / Security

### Lifecycle 状态机

```
[Pending] → [Fetching → Canonicalize → Stage Md] → [Active]
                                                       │
                                              ┌────────┴────────┐
                                              ▼                 ▼
                                       [Tombstoned]      [Updating]
                                       (软删 30 天)       (新版本流程)
                                              │
                                              ▼
                                       (Compactor 物理删除)
```

任何 add/update/delete 只是状态机推进，HNSW 永不单点删。`KbChunk.doc_version == KbDoc.version` 才参与召回。

### Compactor

```rust
pub struct Compactor {
    interval: Duration,                        // 默认 1h
    schedule: Vec<NaiveTime>,                  // 默认 [03:00]
    tombstone_ratio_threshold: f32,            // 0.15
    min_age_for_physical_delete: Duration,     // 30 天
    max_hnsw_rebuild_per_run: usize,
}

async fn tick(&self) -> Result<()> {
    self.purge_expired_tombstones().await?;           // redb + tantivy + md/raw 物理删
    let ratio = self.tombstone_ratio_in_hnsw().await?;
    if ratio > self.tombstone_ratio_threshold {
        self.rebuild_hnsw().await?;                    // 双 buffer 重建
    }
    self.tantivy_compact().await?;
    self.compact_entity_index().await?;
    Ok(())
}
```

**HNSW 双 buffer 重建：** 在 `hnsw/kb_v1024_<id>.next` 建新 index → 原子改名替换 → 重建期间老 index 服务查询；新入库 chunk 同时写新老两份。

手动触发：`rsclaw kb compact` / UI「立即整理」。

### Embedding 模型迁移

```
[Active: kb_v1024_bgem3]
        │
        ▼
用户切到新模型
        │
        ▼
[迁移中: 老 index 服务查询；新 index kb_v1024_<新>_next 重 embed；新入库双写]
        │
        ▼
[切换: 新 index ready → 原子改名 → 查询切到新；老 index 标 Deprecated 保留 7 天]
        │
        ▼
[7 天后物理删除]
```

策略两选一：手动启动 / 自动渐进（每天 embed N 个 chunk）。

### Config (`defaults.toml`)

```toml
[kb]
enabled = true
root_dir = "~/.rsclaw/kb"            # 自包含目录
default_tags = []
keep_raw = true                      # raw/ 目录是否保留原始字节

[kb.embedding]
backend = "local-bgem3"              # local-bgem3 | remote-api
model_path = "~/.rsclaw/models/bge-m3"
remote_provider = ""
dimension = 1024
batch_size_local = 16
batch_size_remote = 64

[kb.ocr]
default_tier = "auto"                # auto | fast | strong | fleet
fast_engine = "rapidocr-onnx"
strong_engine = "paddleocr-vl-1.5"
fleet_endpoint = ""                  # http://rsclaw-server:8444

[kb.ocr.routing]
detect_charts = true
detect_formulas = true
detect_tables = true
detect_handwriting = true
quality_fallback_threshold = 0.7

[kb.chunking]
target_tokens = 512
overlap_tokens = 64
min_tokens = 50
heading_path_prefix = true

[kb.retrieval]
default_k = 8
max_k = 20
default_mode = "hybrid"
diversity = "mmr"
mmr_lambda = 0.5
single_result_max_bytes = 8192
fetch_full_doc_max_bytes = 32768
search_calls_per_turn_limit = 5
entity_alignment = true
require_entities_default = false

[kb.entity]
extractors = ["regex", "jieba"]       # v2 可加 "llm"
resolver_merge_variants = true

[kb.lifecycle]
tombstone_retention_days = 30
compactor_schedule = "03:00"
compactor_interval_secs = 3600
tombstone_ratio_threshold = 0.15

[kb.sync]
enabled = true
scheduler_tick_secs = 300             # 5min
default_daily_budget = 500            # per syncer per day
default_backoff_max_secs = 86400      # 24h 封顶

[kb.security]
allow_remote_embedding = false        # 默认不允许 chunk 走外网
allow_fleet_ocr = true                # rsclaw-server 是自有基础设施
chat_history_self_only = true         # 聊天历史只入自己消息
log_redaction = true                  # 强制 PII redaction
```

所有项 hot-reload。

### Security / 隐私

- **本地默认**：embedding + OCR + entity 默认全本地
- **远程开关显式**：用户启 remote 路径时弹一次确认
- **PII redaction 强制**：所有 log 走 `src/kb/util/redact.rs`，source_id / 内容预览永远是哈希
- **聊天历史**：默认只入用户自己 + @ 自己消息
- **v1 不加密** kb 目录（与现有 store.redb 一致）；v2 考虑 AGE 加密 + raw 目录

## §M Memory ↔ KB 双向流（独有）

rsclaw 有两套独立的"记忆"系统：
- **Agent memory** (`src/agent/memory.rs`)：agent 在对话中自学的、会衰减的、私有的（隐式上下文）
- **KB**：用户主动喂入、不衰减、可溯源（显式上下文）

**双向流让两者协同：**

### Memory → KB 晋升

当一条 agent memory item 满足：
- `stability_score ≥ 0.85`（连续 N 次确认）
- `importance ≥ 0.7`
- **用户在 UI 上手动确认 "promote to KB"** —— 不自动晋升，避免污染

→ 创建对应 KbDoc，`source_kind = Doc`，`source_id = "agent_memory:<mem_id>"`，写到 `md/doc/agent-memory-<slug>.md`，YAML front-matter 标注来源。

**用例：** 用户跟 agent 反复确认"我们项目的部署流程是 X"，agent memory 稳定记下来；用户觉得有价值 → 一键晋升 KB → 其他 agent 也能查到。

### KB → Memory 预热

session 启动时：

```rust
async fn warm_session_memory(thread_id: &str, ctx: &SessionContext) {
    // 1. 拿到对话主题（前 N 条消息 / channel context）
    let topic_summary = ctx.thread_summary().await?;
    
    // 2. KB 检索 top-K 相关 chunks（轻量，k=5，只看 authoritative tier）
    let hits = kb_search(KbSearchRequest {
        query: topic_summary,
        k: 5,
        filter: { min_quality: 0.8, source_kind: None, ... },
        diversity: "mmr",
    }).await?;
    
    let authoritative: Vec<_> = hits.results
        .into_iter()
        .filter(|h| h.citation_tier == "authoritative")
        .collect();
    
    // 3. 注入 agent memory 作为"会话级背景知识"
    ctx.memory.inject_session_context(authoritative).await?;
}
```

**用例：** 用户在 PM 群问"上次说的那个 OKR 怎么定的"，agent session 启动时 KB 已经把相关 PRD 章节预热进 memory，agent 第一句话就能精准回答，不用先 kb_search 一轮。

### 防回路

- Memory → KB 晋升后，**该 memory item 标 `promoted_to_kb_at`，不再参与 KB → Memory 预热**（防止自己喂自己循环）
- KB → Memory 注入的 session context 标 `from_kb_at`，不参与 Memory → KB 晋升候选（防止洗白）

### Config

```toml
[kb.memory_bridge]
enabled = true
promotion_stability_threshold = 0.85
promotion_importance_threshold = 0.7
promotion_requires_user_confirm = true   # 默认 true，避免自动污染
warm_session_enabled = true
warm_session_k = 5
warm_session_min_tier = "authoritative"
```

---

## §S 数据源同步（KbSourceSyncer 框架）

### S.1 核心 Trait

```rust
#[async_trait]
pub trait KbSourceSyncer: Send + Sync + 'static {
    fn source_kind(&self) -> KbSourceKind;
    fn source_id(&self) -> &str;
    fn sync_interval_secs(&self) -> Option<u64> { Some(20 * 60) }

    async fn sync(
        &self,
        ctx: &SyncContext,
        state: &mut SyncState,
        reason: SyncReason,
    ) -> Result<SyncOutcome, SyncError>;

    async fn on_enable(&self, _ctx: &SyncContext) -> Result<(), SyncError> { Ok(()) }
    async fn on_disable(&self, _ctx: &SyncContext) -> Result<(), SyncError> { Ok(()) }
    fn health(&self) -> SyncerHealth { SyncerHealth::Unknown }
}

pub enum SyncReason {
    Periodic,
    Event(EventTrigger),
    Manual,
    OnEnable,
    Catchup,
}

pub enum EventTrigger {
    FsChange(PathBuf),
    ChannelMessage(ChannelMsgRef),
    ConfigChanged,
}

pub struct SyncContext {
    pub kb_store: Arc<KbStore>,
    pub ingest: Arc<IngestPipeline>,
    pub events: Arc<EventBus>,
    pub clock: Arc<dyn Clock>,
    pub now_unix_ms: i64,
    pub cancel: CancellationToken,
}

pub struct SyncOutcome {
    pub docs_added: usize,
    pub docs_updated: usize,
    pub docs_skipped: usize,
    pub api_requests_used: u32,
    pub partial: bool,
    pub next_run_hint: Option<Duration>,
}

pub enum SyncerHealth {
    Healthy,
    Degraded { reason: String },
    Failed { since: i64, error: String },
    Paused,
    Unknown,
}

pub enum SyncError {
    AuthFailed(String),
    RateLimited { retry_after_secs: u64 },
    BudgetExhausted,
    Network(String),
    Parse(String),
    Permanent(String),
    Cancelled,
}
```

### S.2 SyncState（KV 持久化在 kb.redb）

```rust
pub struct SyncState {
    pub source_kind: KbSourceKind,
    pub source_id: String,
    
    pub cursor: Option<String>,           // provider-specific watermark
    pub last_seen_id: Option<String>,     // "第一页第一条已见"短路
    
    pub seen_index: ScalableSeenSet,      // Bloom (假阳性<0.1%) + LRU(10000) 精确集
    
    pub daily_budget: DailyBudget {
        date: NaiveDate,
        used: u32,
        limit: u32,
    },
    
    pub status: SyncStatus,
    pub last_sync_at: Option<i64>,
    pub last_success_at: Option<i64>,
    pub last_error: Option<SyncErrorRecord>,
    pub consecutive_failures: u32,
    pub paused_until: Option<i64>,
    
    pub total_docs_ingested: u64,
    pub total_runs: u64,
    pub total_runs_failed: u64,
    pub created_at: i64,
    pub updated_at: i64,
}
```

**`ScalableSeenSet` 实现：** 1M-entry bloom + 10K-entry LRU 精确集合，序列化到 redb blob。误命中由 chunk-level deterministic id 兜底（同内容 → 同 chunk_id → upsert no-op）。

### S.3 Scheduler 集成

复用 `src/cron/`，注册一个 5min tick：

```rust
pub fn register_kb_sync_cron(cron: &mut CronRegistry, kb: Arc<KbStore>) {
    cron.register(CronJob {
        id: "kb_sync_tick".into(),
        schedule: CronSpec::Interval(Duration::from_secs(300)),
        handler: Arc::new(move |_| Box::pin(tick_all_syncers(kb.clone()))),
    });
}

async fn tick_all_syncers(kb: Arc<KbStore>) -> Result<()> {
    for entry in kb.list_active_syncers().await? {
        let kb = kb.clone();
        tokio::spawn(async move {
            if let Err(e) = maybe_run_one(kb, entry.clone()).await {
                log::warn!("[kb_sync] {} errored: {}", redact(&entry.source_id), e);
                // 永远不 panic out
            }
        });
    }
    Ok(())
}
```

**Event-driven sync**（毫秒级响应）：启动时订阅 `FsChangeEvent` / `ChannelMessageEvent`，命中匹配的 syncer 立即 `run_now(SyncReason::Event(...))`。

**Run-now 与 tick 防并发：** 每 source_id 一个 `Mutex<()>`，命中正在跑就 no-op + 返回 "already running"。

### S.4 V1 四个 Syncer

#### ManualUploadSyncer

`sync_interval_secs() = None`（不进周期）。用户在 UI 拖文件 / CLI `add` 时直接调 `ingest.ingest_*`。存在意义：**统一所有 source 的代码路径**（list/统计/健康监控）。

#### UrlSyncer

```rust
pub struct UrlSyncer {
    url: String,
    schedule: CronSpec,
    use_browser: bool,
}
```

流程：HEAD 拿 ETag/Last-Modified → 若未变直接返回 → GET 拿 body → content_hash 兜底 → ingest_url。

cursor 形式：`"etag:xxx"` / `"lastmod:xxx"` / `"contenthash:xxx"`。

错误：4xx → `Permanent`（暂停 syncer 等用户改 URL）；5xx/timeout → `Network`（退避重试）。

#### LocalFolderSyncer

```rust
pub struct LocalFolderSyncer {
    root: PathBuf,
    include_globs: Vec<String>,       // 默认 ["**/*.{pdf,md,markdown,txt,docx}"]
    exclude_globs: Vec<String>,       // 默认 [".*", "**/node_modules/**", "**/target/**", "**/.git/**"]
    watch_realtime: bool,
    schedule: CronSpec,
}
```

- **on_enable**：若 `watch_realtime` 启 notify watcher → FsChangeEvent → event-driven sync
- **周期 sync**：walk + glob 过滤 → `(path_hash, mtime, size)` 当 entry_id 查 `synced_ids` → 新增就 ingest_file
- **删除检测**：周期扫识别 `synced_ids` 里有但文件没了的 → tombstone 对应 doc（30 天恢复期）
- **大目录首次 backfill**：daily_budget 用尽 → `partial=true`，跨天分批

v1 不识别 rename/move（被当"删除+新增"）；v2 加 inode 追踪。

#### ChannelHistorySyncer

```rust
pub struct ChannelHistorySyncer {
    channel_id: String,
    backfill_until: Option<NaiveDate>,
    self_messages_only: bool,         // 默认 true
    incremental_via_event: bool,
}
```

- **on_enable**：若 `incremental_via_event` 订阅 channel 新消息事件 → 实时增量
- **首次 sync**：backward 拉历史到 `backfill_until`，可断点续传
- **后续 sync**：forward 增量
- **隐私过滤**：`self_messages_only=true` 默认只入用户消息 + @ 自己
- **bucket**：按 5min idle gap 切对话块（复用 chunker chat 逻辑）
- **复用 `src/channel/<provider>`** 现有 `fetch_messages`，不重复实现 channel adapter

### S.5 失败 / 重试 / 暂停

**指数退避表（consecutive_failures → paused_until 增量）：**

| 失败次数 | paused_until 增量 |
|---|---|
| 1 | 0 (立即下次 tick 再试) |
| 2 | 1 min |
| 3 | 5 min |
| 6 | 1 h |
| 12 | 6 h |
| >12 | 24 h (封顶) |

成功一次 → `consecutive_failures = 0` + `paused_until = None`。

**SyncError 分类处理：**

| 错误 | 重试 | 用户通知 |
|---|---|---|
| `AuthFailed` | 否 | 是（UI 红点 + 通知"凭证失效"） |
| `RateLimited { retry_after }` | 是（按 retry_after，不算 failure） | 否 |
| `BudgetExhausted` | 跨天自动重试 | UI 显示"今日配额已用尽" |
| `Network` | 是（退避） | 持续 >1h 才通知 |
| `Parse` (单 doc) | 否（跳过该 doc） | 是（日志 + UI"X 篇解析失败"） |
| `Permanent` | 否（暂停 syncer） | 是（必须用户改配置） |
| `Cancelled` | 不算失败 | 否 |

**用户暂停**：`state.status = Paused` + `paused_until = None`。scheduler 直接跳过。恢复需用户主动。

### S.6 与 ingest pipeline 的关系

所有 syncer 都通过 `IngestPipeline` 入库：

```rust
impl IngestPipeline {
    pub async fn ingest_file(&self, path: &Path, tags: &[String]) -> Result<KbDocId>;
    pub async fn ingest_url(&self, url: &str, body: Bytes, tags: &[String]) -> Result<KbDocId>;
    pub async fn ingest_chat_batch(&self, channel: &str, batch: ChatBatch) -> Result<KbDocId>;
    pub async fn ingest_image(&self, path: &Path, tags: &[String]) -> Result<KbDocId>;
    pub async fn ingest_markdown(&self, doc: CanonicalizedSource) -> Result<KbDocId>;
}
```

`ingest_*` 内部：fetch + canonicalize + stage_doc + 写 KbDoc + enqueue `ChunkAndEmbed` job → 立即返回 doc_id。syncer 不阻塞在 OCR/embedding 上，job worker 后台异步跑完整 pipeline。

### S.7 三层 Dedup

| 层 | 机制 | 漏掉时谁兜底 |
|---|---|---|
| API/HTTP 层 | cursor + ETag + after_filter | SyncState |
| SyncState 层 | `synced_ids` BloomLru | Chunk |
| Chunk 层 | deterministic `chunk_id = sha256(...)` | — |

任一层漏掉，下一层兜住。BloomLru 的假阳性（误判已同步）由 chunk-level deterministic id 完全消化。

## §7 实施分期

| Phase | 内容 | 工期 |
|---|---|---|
| **1 MVP** | model + redb + tantivy + hnsw + canonicalize (md/text/html + 文本层 PDF) + content_store + chunker (deterministic id + heading_path) + LocalBgeM3 + entity (regex+jieba) + Writer + Jobs queue 基础 + Hybrid+RRF + kb_search/fetch/list_docs + ManualUploadSyncer + CLI 基础 | **3 周**（比 v1 多 1 周，主要 content_store + jobs queue + entity index） |
| **2 基础可用** | Tauri 控制台「知识库」面板（文档 tab）+ 拖拽上传 + 任务进度 + Citation 渲染全套 + entity_alignment + require_entities + RAG 引用纪律 prompt + MMR 默认开 + 远程 embedding 备路 + kb_search_entities 工具 + **citation_confidence 评分 + kb_explain trace 工具** | 2 周 |
| **3 OCR 接入** | OcrEngine trait + Tier Fast (RapidOCR) + 预扫描路由 + OCR 任务异步队列 + 断点续传 + 扫描 PDF / 单图入库 | 2 周 |
| **4 Strong/Fleet 层** | Tier Strong (PaddleOCR-VL 1.5) + Tier Fleet (Qianfan-OCR via rsclaw-server :8444 vLLM sidecar) + 自动路由 + 部署脚本 | 2 周 |
| **5 Syncer 框架** | KbSourceSyncer trait + SyncState + ScalableSeenSet + scheduler 接 src/cron + UrlSyncer + LocalFolderSyncer + ChannelHistorySyncer + 数据源 tab UI + sync CLI 全套 | 2 周 |
| **5.5 Fleet + Memory 桥（独有）** | jobs/fleet_dispatch.rs + rsclaw-server `/v1/embed/batch` + `/v1/entity/batch` endpoint + retrieval/memory_bridge.rs + Memory↔KB 晋升 UI + warm_session 钩子 | 1.5 周 |
| **6 Compactor / 迁移 / 收尾** | Compactor 后台 + Embedding 迁移流程 + 整体 e2e 测试 + 文档 + 灰度发布 | 1 周 |
| **总工期** | | **~13.5 周** |

**v2 留作：**

- **MailSyncer**：IMAP / Gmail / Outlook 直连，cursor = internalDate epoch ms，bucket by participants
- Reranker (BGE-Reranker-v2-m3)
- Summary tree 架构（per-source / per-topic / global 三层 summary，给聊天历史和邮件流这种高基数源用，bucket-seal 模式）
- Admission gate（score 模块，给流式源做"keep or drop"决定）
- `drill_down` retrieval 工具（从 summary 钻到 leaves）
- LLM EntityExtractor（NER + 重要性）
- URL 整站爬取（sitemap.xml + per-page state）
- 引用图谱可视化
- AGE 加密
- 多用户 / per-agent 库
- 聊天 `@kb:doc_id` 提及
- 浏览器右键发送
- rename/move 识别（inode 追踪）

## Open Questions（实施前需 review）

无 —— 所有设计岔路均与用户对齐：
- 全局一个库 / Tool-call retrieval
- 文档源 v1 = 本地+URL+聊天+图片，不含代码 repo / 整站
- OCR 三层路由 + Qianfan 走 vLLM sidecar
- Embedding BGE-M3 本地 + 远程备
- Citation 必带 + click-to-jump
- 目录布局 `~/.rsclaw/kb/{md,raw,db,idx,hnsw,state}/`
- `raw/` 默认开
- chunk_id 走 deterministic sha256
- Canonicalize-first + content_store on disk
- Entity inverted index 解决"伊利问题"
- Syncer 框架 + 4 v1 syncers
- BloomLru / 30 天 tombstone / v1 不做整站 / self_messages_only=true / 5min tick / 单一 500/day 默认（这些是推荐默认，用户 review 时可改）

## 边界场景测试清单（实施验收用）

- [ ] **「伊利问题」**：query 含库里没有的实体 → entity_alignment warning 出现 → agent 不张冠李戴
- [ ] **同名歧义**：「小米」（品牌 vs 粮食）→ entity resolver 区分 canonical_id
- [ ] **跨文档矛盾**：两份 doc 说法不一 → 召回两条，agent 应识别并告知
- [ ] **时间敏感**：旧版 vs 新版文档 → version + updated_at 排序
- [ ] **大文档**：5000 页 PDF 入库 → 任务断点续传、进度可见、不阻塞 UI
- [ ] **大库查询**：百万 chunk → search 延迟 <500ms
- [ ] **HNSW 重建**：模拟 20% tombstone → compactor 触发 → 重建期间查询不中断
- [ ] **Embedding 模型迁移**：切换 backend → 双写 → 进度可见 → 7 天后老 index 自动清
- [ ] **OCR 路由**：扫描合同自动路由到 Fleet；纯文本扫描走 Fast
- [ ] **隐私**：聊天历史入库时他人消息被过滤
- [ ] **幂等性**：同一文件重复 `kb add` → 同 doc_id 同 chunk_id → upsert no-op
- [ ] **同步 dedup**：URL 周期重抓未变化时 entity_alignment 返回 docs_skipped=1，0 增量
- [ ] **Bloom 假阳性**：人为构造 BloomLru 误命中 → chunk-level deterministic id 兜底正确
- [ ] **退避**：连续 12 次失败后退避 6h，成功一次后重置
- [ ] **跨重启**：进程崩溃 → Job worker `recover_stale_locks` → 任务续跑
- [ ] **PII redaction**：日志里无明文 source_id 或内容预览
- [ ] **自包含**：`cp -r ~/.rsclaw/kb/ /tmp/backup/`，改 root_dir 指向 backup → 完整可用
- [ ] **`keep_raw=false`**：raw/ 不写；KbDoc.raw_path = None；canonicalize 后立即丢弃 raw bytes
- [ ] **.eml 上传**：拖一个 .eml 文件入 KB → 自动路由到 MailCanonicalizer → 写 `md/mail/` 而非 `md/doc/`，source_kind=mail，participants 正确解析
- [ ] **.mbox 上传**：批量邮件 .mbox → 按 thread 切多份 KbDoc，participants 相同的合并到同一个月份文件
- [ ] **Fleet ingest 阈值**：拖 100+ PDF → 自动走 fleet 路径；fleet 不可用 → 静默 fallback 本地，UI 显示路径
- [ ] **citation_confidence**：相同 score 的两 chunks，一个 90 天前一个昨天 → 后者 confidence 显著高，tier 更高
- [ ] **kb_explain 完整性**：拿 trace_id 调 explain → 返回所有命中 term / 激活维度 / MMR 决策的完整解释
- [ ] **Memory→KB 晋升**：稳定 memory item 用户点 promote → 出现在 `md/doc/agent-memory-*.md`，front-matter 标注来源
- [ ] **KB→Memory 预热**：session 启动 → 相关 authoritative chunks 入 agent context；不会回流到 KB 晋升候选

## References

### 算法 / 模式（公开发表的方法）

- **RRF 融合**：Cormack et al., "Reciprocal Rank Fusion outperforms Condorcet and individual Rank Learning Methods" (SIGIR 2009)
- **MMR 多样性**：Carbonell & Goldstein, "The Use of MMR, Diversity-Based Reranking for Reordering Documents and Producing Summaries" (SIGIR 1998)
- **SimHash**：Charikar, "Similarity estimation techniques from rounding algorithms" (STOC 2002)
- **BM25**：Robertson & Walker, "Some Simple Effective Approximations to the 2-Poisson Model for Probabilistic Weighted Retrieval" (SIGIR 1994)
- **HNSW**：Malkov & Yashunin, "Efficient and robust approximate nearest neighbor search using Hierarchical Navigable Small World graphs" (2016)
- **BGE-M3**：BAAI, "BGE M3-Embedding: Multi-Lingual, Multi-Functionality, Multi-Granularity Text Embeddings Through Self-Knowledge Distillation"

### 工具 / 模型

- **RapidOCR**：PP-OCRv4 蒸馏 ONNX 实现
- **PaddleOCR-VL 1.5**：Apache 2.0, OmniDocBench v1.5 SOTA pipeline
- **Qianfan-OCR 4B**：Apache 2.0, end-to-end SOTA + KIE，via vLLM/SGLang serve

### rsclaw 内部依赖

- `src/agent/memory.rs` —— lifecycle 区别参照
- `src/store/` —— redb + tantivy + hnsw_rs 基础设施
- `src/cron/` —— syncer scheduler 集成点
- `src/channel/` —— ChannelHistorySyncer 复用 `fetch_messages`
- `src/browser/` —— UrlSyncer 复用渲染
- `project_rsclaw_llm_rollout.md`（auto-memory）—— Fleet 部署上下文
- `project_context_mgmt_v2.md`（auto-memory）—— KV cache 优化路线

### 设计灵感

- Notion AI / Perplexity —— citation 渲染 UX
- Obsidian —— `.md` 文件本地优先的 PKM 模型
