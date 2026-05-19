# Knowledge Base — Design Spec

## Overview

给 rsclaw 加**用户级知识库**：用户主动喂入 PDF / DOCX / Markdown / TXT / URL / 聊天历史 / 图片，agent 通过 `kb_search` / `kb_fetch` / `kb_list_docs` / `kb_similar` 工具检索使用，回答里带可点击的出处引用。

**和现有 `src/agent/memory.rs` 的区别**：memory 是 agent 自学/会衰减/会淘汰；KB 是用户喂入/不衰减/可溯源/有版本。共底层（redb + tantivy + hnsw_rs）不共 lifecycle。

**核心定位：**
- **全局一个库**，所有 agent 共享读写（不做 per-agent / per-user 隔离）
- **Tool-call retrieval**（agent 主动调），不做 auto-RAG
- **三层 OCR 路由**：RapidOCR (Fast) / PaddleOCR-VL 1.5 (Strong) / Qianfan-OCR 4B via rsclaw-llm fleet (Fleet)
- **Embedding**：BGE-M3 本地默认 + 远程 API 备路
- **独立 redb 文件**（`~/.rsclaw/kb.redb`），与 hot KV `store.redb` 完全平级
- **Citation 必带**：UI 渲染统一风格 + 点击跳源

## 设计决策

| 决策点 | 选择 | 原因 |
|---|---|---|
| 用户边界 | 全局一个库 | YAGNI；多租户 v2 再说 |
| Retrieval 方式 | Tool-call（agent 主动） | KV cache 友好；与 agent-loop 哲学一致 |
| 文档源 v1 | 本地文件 + URL（单页）+ 聊天历史 | 覆盖 80% 场景；代码 repo / 整站留 v2 |
| 存储后端 | 复用 redb + tantivy + hnsw_rs，独立 collection + 独立 DB 文件 | 零新依赖；和 KV cache/context-mgmt v2 兼容 |
| 删除机制 | Tombstone + 查询 filter + 后台 compactor | hnsw_rs 不支持 true delete 的标准解法 |
| Hybrid 检索 | Dense (BGE-M3) + Sparse (BM25) + **RRF 融合** | 不用调权重，对分数尺度不敏感 |
| Reranker | v1 不接，留 trait | 显著质量提升但单独算时间最长，留 hook |
| Citation | agent 用 `[^kb:<chunk_id>]` 标记，前端组件渲染 | 不让 agent 自己拼 URL，避免幻觉 |
| Locator 设计 | enum (PdfPage/MdSection/UrlAnchor/ChatMsgs/Image/Offset) | UI 能跳源到具体位置（PDF 翻页、MD 滚动、bbox 高亮） |
| Chunking | 默认 512/64，**强制带 `heading_path` 前缀** | 保护"主语 + 属性"完整性，防止 chunk 切散后实体歧义 |
| 实体感知 | `entity_alignment` 返回字段 + `require_entities`/`boost_entities` 参数 + RAG 引用纪律 prompt | 防"伊利问题"翻车（query 含实体但 chunk 不含） |
| 多样性 | MMR 默认开 (lambda=0.5) | RAG "5 个 chunk 说同一件事"是最常见失败模式 |
| 入库去重 | doc 级 sha256 + chunk 级 SimHash (hamming≤3) | 8 字节代价换大幅节省 + 关联溯源 |
| OCR 引擎选型 | Fast=RapidOCR (PP-OCRv4 ONNX) / Strong=PaddleOCR-VL 1.5 / Fleet=Qianfan-OCR 4B | RapidOCR 中文准确率显著高于 tesseract；PaddleOCR-VL 是 OmniDocBench SOTA pipeline；Qianfan 是 end-to-end SOTA + KIE + 图表理解 SOTA |
| OCR 路由 | 按文档特征自动路由（预扫描决定） | 资源等级 ≠ 任务类型；图表必须 Vision LLM (CharXiv 警告) |
| Fleet 部署 | rsclaw-server :8444 vLLM/SGLang sidecar | llama.cpp 不支持 InternVL vision encoder，必须 vLLM；不挂百度千帆云 API |
| Embedding 默认 | BGE-M3 本地 (1024 维)，远程 API 备路 | desktop-first；几千 chunk 走 API 太贵且要联网 |
| 模型迁移 | 双写 + 渐进重建 + 7 天回滚窗口 | 不能让旧 vector 立刻失效 |
| 配额 | search ≤8KB / fetch_full ≤32KB / ≤5次 search 每轮 | 防 context 爆 / search-spam |
| KV cache 友好 | chunk 严格按 (score, chunk_id) 字典序，不带 timestamp/uuid | 同一 query 命中同一组 chunks → tool result 完全一致 → cache hit |
| Lifecycle 隔离 | KB 不衰减，永久保留；删除 30 天恢复期 | 跟 MemoryDoc 区分开 |
| Compactor | 1 小时 tick + 凌晨 03:00 强制 + 残骸率 >15% 触发 | HNSW 双 buffer 重建，μs 级原子切换 |
| Security 默认 | 本地全栈，远程开关显式确认 | chunk 文本不出本机；用户启 remote 时弹一次确认 |
| 聊天历史隐私 | 默认只入用户自己消息 + @ 自己消息 | 不把他人发言入库；UI 可关 |

