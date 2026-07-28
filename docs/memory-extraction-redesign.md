# Memory 抽取重设计

**状态:** 提案 / 待后端实现  
**范围:** `src/agent/runtime.rs`(auto-capture)、`src/agent/memory.rs`(write/tier/dedup)、`src/agent/compaction.rs`(已有抽取路径)、`src/agent/tools_misc.rs`/`runtime.rs`(memory search)、`ui/app/components/memory-page.tsx`(错误态)  
**主目标:** 修复 memory store 被 raw 对话污染的问题,并让未来 auto-recall 能安全使用干净记忆。  
**次目标:** UI 不能因为 gateway 未授权/接口错误崩溃;它应清楚显示连接或授权问题。

## 结论

这份重构方向**能解决核心污染问题**,但原版还不够完整。它已经正确指出 `runtime.rs:3302` 的 raw auto-capture 是主因;要真正修好,还必须补齐以下约束:

1. 长期 memory 只能写入抽取后的事实/偏好/实体,不能继续把普通 user text 原文直存为 `note`。
2. 显式 `/remember` / `memory.put(kind=remember)` 要保留直写能力,但要走去重、分类和 scope 统一。
3. 自动 recall 必须使用 ephemeral context / `user_system` 这类非持久通道,不能把 recall 结果塞回历史 messages。
4. 记忆页要处理 `401` / `{ "error": "unauthorized" }`,否则用户看不到问题本体,只会看到 React 崩溃页。
5. 现有脏数据需要一次性清理或降级,否则上线后 auto-recall 仍会召回历史污染条目。

---

## 问题

rsclaw-lead 的 memory store 里 85 条记忆,几乎全是聊天原话和注入的系统 prompt,不是"记忆"。
当前 auto-capture(`runtime.rs:3302`)做的不是抽取,是把每条用户消息原样落库成 `note`。

实测分布:

```text
total=85
kind:  note=85
tier:  working=85
scope: agent:main=81, agent:test-w=4
pinned=0
duplicates:
  ask_user 偏好收集 prompt ×5
  "用ask_user问我3个问题..." ×2
  "我的幸运数字是啥？" ×2
  "怎么回事" ×2
  "在吗？" ×2
```

另一个现场问题:Chrome 里的 `RsClaw -> 记忆管理` 页面会崩溃:

```text
TypeError: Cannot convert undefined or null to object
at MemoryPage (.../memory-page.tsx:122)
```

这不是 memory 污染的根因,但会阻断排查。当前 browser/PWA 环境可能拿不到 gateway token,接口返回 `{ "error": "unauthorized" }`,前端仍按正常 stats 读取 `stats.by_kind` / `stats.by_scope`,于是崩溃。

### 4 个异常(实测样本)

**① 全是 raw 用户消息原文,不是抽取的知识**
```
"在吗？"  "搞完了吗"  "怎么回事"  "怎么啦?"  "你用美团skill搜索一下看看"
```
memory 应存持久事实(用户身份、偏好、关键数据),不是每句闲聊。

**② 内部 prompt / 测试指令泄漏进记忆**
```
"请用 ask_user 工具问我 3 个问题..."          ×5
"请按以下顺序完成 USER.md 偏好收集..."
"Multi-hop task: delegate to agent_a1..."
"Depth-3 chain test. Send ONE call to agent_a3..."
```
这些是发给 agent 的 banner 引导词 + 测试 prompt,根本不是用户知识。写入端没判断"这条是不是用户真实意图",一锅端。

**③ 零分类**
- `kind` 全是 `note`(没有 `fact` / `preference` / `entity` 区分)
- `tier` 全是 `working`(没有 `core` / `peripheral` 分层)
- 抽取流水线根本没跑分类。

**④ 不去重**
```
"我的幸运数字是啥？"  ×2
ask_user banner prompt  ×5
```
同句反复入库,store 越滚越脏。

---

## 根因

`runtime.rs:3302-3329` 的 auto-capture:

```rust
// Auto-Capture (AGENTS.md §31): persist user message as memory note.
if let Some(ref mem) = self.memory
    && text.len() > 8
    && !reply.text.starts_with(NO_REPLY_TOKEN)
    && !internal_channel
{
    let doc = MemoryDoc {
        kind: "note",                       // 永远 note
        text: text.to_owned(),              // 用户原话,逐字
        importance: if has_key_digits { 0.85 } else { 0.5 },  // 平铺
        tier: Default::default(),           // = Working (memory.rs:61)
        ...
    };
}
```

三个硬伤:

