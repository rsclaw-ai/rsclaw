# CONTEXT.md — RsClaw 热更新（hot-reload）工作上下文恢复

> 用途：恢复会话上下文。本文件记录目标、已完成工作、当前 git 状态、待解决的 review 阻塞项，以及恢复所需的关键技术细节。
> 最后更新：2026-07-23

---

## 1. 本阶段目标

把原本需要 `rsclaw restart`（修复前 60–120s 卡顿）才能生效的变更，改为 `rsclaw reload` 零停机热更新，并修复 review 发现的资源泄漏 / 卡顿 / 安全问题。

**核心结论（第一性原理）：** 5s 重启 + skills/MCP reload 已覆盖 95% 场景；本阶段把剩余 5%（plugins / channels / agents / providers 增删改）也做成热更新，并修复了重启卡顿与多处任务泄漏。

---

## 2. 已完成并验证的工作

### 2.1 `rsclaw reload` 热更新能力（全部 scope）

```bash
rsclaw reload                          # 全部热更新（= rsclaw gateway reload）
rsclaw reload --scope plugins          # WASM swap + JS kill/respawn
rsclaw reload --scope skills           # 目录重扫
rsclaw reload --scope mcp              # kill + respawn
rsclaw reload --scope providers        # build_providers 重建 + swap 到所有 handle
rsclaw reload --scope channels         # 13 个 channel 动态增删
rsclaw reload --scope agents           # 增 / 删 / 改（model+prompt 变更触发 re-spawn）
```

- 入口：`POST /api/v1/reload?scope=...`（`crates/rsclaw-runtime/src/server/mod.rs` 的 `http_reload`）。
- CLI：`rsclaw reload` 是 `rsclaw gateway reload` 的隐藏别名（`crates/rsclaw-cli/src/lib.rs` + `cmd/gateway.rs`）。
- 配置非法时返回 400 且不应用任何变更（fail-safe，已 e2e 验证）。

### 2.2 三个泄漏 / 卡顿修复

| 问题 | 修复 | Commit |
|------|------|--------|
| 重启 drain 卡 20s（WS/SSE 连接不关） | WS handler 改 `write_task.abort()`；`/stream`、`/computer-use/stream` SSE 加 shutdown 感知 → drain 瞬间完成 | `007595f8` |
| Agent 移除/重启任务泄漏（task 持有 `runtime.handle.tx`，`rx.recv()` 永不返回 None） | `AgentHandle` 加 lifetime `CancellationToken`，两个 spawn 循环 select 它，`remove_handle` fire 它 | `94d7767c` |
| Channel 移除泄漏 + 行为错误（run-loop 继续跑、inbound 仍处理但 outbound 断） | `ChannelManager` 加 `register_cancel_token`/`cancel_channel`，13 个 channel 的 run/outbound 循环 select per-channel token，`unregister` 自动 fire | `be66c3e3` |

### 2.3 验证状态

- `cargo check` 通过；`rsclaw-runtime` 159 个单测通过。
- E2E：`rsclaw reload`（全 scope）幂等正常；gateway 健康；13 个 channel 正常运行。
- **Wechat 真实移除/恢复 e2e 通过**：移除时日志出现 `wechat: channel cancelled, stopping long-poll loop / outbound sender`；恢复后 `wechat personal channel started`。测试后 config 已还原。
- 重启 drain 从日志确认瞬间清空（0.00004s）。

---

## 3. 当前 Git 状态（重要：有他人/后续提交与未提交改动）

分支：`dev`。**本阶段我提交的 8 个 commit**（从旧到新）：

```
6194e7d7 feat: add 'rsclaw gateway reload' for hot-reloading skills and MCP servers
8a69b491 feat: full 'rsclaw gateway reload' — hot-reload plugins, channels, agents, skills, MCP
124b9596 feat: add 'rsclaw reload' shortcut alias for 'rsclaw gateway reload'
698c66f9 feat: dynamic channel hot-add/remove via 'rsclaw reload --scope channels'
ce51b664 feat: telegram hot-add + agent model/prompt hot-reload
d2fe538a feat: provider hot-reload via 'rsclaw reload --scope providers'
007595f8 fix: graceful restart drain no longer hangs on open WS/SSE connections
94d7767c fix: agent message-loop task no longer leaks on removal/re-spawn
be66c3e3 feat: per-channel CancellationToken for graceful hot-reload removal
```

