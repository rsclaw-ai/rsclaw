# Knowledge Base — Design Spec

## Overview

给 rsclaw 加**用户级知识库**：用户喂入文件 / URL / 聊天历史 / 邮件 / 图片，agent 通过工具调用检索，回答带可点击的出处引用。

**和现有 `src/agent/memory.rs` 的区别**：memory 是 agent 自学/会衰减；KB 是用户喂入/不衰减/可溯源/版本化。共底层存储但完全独立的 lifecycle、DB 文件、隔离的 module 边界。

**4 个核心架构机制**（解决 RAG 系统常见的失败模式）：

1. **SourceIdentity + VersionGraph** —— logical_source_id（基于内容哈希 / URL 规范化 / 时间窗口）让"同物再传"幂等；KbDoc 版本链支持时间旅行查询和回滚
2. **IngestLedger + Outbox** —— 文件 stage 永不直接删；redb 单事务原子写 KbDoc + ledger + outbox job + seen_items；后台 compactor 按 ledger 清理孤儿文件。解决文件系统与 DB 无法天然事务的根本矛盾
3. **PermissionScope (visibility)** —— doc-level visibility 标签 + caller_scope 过滤，多 agent / 多 channel 不会互窜数据。聊天历史 / 邮件默认局部，不裸泳
4. **Index Rebuild Contract** —— redb 是 source of truth；HNSW 和 tantivy 是可重建缓存；进程内 ArcSwap 切换，磁盘 snapshot 仅做启动加速；缓存损坏直接从 redb 重建

**MVP 范围（4 周）：** 本地文件 + URL 两种 source；ManualUploadSyncer；canonicalize → outbox → 异步 chunk+embed+index；hybrid 检索；citation 渲染基础。**MVP 移除**：fleet batch ingest、Memory↔KB bridge、kb_explain、citation_confidence、image/chat/mail source、OCR Strong/Fleet、4 syncer 全套（保留 ManualUpload）。这些都进 v2 backlog。

**借鉴：** RAG 领域成熟模式（RRF、MMR、HNSW、BM25、SimHash、BGE-M3）；通用 production 模式（outbox pattern、job queue 的 dedupe_key + claim_token 设计参考 Sidekiq / RQ / Faktory 系列）。**实施时 clean-room 写代码**，不参考任何带 license 风险的具体项目。

## 设计决策

| 决策点 | 选择 | 原因 |
|---|---|---|
| **MVP 边界** | 本地文件 + URL；ManualUploadSyncer；无 fleet / memory bridge / kb_explain / citation_confidence | 4 周可交付；其余 v2 |
| 用户边界 | 全局 KB pool + per-doc visibility | YAGNI 多租户；视野控制走 visibility 标签 |
| Retrieval 方式 | Tool-call（agent 主动） | KV cache 友好；与 agent-loop 哲学一致 |
| 存储后端 | redb + tantivy + hnsw_rs，独立 DB 文件 | 零新依赖；与现有 store 平级隔离 |
| **目录布局** | `~/.rsclaw/kb/{md,raw,db,idx,hnsw,state}/` | 自包含，可整目录搬运 |
| Content store | canonicalized markdown 作为 `.md` 文件落 `md/<kind>/`；DB 只存 path + sha256 + byte_offset | Obsidian / grep 友好；DB 不臃肿；备份=copy 文件夹 |
| Raw cache | 默认开（`kb.keep_raw=true`） | 可重新 canonicalize；用户可关 |
| **logical_source_id** | `file:sha256:<hash>` / `url:<normalized>` / `chat:<channel>:<window>` / `mail:<msg_id>` | 幂等性的真正 key；与 doc_id（ULID 实例）分离 |
| **VersionGraph** | KbDoc.version + latest_version 表 + 老版本保留 | 重传 = 新 version；time-travel 查询；回滚窗口 |
| **chunk_id** | deterministic `sha256(logical_source_id\|seq\|content)` 截 32 hex | 真幂等（同 logical_source_id 同内容 → 同 id） |
| Canonicalize-first | 所有源 → CanonicalizedSource { markdown, metadata } | 下游零分支 |
| Chunker | 默认 512/64 token，**heading_path 强制前缀**注入 indexed text，SimHash 去重 | 防"伊利问题" |
| **IngestLedger + Outbox** | 文件只 stage 不删；redb 单 tx 写 KbDoc + Ledger + Job + seen_items；compactor 按 ledger 清孤儿 | 唯一能让 FS + DB 在崩溃下保持一致的方案 |
| **Jobs queue** | redb 显式索引表：`jobs_by_id` / `jobs_by_dedupe_active` / `jobs_by_status_priority` / `job_claims` | redb 不是 SQLite，必须用 KV 思维建模；单写事务保证原子 |
| **seen_items** | redb 表 `seen_items: (source_id, item_id) → SeenRecord` | B-tree 百万级 lookup μs 级；不用 Bloom（Bloom 假阳性会让 syncer 漏数据） |
| Entity inverted index | `KbEntity` + `KbEntityIndex` 表 | 入库 O(N) 建索引；查询 O(1)；驱动 entity_alignment + kb_search_entities |
| Hybrid 检索 | Dense (BGE-M3) + Sparse (BM25) + RRF + MMR | 不调权重；MMR 默认开 (λ=0.5) |
| **kb_explain** (v2) | 仅解释 BM25 term / Dense rank+score / RRF 贡献 / entity hit/miss / MMR 决策 / citation factors —— **不解释 embedding 维度** | embedding 维度对人不可读，承诺会卖假药 |
| **citation_confidence** (v2) | `f(quality, recency_policy, is_latest_version, entity_alignment, source_kind_trust)` | 三档 tier 引导 agent 引用措辞 |
| **recency_policy** (v2) | 每 doc：Evergreen / Versioned / TimeSensitive | 一刀切 decay 会误伤合同 / API spec |
| **PermissionScope** | doc-level `visibility: KbVisibility { Global, Agent, Channel, Private }` | 全局共享 + per-doc 边界，按 source_kind 默认 |
| **HNSW as cache** | redb 是 source of truth (chunks + vectors)；进程内 `ArcSwap<Hnsw>` 原子切换；磁盘 snapshot 仅启动加速 | 重建期间不阻塞读；缓存损坏从 redb 重建 |
| OCR (v1 仅 Fast) | RapidOCR (PP-OCRv4 ONNX) via `ort` | 中文准确率显著高于 tesseract；ONNX 集成轻 |
| OCR Strong/Fleet | v2：PaddleOCR-VL 1.5 / Qianfan-OCR 4B | MVP 不上 |
| Embedding | BGE-M3 本地 (1024) 默认；远程 API 备路 | desktop-first |
| Citation | agent 用 `[^kb:<chunk_id>]` 标记，前端 `<KbCitation>` 渲染 + 点击跳源 | 不让 agent 拼 URL |
| Locator | enum (PdfPage/MdSection/UrlAnchor/ChatMsgs/Image/Offset) | UI 跳源到具体位置 |
| 删除机制 | Tombstone + filter + 后台 compactor，30 天恢复期 | hnsw_rs 不支持单点删 |
| 模型迁移 | 双写 + 渐进重建 + 7 天回滚（v2） | 不能让旧 vector 立刻失效 |
| PII redaction | 日志走 `util/redact.rs` | source_id / 内容预览永远哈希 |
| 聊天历史隐私 | 默认 `self_messages_only=true` + visibility=Channel | 不把他人发言入库 |

## 模块布局