1. **幽灵 spec。** 注释引的 `AGENTS.md §31` **不存在**——AGENTS.md 没有第 31 节,也没有任何 Auto-Capture 章节。这段行为从未被真正规范过。
2. **过滤条件形同虚设。** 只有"长度>8 + 非 heartbeat/cron/system + agent 回了"。"在吗?"轻松通过。注入的 banner prompt 也通过,因为它是以 user 角色进的消息。
3. **写入端无分类、无 tier 判定、无去重。** `MemoryStore::add()`(memory.rs:492)只 embed + 插入,不查相似项。

抽取的活其实**已经存在**,只是没被当主路径用:
- `compaction.rs:531` entity 抽取 → `kind="entity"`, Core, pinned
- `compaction.rs:548` key-fact 抽取 → `kind="compaction_fact"`, 0.7
- `runtime.rs:3527` session summary → `kind="session_summary"`, 0.8

这些是 LLM 蒸馏过的、有信号的。raw note 直存把它们淹没了。

还有 4 个实现层缺口:

1. **配置开关是死的。** `MemoryConfig.auto_capture` / `auto_recall`(schema.rs:1859)定义了,但全代码无人读取(`grep .auto_capture src/agent src/gateway` 零命中)。用户去配置里关 auto_capture 没有任何效果——行为完全硬编码。
2. **scope 不统一。** auto-capture 用 `agent:<id>`,而 `memory_put` 默认用裸 `ctx.agent_id`;同一个 agent 的搜索/统计会被拆散。
3. **BM25 已写未读 + RRF 是死代码。** auto-capture(runtime.rs:3350)和 `memory_put`(6953)会 `index_memory_doc` 灌 tantivy,但 `tool_memory_search`(6565)只有 `store.search()` 一行(纯 HNSW)。`runtime.rs:7425` 的 `rrf_fuse` 是私有 `fn`、全文件从未被调用——已定义的死代码,关键词类记忆(电话/ID/专名)直接漏召回。
4. **写入锁粒度不一致。** auto-capture 走 `mem.lock().await.add(doc).await`,embedding 会在锁内跑;`memory_put` 已有 `add_off_lock`,应统一到 off-lock 写入。

---

## 目标设计(最完美表现)

原则:**memory = 蒸馏后的持久知识,不是 transcript。** 对话流水账已在 redb session history 里,memory 不该重复存它。

### 1. 抽取 pass 取代逐条直存
不再"每条 user 消息 → note"。改成:一轮(或一次 compaction)结束时,跑一次轻量抽取,判断"这段对话里有没有值得长期记的东西",只落**抽取出的事实**,丢弃"在吗"这类闲聊。

推荐分两级:

- **L0 deterministic extractor:** 每轮同步跑,只抓强结构化实体,例如手机号、邮箱、身份证、地址、生日、姓名、偏好句式、"记住 X"。这层成本低、可测试、适合实时写 Core/entity。
- **L1 LLM extractor:** 只在高信号 turn 或 compaction 时跑,输出 JSON lines,用于 fact/preference/relationship/project 状态。低信号闲聊直接跳过,避免每轮多一次 LLM 调用。

复用 `compaction.rs` 已有的 entity + fact 抽取链,但不要把 compaction 当唯一入口。compaction 是兜底沉淀;实时记忆应有自己的小型 extraction pipeline。

### 2. 写入策略:先分类,再入库

分类最怕两种极端:太少全变 `note`,太多让 extractor 犹豫、统计和检索也跟着复杂。
MVP 固定 **8 类**,基本够覆盖真实长期记忆:

| kind | 含义 | 例子 |
|---|---|---|
| `entity` | 身份/联系方式/稳定实体 | 姓名、生日、电话、地址、幸运数字 88 66 99 |
| `preference` | 用户偏好 | 回复风格、工具偏好、代码风格、禁忌(不要 Co-Authored-By) |
| `fact` | 稳定事实 | 用户在 IDC 机房有 1000 台 4070、账号状态、长期背景 |
| `project_state` | 项目状态 | 某 repo/任务/计划当前进展、决策、阻塞 |
| `relationship` | 人/组织/agent/服务之间的关系 | A2A lead 有 a1/a2/a3 三个 peer |
| `procedure` | 可复用流程(操作型知识) | "发布流程:先 cargo test → 查 UI → commit";"排障:先看 journalctl → 再看 nginx log" |
| `remember` | 用户显式要求记住的内容 | `/remember` 或明确说"记住这个" |
| `summary` | compaction / session 摘要 | 上下文压缩产物 |

