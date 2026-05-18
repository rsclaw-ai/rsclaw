# /watch — Live Event Stream → Chat Design Spec

## Overview

`/watch` 是 chat slash command，让用户订阅一个**事件源**（文件 tail / SSE 流 / shell subprocess），事件实时推回当前 channel/peer。和 `/loop` 互补：`/loop` 定时回放 prompt（poll 模型），`/watch` 事件触发即推（push 模型），SSE live stream 这种场景 `/loop` 做不了。

`/watch` **不过 agent LLM**，零 token 成本，事件 ~ms 级到达 chat。共用一套 EventSource + Filter + RateLimiter 核心，未来 `Monitor` agent tool（v2）只需替换末端 sink。

## 设计决策

| 决策点 | 选择 | 原因 |
|---|---|---|
| 命名 | `/watch`（slash）+ `Monitor`（v2 agent tool） | watch=被动观察，monitor=agent 主动监控；语义区分 |
| 路由 | 直接推 chat，不过 agent | 零 LLM 成本，真实时 ~ms 延迟，不污染 context |
| 源类型 v1 | shell + SSE + file | SSE 是核心动机；file 跨平台替代 `tail -f`；shell 留 escape hatch |
| WebSocket | 不做（v2） | 多一层协议复杂度（auth/ping-pong/重连） |
| 持久化 | in-memory only | 重启清空；跨重启用 `/loop 10m /watch ...` 组合实现 |
| Dedup | `(channel, peer, normalize(source))` HashMap | 单一权威；`/loop` 重复回放 = 命中 = no-op |
| Dedup hit reply | User origin 显式提示 / Cron origin 静默 | 治理 `/loop` 触发的 chat 噪声 |
| 并发上限 | 单 (channel, peer) 5 个 watch | 防误操作打爆 chat |
| 生命周期 | 无超时直到 stop / 重启 | 跟 `tail -f`、SSE 长连语义一致 |
| 心跳 | 10min 无事件推一条 active 通知 | 证明 watch 还活着 |
| Rate limit | 默认 2s/event + batch 合并 | IM 平台心理阈值；`--rate 0` 关 |
| Filter | `--grep <regex>`（must）+ `--jq <expr>`（stretch） | grep 覆盖 90% 场景；jq 等 jaq 集成 |
| SSE auth | `${ENV_VAR}` 替换 + 字面量 header | 安全用户自负，两种都支持 |
| SSE reconnect | 指数退避 2s→30s cap，无重试上限；4xx (401/403/404) fatal | 配错不该死循环；其他暂时性错误持续重连 |
| SSE 客户端心跳超时 | 90s 没收到任何 byte → 主动断开重连 | TCP keepalive 常被中间代理无声 drop |
| Last-Event-ID | **客户端实现，记录 `id:` 并在重连时发 header** | server 现在不发，但客户端先实现 = server 上线即生效 |
| 安全 | 用户自负 | 不挡 shell 注入 / SSRF / 内网 URL |
| Windows 兼容 | file/SSE Rust 原生 + shell 留 escape hatch | 90% 场景跨平台，shell 用户自知差异 |

## 命令文法

```
/watch ::= START | LIST | STOP

START ::= /watch [SOURCE_KIND] SOURCE_ARGS [FLAGS...]
SOURCE_KIND ::= "file" | "sse" | "shell"
                (省略时按 SOURCE_ARGS 首 token auto-detect)

SOURCE_ARGS:
  file:  <path>
  sse:   <url>
  shell: <quoted-or-rest-of-line>

FLAGS:
  -H 'Header: value'      # SSE only，可多次；value 可含 ${VAR}
  --grep <regex>          # 任意 source
  --jq <expr>             # 任意 source (stretch goal v1)
  --rate <ms>             # 默认 2000；0 = 不限流
  --only <types>          # nice-to-have，按 event type 白名单（如 hit,error）
  --tee <path>            # nice-to-have，事件同步落本地 jsonl

LIST ::= /watch list
STOP ::= /watch stop <watch-id> | /watch stop all
```