```
src/kb/
  mod.rs              # public façade
  paths.rs            # ~/.rsclaw/kb/ root + subdirs
  model/
    mod.rs
    doc.rs            # KbDoc + KbStatus + KbVisibility + CallerScope
    chunk.rs          # KbChunk + chunk_id() function
    source.rs         # KbSource + KbSourceKind + MailSource + LogicalSourceId
    locator.rs        # KbLocator
    entity.rs         # KbEntity + KbEntityIndex + EntityKind
    simhash.rs        # SimHash-64
    version.rs        # KbDocVersion + latest_version helpers
  content_store/
    mod.rs            # stage_doc / read_doc_body / read_doc_range public API
    atomic.rs         # tempfile + fsync + rename + parent fsync
    paths.rs          # markdown_rel_path / raw_rel_path / slugify
    compose.rs        # YAML front-matter + body composition
    read.rs           # parse front-matter, read body, verify SHA
  store/
    mod.rs            # KbStore facade
    schema.rs         # redb table definitions
    doc_access.rs     # KbDoc accessors (+ latest_version)
    chunk_access.rs   # KbChunk accessors
    entity_access.rs  # KbEntity + KbEntityIndex accessors
    seen_access.rs    # seen_items table accessors
    tantivy_schema.rs # tantivy schema + open helpers
    hnsw_cache.rs     # ArcSwap<Hnsw> cache; build_from_redb; snapshot to disk
  canonicalize/
    mod.rs            # Canonicalizer trait + CanonicalizedSource
    text.rs / md.rs / html.rs / pdf.rs  # MVP source canonicalizers
    mime.rs           # mime detection + dispatch
    # v2: chat.rs / image.rs / mail.rs
  chunker/
    mod.rs            # chunk_markdown(input) -> Vec<Chunk>
    splitter.rs       # paragraph + sentence splitters (CJK aware)
    tokens.rs         # approximate token count
  embedder.rs         # KbEmbedder trait + LocalBgeM3 + RemoteApi(v2)
  entity/             # v1 仅 regex + jieba
    mod.rs            # CompositeExtractor
    regex.rs
    jieba.rs
    resolver.rs       # 实体规范化
  ledger/             # 新：IngestLedger
    mod.rs            # public API: enqueue / commit_complete / list_pending
    types.rs          # IngestLedgerEntry + LedgerOp + LedgerStatus
    store.rs          # redb accessors
    compactor.rs      # 后台 reconcile + 物理删孤儿文件
  jobs/               # redb-native job queue
    mod.rs            # public API: enqueue_tx / claim_next / mark_done / mark_failed / reclaim_stale_jobs
    types.rs          # JobKind / JobStatus / Job / ClaimToken
    store.rs          # 4 表：jobs_by_id / jobs_by_dedupe_active / jobs_by_status_priority / job_claims
    worker.rs         # worker pool (3 task)
    handlers/         # 每 JobKind 一个 handler
  retrieval/
    mod.rs            # kb_search / kb_fetch / kb_list_docs / kb_similar / kb_search_entities
    hybrid.rs         # Dense + BM25 + RRF
    mmr.rs            # MMR
    alignment.rs      # entity_alignment（查倒排索引）
    scope.rs          # CallerScope + visibility filter
  syncer/             # v1 仅 ManualUploadSyncer；HistoryProvider trait 留 v2
    mod.rs            # KbSourceSyncer trait + SyncState
    state.rs          # SyncState 持久化
    scheduler.rs      # 复用 src/cron/ tick（v1 几乎只跑 ManualUpload）
    impls/
      manual.rs       # ManualUploadSyncer
      url.rs          # UrlSyncer（v1）
      # v2: folder.rs / chat_history.rs (HistoryProvider 落地后)
  util/
    redact.rs         # PII redaction for logs

src/cmd/              # v1 CLI
  kb_add.rs / kb_ls.rs / kb_rm.rs / kb_search.rs / kb_show.rs
  kb_compact.rs / kb_stats.rs / kb_export.rs
  # v2: kb_sync_*, kb_reindex, kb_explain

src/agent/tools_kb.rs # tool 注册

ui/app/components/kb/ # v2（MVP 不上 UI 面板，CLI 跑通先）
```

## 存储布局

```
~/.rsclaw/kb/
  md/{doc,url}/              # canonicalize 后整文档 (Obsidian/grep 友好)
                             # v2 加 {chat, img, mail}/
  raw/                       # 原始字节（kb.keep_raw=true 时存）
  db/
    kb.redb                  # 所有元数据 + ledger + jobs + seen_items
  idx/                       # tantivy FTS 索引目录（可重建缓存）
  hnsw/
    kb_v1024_<embedder>.snap # HNSW snapshot（仅启动加速；可重建缓存）
  state/                     # syncer state（v1 几乎为空）
```

**关键约束：**
- `~/.rsclaw/kb/` 整目录自包含；备份 = `cp -r kb/ backup/`
- **redb 是 source of truth**；`md/` `idx/` `hnsw/` 全可从 redb 重建
- `md/` body 不可变（写入后只读）；YAML front-matter tags 块可独立重写

---

## §1 数据模型

```rust
// src/kb/model/source.rs

pub enum KbSourceKind {
    Doc,    // on-wire "doc"
    Chat,   // on-wire "chat"   (v2)
    Url,    // on-wire "url"
    Img,    // on-wire "img"    (v2)
    Mail,   // on-wire "mail"   (v2)
}

pub enum KbSource {
    Doc  { path: PathBuf },
    Url  { url: String, fetched_at: i64 },
    Chat { channel: String, range: (i64, i64) },
    Img  { path: PathBuf },
    Mail { source: MailSource },
}

/// 幂等性的真正 key。同 logical_source_id 的重复 ingest 视为新版本，
/// 不产生新的 chunk_id 集合（同内容 → 同 chunk_id）。
pub struct LogicalSourceId(pub String);

impl LogicalSourceId {
    pub fn for_file(sha256_hex: &str) -> Self {
        Self(format!("file:sha256:{sha256_hex}"))
    }
    pub fn for_url(normalized_url: &str) -> Self {
        Self(format!("url:{normalized_url}"))
    }
    pub fn for_chat_bucket(channel: &str, window_start_unix: i64) -> Self {
        Self(format!("chat:{channel}:{window_start_unix}"))
    }
    pub fn for_mail(message_id: &str) -> Self {
        Self(format!("mail:{message_id}"))
    }
}
```

```rust
// src/kb/model/doc.rs

pub struct KbDoc {
    pub id: String,                       // ulid (instance)
    pub logical_source_id: String,        // 幂等 key（详见 §I）
    pub source: KbSource,
    pub source_kind: KbSourceKind,
    pub title: String,
    pub mime: String,
    pub raw_sha256: String,               // sha256 of 原始 bytes
    pub markdown_path: String,            // 相对 kb_root: "md/doc/<slug>.md"
    pub markdown_sha256: String,          // sha256 of body bytes only
    pub raw_path: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
    pub version: u32,                     // 版本链：见 §I VersionGraph
    pub status: KbStatus,
    pub visibility: KbVisibility,         // §K PermissionScope
    pub tags: Vec<String>,
    pub meta: serde_json::Value,
}

pub enum KbStatus { Active, Tombstoned, Updating }

/// 权限边界。Agent 检索时按 caller_scope 过滤。
pub enum KbVisibility {
    Global,                                // 任意 agent / 任意 caller
    Agent   { agent_id: String },
    Channel { channel_id: String },
    Private,                               // 仅 owner（一般指用户本人）
}

pub struct CallerScope {
    pub agent_id: Option<String>,
    pub channel_id: Option<String>,
    pub user_id: Option<String>,           // 与 owner 比对
}
```

**Visibility 默认表（按 source_kind）：**

| source_kind | 默认 visibility |
|---|---|
| Doc (manual upload) | Global |
| Url | Global |
| Img (v2) | Global |
| Mail (v2) | **Private** |
| Chat (v2) | **Channel** (scoped to 来源) |