## 模块布局

```
src/kb/
  mod.rs           # KbStore facade: add / update / delete / search / list / similar
  model.rs         # KbDoc / KbChunk / KbSource / KbLocator / KbStatus
  ingest/
    mod.rs         # Ingester trait + dispatch
    file.rs        # 本地文件（按 mime 分派 parser）
    url.rs         # 单页 URL
    chat.rs        # channel 历史范围
  parser/
    mod.rs         # Parser trait + ParsedSection
    pdf.rs         # pdf-extract 抽文本层 + 扫描页检测 → 转 ocr/
    docx.rs        # docx-rs，按 paragraph + heading_path
    markdown.rs    # 按 # / ## 切，累计 heading_path
    html.rs        # lol-html 剥脚本 → markdown 转换
    text.rs        # 兜底
    image.rs       # PNG/JPG/HEIC → OCR
  ocr/
    mod.rs         # OcrEngine trait + 三层路由
    rapidocr.rs    # Tier Fast：PP-OCRv4 ONNX via `ort`
    paddleocr_vl.rs # Tier Strong：PaddleOCR-VL 1.5 via `ort`
    qianfan.rs     # Tier Fleet：HTTP 到 rsclaw-server :8444
    prescan.rs     # 预扫描特征检测（图表/公式/表格/手写）
  chunker.rs       # 默认 512/64 + heading_path 强制前缀 + SimHash
  embedder.rs      # KbEmbedder：LocalBgeM3 主 / RemoteApi 备
  retrieval/
    mod.rs         # kb_search / kb_fetch / kb_list_docs / kb_similar 实现
    hybrid.rs      # Dense + BM25 + RRF
    mmr.rs         # MMR 多样性
    entity.rs      # entity_alignment / require_entities / boost_entities
  compactor.rs     # 后台 tokio task
  migrator.rs      # embedding 模型迁移流程
  tasks.rs         # 入库任务队列（异步、断点续传）

src/cmd/
  kb_add.rs
  kb_ls.rs
  kb_rm.rs
  kb_search.rs
  kb_show.rs
  kb_reindex.rs
  kb_compact.rs
  kb_stats.rs
  kb_export.rs

src/agent/tools_kb.rs  # tool 注册 + JSON schema

ui/app/components/kb/  # 控制台「知识库」面板
  panel.tsx
  upload.tsx
  doc-list.tsx
  search-preview.tsx
  settings.tsx
  citation.tsx       # <KbCitation> 渲染组件
  citation-cache.ts  # frontend chunk meta 缓存
```

## §1 数据模型