### Auto-detect 表（无 SOURCE_KIND 时）

| 首 token 模式 | 推定 kind |
|---|---|
| `^https?://` | sse |
| `^/`、`^~/`、`^\./`、`^\.\./`、`^[A-Za-z]:[\\/]` | file |
| 其他 | 报错：`unknown source; prefix with file/sse/shell` |

### 示例

```
/watch /var/log/app.log                            → FileSource("/var/log/app.log")
/watch file /var/log/app.log                       → 同上
/watch https://api/events -H 'Auth: Bearer ${T}'   → SseSource(...)
/watch shell "tail -f x | grep ERR"                → ShellSource(...)
/watch tail -f x                                   → ERROR: prefix with shell

/watch list                                        → 列出当前 (channel, peer) 下的 watches
/watch stop w_abc12345                             → 停某个
/watch stop all                                    → 全停
```

## 模块结构

```
src/gateway/watch/
  mod.rs            # WatchRegistry 单例 + handle_command(...)
  source.rs         # EventSource trait + FileSource / SseSource / ShellSource
  filter.rs         # grep + jq（jq stretch）
  rate_limit.rs     # 2s 窗口 + batch
  delivery.rs       # 推 chat（包装 channel.send_message）
  dedup.rs          # normalize_source + dedup_key（纯函数，可独测）
  parser.rs         # 命令文法解析
```

### 接入点

| 文件 | 改动 |
|---|---|
| `src/gateway/preparse.rs` | 识别 `/watch ...`，dispatch 到 `gateway::watch::handle_command(...)` |
| `src/gateway/preparse.rs` | `try_preparse_locally` 加 `origin: PreparseOrigin` 参数 |
| `src/gateway/startup.rs` | 初始化 `WatchRegistry::global()`，参考 `task_queue` 的位置 |
| `src/cron/mod.rs:1703` | 调 `try_preparse_locally` 时传 `PreparseOrigin::Cron` |
| gateway server caller | 调 `try_preparse_locally` 时传 `PreparseOrigin::User` |

## Components

### `WatchRegistry`（`mod.rs`）

```rust
pub struct WatchRegistry { inner: Mutex<HashMap<DedupKey, WatchTask>> }

pub struct WatchTask {
    pub id: WatchId,                        // w_<8 hex>
    pub source: String,                     // 原始 source 字符串（执行用，未归一化）
    pub channel: String, peer: String,
    pub started_at_ms: u64,
    pub event_count: AtomicU64,
    pub error_count: AtomicU64,             // jq runtime err 等
    pub dropped_count: AtomicU64,           // mpsc buffer 满
    pub stop_tx: oneshot::Sender<()>,
    handle: JoinHandle<()>,
}

impl WatchRegistry {
    pub fn global() -> &'static Self;
    pub async fn start(&self, spec: WatchSpec) -> Result<StartOutcome, WatchStartError>;
    pub fn stop(&self, channel: &str, peer: &str, id: &WatchId) -> bool;
    pub fn stop_all_for(&self, channel: &str, peer: &str) -> usize;
    pub fn list_for(&self, channel: &str, peer: &str) -> Vec<WatchInfo>;
}

pub enum StartOutcome {
    Started(WatchId),
    AlreadyRunning { id: WatchId, started_at_ms: u64, event_count: u64 },
}

pub enum WatchStartError {
    LimitReached { current: usize, max: usize },
    InvalidPath(String),
    InvalidRegex(String),
    InvalidJq(String),
    UnresolvedEnv(String),
    SourceFailedImmediately(String),
}
```

### `EventSource` trait（`source.rs`）