```rust
// src/kb/model/chunk.rs

pub fn chunk_id(logical_source_id: &str, seq: u32, content: &str) -> String {
    let mut h = Sha256::new();
    h.update(logical_source_id.as_bytes());
    h.update([0u8]);
    h.update(&seq.to_be_bytes());
    h.update([0u8]);
    h.update(content.as_bytes());
    let mut hex = String::with_capacity(64);
    for b in h.finalize().iter() {
        use std::fmt::Write;
        let _ = write!(hex, "{b:02x}");
    }
    hex.truncate(32);
    hex
}

pub struct KbChunk {
    pub id: String,                       // 32-hex deterministic
    pub doc_id: String,                   // 链回当前 KbDoc 实例
    pub logical_source_id: String,        // 链回逻辑身份（用于跨版本去重）
    pub doc_version: u32,
    pub seq: u32,
    pub heading_path: Vec<String>,
    pub byte_offset: (u64, u64),          // 在 markdown_path body 内
    pub indexed_text: String,             // heading_path > ... \n\n body
    pub vector: Vec<f32>,                 // 1024 (BGE-M3)，v1b 写入
    pub simhash: u64,
    pub locator: KbLocator,
    pub status: ChunkStatus,
    pub source_quality: f32,
    pub embedder_id: String,
}

pub enum ChunkStatus { Active, Tombstoned }
```

```rust
// src/kb/model/entity.rs

pub enum EntityKind { Brand, Person, Org, Email, Url, Hashtag, Other }

pub struct KbEntity {
    pub canonical_id: String,             // "ent_yili"
    pub surface_forms: Vec<String>,
    pub kind: EntityKind,
    pub created_at: i64,
}

pub struct KbEntityIndex {
    pub entity_id: String,
    pub chunk_id: String,
    pub doc_id: String,
    pub mention_count: u32,
    pub score: f32,
}
```

```rust
// src/kb/model/locator.rs

pub enum KbLocator {
    PdfPage   { page: u32, bbox: Option<(f32,f32,f32,f32)> },
    MdSection { heading_path: Vec<String> },
    UrlAnchor { fragment: Option<String> },
    ChatMsgs  { first_ts: i64, last_ts: i64 },
    Image     { bbox: Option<(f32,f32,f32,f32)> },
    Offset    { start: usize, end: usize },
}
```

### 存储映射（redb 表）

| 表 | Key | Value |
|---|---|---|
| `kb_docs` | doc_id (ulid) | JSON(KbDoc) |
| `kb_doc_latest_version` | logical_source_id | (doc_id, version) — 当前活跃版本指针，§I |
| `kb_chunks` | chunk_id (32-hex) | JSON(KbChunk) |
| `kb_chunk_by_logical` | `{logical_source_id}\0{chunk_id}` | () — 按 logical 反查 |
| `kb_entities` | canonical_id | JSON(KbEntity) |
| `kb_entity_index` | `{entity_id}\0{chunk_id}` | JSON(KbEntityIndex) |
| `kb_seen_items` | `{source_id}\0{item_id}` | JSON(SeenRecord) — §S |
| `kb_ledger` | ledger_id (ulid) | JSON(IngestLedgerEntry) — §J |
| `kb_jobs_by_id` | job_id | JSON(Job) — §J jobs queue |
| `kb_jobs_by_dedupe_active` | dedupe_key | job_id — 仅含 Ready/Running |
| `kb_jobs_by_status_priority` | `{status_byte}{prio_byte}{created_at_be}{job_id}` | () |
| `kb_job_claims` | job_id | JSON(ClaimToken) |
| `kb_sync_state` | source_id | JSON(SyncState) — §S |

---

## §I SourceIdentity + VersionGraph

### Logical Source Identity

一份 KB 文档有两层身份：

| 字段 | 类型 | 用途 |
|---|---|---|
| `id` | ULID | doc 实例 id；每次 ingest 都不同 |
| `logical_source_id` | String | 内容 / 来源的稳定 key；重复 ingest 同物 = 同 id |

**生成规则（已 spec'd）：**

| source_kind | logical_source_id 形如 | 备注 |
|---|---|---|
| Doc (file) | `file:sha256:<64-hex>` | sha256 of raw bytes（OCR 前的原文件） |
| Url | `url:<normalized>` | URL canonicalize：strip `utm_*`、sort query params、lowercase host、去 fragment |
| Chat (v2) | `chat:<channel>:<window_start_unix>` | window 默认 5 分钟 idle 切块（与 chunker chat 逻辑一致） |
| Mail (v2) | `mail:<rfc822_message_id>` | 直接用 Message-ID header |
| Img (v2) | `file:sha256:<64-hex>` | 同文件路径 |

**URL canonicalization（v1 实现）：**

```rust
fn canonicalize_url(raw: &str) -> Result<String> {
    let mut u = url::Url::parse(raw)?;
    // lowercase scheme + host
    let scheme = u.scheme().to_lowercase();
    u.set_scheme(&scheme).ok();
    if let Some(host) = u.host_str() {
        let lc = host.to_lowercase();
        u.set_host(Some(&lc)).ok();
    }
    // strip fragment
    u.set_fragment(None);
    // sort + filter query params
    let mut pairs: Vec<(String, String)> = u.query_pairs()
        .filter(|(k, _)| !k.starts_with("utm_") && k != "fbclid" && k != "gclid")
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    pairs.sort();
    u.query_pairs_mut().clear();
    for (k, v) in pairs { u.query_pairs_mut().append_pair(&k, &v); }
    if u.query() == Some("") { u.set_query(None); }
    Ok(u.to_string())
}
```

### Version Graph

同 logical_source_id 的多次 ingest 形成版本链：

```
file:sha256:abc → KbDoc { id: ulid1, version: 1, status: Active }   (旧版)
                  KbDoc { id: ulid2, version: 2, status: Active }   (新版)
                  KbDoc { id: ulid3, version: 3, status: Active }   (当前)
                                                ▲
                                                │
kb_doc_latest_version[file:sha256:abc] = (ulid3, 3)
```

**约定：**

- 重新 ingest 同 logical_source_id → `version = current_latest + 1`，写新 KbDoc + 更新 `kb_doc_latest_version` 指针（同一 redb 事务）
- 默认检索只看 latest version（`KbChunk.doc_version == kb_doc_latest_version[doc.logical_source_id]`）
- 老版本保留 30 天（lifecycle），支持 time-travel 查询（v2 `kb_search --as_of=<ts>`）和回滚
- 回滚 = 把 `kb_doc_latest_version` 指回老 doc_id（一个原子 put）
- 30 天后老版本由 compactor 移除（tombstone → 物理删 chunks + md 文件）

**幂等性强约束：**

- 重 ingest 完全相同内容（同 logical_source_id + 同 markdown body）→ chunk_id 完全相同 → upsert no-op；但 `KbDoc.version` 还是会 +1（数据无变化但实例新建）
- 优化：ingest 路径在写 KbDoc 之前先比 `raw_sha256` 与 latest 是否一致，若一致直接返回现有 doc_id（NOOP），不动 ledger / job
- 这把"刷新一下"的场景 short-circuit，避免膨胀版本链

---

## §J IngestLedger + Outbox

### 问题

文件系统操作（写 .md、删 .md、写 raw）和 redb 事务**无法天然原子**。任何"先写文件再写 DB"或"先写 DB 再删文件"都会在崩溃时产生：

- 孤儿文件（DB 没记录的 md/raw）
- 悬空指针（DB 记录指向不存在的文件）
- 漏 enqueue 的 job（chunks 永远不会被 embed）

### 解决：Outbox + Ledger 两层

**Outbox**：Job 不在 ingest 路径直接 enqueue 给 worker，而是写到 redb `kb_jobs_*` 表，跟 KbDoc 同事务。Worker 异步轮询。

**Ledger**：每次 ingest 产生一条 `IngestLedgerEntry`，记录"打算做什么 + 当前到哪一步"。文件系统操作只 stage（永不直接删旧文件）。Compactor 按 ledger 推进物理清理。

### Schema