> 注意：`79bd8f91 refactor: split runtime.rs (11k lines) into focused modules` 是**别人/其他流程**做的重构（把 `runtime.rs` 拆成 `runtime/mod.rs`、`runtime/run_turn.rs` 等模块），不是本会话的工作。

**后续（非本会话）针对 code review 的修复提交：**

```
2d0c401f fix: address code review findings (provider staleness, reload races, spawner cancel)
5240be9f fix: reload no longer loses agents.defaults.model (vision/flash/primary)
```

**当前未提交的改动（`git status -s`）—— 疑似仍在修 review 阻塞项：**

```
 M crates/rsclaw-agent/src/registry.rs          (+37)
 M crates/rsclaw-agent/src/runtime/mod.rs       (±2)
 M crates/rsclaw-agent/src/runtime/run_turn.rs  (+6)
 M crates/rsclaw-agent/src/spawner.rs           (+2)
 M crates/rsclaw-runtime/src/gateway/startup.rs (+6)
 M crates/rsclaw-runtime/src/server/mod.rs      (+45/-11)
?? docs/reviews/dev.md                          (code review 报告，未跟踪)
```

> 恢复时第一步：`git diff` 看清这 6 个文件未提交改动具体在修什么，再决定提交 / 继续 / 丢弃。

---

## 4. Code Review 阻塞项（docs/reviews/dev.md，verdict: BLOCKED，14 项）

review 范围：`6b9dd6a9..be66c3e3`。`cargo test --workspace` 通过，但下列问题不在该测试覆盖内。

### 4.1 与本次热更新相关（5 项）

| # | 问题 | 位置 | 状态 |
|---|------|------|------|
| 1 | Reload 会删除隐式默认 `main` agent（有 `agents.defaults` 无 `agents.list` 时，startup 合成 main，reload 转成空集删除所有 agent，`default_id` 仍是 main → dispatch 失败） | server/mod.rs:2111 | 部分修复见 `5240be9f`（model 部分）；**需确认 main 合成是否完整保留 + 补回归测试** |
| 2 | Channel reload 删除 custom webhook channel 但保留其路由（`/hooks/<name>` 仍 202，被取消的 outbound consumer 丢弃回复） | server/mod.rs:1981；custom.rs:408-413 | **待修**：diff custom channel 名，原子地移除/重建 webhook 注册 |
| 3 | Custom WebSocket task 在 channel 移除后存活（`start_custom_websocket` 未注册 token，只观察全局 shutdown） | custom.rs:782 | **待修**：注册 channel token 并在 recv/outbound 两个 task select |
| 4 | 多账号 channel 取消只停最后一个账号（token 以 `DiscordChannel::name()`=`"discord"` 为 key，每个账号覆盖前一个；Slack 等同理） | discord.rs:511；rsclaw-channel/discord.rs:623-626 | **待修**：token 以注册名/账号名为 key，取消被删 channel 的所有别名 |
| 5 | 并发 reload 请求可能永久 double-start 一个 channel（两个 hot-add 都看到 channel 缺失，后注册覆盖先注册，后续移除只能取消第二组 task） | server/mod.rs:2004；rsclaw-channel/lib.rs:665-671,709-722 | `2d0c401f` 提到 "reload races"，**需确认是否已序列化 reload 事务 / 原子 check-and-insert** |

### 4.2 预存在的安全 / 发布 / OCR 缺陷（9 项，非本阶段引入）