```rust
#[derive(Clone, Debug, serde::Serialize)]
pub struct EventRecord {
    pub event: String,                  // "message" | "hit" | "line" | "_disconnect" | "_timeout" | ...
    pub data: serde_json::Value,
    pub raw: Option<String>,            // 原始字符串（grep 用）
    pub event_id: Option<String>,       // SSE `id:` 字段，用于 Last-Event-ID resume
    pub ts_ms: u64,
}

#[async_trait]
pub trait EventSource: Send {
    async fn run(self: Box<Self>, tx: mpsc::Sender<EventRecord>, stop: oneshot::Receiver<()>);
}

pub struct FileSource  { pub path: PathBuf }
pub struct SseSource   {
    pub url: String,                    // ${VAR} 替换后的最终值
    pub headers: Vec<(String, String)>, // ${VAR} 替换后的最终值
    // 运行时状态（不在 spec 表面，但实现需要）：
    //   last_event_id: Option<String>     -- 重连时塞进 Last-Event-ID header
    //   last_byte_at:  Instant            -- 90s 心跳超时检测
}
pub struct ShellSource { pub cmd: String }
```

#### FileSource 实现策略

- 打开 `path`，seek 到末尾（默认 `tail -f` 不读历史）
- `tokio::time::interval(200ms)` 轮询：
  - stat：inode 变了（Unix）/ 文件大小变小 → 重新打开
  - 读 remaining bytes，按行切，每行发 `EventRecord { event: "line", data: Value::String(line), raw: Some(line), ts_ms }`
- 不引入 `notify` crate；200ms 延迟在 chat rate-limit（2s）阴影里

#### SseSource 实现策略（对齐 `quick_stream.py` + 加固）

**Request headers**（必发）：

```
Accept: text/event-stream
Cache-Control: no-cache
Accept-Encoding: identity            ← 关键，禁 gzip。gzip 会缓冲整个响应，SSE 完全废
Authorization: ...                   ← 如用户传 -H 且 ${VAR} 替换后非空
Last-Event-ID: <last seen id>        ← 重连时若客户端已记录到 id，必发
```

**SSE wire 解析**（状态机）：

- 行分隔符：`\n` 或 `\r\n`，按字节流读，不假设行边界对齐 chunk 边界
- `event: <type>` → 当前 event 的 type（默认 `"message"`）
- `data: <text>` → 累积到 data buffer；**多个 data: 行用 `\n` 拼接**（SSE spec）
- `id: <id>` → 记录到客户端 `last_event_id` 状态，**重连时塞 Last-Event-ID header**
- `retry: <ms>` → 服务器建议的重连延迟，覆盖客户端 backoff（如果有）
- `:` 开头 → comment，**忽略**（很多服务器发 `: ping\n\n` 当应用层心跳）
- 空行 → event 终止，flush 一个 `EventRecord`
- 连续空行 → no-op（不发空 event）

`event` 字段默认 `"message"`，`data:` JSON parse 失败 → `data: { "_parse_error": "...", "_raw": "..." }`。

**心跳超时**（很容易漏 — TCP keepalive 不靠谱）：

- 维护 `last_byte_at: Instant`，每收到任意字节更新
- tokio interval 5s tick 检查 `now - last_byte_at > 90s` → 主动断开 + 推 `EventRecord { event: "_timeout" }` + 进入重连
- 不依赖 TCP keepalive（中间代理常无声 drop）
- 90s 是「服务器一般 30s 一次 heartbeat，给 3× 容忍」的经验值

**Reconnect 策略**：

| 触发 | 处理 |
|---|---|
| 网络断 / EOF / 心跳超时 | 指数退避 **2s→4s→8s→16s→30s 封顶**，无重试上限 |
| 5xx 响应 | 同上（暂时性） |
| `retry: <ms>` 头 | 服务器建议，覆盖下次退避 |
| 401 / 403 / 404 | **fatal，立即终止 watch + 删 entry + 推 chat 错误** |
| Server clean close（200 但流结束） | 推一条 `EventRecord { event: "_disconnect", data: { reason: "server_closed", url } }` 然后重连 |

无重试上限 vs 之前的 ×5：用户场景包括「盯 24h 生产 SSE」，5 次太少；只要不是配错（4xx），就一直试。心跳超时机制保证不会因为 stale connection 永远阻塞。

**${VAR} 替换**：

