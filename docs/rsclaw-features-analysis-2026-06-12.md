# RsClaw 功能分析文档

> 分析日期：2026-06-12
> 分析版本：基于 dev 分支（commit ef381d4）

本文档整理了 RsClaw 的核心功能模块，包括 A股数据系统、CAP 编程代理协议、以及完整的斜杠命令列表。

---

## 目录

1. [A股数据系统（astock）](#1-a股数据系统astock)
2. [CAP 编程代理协议](#2-cap-编程代理协议)
3. [斜杠命令完整列表](#3-斜杠命令完整列表)
4. [附录：配置说明](#4-附录配置说明)

---

## 1. A股数据系统（astock）

### 1.1 系统架构

RsClaw 的 A股功能依赖外部 Go gateway —— [astock](https://github.com/oopos/astock)，通过 HTTP API `/v1/*` 接口获取数据。

**astock 核心能力：**
- 通达信 pytdx 连接（实时行情）
- DuckDB 历史数据存储
- iwencai（问财）自然语言查询
- x402 paywall 计费

**rsclaw 增强层：**
- 代码标准化（600519 / SH600519 / 600519.SH 自动识别）
- 股票名称自动填充
- IM 友好渲染（万/亿单位、涨跌幅符号）
- 关注列表持久化（memory store）
- Cron 定时简报推送

### 1.2 LLM 工具层

Agent 可调用的 7 个股票工具：

| 工具名 | 功能 | 参数 | 返回字段 |
|--------|------|------|----------|
| `stock_quote` | 实时行情报价（单股/批量） | `code` 或 `codes[]`, `format` | `quotes[]`, `markdown` |
| `stock_kline` | K线历史数据 | `code`, `period`, `count`, `offset`, `adjust` | `bars[]`, `data_quality`, `markdown` |
| `stock_snapshot` | 全市场快照 | `ts`, `market`, `codes[]`, `adjust`, `limit`, `sort_by`, `order`, `use_watchlist` | `rows[]`, `total`, `used_watchlist`, `markdown` |
| `stock_ask` | 自然语言查询（iwencai） | `query`, `page`, `limit`, `call_type` | `response`（结构化问财结果） |
| `stock_query` | SQL 查询（DuckDB） | `sql` | `columns`, `rows[]` |
| `stock_chart` | K线图渲染（PNG） | `code`, `period`, `count`, `adjust`, `ma[]`, `name` | `__send_file`, `path`, `filename`, `summary` |
| `stock_watchlist` | 关注列表管理 | `action`, `codes[]` | `codes[]`, `count`, `added/removed` |

**工具设计要点：**

1. **`stock_snapshot` 默认行为：**
   - 无参数时自动使用用户关注列表过滤
   - `use_watchlist=false` 强制全市场快照
   - 默认按成交额降序、50 行上限（可调至 5000）

2. **`stock_ask` vs `stock_snapshot`：**
   - `stock_ask`：自然语言查询，问财解析，适合"今天涨停的科技股有哪些"
   - `stock_snapshot`：结构化筛选，适合已知条件的市场扫描

3. **`stock_chart` 特性：**
   - 默认前复权（qfq）
   - MA5/10/20/60 均线叠加
   - 红涨绿跌配色
   - 标题显示实时价格 + 涨跌幅（区分盘中/收盘）
   - CJK 字体支持

### 1.3 CLI 命令

```bash
# 单股报价
rsclaw stock quote 600519

# 批量报价
rsclaw stock quote-batch --code 600519,000001,300750

# K线数据
rsclaw stock kline 600519 --period 1d --count 60 --adjust qfq

# 全市场快照
rsclaw stock snapshot --market SH --limit 20

# 问财查询
rsclaw stock ask "北向资金净流入前20"

# SQL 查询
rsclaw stock query "SELECT code, name, price FROM stocks WHERE market='SH' LIMIT 10"
```

### 1.4 斜杠命令

`/astock` 系列 IM 快捷操作：

```
/astock quote <code>           # 快速报价
/astock kline <code>           # K线摘要
/astock snapshot [codes...]    # 关注列表快照
/astock ask <query>            # 问财查询
/astock chart <code>           # 生成 K线图发送到 IM
/astock screen                 # 选股筛选
/astock briefing list          # 简报调度列表
/astock briefing next          # 下次简报时间
/astock briefing run <slot>    # 手动触发简报（premarket/midday/postmarket）
/astock watchlist list         # 显示关注列表
/astock watchlist add <codes>  # 添加关注
/astock watchlist remove <codes> # 移除关注
/astock watchlist clear        # 清空关注列表
```

### 1.5 定时简报系统

三个时段自动推送简报到 IM 用户：

| 时段 | 时间（Asia/Shanghai） | 简报内容 |
|------|----------------------|----------|
| 早盘前简报 | 07:50（默认） | 关注股票最新价/涨跌幅 + 开盘看点 |
| 午间简报 | 12:05（默认） | 上午表现 + 下午看点/关键位 |
| 收盘简报 | 18:30（默认） | 收盘总结 + 明日展望 + 研报公告提示 |

**简报特性：**
- 仅推送给设置了 `stock_watchlist` 的用户
- 自动跳过周六/周日（暂不识别法定节假日）
- 每个时段可自定义时间（config.astock.briefing.slots）
- 简报通过任务队列发送，LLM 可调用 stock_* 工具生成内容
- 自动追加免责声明

### 1.6 关注列表系统

**存储结构：**
```
scope: agent:{agent_id}:watchlist:{channel}:{peer_id}
kind: "watchlist"
text: "600519"
pinned: true  # 免疫记忆衰减
tags: ["stock_watchlist"]
importance: 0.9
```

**限制：**
- 上限 50 只股票（WATCHLIST_CAP）
- 每个 (agent, channel, peer) 独立列表
- 自动去重

**自动填充逻辑：**
`stock_snapshot` 无参数时：
1. 检查 `use_watchlist != false`
2. 检查 `codes.is_empty() && market.is_none() && ts.is_none()`
3. 从 memory store 读取当前 peer 的 watchlist
4. 用关注列表代码过滤快照

### 1.7 IM 渲染优化

返回 `markdown` 字段直接发送，免 LLM 重新格式化：

**报价单股格式：**
```
600519 贵州茅台  ¥1272.86  +0.07%  额39.84亿  量313.04万手
```

**报价批量格式：**
```
- 600519 贵州茅台  ¥1272.86  +0.07%  额39.84亿  量313.04万手
- 000001 平安银行  ¥10.23  -0.50%  额8.56亿  量84.32万手
```

**快照表格格式：**
```
**关注列表** (5 只, 按 amount)

| 代码 | 名称 | 现价 | 涨跌幅 | 成交额 |
|---|---|---:|---:|---:|
| 600519 | 贵州茅台 | 1272.86 | +0.07% | 39.84亿 |
| 000001 | 平安银行 | 10.23 | -0.50% | 8.56亿 |
```

**单位转换：**
- 成交额：万（≥1e4）、亿（≥1e8）
- 成交量：万手（≥1e4）、亿手（≥1e8）

---

## 2. CAP 编程代理协议

### 2.1 协议概述

CAP (CLI Agent Protocol) 是 RsClaw 通过 `cap-rs` 库驱动外部编码代理的协议层。支持将复杂编程任务委托给专业 CLI 工具。

**cap-rs 库：**
- 独立 Rust crate（已发布 crates.io 0.1.0）
- 提供 `Driver` trait + 多种 driver 实现
- stream-json 协议（主模式）+ ACP/MCP fallback

### 2.2 支持的 Coding Agent

| Agent | 标识符 | CLI 命令 | 特点 |
|-------|--------|----------|------|
| **Claude Code** | `claudecode` | `claude` | Anthropic 官方，工具调用最强 |
| **OpenClaude** | `openclaude` | `openclaude` | Claude 兼容的开源 fork |
| **OpenCode** | `opencode` | `opencode` | TUI 原生，迭代快速 |
| **Codex** | `codex` | `codex` | OpenAI Codex，推理能力强 |
| **Qoder** | `qoder` | `qodercli` | Claude Code 兼容替代 |

### 2.3 Driver 协议模式

| 模式 | 协议 | 适用场景 | 特点 |
|------|------|----------|------|
| **stream-json** | 主模式 | 新版 agent | 低开销、流式输出、会话持久化 |
| **ACP** | 降级模式 | OpenCode 老版本 | MCP-like 协议回退 |
| **MCP** | 降级模式 | Codex 老版本 | MCP server 模式回退 |

**spawn 自动降级：**
```rust
// 优先尝试 stream-json
match ClaudeCodeDriver::opencode_builder(cwd).spawn().await {
    Ok(d) => Box::new(d),
    Err(e) => {
        // 降级到 ACP
        Box::new(AcpDriver::opencode(&cwd).await?)
    }
}
```

### 2.4 LLM 工具层

Agent 可调用的 5 个 CAP 工具：

#### 2.4.1 `cap` — 异步任务提交

```json
{
  "name": "cap",
  "parameters": {
    "agent": "claudecode|openclaude|opencode|codex|qoder",
    "task": "任务描述",
    "cwd": "可选工作目录"
  },
  "returns": {
    "status": "submitted",
    "session_id": "cap-xxx-uuid"
  }
}
```

**特点：**
- 提交后立即返回，不阻塞当前轮次
- 结果异步推送到 IM + 注入 agent inbox
- 适合"fire-and-forget"式大任务
- 5 分钟超时保护

#### 2.4.2 `cap_live` — 同步交互式调用

```json
{
  "name": "cap_live",
  "parameters": {
    "agent": "claudecode|openclaude|opencode|codex|qoder",
    "task": "本轮提示",
    "session_id": "可选，继续已有会话",
    "cwd": "可选，新会话的工作目录"
  },
  "returns": {
    "session_id": "返回的会话 ID",
    "output": "agent 完整回复"
  }
}
```

**特点：**
- 等待完整回复后返回
- 多轮会话保持 driver 热启动（进程不重启）
- 适合编排多个 agent（codex 设计 → claude 实现 → opencode review）
- 全局会话上限 8 个

#### 2.4.3 `cap_live_end` — 结束会话

```json
{
  "name": "cap_live_end",
  "parameters": {
    "session_id": "要结束的会话 ID"
  },
  "returns": {
    "session_id": "...",
    "status": "closed"
  }
}
```

**用途：**
- 释放 driver 进程
- 避免占用全局会话上限
- 显式清理比依赖 idle GC 更可靠

#### 2.4.4 `cap_bind_sticky` — 粘性绑定（IM 直通）

```json
{
  "name": "cap_bind_sticky",
  "parameters": {
    "agent": "claudecode|openclaude|opencode|codex|qoder",
    "cwd": "可选工作目录"
  },
  "returns": {
    "agent": "...",
    "session_id": "...",
    "status": "bound"
  }
}
```

**特点：**
- 绑定后用户消息**绕过主 LLM**，直通 coding agent
- 相当于 `/cap <agent>` 的自然语言版本
- 自动注入 rsclaw memory recall 到首轮（上下文传递）

#### 2.4.5 `cap_unbind_sticky` — 解绑

```json
{
  "name": "cap_unbind_sticky",
  "parameters": {},
  "returns": {
    "status": "released|not_bound",
    "session_id": "...",
    "agent": "..."
  }
}
```

**特点：**
- 恢复主 LLM 对话模式
- 同步拆除底层 driver
- 相当于 `/cap-exit` 的自然语言版本
- 安全调用（无绑定时返回 not_bound）

### 2.5 斜杠命令

| 命令 | 功能 |
|------|------|
| `/cap <agent> [path]` | 打开并绑定粘性会话 |
| `/cap -h` `/cap help` | 显示 CAP 命令帮助 |
| `/cap-resume <agent> [session_id]` | 恢复历史会话（无 ID 则恢复最近） |
| `/cap-exit` | 释放粘性绑定 |

**会话恢复机制：**

| Agent | 恢复命令 | Session ID 来源 |
|-------|----------|-----------------|
| Claude Code | `claude --resume <uuid>` | ~/.claude/projects/<path>/<uuid>.jsonl |
| OpenClaude | `openclaude --resume <uuid>` | 同 Claude Code |
| OpenCode | `opencode run --session <id>` | 内部 ses_xxx 格式 |
| Codex | `codex exec resume <thread_id>` | 内部 thread_id |
| Qoder | `qodercli --resume <uuid>` | 同 Claude Code |

**`--continue-last` 模式：**
- `/cap-resume <agent>`（无 ID）恢复最近一个会话
- 各 agent CLI 的 `--continue` 等效命令

### 2.6 事件流（AgentEvent）

Driver 发出的事件类型：

```rust
AgentEvent::Ready { session_id }        // 启动就绪，含 agent 原生 session_id
AgentEvent::TextChunk { text, channel } // 文本流（Assistant/Thought/System）
AgentEvent::Thought { text }            // 推理过程（codex 专用）
AgentEvent::ToolCallStart { name, input } // 工具调用开始
AgentEvent::ToolCallEnd { is_error }    // 工具调用结束
AgentEvent::PermissionRequest { req_id, tool, risk_level } // 权限请求
AgentEvent::AskUser { ask_id, prompt }  // 交互提问
AgentEvent::Done { stop_reason, usage } // 轮次结束
```

**事件处理策略：**

| 事件 | 处理 |
|------|------|
| `Ready` | 捕获 native session_id，供 `/cap-resume` 使用 |
| `TextChunk` Assistant | 写入 reply_buf + 推送到 agent_event bus |
| `TextChunk` Thought | 推送到 bus（Thought channel），不写 reply |
| `ToolCallStart` | Debug 日志，不推 IM（防刷屏） |
| `PermissionRequest` | 自动批准（skip_permissions 模式） |
| `AskUser` | 自动取消（返回 "cancelled"） |
| `Done` | 结束本轮，推送完成通知到 IM |

### 2.7 资源治理

| 参数 | 默认值 | 说明 |
|------|--------|------|
| `max_sessions` | 8 | 全局 live 会话上限 |
| `idle_timeout` | 10 分钟 | 空闲会话自动回收 |
| `PROMPT_TIMEOUT` | 5 分钟 | 单次 prompt 超时 |
| `TURN_TIMEOUT` | 5 分钟 | 任务模式轮次超时 |

**粘性绑定切换：**
- `/cap codex` 切换时自动拆除前一个 driver
- 防止资源泄漏（旧 driver 空跑直到 idle GC）

### 2.8 编排模式示例

#### 多 Agent 协作（LLM 在中间）

```
用户: "帮我完成这个功能：先用 codex 设计架构，再让 claudecode 实现，最后 opencode review"

LLM:
1. cap_live { agent: "codex", task: "设计架构...", cwd: "~/myproj" }
   → session_id: "s1", output: "架构设计..."

2. cap_live { agent: "claudecode", task: "根据架构实现...", cwd: "~/myproj" }
   → session_id: "s2", output: "已实现..."

3. cap_live { agent: "opencode", task: "审查代码...", cwd: "~/myproj" }
   → session_id: "s3", output: "审查结果..."

4. cap_live_end { session_id: "s1" }
   cap_live_end { session_id: "s2" }
   cap_live_end { session_id: "s3" }

LLM 总结: "设计、实现、审查已完成，以下是各阶段关键输出..."
```

#### 用户直通模式（粘性绑定）

```
用户: "接下来让 claudecode 来帮我写代码"

LLM: cap_bind_sticky { agent: "claudecode" }
→ { status: "bound", session_id: "xxx" }

[后续用户消息直通 claudecode driver]
用户: "帮我实现一个 HTTP client"
→ claudecode 直接回复（绕过 rsclaw LLM）

用户: "回到正常对话"
LLM: cap_unbind_sticky {}
→ { status: "released" }
```

---

## 3. 斜杠命令完整列表

### 3.1 状态/版本类

| 命令 | 功能 | 示例 |
|------|------|------|
| `/version` | 显示 rsclaw 版本号 | `rsclaw v2026.6.12` |
| `/uptime` | 显示运行时长 | `up 3h 25m` |
| `/status` | 显示完整运行状态 | 模型、会话、任务、cap 绑定 |
| `/health` | 健康检查（快速响应） | `healthy` |
| `/help` `/?` | 显示命令帮助 | 分类列出所有命令 |

### 3.2 会话管理类

| 命令 | 功能 |
|------|------|
| `/new` | 开始新会话（清除历史） |
| `/clear` | 清除当前会话历史 |
| `/abort` | 中止当前/所有运行中的轮次 |
| `/sessions` | 列出所有会话 |

### 3.3 模型类

| 命令 | 功能 |
|------|------|
| `/model` | 显示当前使用的模型 |
| `/models` | 列出所有可用模型 |
| `/model <name>` | 切换主模型 |

### 3.4 任务/调度类

| 命令 | 功能 |
|------|------|
| `/task <任务>` | 多轮任务执行（自动迭代直到完成） |
| `/task -h` | 显示任务帮助 |
| `/task -n <N> -t <时长> <任务>` | 带参数的任务（最大轮数、超时） |
| `/loop <间隔> <提示>` | 定时循环执行（最小 2s） |
| `/loop -h` | 显示循环帮助 |
| `/cron` `/cron list` | 列出定时任务 |
| `/goal <condition>` | 结果驱动循环（模型自评 GOAL_ACHIEVED/FAILED 终止） |
| `/goal` | 显示当前 goal 进度 |
| `/goal clear` `/goal stop` `/goal abort` | 清除 goal |

**间隔格式：**
- `30s` — 30 秒
- `5m` — 5 分钟
- `1h` — 1 小时
- `2h30m` — 2 小时 30 分钟
- `1d` — 1 天

**Goal 示例：**
```
/goal cargo test 全过 --max 50
/goal 完成用户登录功能
/goal clear
```

### 3.5 A股类（/astock）

| 命令 | 功能 |
|------|------|
| `/astock` `/astock help` | A股帮助 |
| `/astock quote <code>` | 实时报价 |
| `/astock kline <code>` | K线摘要 |
| `/astock snapshot [codes...]` | 市场快照 |
| `/astock ask <query>` | 问财查询 |
| `/astock chart <code>` | K线图（PNG 发送） |
| `/astock screen` | 选股筛选 |
| `/astock briefing list` | 简报调度列表 |
| `/astock briefing next` | 下次简报时间 |
| `/astock briefing run <slot>` | 手动触发简报 |
| `/astock watchlist list` | 显示关注列表 |
| `/astock watchlist add <codes>` | 添加关注 |
| `/astock watchlist remove <codes>` | 移除关注 |
| `/astock watchlist clear` | 清空关注列表 |

### 3.6 文件/截图类

| 命令 | 功能 |
|------|------|
| `/ls [path]` | 列出工作区目录 |
| `/cat <file>` | 查看文件内容 |
| `/ss` `/screenshot` | 桌面截图 |
| `/webshot <url>` | 网页截图 |

### 3.7 技能/插件类

| 命令 | 功能 |
|------|------|
| `/skill list` | 列出已安装技能 |
| `/plugin` | 显示所有插件状态 |
| `/plugin <name>` | 显示单个插件信息 |
| `/plugin <name> off` | 隐藏插件 |
| `/plugin <name> on` | 启用插件（默认工具集） |
| `/plugin <name> all` | 注入所有工具 |
| `/plugin <name> <tools>` | 注入指定工具（逗号分隔） |
| `/plugin reset` | 重置所有插件覆盖 |
| `/plugin pin <plugin>__<tool>` | 固定工具到 user_tools |
| `/plugin unpin <plugin>__<tool>` | 取消固定 |
| `/plugin headlines <plugin>` | 显示插件 headline 工具 |

### 3.8 编程代理直连类

| 命令 | 功能 |
|------|------|
| `/cap <agent> [path]` | 绑定会话直连 coding agent |
| `/cap -h` | CAP 命令帮助 |
| `/cap-resume <agent> [session_id]` | 恢复历史会话 |
| `/cap-exit` | 释放绑定，恢复主 LLM |

**Agent 列表：** `claudecode`, `openclaude`, `opencode`, `codex`, `qoder`

### 3.9 事件监控类（/watch）

| 命令 | 功能 |
|------|------|
| `/watch <源> [flags]` | 实时推送事件到 chat |
| `/watch -h` | watch 命令帮助 |
| `/watch list` | 列出活跃监控 |
| `/watch stop <id>` | 停止指定监控 |
| `/watch stop all` | 停止所有监控 |

**源类型：**
- `/watch /path/to/file.log` — 文件跟踪（跨平台 tail -f）
- `/watch https://api/events` — SSE 流订阅
- `/watch shell tail -f x` — 原生 shell 命令

**Flags：**
- `--grep <regex>` — 仅推送匹配的事件
- `--event <type>` — 仅推送指定 SSE event 类型
- `--jq <expr>` — jq 过滤/转换（支持 `.codes[]` 数组展开）
- `--template <tpl>` — 输出模板（`${{.field}}` 取 JSON 字段）
- `--rate <ms>` — 限流（默认 2000ms；0=不限）
- `-H 'Header: value'` — SSE 请求头（支持 ${VAR} 环境变量）

**持久化：**
- 内存存储，重启清空
- 跨重启需用 `/loop 10m /watch <源>`

### 3.10 记忆类

| 命令 | 功能 |
|------|------|
| `/remember <fact>` | 添加记忆事实 |
| `/recall <query>` | 搜索记忆 |

### 3.11 其他类

| 命令 | 功能 |
|------|------|
| `/btw <问题>` | 旁路一次性提问（不写入会话历史） |
| `!cmd` | 在工作区执行 shell 命令 |
| `$cmd` | 同上（等效别名） |
| `/run <cmd>` | 同上 |
| `/sh <cmd>` | 同上 |
| `/exec <cmd>` | 同上 |

### 3.12 命令分类统计

| 类别 | 命令数量 |
|------|----------|
| 状态/版本 | 5 |
| 会话管理 | 4 |
| 模型 | 3 |
| 任务/调度 | 8 |
| A股 | 15+ |
| 文件/截图 | 4 |
| 技能/插件 | 10+ |
| 编程代理 | 4 |
| 事件监控 | 5+ |
| 记忆 | 2 |
| 其他 | 6 |
| **总计** | **~60** |

---

## 4. 附录：配置说明

### 4.1 A股配置

```json5
// ~/.rsclaw/rsclaw.json5
astock: {
  enabled: true,
  baseUrl: "http://localhost:8080",  // astock gateway 地址
  auth_token: "your-token",          // 可选，支持 SecretOrString
  briefing: {
    slots: {
      premarket: "07:50",   // 可自定义时间
      midday: "12:05",
      postmarket: "18:30"
    }
  }
}
```

### 4.2 CAP 配置

CAP 目前无独立配置块，依赖各 CLI 工具已安装且在 PATH 中：

- `claude` — Claude Code CLI
- `openclaude` — OpenClaude CLI
- `opencode` — OpenCode CLI
- `codex` — Codex CLI
- `qodercli` — Qoder CLI（或通过 $QODER_BIN 环境变量指定）

### 4.3 Memory 配置（关注列表依赖）

```json5
memory: {
  enabled: true,
  // 关注列表作为 memory doc 存储
  // scope: agent:{id}:watchlist:{channel}:{peer}
}
```

---

## 参考文件

| 文件路径 | 内容 |
|----------|------|
| `src/astock/mod.rs` | A股模块入口、全局 client |
| `src/astock/client.rs` | HTTP client 实现 |
| `src/astock/chart.rs` | K线图渲染 |
| `src/astock/briefing.rs` | 定时简报调度器 |
| `src/agent/tools_stock.rs` | LLM 工具实现 + IM 渲染 |
| `src/cli/stock.rs` | CLI 命令定义 |
| `src/cmd/stock.rs` | CLI 命令处理 |
| `src/cap/mod.rs` | CAP 模块入口 |
| `src/cap/runtime.rs` | CapAgentManager + driver spawn |
| `src/cap/live.rs` | CapLiveManager + sticky 绑定 |
| `src/cap/bridge.rs` | AgentEvent → sinks dispatch |
| `src/agent/tools_cap.rs` | LLM CAP 工具实现 |
| `src/gateway/preparse.rs` | 斜杠命令解析 + help 文本 |
| `src/agent/tools_builder.rs` | ToolDef 定义 |

---

## 更新记录

| 日期 | 版本 | 更新内容 |
|------|------|----------|
| 2026-06-12 | v1 | 初始版本，整理 astock、CAP、斜杠命令 |