| # | 问题 | 位置 |
|---|------|------|
| 6 | 发布复用陈旧内部 crate 版本（内部 crate 都是 `0.1.0`，脚本把 "already exists" 当成功） | scripts/cargo-publish.sh:66 |
| 7 | 开放 gateway 信任客户端伪造的 forwarding header（auth 关闭时从 `X-Forwarded-For` 判定 trusted，外部可伪造 127.0.0.1 → 会话上下文 spoofing） | server/mod.rs:3597,3651-3705 |
| 8 | Custom Responses / Ollama provider 仍泄漏 `OPENAI_API_KEY`（自定义 baseUrl 无 apiKey 时回退 OPENAI_API_KEY 并作为 Bearer 发出） | rsclaw-provider/build.rs:141；openai.rs:764-766 |
| 9 | CLI image 输出写到可预测共享临时路径（`/tmp/rsclaw-cli-images/...`，跟随符号链接，可被本地攻击者重定向） | rsclaw-channel/cli.rs:55 |
| 10 | HTTP rate limiter 每个观察到的 IP 泄漏一个 map entry（无 TTL/LRU 驱逐） | server/mod.rs:93 |
| 11 | WeChat 4.x Windows OCR 可能截取前台应用（WeChat 4.x 用 `Weixin.exe`，失败时回退任意矩形 0,0,1200,800 并对前台应用发 Alt+A/Enter） | rsclaw-desktop/native.rs:442 |
| 12 | 并发 macOS OCR 请求覆盖进程全局临时图片（`/tmp/rsclaw_ocr_<pid>.png`，无 mutex） | rsclaw-desktop/native.rs:687 |
| 13 | OCR CLI 宣称支持但忽略 `--model`/`--lang` 覆盖 | rsclaw-runtime/cmd/image.rs:27；rsclaw-cli/image.rs:39-45 |
| 14 | Agent OCR 静默截断越界 `max_tokens`（`u64 as u32`，4294967296 变 0） | rsclaw-agent/tools_ocr.rs:56 |

---

## 5. 热更新系统关键架构（恢复时定位用）

### 可 swap 的共享 slot 模式
所有热更新字段都用 `Arc<RwLock<Arc<T>>>`（tokio RwLock）或 `Arc<std::sync::RwLock<...>>` 包裹，reload 时整体替换内层 Arc；runtime 在每个 turn 开始从 handle 刷新快照。

| 字段 | 类型 | 位置 |
|------|------|------|
| `AppState.providers` | `Arc<tokio::sync::RwLock<Arc<ProviderRegistry>>>` | server/mod.rs |
| `AppState.wasm_plugins` / `plugins` / `skills` | `Arc<tokio::sync::RwLock<Arc<...>>>` | server/mod.rs |
| `AppState.mcp` | `Arc<McpRegistry>`（内部 `Mutex<HashMap>`，`clients.clear()` 重建；child 有 `kill_on_drop`） | server/mod.rs |
| `AppState.channel_manager` | `Arc<ChannelManager>`（内含 `cancel_tokens: RwLock<HashMap<String,CancellationToken>>`） | rsclaw-channel/lib.rs |
| `AppState.agent_spawner` / `task_queue` / `channel_senders` | 用于 channel/agent hot-add | server/mod.rs |
| `AgentHandle.providers` | `Arc<std::sync::RwLock<Arc<ProviderRegistry>>>`，`providers_snapshot()`/`set_providers()` | rsclaw-agent/registry.rs |
| `AgentHandle.lifetime` | `CancellationToken`，`remove_handle` fire 它 | rsclaw-agent/registry.rs |

### Runtime turn 开始刷新（`runtime/run_turn.rs`，原 runtime.rs）
```rust
self.wasm_plugins = self.handle.wasm_plugins_snapshot();
self.providers   = self.handle.providers_snapshot();
```

### Channel 取消模式（13 个 channel start 函数）
```rust
let chan_name = <注册名>;                              // 多账号须用 "type/account"，不能用 bare name（见阻塞项 #4）
let cancel_token = manager.register_cancel_token(&chan_name);
let cancel_for_out = cancel_token.clone();
// outbound loop select 加: () = cancel_for_out.cancelled() => break
// run loop select 加:      () = cancel_token.cancelled() => {}
```
`ChannelManager::unregister(name)` 会先 `cancel_channel(name)` 再删路由。