```rust
// src/kb/ledger/types.rs

pub struct IngestLedgerEntry {
    pub id: String,                       // ulid
    pub created_at: i64,
    pub updated_at: i64,
    pub doc_id: String,
    pub logical_source_id: String,
    pub op: LedgerOp,
    pub new_paths: Vec<String>,           // 本次新写的 md/raw 路径
    pub old_paths: Vec<String>,           // 被新版本取代的老 md/raw 路径（待清理）
    pub status: LedgerStatus,
    pub error: Option<String>,
}

pub enum LedgerOp {
    Create,                               // 首次 ingest
    Update,                               // 新版本（version > 1）
    Delete,                               // 用户 tombstone
}

pub enum LedgerStatus {
    Pending,                              // tx commit 之后还有 finalize 步骤
    IndexingComplete,                     // chunks 已写 redb + tantivy + hnsw
    CleanupPending,                       // 等 compactor 清旧文件
    Done,                                 // 完成
    Failed,                               // 永久失败（人工介入）
}
```

### Ingest 流程

```
1. 计算 raw_sha256(bytes) → logical_source_id
2. 查 kb_doc_latest_version[logical_source_id]：
   - 命中且 raw_sha256 一致 → 返回现有 doc_id（NOOP，不动 ledger）
   - 否则继续

3. stage_doc(canonical, raw): 写 md/<kind>/<slug>.md + raw/<doc_id>.<ext>
   (原子写：tempfile + fsync + rename，永不覆盖已存在文件)

4. 准备 KbDoc { id: ulid, version: next, markdown_path, raw_path, ... }
5. 准备 IngestLedgerEntry { op: Create|Update, new_paths, old_paths, status: Pending }
6. 准备 Job { kind: ChunkAndEmbed(doc_id), dedupe_key: "chunk_embed:doc_id", status: Ready }

7. begin_write_tx():
     put kb_docs[doc_id] = doc
     put kb_doc_latest_version[logical_source_id] = (doc_id, version)
     put kb_ledger[ledger_id] = ledger_entry
     put kb_jobs_by_id[job_id] = job
     put kb_jobs_by_dedupe_active[dedupe_key] = job_id  (if absent)
     put kb_jobs_by_status_priority[key] = ()
   commit()

8. 返回 doc_id  (← 此时已经原子持久；崩溃可恢复)

[异步 worker]

9. Worker claim Job → chunk + embed + tantivy + hnsw 写入
10. 更新 IngestLedger status = IndexingComplete
11. 标记 Job Done

[后台 compactor，独立 tokio task]

12. 扫 kb_ledger where status = IndexingComplete:
    - 检查 old_paths：是否还被任何 active KbDoc 引用
    - 不再引用 → 物理删文件
    - 更新 ledger status = CleanupPending
13. 检查 CleanupPending 超过 retention（默认 30 天）→ status = Done
14. Failed 进入 dead letter queue（UI 显示 + 人工介入）
```

### 崩溃恢复矩阵

| 崩溃在哪一步 | 现象 | 恢复 |
|---|---|---|
| 1-2 之间 | 无副作用 | 无 |
| 3 完成，7 之前 | 孤儿 md/raw 文件 | Compactor 周期扫 `md/` `raw/` 文件，对照 ledger.new_paths 找不到的视为孤儿，删（grace period 1h，防进行中 ingest） |
| 7 commit 之前 | 文件已 stage，但 DB 无任何记录 | 同上，compactor 清 |
| 7 commit 之后，worker 没接到 | DB 完整；job 在 Ready；新 md 在硬盘；老 md 还在 | 进程重启后 worker 自动从 jobs_by_status_priority 拉到，继续 |
| 9 中途崩溃 | chunks 部分写，job 在 Running 但 claim_token 过期 | reclaim_stale_jobs 把 status 改回 Ready；handler 必须**幂等**（按 chunk_id deterministic 重写即可） |
| 11 之后，12 没跑 | ledger 在 IndexingComplete | Compactor 下次 tick 处理 |

### Compactor 协议

```rust
pub async fn run_compactor_tick(store: &KbStore) -> Result<()> {
    // 1. Orphan file scan: md/ + raw/ 文件不在任何 ledger.new_paths 中且
    //    文件 mtime > 1h 前 → 删除（grace period 防进行中 ingest）
    scan_and_delete_orphan_files(store).await?;

    // 2. Ledger 状态推进
    for entry in list_ledger_status(store, LedgerStatus::IndexingComplete)? {
        if all_old_paths_unreferenced(store, &entry.old_paths)? {
            delete_files(&entry.old_paths)?;
            update_ledger_status(store, &entry.id, LedgerStatus::CleanupPending)?;
        }
    }

    // 3. CleanupPending 超过保留期 → Done
    let now = now_unix_ms();
    for entry in list_ledger_status(store, LedgerStatus::CleanupPending)? {
        if now - entry.updated_at > kb.lifecycle.tombstone_retention_days * 86400000 {
            update_ledger_status(store, &entry.id, LedgerStatus::Done)?;
        }
    }

    Ok(())
}
```

Compactor 跑频：默认每 1h 一次 + 凌晨 03:00 强制一次。

---

## §K PermissionScope

### Visibility Enum + Caller Scope

```rust
pub enum KbVisibility {
    Global,
    Agent   { agent_id: String },
    Channel { channel_id: String },
    Private,
}

pub struct CallerScope {
    pub agent_id: Option<String>,
    pub channel_id: Option<String>,
    pub user_id: Option<String>,
}
```

### 检索过滤规则

`kb_search` / `kb_fetch` / `kb_list_docs` / `kb_similar` 都接 `caller_scope` 参数（agent runtime 自动注入，agent 不能伪造），按下表过滤 doc：

| doc.visibility | 通过条件 |
|---|---|
| `Global` | 永远通过 |
| `Agent { id }` | `caller_scope.agent_id == Some(id)` |
| `Channel { id }` | `caller_scope.channel_id == Some(id)` |
| `Private` | `caller_scope.user_id == Some(doc.owner_user_id)` |

过滤在召回前做（filter 阶段，与 status/tags 同层），不命中的 chunk 直接 skip。

### Default 表（再列一次）

| source_kind | 默认 visibility |
|---|---|
| Doc (manual) | Global |
| Url | Global |
| Img (v2) | Global |
| Mail (v2) | **Private** |
| Chat (v2) | **Channel { id: 来源 channel }** |

用户 UI 可改任意 doc 的 visibility（v1 CLI: `rsclaw kb visibility <doc_id> <Global|Private|...>`；UI 由 v2 实现）。

### 多 agent 跨问题

若 agent A 通过 sub-agent / @ 提及 / collaboration 调用 agent B 的工具，`caller_scope` 透传 A 的 scope，不是 B 的。这避免越权。

A 的 caller_scope 不含 B 私有内容；如 A 需要看 B 的，必须由用户手动 promote doc visibility。

---

## §L Index Rebuild Contract

### 哪些是 source of truth，哪些是缓存

| 数据 | source of truth | 缓存层 |
|---|---|---|
| KbDoc / KbChunk metadata | redb | — |
| Chunk vector | redb (`KbChunk.vector` 字段) | hnsw_rs in-process |
| Chunk text (for FTS) | `md/*.md` 文件 + redb chunk byte_offset | tantivy in-process index |
| Chunk text (for serve) | `md/*.md` 文件 | — (lazy read) |
| Entity index | redb (`kb_entity_index` 表) | — (查询时直接走 redb) |

**HNSW 和 tantivy 都是可重建缓存**。损坏 / 丢失 → 重建。

### 进程内 ArcSwap 切换

```rust
pub struct HnswCache {
    active: ArcSwap<Hnsw<f32, DistCosine>>,
}

impl HnswCache {
    pub fn search(&self, query: &[f32], k: usize) -> Vec<(usize, f32)> {
        let h = self.active.load();
        h.search(query, k, 64)
    }

    /// 重建：在后台构造一份新 hnsw，原子 swap 替换 active。
    pub async fn rebuild(&self, store: &KbStore) -> Result<()> {
        let new_hnsw = build_hnsw_from_redb(store).await?;
        self.active.store(Arc::new(new_hnsw));
        Ok(())
    }

    /// 增量写：直接写 active（hnsw_rs add 是 thread-safe），无需 swap。
    /// 重建期间的增量写需要双写，详见下面"双写期间"。
    pub fn insert(&self, id: usize, vector: &[f32]) -> Result<()> {
        let h = self.active.load();
        h.insert((vector, id));
        Ok(())
    }
}
```