```rust
// src/kb/model.rs

pub struct KbDoc {
    pub id: String,              // ulid
    pub source: KbSource,
    pub title: String,
    pub mime: String,
    pub hash: String,            // sha256(原始 bytes)，doc-level dedup
    pub created_at: i64,
    pub updated_at: i64,
    pub version: u32,            // 重新入库递增
    pub status: KbStatus,
    pub tags: Vec<String>,
    pub meta: serde_json::Value, // source-specific
}

pub enum KbSource {
    File  { path: PathBuf },
    Url   { url: String, fetched_at: i64 },
    Chat  { channel: String, range: (i64, i64) },
    Image { path: PathBuf },
}

pub enum KbStatus { Active, Tombstoned, Updating }

pub struct KbChunk {
    pub id: String,              // ulid
    pub doc_id: String,
    pub doc_version: u32,         // 必须等于 KbDoc.version 才参与召回
    pub seq: u32,
    pub text: String,             // chunk 原文
    pub heading_path: Vec<String>, // ["蒙牛奶粉冲泡指南", "建议比例"]
    pub indexed_text: String,     // heading_path.join(" > ") + "\n\n" + text，用于 embed/BM25
    pub vector: Vec<f32>,         // 1024（BGE-M3）
    pub simhash: u64,             // chunk-level near-dup
    pub locator: KbLocator,
    pub status: KbStatus,
    pub source_quality: f32,      // OCR confidence 或 1.0
    pub embedder_id: String,      // "bge-m3@v1" 等，用于模型迁移
}

pub enum KbLocator {
    PdfPage   { page: u32, bbox: Option<(f32,f32,f32,f32)> },
    MdSection { heading_path: Vec<String> },
    UrlAnchor { fragment: Option<String> },
    ChatMsgs  { first_ts: i64, last_ts: i64 },
    Image     { bbox: Option<(f32,f32,f32,f32)> },
    Offset    { start: usize, end: usize },
}
```

### 存储映射

```
~/.rsclaw/
  store.redb               # 现有：hot KV / 会话历史 / agent memory
  kb.redb                  # 新：KbDoc / KbChunk / kb_tasks
  kb_v1024_<model>.hnsw    # 新：KB 向量索引（按 embedder 命名）
  kb_index/                # 新：tantivy KB 索引目录
```

- `redb` 表：`kb_docs` (id → KbDoc)、`kb_chunks` (id → KbChunk)、`kb_tasks` (id → IngestTask)
- `tantivy` index：`kb_chunks`，BM25 over `indexed_text`，doc_id / tags / status 作 facet
- `hnsw_rs`：独立 `kb_v1024_<embedder_id>` 实例

## §2 Ingestion Pipeline

```
source ──▶ fetcher ──▶ parser ──▶ [ocr if needed] ──▶ chunker ──▶ embedder ──▶ writer
                                                                                  │
                                                ┌─────────────────────────────────┤
                                                ▼              ▼                  ▼
                                            redb            tantivy          hnsw_rs
```

### Fetcher
- `File`：直接读
- `Url`：复用 `src/browser/` 或 reqwest；HTML 走 browser 拿渲染后版本
- `Chat`：从 redb 拿 channel 范围消息，按"5 分钟无新消息"切对话块
- `Image`：直接读

### Parser

```rust
pub struct ParsedSection {
    pub text: String,
    pub locator: KbLocator,
    pub heading_path: Vec<String>,
    pub semantic_unit: bool,
}
```

- **PDF**：先 pdf-extract 抽文本层；每页文本密度 <0.05 字符/单位面积 → 判定扫描页 → 转 OCR
- **DOCX**：docx-rs 按 paragraph，累计 heading 路径
- **Markdown**：按 `#`/`##` 切，`heading_path: Vec<String>` 累计
- **HTML**：lol-html 剥脚本 → markdown 转换 → 按 heading 切
- **Chat**：每个 ParsedSection = 一个 "5 分钟会话块"
- **Image**：直接转 OCR

### OCR 三层路由