### Agent spawn 循环（两处）
- `gateway/startup.rs` 的 `spawn_agent_tasks`（config agent）
- `rsclaw-agent/spawner.rs` 的 `spawn_agent_with_kind`（动态 agent）
两者都改成 `loop { tokio::select! { _ = handle.lifetime.cancelled() => break, msg = rx.recv() => ... } }`。

### Reload handler 各 scope 逻辑（server/mod.rs `http_reload`）
- skills: `load_skills` → swap。
- plugins: `load_all_plugins` → 先 `shutdown()` 旧 JS 子进程 → swap WASM（含所有 handle）+ JS。
- providers: `build_providers(&fresh_config)` → swap AppState + 所有 handle。
- mcp: `clients.clear()`（kill_on_drop）→ `respawn_mcp_servers`。
- channels: diff `configured_channels` vs `manager.names()` → 移除（unregister 触发 cancel）+ 新增（调 `start_*_if_configured`）。
- agents: diff config vs registry → model/system 变更则 cancel + remove + re-spawn；新增 spawn；缺失 remove。

---

## 6. 已知限制 / 设计决策

- **Channel 热删除**：built-in channel 已支持；custom webhook/websocket channel 有阻塞项（#2/#3）。
- **多账号 channel 取消**：当前 token 以 bare name 为 key，多账号只停最后一个（阻塞项 #4，**重要回归风险**）。
- **并发 reload**：可能 double-start（阻塞项 #5）。
- **冷启动 ~13s**：tantivy 合并 + BGE 模型 + provider 初始化，属固有成本，非 bug；drain 本身已瞬间完成。
- **`rsclaw-channel` 的 `#[cfg(test)]` 模块编译失败**：预存在问题，test 模块引用 `rustls`（未链接），与本次改动无关；lib 本身编译正常。
- **手动 reload 而非自动 file-watch**：用户明确选择手动（避免文件写一半触发 reload）。config file watcher 仍存在但只对 hot-safe 字段生效。

---

## 7. 恢复后的建议下一步

1. **先 `git diff` 审查 6 个未提交文件**，确认在修哪些 review 阻塞项，决定提交/继续/丢弃。
2. **优先修热更新相关阻塞项**（#1 main agent、#4 多账号取消、#5 并发 reload、#2/#3 custom channel）——这些是本阶段功能的正确性/回归风险。
3. 安全/发布/OCR 缺陷（#6–#14）属预存在问题，可按优先级单独排期。
4. 每修一项补对应回归测试（review 多次要求 "add a regression test"）。
5. 构建/测试命令：
   ```bash
   RSCLAW_BUILD_VERSION=dev RSCLAW_BUILD_DATE=test cargo check
   RSCLAW_BUILD_VERSION=dev RSCLAW_BUILD_DATE=test cargo test -p rsclaw-runtime --lib
   # e2e: 启动 gateway 后
   RSCLAW_AUTH_TOKEN=<token> ./target/debug/rsclaw reload
   ```
   注意磁盘空间：`target/debug/incremental` 会膨胀，必要时 `rm -rf target/debug/incremental`。

---

## 8. 关键文件速查

| 关注点 | 文件 |
|--------|------|
| Reload endpoint + 各 scope 逻辑 | `crates/rsclaw-runtime/src/server/mod.rs`（`http_reload`） |
| ChannelManager + cancel token | `crates/rsclaw-channel/src/lib.rs` |
| 13 个 channel start 函数 | `crates/rsclaw-runtime/src/gateway/channels/*.rs` |
| Agent registry / lifetime / providers slot | `crates/rsclaw-agent/src/registry.rs` |
| Agent spawn 循环（config agent） | `crates/rsclaw-runtime/src/gateway/startup.rs`（`spawn_agent_tasks`） |
| Agent spawn 循环（动态 agent） | `crates/rsclaw-agent/src/spawner.rs` |
| Runtime turn 刷新 | `crates/rsclaw-agent/src/runtime/run_turn.rs` |
| WS drain 修复 | `crates/rsclaw-runtime/src/ws/handshake.rs` |
| Provider 构建 | `crates/rsclaw-provider/src/build.rs` |
| Code review 报告 | `docs/reviews/dev.md` |