### 启动恢复

```
进程启动
  │
  ▼
读 hnsw/kb_v1024_<embedder>.snap （如果存在）
  │
  ├── 成功 → 加载到 ArcSwap
  │
  └── 失败 / 不存在 → from_redb 重建
  │
  ▼
继续启动其他模块
```

Snapshot 是**性能加速**，不是正确性来源。损坏 / 丢失只影响启动时间，不丢数据。

### 重建期间的双写

罕见场景：admin 触发"重建 HNSW"且同时有 ingest 进来。

```rust
pub struct HnswCache {
    active: ArcSwap<Hnsw>,
    rebuild_active: AtomicBool,           // 是否正在重建
    pending_writes: Mutex<Vec<(usize, Vec<f32>)>>,
}

// 重建期间，insert 不仅写 active（老 index）也 push 到 pending_writes。
// 重建完成后，把 pending_writes 应用到 new_hnsw，再 swap。
```

### Snapshot 周期

- 默认每 1h dump 一次 snapshot 到 `hnsw/*.snap.next`，原子改名替换 `.snap`
- snapshot 失败不影响运行时（缓存层）
- snapshot 文件可手动删除，下次启动会从 redb 重建

### Tantivy 同理

tantivy 也是缓存。`idx/` 目录可删除，启动时检测到缺失 → 从 redb chunks 重建（遍历 chunks → add_document → commit）。重建时间随 chunk 数线性增长（百万级 chunk ~ 几分钟）。

---

## §2 Canonicalize-first Ingestion

```
syncer 拉到 raw bytes
        │
        ▼
canonicalize/<kind>.rs ── CanonicalizedSource { markdown, metadata }
        │
        ▼
ingest pipeline（§J 流程）：
        │
        ├─ stage_doc → md/ + raw/ 落盘
        │
        ├─ 单 redb tx: KbDoc + IngestLedger + Job + seen_items
        │
        └─ 返回 doc_id
        │
        ▼ (异步 worker)
chunker → embedder → tantivy add → hnsw insert → ledger IndexingComplete
        │
        ▼ (compactor 后台)
旧文件清理 → ledger Done
```

### Canonicalizer (v1: Doc + Url 两个)

```rust
pub struct CanonicalizedSource {
    pub markdown: String,
    pub metadata: CanonicalMetadata,
}

pub struct CanonicalMetadata {
    pub source_kind: KbSourceKind,
    pub logical_source_id: String,       // 由 canonicalizer 计算
    pub title: String,
    pub mime: String,
    pub created_at_ms: i64,
    pub tags: Vec<String>,
    pub extra: serde_json::Value,
}

pub trait Canonicalizer: Send + Sync {
    fn source_kind(&self) -> KbSourceKind;
    fn supports_mime(&self, mime: &str) -> bool;
    fn canonicalize(&self, input: CanonicalizeInput<'_>) -> Result<Option<CanonicalizedSource>>;
}
```

**v1 实现：**

- `DocCanonicalizer` 内部按 mime 分派：
  - `text/markdown` → `md.rs`：passthrough + heading_path 抽取
  - `text/plain` → `text.rs`：passthrough
  - `text/html` → `html.rs`：lol-html 剥脚本 → markdown
  - `application/pdf` → `pdf.rs`：pdf-extract 抽文本层；扫描页（密度阈值 <5%）跳过（v2 OCR 接入）
- `UrlCanonicalizer`：拉网页 → `html.rs` 流程；logical_source_id 用 canonicalize_url

**v2 实现**：image、chat、mail。

### Chunker

- 目标 chunk size **~512 token**，overlap **~64 token**（BGE-M3 tokenizer）
- 优先尊重 semantic_unit 边界（段落 / 标题 / 对话）
- 超 target 按 sentence 切（CJK 标点感知：`。！？；`）
- 太小（<50 token）相邻 chunk 合并
- 强制 `heading_path` 前缀注入 `indexed_text`
- 每 chunk 算 SimHash-64，入库前查 hamming ≤ 3 → 跳过（不写 chunk，记录引用关系）
- chunk body 不落 DB，只存 `byte_offset` 指向 markdown 文件

### Embedder

```rust
pub trait KbEmbedder: Send + Sync {
    fn embed_batch(&self, texts: &[String]) -> Result<Vec<Vec<f32>>>;
    fn dimension(&self) -> usize;
    fn embedder_id(&self) -> &str;
}
```

v1: `LocalBgeM3Embedder`（onnxruntime + BGE-M3，1024 维）。Batch local=16。v2: `RemoteApiEmbedder`（走 ProviderRegistry，batch=64）。

### Entity Extraction

v1: `CompositeExtractor = RegexEntityExtractor + JiebaEntityExtractor`：

- Regex：email / URL / `@handle` / `#hashtag`
- Jieba：中文分词 + 大写词 / 专有名词 boost
- Resolver：大小写规范化 / `@`/`#` 去前缀 / 变体合并（"伊利" / "Yili" / "伊利股份" → 同 canonical_id）
- 写入 `kb_entities` (upsert) + `kb_entity_index`（每 chunk 多行）

v2: `LlmEntityExtractor`（NER + 重要性评分）。

### Writer（按 §J 流程）

```rust
pub async fn ingest_canonicalized(
    store: &KbStore,
    paths: &KbPaths,
    canon: CanonicalizedSource,
    raw_bytes: Option<&[u8]>,
    raw_ext: Option<&str>,
    visibility: KbVisibility,
) -> Result<KbDocId> {
    // 1. NOOP short-circuit: 若 logical_source_id 已有 latest 且 raw_sha256 一致
    let raw_sha = raw_bytes
        .map(sha256_hex)
        .unwrap_or_else(|| sha256_hex(canon.markdown.as_bytes()));
    if let Some(existing) = store.find_doc_by_logical_and_hash(
        &canon.metadata.logical_source_id, &raw_sha,
    )? {
        log::info!("[kb] ingest noop: {} {}",
            redact(&canon.metadata.logical_source_id), existing);
        return Ok(existing);
    }

    // 2. Stage markdown + raw 文件
    let doc_id = ulid::Ulid::new().to_string();
    let staged = content_store::stage_doc(paths, /* ... */)?;

    // 3. 准备 KbDoc / Ledger / Job
    let next_version = store.next_version_for(&canon.metadata.logical_source_id)?;
    let old_paths = store.paths_for_doc(
        store.latest_doc_for(&canon.metadata.logical_source_id)?
    )?;
    let doc = KbDoc { /* ... version: next_version, visibility, ... */ };
    let ledger = IngestLedgerEntry {
        op: if next_version == 1 { LedgerOp::Create } else { LedgerOp::Update },
        new_paths: vec![staged.markdown_rel_path.clone(),
                        staged.raw_rel_path.clone().unwrap_or_default()],
        old_paths,
        status: LedgerStatus::Pending,
        /* ... */
    };
    let job = Job::new_chunk_and_embed(&doc_id);

    // 4. 单 redb tx 原子写所有内容
    let wtx = store.begin_write()?;
    {
        wtx.put_doc(&doc)?;
        wtx.set_latest_version(&doc.logical_source_id, &doc.id, doc.version)?;
        wtx.put_ledger(&ledger)?;
        wtx.enqueue_job(&job)?;   // 处理 dedupe_active + status_priority 两表
        wtx.mark_seen(&doc.logical_source_id, &raw_sha)?;
    }
    wtx.commit()?;

    Ok(doc_id)
}
```

---

## §3 Retrieval (v1 范围)

### Tool Surface