- URL 和 header value 都做替换（URL 也可能含 token query param）
- 启动时（`start()`）一次性替换，结果保存在 `SseSource { url, headers }`
- 任一 `${VAR}` 未定义或值为空 → `WatchStartError::UnresolvedEnv(var_name)`，拒绝启动；**不允许悄悄发出空 bearer**
- 替换后的 URL/headers **不进 tracing log**（机密泄漏）：log 只记 `host + path`，不记 query；只记 header name，不记 value

#### ShellSource 实现策略

- 平台分支：`sh -c <cmd>` (Unix) / `powershell -Command <cmd>` (Windows)
- `tokio::process::Command::stdout(Stdio::piped()).stderr(Stdio::piped())`
- `BufReader::lines()` 读 stdout + stderr（合并），每行 `EventRecord { event: "line", data: line, raw: Some(line), ts_ms }`
- 进程退出（任何 exit code）→ 走 EOF 路径
- spawn 后 100ms 内 wait：如果立即退出且 code≠0 → 当作 immediate failure，注册前拒绝

### `Filter`（`filter.rs`）

```rust
pub enum Filter {
    None,
    Grep(regex::Regex),
    Jq(jaq_interpret::Filter),     // stretch
    Combined(Box<Filter>, Box<Filter>),
}
impl Filter {
    pub fn apply(&self, ev: &EventRecord) -> Option<String>;
    // Some(s) → 通过，s 是给 chat 看的文本
    // None    → drop
}
```

- **Grep**：对 `ev.raw.unwrap_or(stringify(&ev))` 做正则匹配，命中返回 raw 本身
- **Jq**：传 `{event, data}` 给 jq 求值；输出空/null/false → drop；输出值 → stringify
- **Combined(grep, jq)**：先 grep 再 jq
- 运行时错误（jq 抛异常）：返 None + `error_count += 1`，不轰炸 chat

### `RateLimiter`（`rate_limit.rs`）

```rust
pub struct RateLimiter {
    window_ms: u64,        // 默认 2000
    max_per_window: usize, // 默认 1（0 = unlimited）
    buffer: Vec<String>,
    last_emit_ms: u64,
}

pub enum DeliveryMsg {
    Single(String),
    Batch { last: String, dropped: usize },  // "N more events in 2s, last: <last>"
}

impl RateLimiter {
    pub fn admit(&mut self, msg: String, now_ms: u64) -> Option<DeliveryMsg>;
    pub fn flush_pending(&mut self, now_ms: u64) -> Option<DeliveryMsg>;
}
```

- 窗口内首条 → `Some(Single(msg))`，更新 `last_emit_ms`
- 窗口内后续 → buffer 累积，返 `None`
- 窗口结束（外部 interval tick 调 `flush_pending`）→ buffer 非空 → `Some(Batch { last: 最后一条, dropped: buffer.len() - 1 })`
- `max_per_window = 0` → 永远 `Single`，跳过 buffer

### `Delivery`（`delivery.rs`）

复用 cron 的 delivery 路径 — `ChannelManager::get(name)` 拿 `Arc<dyn Channel>`，再调 `Channel::send(OutboundMessage)`。

```rust
pub async fn deliver(
    channels: &ChannelManager,
    channel: &str,
    peer: &str,
    body: String,
) -> Result<()> {
    let resolved = if channel == "ws" { "desktop" } else { channel };  // cron 同款 remap
    let ch = channels.get(resolved)
        .ok_or_else(|| anyhow!("channel '{channel}' not registered"))?;
    ch.send(OutboundMessage { to: peer.into(), text: body, ..Default::default() }).await
}
```

WatchRegistry 在 gateway startup 阶段被注入 `Arc<ChannelManager>`：

```rust
pub fn init(channels: Arc<ChannelManager>);   // 一次性初始化全局单例
pub fn global() -> &'static Self;             // init 之后才能调
```

调用顺序：`gateway/startup.rs` 在 ChannelManager 注册完所有 channel 之后、`configure_channels` 完成之后调 `WatchRegistry::init(channels.clone())`。

