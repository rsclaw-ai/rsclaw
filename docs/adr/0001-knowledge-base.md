# ADR 0001: Knowledge Base — 用户级 RAG 知识库

- **Status**: Accepted
- **Date**: 2026-05-19
- **Spec**: [docs/specs/2026-05-19-knowledge-base.md](../specs/2026-05-19-knowledge-base.md)

## Context

rsclaw 当前没有用户主动管理的知识库。`src/agent/memory.rs` 提供的是 agent 自学/会衰减的长期记忆，不能满足以下需求：

- 用户喂入产品文档、PDF、URL、聊天历史，agent 在回答时引用
- 引用必须可溯源（点击跳转到原文）
- 内容不会被 agent 自然遗忘 / 衰减
- 多 agent 共享同一份知识

memory 系统的衰减、importance、tier transition 等机制对知识库场景是反模式（用户不希望昨天上传的合同今天被"忘了"）。

## Decision

新增 `src/kb/` 模块，复用 redb + tantivy + hnsw_rs 三件套但**独立 DB 文件 + 独立 lifecycle**。核心选择：

| 决策点 | 选择 |
|---|---|
| 用户边界 | 全局一个库（所有 agent 共享） |
| 文档源 v1 | 本地文件 + URL（单页）+ 聊天历史 + 图片 |
| Retrieval | Tool-call（`kb_search` / `kb_fetch` / `kb_list_docs` / `kb_similar`），不做 auto-RAG |
| 存储后端 | redb + tantivy + hnsw_rs（复用），独立 DB 文件 `~/.rsclaw/kb.redb` |
| Hybrid | Dense (BGE-M3 1024) + Sparse (BM25) + RRF 融合 |
| Chunking | 512/64 token，**强制 `heading_path` 前缀注入 indexed text** |
| Citation | agent 用 `[^kb:<chunk_id>]` 标记，前端 `<KbCitation>` 渲染 + 点击跳源 |
| Embedding | BGE-M3 本地默认，远程 API 备路 |
| OCR | 三层路由：RapidOCR (Fast) / PaddleOCR-VL 1.5 (Strong) / Qianfan-OCR 4B via rsclaw-llm fleet (Fleet) |
| 实体感知 | `entity_alignment` 返回字段 + `require_entities` / `boost_entities` 参数 + RAG 引用纪律 prompt |
| 多样性 | MMR 默认开 (λ=0.5) |
| 去重 | Doc 级 sha256 + chunk 级 SimHash |
| 删除 | Tombstone + filter + 后台 compactor，30 天恢复窗口 |
| 模型迁移 | 双写 + 渐进重建 + 7 天回滚 |
| 实施分期 | 5 phases / 9-10 周 |

## Consequences

### 正面
- **零新依赖**（除 `ort` for ONNX，OCR 阶段才引入），rsclaw 二进制体积可控
- **和 KV cache / context-mgmt v2 完全兼容**：tool-call 路径 + 确定性 chunk 排序
- **多 agent 共享天然成立**：全局库无需协调
- **可溯源 + 可点击跳源**：UI 体验贴近 Notion AI / Perplexity
- **Phase 1 MVP 2 周可交付**：先跑通文本 PDF/MD 入库 + 检索

### 负面
- **hnsw_rs 不支持单点删** → 引入 tombstone + 后台 rebuild 复杂度（双 buffer 原子切换缓解）
- **OCR Fleet 层引入 vLLM/SGLang sidecar** → 多一个 Python 服务要维护（复用数字人 pipeline 的 sidecar 模式）
- **9-10 周工期** → 不是小投入；建议按 phase 灰度交付

### 中性
- KB 体积可能 GB 级（百万 chunk × 1024 维），但独立 DB 文件隔离了对 hot KV 路径的影响
- BGE-M3 模型 ~2GB，首次启动需下载 / 用户手动放置

## Alternatives Considered

### A. Auto-RAG（每轮自动检索注入 system prompt）
**否决**：每轮 top-K 变 → system prompt 变 → KV cache 全废。与 rsclaw 刚做完的 context-mgmt v2 + KV 缓存优化路线冲突。Hybrid auto+tool 也不行，auto 那一层会持续污染 context。

### B. 独立向量库 (sqlite-vec / lancedb)
**否决**：多一套存储依赖；lancedb 二进制 +50MB；sqlite-vec 还年轻；和现有 memory 检索逻辑割裂。等真到千万级文档再考虑切换。

### C. 外接 qdrant / milvus
**否决**：违背 desktop-first；用户需额外部署 service；和 rsclaw 单机 / fleet 架构不符（KB 是端侧概念，不该跑去 GPU 机房）。

### D. OCR 选 Tesseract
**否决**：中文准确率显著低于 RapidOCR (PP-OCRv4 蒸馏)；ONNX 路径接入成本相当。

### E. Fleet OCR 走百度千帆云 API
**否决**：违背"chunk 文本不出本机"的隐私默认。改为 rsclaw-llm fleet 自部署 vLLM sidecar，复用自有 GPU 集群。

### F. KB spec 不入 git（沿用 `docs/superpowers/` ignored）
**否决**：9-10 周 / 多模块 / 可能多人接手的项目级 feature，spec 必须可被 PR / review / implementation 引用。spec 入 `docs/specs/`，ADR 入 `docs/adr/`，AI brainstorming 草稿继续放 `docs/superpowers/`（ignored）。

## References

- 完整设计：[docs/specs/2026-05-19-knowledge-base.md](../specs/2026-05-19-knowledge-base.md)
- 现有 memory 系统：`src/agent/memory.rs`
- 现有存储层：`src/store/` (redb + tantivy + hnsw_rs)
- rsclaw-llm fleet：`project_rsclaw_llm_rollout.md` (memory)
- KV cache 优化：`project_context_mgmt_v2.md` (memory)