```jsonc
// kb_search
{
  "query": "string",
  "k": 8,
  "filter": {
    "tags": ["string"],
    "source_kind": "doc|url",     // v1 仅这两个
    "doc_ids": ["string"],
    "entity_ids": ["string"]
  },
  "mode": "auto|dense|bm25|hybrid",   // 默认 hybrid
  "diversity": "off|mmr",             // 默认 mmr
  "mmr_lambda": 0.5,
  "require_entities": ["string"],
  "boost_entities":  ["string"]
  // caller_scope 由 agent runtime 注入，agent 不能传
}

// 返回
{
  "results": [
    {
      "chunk_id": "...",
      "doc_id": "...",
      "doc_title": "蒙牛奶粉冲泡指南.pdf",
      "text": "...",                 // 按需 read_doc_range
      "heading_path": [...],
      "score": 0.83,
      "citation": {
        "source": "file:///...",
        "locator_human": "p.12 §建议比例",
        "locator_machine": { /* KbLocator */ }
      },
      "entities": ["ent_mengniu"]
    }
  ],
  "entity_alignment": [
    { "entity_surface": "伊利", "canonical_id": "ent_yili", "matched_chunks": 0, "total": 5 }
  ],
  "warnings": [
    "query 含关键词 [伊利]，召回 chunks 中 0/5 包含此词，可能存在实体不匹配"
  ]
}

// kb_fetch
{ "chunk_id": "...", "expand": "none|neighbor|full_doc" }

// kb_list_docs
{ "tags": [...], "source_kind": "...", "limit": 50, "cursor": "..." }

// kb_similar
{
  "chunk_id": "...", "k": 8,
  "scope": "any|same_doc|other_docs",
  "min_score": 0.7, "exclude_neighbors": true
}

// kb_search_entities
{ "query": "伊利", "kind": "Brand|...|any", "limit": 20 }
```

### Pipeline

```
query
  │
  ├──▶ dense:  BGE-M3 embed → hnsw cache.search(k*3)
  │
  ├──▶ sparse: tantivy BM25(k*3)
  │
  └──▶ [filter:
          visibility (caller_scope)
          status = Active
          doc_version = latest_version[logical_source_id]
          tags / source_kind / doc_ids / entity_ids
          require_entities]
              │
              ▼
       RRF fusion (k=60)
              │
              ▼
       boost_entities apply
              │
              ▼
       MMR diversity (λ=0.5)
              │
              ▼
       entity_alignment 计算（查倒排索引）
              │
              ▼
       lazy read body via content_store.read_doc_range
              │
              ▼
       top-k 截断 → 返回
```

### V2 留作（不在 MVP）

- **kb_explain 工具**：返回检索 trace。能解释的：BM25 命中 term + tf-idf / Dense rank + cosine score（**不解释维度激活**）/ RRF 各路 rank 贡献 / entity hit/miss / MMR 选/弃理由 / citation_confidence 因子分解
- **citation_confidence 字段** + 三档 `citation_tier`（authoritative / supporting / indicative）
- **recency_policy** per doc：Evergreen（默认 Doc / Mail / Img 不衰减）/ Versioned（Versioned 文档老版本 ×0.5）/ TimeSensitive（exp(-days/N) for Chat / 新闻 URL）
- **Reranker**（BGE-Reranker-v2-m3 或 Cohere Rerank API）

### Citation 格式

- `locator_human`：Rust 端格式化（"p.12 §建议比例"）
- `locator_machine`：enum 序列化，UI 跳源用
- agent 不能自己拼 locator（防幻觉）

### RAG 引用纪律 Prompt

加进 `src/agent/prompt_builder.rs`：

```
使用 kb_search 时：
- 返回的 chunk 是语义相关而非精确匹配
- 引用前必须验证 chunk 中的实体 / 品牌 / 数值与用户问题一致
- 若 entity_alignment 显示某关键词 matched_chunks=0，必须明确告知用户
  「知识库未找到 X 的相关数据」，不得套用其他实体的数据
- 引用必须用 [^kb:<chunk_id>] 标记，由 UI 渲染为可点击引用
- 你看不到的 doc（visibility 限制）不会在结果里出现，不要假设"应该有"
```

### KV cache 友好

chunk 排序严格 `(score desc, chunk_id asc)`，不带 timestamp / uuid / request_id / trace_id（trace_id 是 v2 kb_explain 引入的，MVP 不带）。

---

## §S 数据源同步（KbSourceSyncer 框架）

### V1 仅 ManualUploadSyncer + UrlSyncer

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
}

pub enum SyncReason { Periodic, Event(EventTrigger), Manual, OnEnable }
pub struct SyncOutcome {
    pub docs_added: usize,
    pub docs_updated: usize,
    pub docs_skipped: usize,
    pub partial: bool,
}
pub enum SyncError {
    AuthFailed(String), RateLimited { retry_after_secs: u64 },
    BudgetExhausted, Network(String), Parse(String),
    Permanent(String), Cancelled,
}
```

### SyncState (持久化于 kb.redb)

```rust
pub struct SyncState {
    pub source_kind: KbSourceKind,
    pub source_id: String,
    pub cursor: Option<String>,
    pub last_seen_id: Option<String>,
    pub daily_budget: DailyBudget,
    pub status: SyncStatus,
    pub last_sync_at: Option<i64>,
    pub last_success_at: Option<i64>,
    pub last_error: Option<SyncErrorRecord>,
    pub consecutive_failures: u32,
    pub paused_until: Option<i64>,
    /* stats */
}
```

**注意：** SyncState **不含 seen_index 字段**。`seen_items` 是独立 redb 表（`(source_id, item_id) → SeenRecord`），按需 lookup。

### Dedup 两层（不是三层）

| 层 | 机制 |
|---|---|
| 1. SyncState + 持久 seen_items 表 | redb B-tree lookup `seen_items[source_id, item_id]`；命中 = skip |
| 2. Chunk-level deterministic id | logical_source_id 内容稳定 → chunk_id 稳定 → upsert no-op |

第 1 层是**精确表**，不再用 Bloom。redb 百万级 lookup 几十 μs，CPU/IO 都不是瓶颈。

### Scheduler 集成

复用 `src/cron/`，每 5min tick：

```rust
async fn tick_all_syncers(store: Arc<KbStore>) -> Result<()> {
    for entry in store.list_active_syncers().await? {
        let store = store.clone();
        tokio::spawn(async move {
            if let Err(e) = maybe_run_one(store, entry.clone()).await {
                log::warn!("[kb_sync] {} errored: {}",
                    redact(&entry.source_id), e);
            }
        });
    }
    Ok(())
}
```

Scheduler **永不 panic**。单 syncer 失败不影响其他。

### V1 syncer impl

**ManualUploadSyncer**：`sync_interval_secs() = None`（不进 scheduler）。CLI / 未来 UI 调 `ingest_canonicalized` 直接走 §J 流程。存在意义：统一所有 source 的 SyncState / 健康监控视图。

**UrlSyncer**：HEAD 拿 ETag/Last-Modified → 若未变直接返回 docs_skipped+=1；GET 拉 body → content_hash 兜底 → canonicalize → §J ingest。cursor = `etag:xxx` / `lastmod:xxx` / `contenthash:xxx`。

### V2 留作

- **LocalFolderSyncer**：notify watcher + 周期扫描；删除检测（孤儿 tombstone）
- **ChannelHistorySyncer**：需要 `HistoryProvider` capability trait 落地。**Trait 定义可以在 v1 写**（参考下面），**impl 留 v2**，首批接 Feishu 一个 channel
- **EmailSyncer / MailSyncer**：IMAP / Gmail API（独立 OAuth 复杂度）

### HistoryProvider Trait（v1 仅定义，无 impl）

```rust
#[async_trait]
pub trait HistoryProvider: Send + Sync {
    /// 该 provider 支持的 channel kind（feishu / wechat / discord / ...）
    fn channel_kind(&self) -> &str;