### `Dedup`（`dedup.rs`）

```rust
pub fn normalize_source(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}
pub fn dedup_key(channel: &str, peer: &str, source: &str) -> DedupKey {
    (channel.to_owned(), peer.to_owned(), normalize_source(source))
}
```

执行时用**原始 source**（保留 quoted-internal-spaces 等语义），HashMap key 用归一化版本。

### `PreparseOrigin`（preparse.rs 改动）

```rust
pub enum PreparseOrigin { User, Cron }

pub struct Reply {
    pub text: String,
    pub silent: bool,         // 新增：true 表示不送 channel，但 preparse 已处理
}

pub async fn try_preparse_locally(
    text: &str,
    handle: &Handle,
    channel: &str,
    peer: &str,
    origin: PreparseOrigin,
) -> Option<Reply>;
```

- caller 见 `silent: true` 不送 channel
- 仅 `/watch` 当前用到 `silent`；其他 slash 命令保持现行行为（`silent: false` 默认）

## Data Flow

```
[source task]                                                [processor task]
                                                              
  raw bytes from kernel / socket                                
       │                                                        
       ▼                                                        
  parse → EventRecord                                           
       │                                                        
       │ mpsc::Sender::try_send  (buffer 256)                   
       ▼                                                        
   ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─  
                                                                
                                              select! {        
                                                  ev = rx.recv() →
                                                      Filter::apply(&ev)
                                                          │
                                                          ├── None → drop
                                                          └── Some(s) →
                                                              RateLimiter::admit(s)
                                                                  │
                                                                  ├── None → buffered
                                                                  ├── Single(s) → deliver(s)
                                                                  └── Batch{last, dropped} → deliver(...)
                                                                  
                                                  _ = rate_tick (interval 2s) →
                                                      flush_pending() → maybe deliver
                                                  
                                                  _ = heartbeat_tick (interval 10min) →
                                                      if event_count unchanged 10min:
                                                          deliver("watch w_xxx active, 0 events")
                                                  
                                                  _ = stop_rx →
                                                      flush_pending() → break loop
                                                  
                                                  rx closed (source EOF) →
                                                      flush_pending() → deliver("watch w_xxx ended: <reason>") → break
                                              }
```

mpsc buffer 满时 source `try_send` 失败 → `dropped_count += 1`，不阻塞 source 读取。

## Watch 状态机

```
   start()
      │
      ▼
   Starting ──spawn fail / regex err──▶ rejected (返 WatchStartError, 不入 registry)
      │
      │ source task running
      ▼
   Running ◀─────┐
      │          │
      │ event    │ reconnect (SSE only)
      ▼          │
   Running ──────┘
      │
      │ (a) stop signal received
      │ (b) source EOF / SSE 5×fail
      │ (c) registry shutdown
      ▼
   Terminating ──▶ flush pending batch ──▶ remove from HashMap ──▶ Terminated
```

## Error Handling 矩阵