```rust
pub enum OcrTier { Fast, Strong, Fleet }

pub trait OcrEngine: Send + Sync {
    fn recognize(&self, image: &DynamicImage, langs: &[&str]) -> Result<OcrResult>;
    fn engine_id(&self) -> &str;  // "rapidocr@PP-OCRv4" / "paddleocr-vl@1.5" / "qianfan-ocr@4b"
}

pub struct OcrResult {
    pub text: String,
    pub lines: Vec<OcrLine>,      // 每行 text + bbox + confidence
    pub markdown: Option<String>,  // Strong/Fleet 可输出结构化 markdown
}
```

**路由策略（预扫描后决定）：**

| 检测到 | 路由 |
|---|---|
| 文本层 PDF | Skip OCR |
| 收据/发票/证书/病历/身份证 (KIE) | Fleet (Qianfan) |
| 图表密集 | Fleet (Qianfan) |
| 手写体 | Fleet (Qianfan) |
| 公式密集 (`∫∑∂√`) | Strong (PaddleOCR-VL) |
| 复杂表格 (合并/旋转) | Strong (PaddleOCR-VL) |
| 多栏排版 | Strong (PaddleOCR-VL) |
| 纯文本扫描 / 简单单栏 | Fast (RapidOCR) |
| 场景文本 | Fast → 质量不够升 Strong |
| 多语言（非中英） | Strong / Fleet |

预扫描跑 RapidOCR 抽 1-2 页特征（边/直线密度、文本块分布、特殊字符），10-30ms 一页。

**Fleet 部署**：rsclaw-server 加 `/v1/ocr/parse` endpoint，转发到 4090 节点上的 vLLM sidecar：

```bash
# scripts/deploy-ocr-sidecar.sh
vllm serve baidu/Qianfan-OCR --trust-remote-code --port 8444
```

### Chunker

- 目标 chunk size：**~512 token**，overlap **~64 token**（BGE-M3 tokenizer 数）
- **优先尊重 `semantic_unit` 边界**：原生段落/标题/对话整块保留
- 超过 target 按 sentence 边界切
- 太小（<50 token）相邻同 section chunk 合并
- **强制带 `heading_path` 前缀** 到 `indexed_text`（防"伊利问题"）
- 每个 chunk 算 SimHash，入库时查重（hamming ≤ 3 视为重复，不写但记录引用关系）

### Embedder

```rust
pub trait KbEmbedder: Send + Sync {
    fn embed_batch(&self, texts: &[String]) -> Result<Vec<Vec<f32>>>;
    fn dimension(&self) -> usize;
    fn embedder_id(&self) -> &str;
}

// 默认：LocalBgeM3 (1024 维)
// 备路：RemoteApiEmbedder (走 ProviderRegistry，可选 openai/qwen/doubao)
```

- 默认 local；local 不可用自动降级 remote（要弹一次确认）
- batch：local 16、remote 64
- `embedder_id` 落 `KbChunk.embedder_id`，模型变更触发迁移流程

### Writer（事务原子）

```rust
fn upsert(doc: KbDoc, chunks: Vec<KbChunk>) -> Result<()> {
    let mut wtx = redb.begin_write()?;
    if let Some(_old) = wtx.get_doc(&doc.id)? {
        wtx.tombstone_chunks_for(&doc.id)?;  // 软删老 chunks
    }
    wtx.put_doc(&doc)?;
    for c in &chunks { wtx.put_chunk(c)?; }
    wtx.commit()?;
    // 二阶段：tantivy + hnsw（失败由 redb 状态机重放）
    tantivy.add_chunks(&chunks)?;
    hnsw.add_chunks(&chunks)?;
    Ok(())
}
```

hnsw 写入失败不回滚 redb，启动时校验 hnsw vs redb 缺的补。

### Tasks queue

入库是异步任务，`kb_tasks` 表存进度：`Pending → Fetching → Parsing → Chunking → Embedding → Indexing → Done | Failed`。进程崩了重启 resume。Tauri UI 显示进度条。

## §3 Retrieval

### Tool Surface