`procedure` 单独列出来很关键:这种"以后遇到 X 就按 Y 做"的操作型知识在运维/发布场景里反复沉淀,
价值高,但最容易被当成普通对话丢成 `note`。

**`note` 不在这 8 类里。** 它从"自动捕获默认落点"降级为**纯兼容桶**:
- 只用于导入旧数据、迁移、以及实在无法分类的兜底项
- **不再由自动抽取默认产生**
- 默认 **Peripheral + 低 importance(0.1-0.3)**,让它快速衰减
- 否则 `note` 会永远是垃圾桶,把上面 8 类淹掉

### 3. 分 tier(写入即定级)
- `entity`、明确身份/联系方式/稳定偏好 → **Core + pinned**
- `fact` / `preference` / `project_state` / `procedure` / `relationship` → **Working**
- `summary`、低置信但可能有用的信息 → **Working / Peripheral**(按置信度)
- `note`(兼容桶)与任何 raw 兜底捕获 → **Peripheral + 短 TTL / 快速衰减**,让噪音自己沉底,而不是像现在默认 Working 黏着不走

写入时必须带 `confidence` 或等价字段;没有字段也至少把置信度编码进 `importance`:

```text
entity / core / pinned:                       0.85-1.0
explicit remember:                            0.75-0.95
extracted fact/preference/procedure/relation: 0.55-0.80
summary:                                      0.50-0.70
note / raw fallback / peripheral:             0.10-0.30
```

### 4. 去重 / 合并
`add()` 前查语义相似(已有 embedder + HNSW,memory.rs:639):
- cos 相似 > 阈值 → 不新增,bump 已有条目的 `access_count` / `importance`
- 杜绝"幸运数字 ×2"、"banner ×5"

去重不应只靠向量相似。建议三层:

1. **规范化精确去重:** trim、大小写、全半角、空白折叠、中文标点规范化后 hash。
2. **实体键去重:** `entity.kind + entity.value` 唯一,例如 `phone:186...`、`birthday:1988-12-12`。
3. **语义合并:** 相似度超过阈值时,让新信息更新旧 doc 的 `access_count`、`accessed_at`、`importance`,必要时合并 text,而不是新增。

### 5. 过滤注入消息
auto-capture 入口加来源判断:banner 引导词、测试 prompt、系统注入的 user-role 消息**不进 memory**。
判据可用:消息来源标记 / 是否 agent 自己注入 / 内容指纹。

最低限度过滤规则:

- 跳过 `heartbeat` / `cron` / `system` channel,保留现有逻辑。
- 跳过内部 preparse/banner/引导词:包含 `ask_user 工具问我`, `write_workspace_file`, `Multi-hop task: delegate`, `Depth-3 chain test`, `Lead should`, 等测试/系统指令指纹。
- 给 runtime 构造的 user-role 消息加 `source=internal|banner|repair|compaction|user` 这类元数据;长期方案必须靠来源标记,不能只靠文本规则。
- 对真正用户输入也要先过 salience gate:寒暄、催促、报错追问、一次性搜索请求默认不写长期 memory。

### 6. 自动 recall 不破坏 KV cache

auto-recall 可以恢复,但必须和写入重构一起做,否则会把历史脏 note 自动召回。

设计要求:

- recall 结果不能写进 session history、transcript、compaction 输入,也不能作为普通 `Role::User` / `Role::System` message 追加。
- rsclaw provider 优先走 ephemeral turn context;如果协议暂时没有字段,可短期塞进 `LlmRequest.user_system` 的 current-turn block,但不要持久化。
- recall block 要短,默认 top 3-5,带 id/kind/tier/score/age,并标注 "current turn only"。
- 记录 recalled ids 到 `RunContext.recalled_memory_ids`,用于后续 importance/access_count 调整。

示例:

```text
## Relevant Memory (current turn only)
- [id=..., kind=preference, tier=core, score=0.82] 用户偏好直接简洁的回答。
```

这样只增加当前轮 token,不会让历史 message count 变化,也不会污染后续 compaction。

### 7. 检索路径接入 hybrid search

当前写入端已经把 memory 同步进 tantivy BM25,但搜索工具只用向量检索。`runtime.rs:7425` 的 `rrf_fuse` 是死代码,重构时应把它(或 kb 那份 `kb/search/rrf.rs:11` 已验证的实现)接到 `memory.search` / `tool_memory_search`:

- 向量 top K: 语义相似。
- BM25 top K: 关键词、数字、ID、专名。
- RRF 融合后按 tier、pinned、importance、recency 做轻量 rerank。