| 失败点 | 检测时机 | 处理 | 用户看到 |
|---|---|---|---|
| 路径不存在 / 不可读 | `start()` 同步检查 | 拒绝注册 | "invalid path: /no/such" |
| shell 立即 exit (≠0) | spawn 后 100ms 内 wait | 拒绝注册 | "shell exited immediately (code=N)" |
| shell 中途 crash | reader 看到 EOF | 走 EOF 路径，删 entry | "watch w_xxx ended: process exited (code=N)" |
| SSE 401/403/404 | HTTP status | fatal，删 entry | "watch w_xxx errored: 4xx (fatal)" |
| SSE 网络断 / 5xx / EOF | bytes_stream Err | 2s→30s cap 指数退避，无上限重试 | 不告知（高频事件，避免 chat 噪声）；进入 reconnect 状态 |
| SSE 心跳超时 (90s 无 byte) | 5s interval tick 检查 | 主动断开 + 走重连路径 | 推一条 `_timeout` 事件（默认 filter 过滤；用 `--only` 才看见）|
| SSE 非 event-stream | 首个 chunk Content-Type 检查 | fatal | "watch w_xxx errored: server returned text/html" |
| SSE `retry: <ms>` 头 | 解析 | 用这个值覆盖下次 backoff | (透明) |
| `data:` 非 JSON | parse catch | fallback `{_parse_error, _raw}` | (透明) |
| `--grep` 编译失败 | `start()` 同步检查 | 拒绝注册 | "invalid regex: <err>" |
| `--jq` 解析失败 | `start()` 同步检查 | 拒绝注册 | "invalid jq expression: <err>" |
| `--jq` 运行时 err | filter apply catch | skip + error_count++ | (透明)；`/watch list` 显示 `N events / M jq errors` |
| `${ENV}` 未定义 / 空值 | `start()` 替换时（URL 和 headers 都查）| 拒绝注册（**不允许悄悄发空 bearer**）| "unresolved env var: API_KEY" |
| 替换后 header / URL 出 log | tracing 调用点检查 | log 只写 host+path（不含 query），header name（不含 value）| (透明) |
| 并发上限 | `start()` count check | 拒绝注册 | "limit reached (5/5)" |
| Dedup 命中（User）| `start()` HashMap lookup | 返已有 id | "already running (5m 142evt). /watch stop w_xxx" |
| Dedup 命中（Cron）| 同上 | 返已有 id | (静默，silent=true) |
| Dedup miss（Cron）| 同上 | 起 watch | "Watch (re)started: w_xxx" |
| mpsc buffer 满 | source try_send fail | drop event + counter | (透明)；heartbeat 提及 dropped |
| processor panic | tokio 上层捕获 | 删 entry，推 chat | "watch w_xxx crashed internally" + log |
| Gateway shutdown | 全局信号 | 给所有 stop_tx 发信号 join | (不通知，走 gateway 自己的 shutdown msg) |

### 不处理 / 故意放过

- subprocess buffering 不刷：用户加 `stdbuf -oL` / `--line-buffered`
- 大事件 payload：单条 >100KB 不截断（chat channel 自负）
- shell 注入 / SSRF / 内网 URL：用户自负
- jq 运行时频繁报错：error_count 累计，不强制停 watch

## `/loop + /watch` 组合时序

```
t=0     用户：/loop 10m /watch /tmp/x.log
        → preparse(User) → 创建 cron job + chat 回执 "Scheduled loop (every 10m)"

t=0+ε   cron 首次触发 (anchorMs = now)
        → preparse(Cron): "/watch /tmp/x.log"
        → /watch start() → dedup miss → 起 watch w_abc
        → 回 chat "Watch started: w_abc" (silent=false，让用户看见首次启动)

t=10m   cron tick #2
        → preparse(Cron): "/watch /tmp/x.log"
        → start() → dedup hit → silent=true
        → 不送 chat                                     ← 没噪声

...

gateway 重启（watch 死，cron 还在）

t=N     cron tick
        → preparse(Cron): "/watch /tmp/x.log"
        → start() → dedup miss（HashMap 空）→ 起 watch w_def
        → 回 chat "Watch (re)started: w_def" (silent=false)
```

停止：
- `/cron remove loop-xxx` 停定时回放
- `/watch stop w_def` 停当下 watch

## Testing

### Unit tests（无 IO，必做）

```
watch/dedup.rs::tests
  - normalize_source: 多空格、tab、首尾空白、内部 quoted preserve（实际归一化也合并）
  - dedup_key: 同源不同 channel/peer 不同 key
  - 注：执行用原文 vs key 用归一化的对照

watch/filter.rs::tests
  - Grep: 命中 / 不命中 / 编译错
  - Jq: 命中 / 不命中 / 转换 / runtime err 静默 skip（待 jaq 接入）
  - Combined(grep, jq): 串联顺序

watch/rate_limit.rs::tests
  - 窗口内首条 → Single
  - 窗口内第 N>1 → None
  - flush_pending: 1 条 → Single, N 条 → Batch(N-1)
  - --rate 0 → 永远 Single
  - 跨窗口边界重置

watch/source.rs::sse::tests
  - SSE wire 解析（event/data/comment/empty-line/多行 data）
  - JSON parse fail → _parse_error fallback
  - reconnect 状态机（mock retries）

watch/parser.rs::tests
  - auto-detect: URL → sse, path → file, raw → error
  - 显式 kind 覆盖 auto-detect
  - flag 解析：-H 多次、--grep、--jq、--rate
  - /watch list / stop / stop all
```