    /// 拉取消息历史。direction = Backward(until) 用于 backfill；
    /// Forward(since_ts) 用于增量。返回按时间升序排列的消息。
    async fn fetch_messages(
        &self,
        channel_id: &str,
        direction: FetchDirection,
        page_size: usize,
    ) -> Result<Vec<HistoryMessage>>;
}
```

需要 Channel adapter 各自 impl。v1 不接，PR 由后续 channel maintainer 单独贡献。

### 失败 / 退避

| 失败次数 | paused_until 增量 |
|---|---|
| 1 | 0 |
| 2 | 1 min |
| 3 | 5 min |
| 6 | 1 h |
| 12 | 6 h |
| >12 | 24 h（封顶） |

成功一次 → 重置。

---

## §4 Citation 渲染（v1 范围）

### Agent 输出格式

```
根据《蒙牛奶粉冲泡指南》[^kb:01HXY...]，建议比例是 100g 兑 100ml 温水。
```

### 渲染管线

```
agent stream → message store
        │
        ▼
markdown renderer (现有 NextChat-derived)
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

### V1 vs V2

- **V1**：CLI 输出做 plain text rendering（`rsclaw kb search` 在终端把 `[^kb:...]` 替换为 `[1]` + 末尾列引用）。Tauri UI 全套渲染移 v2
- **V2**：Tauri 控制台 `<KbCitation>` 组件 + 悬浮卡 + 跳源 + 「参考资料」折叠区

---

## §5 管理 UI + CLI

### V1: CLI only

```bash
rsclaw kb add <path|url> [--tags=...] [--visibility=global|private]
rsclaw kb ls [--tag=...] [--source-kind=doc|url] [--limit=N]
rsclaw kb rm <doc_id|--tag=...> [--yes]
rsclaw kb search <query> [-k 8] [--filter='{...}']
rsclaw kb show <doc_id|chunk_id>
rsclaw kb visibility <doc_id> <global|agent:<id>|channel:<id>|private>
rsclaw kb compact                # 手动触发 compactor
rsclaw kb stats                  # 文档数 / chunk 数 / 磁盘 / ledger 状态
rsclaw kb export <doc_id> --to <path>
```

CLI 子命令 `src/cmd/kb_*.rs`。

### V2: Tauri 控制台

- 知识库面板（文档 tab）
- 拖拽上传 + 任务进度
- 数据源 tab（v2 syncer 落地后）
- 设置 tab：visibility 默认、recency_policy 默认、keep_raw 开关
- Citation 渲染全套

---

## §6 Lifecycle / Compactor / Config / Security

### Lifecycle 状态机

```
[Ingest]
   │
   ▼
[Pending Ledger] → [worker chunks + embeds] → [IndexingComplete]
   │                                              │
   │                                              ▼
   │                                       [Active in retrieval]
   │                                              │
   │                                              │ (user tombstone or replaced by new version)
   │                                              ▼
   │                                       [Tombstoned]
   │                                              │
   │                                              ▼
   │                                       (Compactor 30 天后物理删)
   │
   ▼
（崩溃恢复见 §J 矩阵）
```

### Compactor

并行 3 件事：

1. 孤儿文件清理（grace 1h）
2. Ledger 状态推进 + 老 path 物理删
3. HNSW snapshot dump（每 1h）+ tantivy segment merge

跑频：1h tick + 03:00 强制。

### Config (`defaults.toml`)

```toml
[kb]
enabled = true
root_dir = "~/.rsclaw/kb"
default_tags = []
keep_raw = true
default_visibility = "global"     # doc/url 默认

[kb.embedding]
backend = "local-bgem3"
model_path = "~/.rsclaw/models/bge-m3"
dimension = 1024
batch_size_local = 16
batch_size_remote = 64
remote_provider = ""               # v2

[kb.ocr]
default_tier = "fast"              # v1 仅 fast
fast_engine = "rapidocr-onnx"      # v2: strong/fleet

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

[kb.entity]
extractors = ["regex", "jieba"]    # v2 加 "llm"
resolver_merge_variants = true

[kb.lifecycle]
tombstone_retention_days = 30
ledger_cleanup_pending_retention_days = 30
orphan_file_grace_secs = 3600
compactor_schedule = "03:00"
compactor_interval_secs = 3600

[kb.permissions]
chat_history_self_only = true
agent_cross_query_allowed = false  # 强约束：agent A 不能查 agent B 私有

[kb.security]
allow_remote_embedding = false
log_redaction = true
```

所有项 hot-reload。

### Security / 隐私

- 本地默认全栈；remote 开关弹确认
- PII redaction 强制
- visibility 不可被 agent 伪造（caller_scope 由 runtime 注入）
- v1 不加密 kb 目录；v2 考虑 AGE 加密 raw/ 目录

---

## §7 实施分期

### MVP (4 周)

| 周 | 内容 |
|---|---|
| 1 | model + paths + content_store + redb schema + 6 表初始化 + canonicalize (text/md/html + 文本层 PDF) + chunker（deterministic id + heading_path + simhash） |
| 2 | IngestLedger 模块 + Outbox/jobs queue (4 表) + LocalBgeM3 embedder + 单 worker 完整 chunk+embed pipeline + crash recovery 测试 |
| 3 | tantivy schema + HNSW cache (ArcSwap) + Hybrid+RRF + MMR + kb_search/fetch/list_docs/similar/search_entities tools + visibility filter + entity_alignment + RAG 引用纪律 prompt |
| 4 | UrlSyncer + ManualUploadSyncer + CLI 全套 + 整 compactor + e2e 测试 + 文档 + v1 发布 |

**MVP 总工期：~4 周（单人全职）**

### V2 Roadmap (按优先级)

| 项 | 描述 | 预估 |
|---|---|---|
| Tauri UI | 知识库面板 + 拖拽 + citation 渲染 | 2 周 |
| kb_explain | trace 收集 + BM25/RRF/MMR/entity 解释（**无 dense dim**） | 1 周 |
| citation_confidence + recency_policy | 三档 tier + Evergreen/Versioned/TimeSensitive | 1 周 |
| OCR Fast 接入 | RapidOCR ONNX | 1 周 |
| OCR Strong | PaddleOCR-VL 1.5 | 1 周 |
| OCR Fleet | rsclaw-server :8444 vLLM Qianfan-OCR | 1.5 周 |
| Image source | OCR-driven canonicalize | 含在 OCR phase |
| LocalFolderSyncer | notify + 周期扫 + 删除检测 | 1 周 |
| HistoryProvider trait + Feishu impl | trait 落地 + 首个 channel | 1.5 周 |
| ChannelHistorySyncer | 走 HistoryProvider | 0.5 周 |
| Mail source + .eml 上传 | MailCanonicalizer | 0.5 周 |
| MailSyncer (IMAP / Gmail) | OAuth + incremental sync | 2 周 |
| Fleet batch ingest | jobs/fleet_dispatch + rsclaw-server endpoints | 1.5 周 |
| Memory ↔ KB bridge | promotion + warm_session | 1 周 |
| Reranker | BGE-Reranker-v2-m3 | 0.5 周 |
| Time-travel queries | `kb_search --as_of=<ts>` | 0.5 周 |
| Embedding 模型迁移 | 双写 + 渐进重建 + 回滚窗口 | 1 周 |
| AGE 加密 | raw/ 加密 | 0.5 周 |
| 整站爬取 | sitemap.xml + per-page state | 1 周 |
| LLM EntityExtractor | NER + 重要性 | 1 周 |
| Summary tree | 三层 (source/topic/global) | 3 周 |
| Drill-down 工具 | 配合 summary tree | 0.5 周 |
| 多用户 / per-agent 库 | 重型重构 | 待定 |

---

## Open Questions（等实施 / review 反馈再敲定）

以下问题在 MVP 实施前需要团队对齐：