这对手机号、生日、幸运数字、项目名、commit hash 这类精确信息尤其重要。

### 8. UI 错误态不是主因,但要一起修

记忆页不是污染源,但现在它会在未授权时崩溃。修复要求:

- `gatewayFetch` 调用后检查 `response.ok`;非 2xx 抛出带 status 的错误,不要直接 `.json()` 当成功。
- `MemoryPage` 对 `stats?.by_kind` / `stats?.by_scope` 使用空对象兜底。
- 如果后端返回 `{error:"unauthorized"}`,页面显示"未授权/请连接 gateway 或重新打开桌面端",而不是 React error boundary。
- UI 显示当前 gateway base URL / token 来源状态,方便区分 `.rsclaw` 和 `.rsclaw-lead`。

---

## 历史遗留

库里已有的那批污染条目(原话 + banner + 测试 prompt)是历史残留。新设计上线后需要一次性清理:
- 按 `kind="note"` + 无信号特征批量降级 Peripheral 让其衰减,或
- 直接 tombstone 重建。

前端 user-md 已改成 wizard 直写 USER.md,不再发 chat prompt,所以**未来的 banner 引导词不会再污染 memory**——只剩存量要清。

推荐清理策略:

1. 先导出快照到 `var/backups/memory-YYYYMMDD.jsonl`。
2. 对明显污染项直接 tombstone:
   - `kind=note` 且文本命中内部 prompt 指纹。
   - 重复的寒暄/催促/测试 prompt。
3. 对可能有价值的 raw note 跑一次离线 extractor,抽出 `entity` / `preference` / `fact` 后再删除原 note。
4. 对不确定项降级 `Peripheral + importance=0.1`,让 decay 清理。

---

## 实现落点

| 改动 | 文件 |
|---|---|
| 删除/改造 raw 逐条直存 | `src/agent/runtime.rs:3302-3329` |
| 抽取主路径(分 kind + tier) | `src/agent/compaction.rs` 抽取链提级 / 新 `memory_extractor` 模块 |
| 写入去重(精确 + 实体键 + 语义合并) | `src/agent/memory.rs::add` / `add_pre_embedded` / `add_off_lock` |
| 来源过滤(挡 banner/测试 prompt) | auto-capture 入口 + 消息来源标记 |
| scope 统一 | `runtime.rs` auto-capture + `tool_memory_put` 默认 scope |
| hybrid search | `tool_memory_search` + `store/search.rs` + `rrf_fuse` |
| auto-recall ephemeral 注入 | `runtime.rs` LLM request 构造 + provider request 字段 |
| UI 错误态 | `ui/app/lib/rsclaw-api.ts` + `ui/app/components/memory-page.tsx` |
| 存量清理脚本 | 一次性 migrate |

> 注:`AGENTS.md §31` 引用是幽灵 spec,实现时应在 AGENTS.md 补一节真正的 memory 抽取规范,或删掉这个引用。

## 建议实施顺序

1. **止血:** 关闭/改造 raw auto-capture;保留 `/remember` 和 deterministic entity extraction。
2. **UI 可观测:** 修记忆页 unauthorized 崩溃,让页面能显示真实后端错误。
3. **抽取主路径:** 新增 `MemoryExtractor`,输出结构化 candidates,只写有 salience 的结果。
4. **写入质量:** scope 统一、去重合并、off-lock embedding、tier/importance/pinned 写入规则。
5. **检索质量:** 接入 BM25 + vector RRF,再做 auto-recall ephemeral 注入。
6. **存量治理:** 快照、清理污染 note、离线抽取有价值信息。

## 验收标准

- 新开 20 轮闲聊/催促/普通搜索请求后,memory 新增条目数为 0 或接近 0。
- 明确事实输入如"我叫用户,幸运数字是 88"后,写入 `entity/preference/fact`,不是 raw `note`。
- 同一事实重复输入 3 次后,store 只有 1 条逻辑记忆,`access_count` 或 `importance` 更新。
- `ask_user` banner、多 agent 测试 prompt、internal repair prompt 不进入普通 agent scope。
- `/remember` 仍可显式保存,但保存内容参与去重和 tier 判定。
- `memory.search("幸运数字")` 能命中抽取后的事实,且不会返回"我的幸运数字是啥？"这种问题句。
- `RsClaw -> 记忆管理` 在未授权时显示错误态,不触发 React error boundary。
- auto-recall 打开后,不向 persisted session messages 追加 recall 内容;下一轮 message count 不因 recall block 消失而缩短。