```jsonc
// kb_search
{
  "query": "string",                              // 必填
  "k": 8,                                         // 默认 8，最多 20
  "filter": {
    "tags": ["string"],
    "source_type": "file|url|chat|image",
    "doc_ids": ["string"],
    "min_quality": 0.6
  },
  "mode": "auto|dense|bm25|hybrid",               // 默认 hybrid
  "diversity": "off|mmr",                         // 默认 mmr
  "mmr_lambda": 0.5,
  "require_entities": ["string"],                 // 硬约束：必须含此词
  "boost_entities":  ["string"]                   // 软约束：含此词 score ×1.5
}
// 返回
{
  "results": [
    {
      "chunk_id": "01HXY...",
      "doc_id": "01HXX...",
      "doc_title": "蒙牛奶粉冲泡指南.pdf",
      "text": "蒙牛奶粉建议100g兑100ml温水",
      "heading_path": ["蒙牛奶粉冲泡指南", "建议比例"],
      "score": 0.83,
      "citation": {
        "source": "file:///Users/x/docs/...",
        "locator_human": "p.12 §建议比例",
        "locator_machine": { /* KbLocator enum */ }
      },
      "quality": 0.95
    }
  ],
  "entity_alignment": [
    { "entity": "伊利", "matched_count": 0, "total": 5 }
  ],
  "warnings": [
    "query 含关键词 [伊利]，召回 chunks 中 0/5 包含此词，可能存在实体不匹配"
  ]
}

// kb_fetch
{ "chunk_id": "...", "expand": "none|neighbor|full_doc" }

// kb_list_docs
{ "tags": [...], "source_type": "...", "limit": 50, "cursor": "..." }

// kb_similar (chunk → chunk)
{
  "chunk_id": "01HXY...",
  "k": 8,
  "scope": "any|same_doc|other_docs",
  "min_score": 0.7,
  "exclude_neighbors": true
}
```

### Pipeline

```
query
  │
  ├──▶ dense: BGE-M3 embed → hnsw.search(k*3)
  │
  ├──▶ sparse: tantivy BM25(k*3)
  │
  └──▶ [filter: tags / source / doc_ids / status≠Tombstoned / quality / require_entities]
              │
              ▼
       RRF fusion (k=60)
              │
              ▼
       boost_entities apply (×1.5 if hit)
              │
              ▼
       MMR diversity (lambda=0.5)
              │
              ▼
       [optional rerank] —— v1 noop trait
              │
              ▼
       entity_alignment 计算 + warnings 生成
              │
              ▼
       top-k 截断 → 返回
```

### Filter 时机

- 硬过滤（status / tags / source / doc_ids / require_entities）：召回时直接 skip
- 软过滤（quality / boost_entities）：召回时打降权（×0.7）或升权（×1.5），不 skip

### Citation 格式

- `locator_human`：Rust 端格式化（"p.12 §建议比例"），给 agent 用
- `locator_machine`：enum 序列化，只回 UI 用
- agent 不能自己拼 locator，避免幻觉

### 配额限流

- `kb_search` 单次返回 chunks 总字数 ≤ 8KB
- `kb_fetch expand=full_doc` ≤ 32KB
- agent 单 turn 内 `kb_search` ≤ 5 次（超出在 tool result 里返回 "rate limit hit, refine your query"）

### KV cache 友好

chunk 排序严格 (score 降序, chunk_id 字典序)，不带时间戳 / uuid / request_id。

### Entity Alignment 实现

中文走 jieba 分词，英文按大小写/首字母规则提取候选实体。对 top-k 每个 chunk 算 `contains(entity)`。`matched_count == 0` 时生成 warning。

### RAG 引用纪律 Prompt

加进 `src/agent/prompt_builder.rs`：