**A. Jobs queue：redb 显式索引表 ✅ 已确认**
- 不引入 SQLite
- 4 表设计（jobs_by_id / jobs_by_dedupe_active / jobs_by_status_priority / job_claims）已 spec'd
- 待验证：高并发 worker 抢任务的实测延迟（目标 < 5ms p99）

**B. logical_source_id schema 边界**
- URL canonicalize：除了 utm/fbclid/gclid 还要剥哪些 tracker？参考 [tracking-query-params-registry](https://github.com/mpchadwick/tracking-query-params-registry) 的列表
- Chat bucket window：5 分钟 idle 切？还是固定 6 小时？两种 trade-off：5min idle 自然但同一会话可能切太碎；6h 固定保会话完整但跨会话不分
- 邮件没 Message-ID 怎么办（极少见）：fallback 到 `mail:<sha256(headers+body)>` ？

**C. recency_policy 默认表（v2）**
- 按 source_kind 默认 vs 按 tag 默认 vs 按用户全局策略？倾向 source_kind 默认 + 每 doc 可覆盖
- TimeSensitive 默认 half_life_days = 30 是否合理

**D. HistoryProvider 首发 channel**
- Feishu vs Slack vs Telegram vs Matrix？倾向 Feishu（rsclaw 主用户在用 + 文档好 + 自研 bot）

**E. PermissionScope 多 agent 跨问行为**
- Agent A 通过 sub-agent / @ B 时，A 的 caller_scope 透传 → 若 A 看不见 B 的私有 doc，应该：
  - (i) 静默 mask（B 调 kb_search 看不到任何 Private 结果）
  - (ii) 显式拒绝（kb_search 返回错误，告知"需要更高权限"）
  - 倾向 (i)（静默 mask 是 safe default；显式拒绝可能泄露"存在但不可见"的信息）

**F. URL canonicalization 测试套件**
- 需要写一组 fixtures：实际看到的奇葩 URL → expected canonical form。建议 v1 MVP 至少覆盖：Google 搜索结果、知乎、GitHub、b 站、微博、wikipedia

**G. HNSW snapshot 周期 + 重启重建阈值**
- snapshot 每 1h vs 每 100 个新 chunk vs 两者较小者？
- 启动检测：snapshot 落后 redb 超过多少 chunks 就重建（不加载 snapshot）？建议 < 10% 差距 → 加载；否则重建

---

## 边界场景测试清单（实施验收）

- [ ] **「伊利问题」**：query 含库里没有的实体 → entity_alignment warning → agent 不张冠李戴
- [ ] **同名歧义**：「小米」（品牌 vs 粮食）→ entity resolver 区分 canonical_id
- [ ] **跨文档矛盾**：两份 doc 说法不一 → 召回两条，agent 应识别并告知
- [ ] **大文档**：1000 页 PDF 入库 → ledger 状态可见、任务断点续传、不阻塞 UI
- [ ] **大库查询**：百万 chunk → search 延迟 < 500ms
- [ ] **HNSW 重建**：删 `hnsw/*.snap` → 启动自动从 redb 重建 → 检索正常
- [ ] **tantivy 重建**：删 `idx/` → 启动自动从 redb 重建
- [ ] **崩溃恢复 - stage 后崩**：写完 md 文件，DB 没记录 → compactor 1h 后清理孤儿
- [ ] **崩溃恢复 - tx commit 后崩**：worker 没接到 → 进程重启 → worker 自动 claim
- [ ] **崩溃恢复 - worker 中途崩**：reclaim_stale_jobs → 重新跑（handler 幂等）
- [ ] **幂等性**：同一文件重复 `kb add` → NOOP（同 raw_sha256 + 同 logical_source_id → 直接返回现有 doc_id）
- [ ] **版本链**：修改文件重新 add → version+1 → latest_version 指向新 doc → 老 doc 仍可 fetch（30 天内）
- [ ] **回滚**：手动改 latest_version 指针指向老 doc → 检索立即看到老内容
- [ ] **URL 周期重抓未变化**：UrlSyncer 命中 ETag → docs_skipped+=1，无任何 DB 写
- [ ] **URL canonicalize 幂等**：`https://example.com/x?utm_source=a` 和 `https://example.com/x` → 同 logical_source_id
- [ ] **visibility - Global**：任意 agent 都能查到
- [ ] **visibility - Private**：仅 owner 能查到；其他 agent 调 kb_search 看不见
- [ ] **visibility - Channel**：仅来自该 channel 的对话能查到
- [ ] **多 agent 跨问**：A 调 B；B 用 A 的 caller_scope；A 看不见的 B 也看不见
- [ ] **PII redaction**：日志无明文 source_id / logical_source_id / 内容预览
- [ ] **自包含**：`cp -r ~/.rsclaw/kb/ /tmp/backup/`，改 root_dir → 完整可用
- [ ] **`keep_raw=false`**：raw/ 不写；KbDoc.raw_path = None；canonicalize 后立即丢弃 raw bytes（无法 re-canonicalize）

---

## References

### 算法 / 模式（公开发表）

- **RRF 融合**：Cormack et al., "Reciprocal Rank Fusion outperforms Condorcet and individual Rank Learning Methods" (SIGIR 2009)
- **MMR 多样性**：Carbonell & Goldstein, "The Use of MMR, Diversity-Based Reranking for Reordering Documents and Producing Summaries" (SIGIR 1998)
- **SimHash**：Charikar, "Similarity estimation techniques from rounding algorithms" (STOC 2002)
- **BM25**：Robertson & Walker, "Some Simple Effective Approximations to the 2-Poisson Model for Probabilistic Weighted Retrieval" (SIGIR 1994)
- **HNSW**：Malkov & Yashunin, "Efficient and robust approximate nearest neighbor search using Hierarchical Navigable Small World graphs" (2016)
- **BGE-M3**：BAAI, "BGE M3-Embedding: Multi-Lingual, Multi-Functionality, Multi-Granularity Text Embeddings Through Self-Knowledge Distillation"
- **Outbox pattern**：通用 production 模式（参考 Microsoft Patterns & Practices / Chris Richardson "Microservices Patterns" 第 3 章）
- **Job queue dedupe + claim_token**：通用 production 模式，参考 Sidekiq / RQ / Faktory / GoodJob

### 工具 / 模型（permissive license）

- **RapidOCR** (Apache 2.0) — PP-OCRv4 蒸馏 ONNX
- **PaddleOCR-VL 1.5** (Apache 2.0) — OmniDocBench v1.5 SOTA pipeline (v2)
- **Qianfan-OCR 4B** (Apache 2.0) — end-to-end SOTA + KIE (v2)
- **jieba-rs** (MIT) — 中文分词
- **ort** (Apache 2.0 / MIT) — ONNX Runtime Rust binding
- **tantivy** (MIT) — FTS
- **hnsw_rs** (Apache 2.0) — HNSW
- **redb** (Apache 2.0 / MIT) — embedded KV
- **arc-swap** (Apache 2.0 / MIT) — ArcSwap for hot-swap cache
- **url** crate (Apache 2.0 / MIT) — URL parsing for canonicalization

### rsclaw 内部依赖

- `src/agent/memory.rs` — lifecycle 区别参照
- `src/store/` — redb + tantivy + hnsw_rs 基础设施
- `src/cron/` — syncer scheduler 集成点
- `src/channel/` — HistoryProvider trait 适配点（v2）
- `src/browser/` — UrlSyncer 渲染（如需 JS 执行）
- `src/agent/prompt_builder.rs` — RAG 引用纪律 prompt 注入点
- `project_rsclaw_llm_rollout.md`（auto-memory）— Fleet 部署上下文
- `project_context_mgmt_v2.md`（auto-memory）— KV cache 优化路线

### 设计灵感

- Notion AI / Perplexity — citation 渲染 UX
- Obsidian — `.md` 文件本地优先的 PKM 模型
- Anthropic Claude Projects / OpenAI Custom GPTs — 用户主动 curate 知识库的产品形态