### Integration tests（写在 `tests/watch_*.rs`，必做）

```
tests/watch_file.rs
  - 起 tmpfile，FileSource，append lines，断言事件
  - Truncate mid-stream → reopen + 后续事件
  - Rotation（rename + create new）→ reopen + 后续（Unix only via inode）

tests/watch_sse.rs
  - 起 local hyper SSE server，SseSource，断言事件
  - Server kill conn → expect reconnect + 后续
  - Server 返 403 → expect errored + 删 entry

tests/watch_dedup.rs
  - 同 source 连续起两次 → 第二次返 AlreadyRunning
  - 不同 (channel, peer) 同 source → 两个独立 watch
  - 归一化：tab/多空格 → 同 key

tests/watch_origin.rs
  - preparse(User) + dedup hit → silent=false, text 含 "already running"
  - preparse(Cron) + dedup hit → silent=true, text=""
```

### E2E（手测，不进 CI）

- 真起 gateway，feishu/wechat chat 输 `/watch /tmp/x.log`，`echo >> /tmp/x.log`，确认 chat 收到
- `/loop 10m /watch /tmp/x.log` → 杀 watch task 模拟重启 → 5min 后 cron 复活
- Windows 路径解析（CI 无 Windows runner，手测）

### 不做的测试

- jaq 内部行为（信赖上游）
- tokio runtime / reqwest TLS（信赖上游）

## Known Gaps（v1 不解决，v2 再说）

- WebSocket source
- watch.json5 持久化（跨重启自愈用 `/loop` 组合）
- Monitor agent tool（共享核心，末端 sink 改 drain queue）
- `--id <name>` 显式标签（YAGNI，dedup 用 source 字符串够了）
- jq runtime（如果 jaq 集成成本高，v1 只交付 grep）

### 已实现但 server 端未配合（client-only）

- **Last-Event-ID resume**：客户端记录 `id:` 字段并在重连时发 header；rsclaw / astock gateway 目前不发 `id:` 也不识别 header，client 部分先 ship，server 上线时无须改 client

### Nice-to-have（v1 有余力就做，否则 v2）

- `--only <types>`：按 event type 白名单过滤，如 `--only hit,error` 静默 heartbeat
- `--tee <path>`：事件同步追加到本地 jsonl 文件，配合 `/loop` 离线分析
- JSON pretty-print：UI 端把 `data` 当 JSON 渲染，关键字段高亮（属 UI 层，gateway 端 spec 不管）

## Implementation Order

1. `dedup.rs` + `parser.rs`（纯函数，先建测试）
2. `rate_limit.rs`（纯状态机）
3. `filter.rs`（grep 优先，jq stretch）
4. `source.rs::FileSource`（最简单，能 e2e）
5. `source.rs::ShellSource`（复用 preparse 已有 sh/powershell 分支）
6. `source.rs::SseSource`（最复杂；分子步骤如下）
   - 6a. 最小可用：headers + wire 解析（含 comment / multi-data / id），单连
   - 6b. Reconnect 状态机（2s→30s cap，4xx fatal，retry: 头）
   - 6c. 心跳超时检测（90s no-byte → 主动断）
   - 6d. Last-Event-ID 记录 + 重连塞 header
   - 6e. ${VAR} 替换（URL + headers）+ 空值拒绝 + log 脱敏
7. `mod.rs` WatchRegistry + handle_command（串起来）
8. `preparse.rs` 接入 + `PreparseOrigin` 重构
9. `cron/mod.rs:1703` 改 origin=Cron
10. Integration tests
11. 文档 + CHANGELOG