```
使用 kb_search 时：
- 返回的 chunk 是语义相关而非精确匹配
- 引用前必须验证 chunk 中的实体/品牌/数值与用户问题一致
- 若 entity_alignment 显示某关键词 matched_count=0，必须明确告知用户「知识库未找到 X 的相关数据」，不得套用其他实体的数据
- 引用时必须用 [^kb:<chunk_id>] 标记，由 UI 渲染为可点击引用
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
            kb-citation plugin (新)：
            - 扫描 [^kb:<id>] 标记
            - 查 frontend kb-cache（无则调 kb_fetch）
            - 替换为 <KbCitation> 组件
                    │
                    ▼
            UI 显示：[1] 上标 + 悬浮卡片 + 点击跳源
```

### `<KbCitation>` 组件

- 内联上标 `[N]`（按出现顺序编号）
- 悬浮卡：doc title + locator + 50 字 snippet
- 点击行为按 `KbLocator` 类型分派：
  - `PdfPage` → 右侧打开 PDF.js viewer 跳 page + bbox 高亮
  - `MdSection` → 右侧打开 markdown 渲染，滚动到 heading
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
result.chunks.forEach(c => kbCache.set(c.chunk_id, {
  doc_title, heading_path, locator_human, locator_machine, source
}));
```

渲染时同步命中，零延迟。缓存随消息生命周期。

## §5 管理 UI + CLI

### Tauri 控制台「知识库」面板

新增主导航 `📚 知识库`：

- 左侧：文档列表（按 source / tag 过滤，文档数统计）
- 右侧：搜索预览 / 单文档详情
- 顶部：`+ 添加` / `⚙ 设置` / `🔄 重建`
- `+ 添加` 三 tab：文件（拖拽多选）/ URL（粘贴 + 周期重抓）/ 聊天（channel + 时间范围）
- `⚙ 设置`：默认 OCR engine / embedding backend / chunk size / tombstone 天数 / compactor 频率 / entity_alignment 开关
- `🔄 重建`：embedding 模型迁移触发，显示预估时间 + 后台进度

### 聊天窗口轻交互

- 拖拽文件入聊天 → 弹「加入知识库 / 仅本轮使用」
- agent 回答的 citation 悬浮卡有「打开知识库」按钮跳面板

### CLI

```bash
rsclaw kb add <path|url> [--tags=...] [--ocr=auto|fast|strong|fleet]
rsclaw kb add-chat <channel> --from <date> --to <date>
rsclaw kb ls [--tag=...] [--source=file|url|chat|image] [--limit=N]
rsclaw kb rm <doc_id|--tag=...> [--yes]
rsclaw kb search <query> [-k 8] [--filter='{...}']
rsclaw kb show <doc_id|chunk_id>
rsclaw kb reindex [--doc=<id>] [--all]
rsclaw kb compact
rsclaw kb stats
rsclaw kb export <doc_id> --to <path>
```

CLI 子命令 `src/cmd/kb_*.rs`，跟现有 `gateway`/`provider` 子命令风格一致。`add` 默认前台进度 + Ctrl-C 转后台；`--detach` 直接后台。

### i18n

- UI 文案进 `ui/app/locales/`，10 语言（cn/en/ja/ko/th/vi/fr/de/es/ru），先写英中两版，其他 fallback 英文
- CLI 帮助走现有 `src/i18n.rs`

## §6 Lifecycle / Compactor / Config / Security

### Lifecycle 状态机

```
[Pending] → [Fetching → Parsing → Chunking → Embedding → Indexing] → [Active]
                                                                         │
                                                                ┌────────┴────────┐
                                                                ▼                 ▼
                                                         [Tombstoned]      [Updating]
                                                          (软删 30 天)      (新版本流程)
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
    self.purge_expired_tombstones().await?;           // redb + tantivy 物理删
    let ratio = self.tombstone_ratio_in_hnsw().await?;
    if ratio > self.tombstone_ratio_threshold {
        self.rebuild_hnsw().await?;                    // 双 buffer 重建
    }
    self.tantivy_compact().await?;                     // segment merge
    Ok(())
}
```

**HNSW 双 buffer 重建**：在 `kb_v1024.next` 建新 index → 原子改名 `kb_v1024` → 重建期间老 index 服务查询 → 新入库 chunk 同时写新老两个 index。

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
data_dir = "~/.rsclaw/kb"
default_tags = []

[kb.embedding]
backend = "local-bgem3"              # local-bgem3 | remote-api
model_path = "~/.rsclaw/models/bge-m3"
remote_provider = ""
dimension = 1024
batch_size_local = 16
batch_size_remote = 64

[kb.ocr]
default_tier = "auto"
fast_engine = "rapidocr-onnx"
strong_engine = "paddleocr-vl-1.5"
fleet_endpoint = ""                  # rsclaw-server :8444

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

[kb.lifecycle]
tombstone_retention_days = 30
compactor_schedule = "03:00"
tombstone_ratio_threshold = 0.15

[kb.security]
allow_remote_embedding = false
allow_fleet_ocr = true
chat_history_self_only = true
```

所有项 hot-reload。

### Security / 隐私

- 本地默认全栈，chunk 文本不出本机
- 远程开关显式弹确认（"你的文档片段将发送到 <provider>"）
- 聊天历史入库默认只入用户自己消息 + @ 自己消息，UI 可关
- v1 不加密 kb.redb（和现有 store.redb 策略一致），v2 考虑 AGE 加密

## §7 实施分期

| Phase | 内容 | 工期 |
|---|---|---|
| **1 MVP** | 数据模型 + 独立 redb + tantivy + hnsw_rs / Parser (md/text/html + 文本层 PDF) / Chunker (512/64 + heading_path) / LocalBgeM3 / Writer 事务 / Hybrid+RRF / kb_search/fetch/list_docs / CLI 基础 | 2 周 |
| **2 基础可用** | Tauri 控制台「知识库」面板 + 拖拽上传 + 任务进度 / Citation 渲染全套 / entity_alignment + require_entities + RAG 引用纪律 prompt / MMR 默认开 / 远程 embedding 备路 | 2 周 |
| **3 OCR 接入** | OcrEngine trait / Tier Fast = RapidOCR / 预扫描 + 路由 / OCR 任务异步队列 + 断点续传 / 扫描 PDF / 单图入库 | 2 周 |
| **4 Strong/Fleet 层** | Tier Strong = PaddleOCR-VL 1.5 / Tier Fleet = Qianfan-OCR 4B via rsclaw-server :8444 vLLM sidecar / 自动路由策略 / 部署脚本 | 2 周 |
| **5 URL/聊天/Compactor** | URL 源（单页）/ 聊天历史源 / Compactor 后台 / Embedding 迁移流程 | 1-2 周 |
| **总工期** | | **~9-10 周** |

**v2 留作：** Reranker (BGE-Reranker-v2-m3) / 整站爬取 / 引用图谱可视化 / AGE 加密 / 多用户 per-agent 库 / 聊天 `@kb:doc_id` 提及 / 浏览器右键发送

## Open Questions（v1 进入前需 review）

无 —— 设计阶段所有岔路均已与用户对齐。

## 关键边界场景测试清单（实施时验收用）

- [ ] 「伊利问题」：query 含库里没有的实体 → entity_alignment warning 出现 → agent 不张冠李戴
- [ ] 同名实体歧义：「小米」（品牌 vs 粮食）→ chunk heading_path 能正确区分
- [ ] 跨文档矛盾：两份 doc 说法不一 → 召回两条，agent 应识别并告知
- [ ] 时间敏感：旧版文档 vs 新版文档 → KbDoc.version + updated_at 排序
- [ ] 大文档：5000 页 PDF 入库 → 任务断点续传、进度可见、不阻塞 UI
- [ ] 大库查询：百万 chunk 库 → search 延迟 <500ms
- [ ] HNSW 重建：模拟 20% tombstone → compactor 触发 → 重建期间查询不中断
- [ ] Embedding 模型迁移：切换 backend → 双写 → 进度可见 → 7 天后老 index 自动清
- [ ] OCR 路由：扫描合同自动路由到 Fleet；纯文本扫描走 Fast
- [ ] 隐私：聊天历史入库时他人消息被过滤
