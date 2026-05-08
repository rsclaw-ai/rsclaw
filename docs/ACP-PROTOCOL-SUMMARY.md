# ACP (Agent Client Protocol) 完整总结

> 基于 agent-client-protocol 官方文档学习总结
> 日期：2026-03-29

---

## 一、协议概述

### 1.1 核心概念

| 概念 | 说明 |
|------|------|
| **Client** | 管理环境、处理用户交互、控制资源访问 (IDE/编辑器/其他UI) |
| **Agent** | 使用LLM自主修改代码的程序 (OpenCode/Claude Code/Cursor等) |
| **Session** | Client和Agent之间的会话，包含独立上下文、历史和状态 |

### 1.2 消息类型

协议基于 **JSON-RPC 2.0**，有两种消息类型：

- **Methods (方法)**: 请求-响应对，期望返回结果或错误
- **Notifications (通知)**: 单向消息，不期望响应

### 1.3 重要规则

- 所有文件路径 **必须** 是绝对路径
- 行号从 1 开始 (1-based)
- 通知消息 **没有** id 字段
- 响应消息 **有** id 字段

---

## 二、完整调用流程

### 流程图

```
┌─────────────────────────────────────────────────────────────────┐
│                        ACP 调用流程                              │
├─────────────────────────────────────────────────────────────────┤
│                                                                 │
│  Client                                           Agent         │
│    │                                                │           │
│    │──── initialize ────────────────────────────────▶│           │
│    │     (协议版本协商 + 能力交换)                    │           │
│    │◀───────────────────────────────────── response ─│           │
│    │                                                │           │
│    │──── session/new ──────────────────────────────▶│           │
│    │     或 session/load (恢复已有会话)              │           │
│    │◀───────────────────────────────────── response ─│           │
│    │     (返回 sessionId)                           │           │
│    │                                                │           │
│    │──── session/prompt ───────────────────────────▶│           │
│    │     (发送用户消息)                              │           │
│    │                                                │           │
│    │◀─── session/update 通知 ───────────────────────│           │
│    │     (plan/agent_message_chunk/tool_call)       │           │
│    │◀─── session/update 通知 ───────────────────────│           │
│    │     (更多内容...)                               │           │
│    │                                                │           │
│    │◀───────────────────────────────────── response ─│           │
│    │     (stopReason + usage)                       │           │
│    │                                                │           │
└─────────────────────────────────────────────────────────────────┘
```

### 必须遵循的顺序

```
1. initialize     ← 必须首先完成
2. session/new    ← 或 session/load
3. session/prompt ← 发送消息
```

**重要**: 必须先完成 initialization 才能创建 session！

---

## 三、初始化阶段 (Initialization)

### 3.1 initialize 请求

```json
{
  "jsonrpc": "2.0",
  "id": 0,
  "method": "initialize",
  "params": {
    "protocolVersion": 1,
    "clientCapabilities": {
      "fs": {
        "readTextFile": true,
        "writeTextFile": true
      },
      "terminal": true
    },
    "clientInfo": {
      "name": "rsclaw",
      "title": "RSClaw Gateway",
      "version": "1.0.0"
    }
  }
}
```

### 3.2 initialize 响应

```json
{
  "jsonrpc": "2.0",
  "id": 0,
  "result": {
    "protocolVersion": 1,
    "agentCapabilities": {
      "loadSession": true,
      "promptCapabilities": {
        "image": true,
        "audio": false,
        "embeddedContext": true
      },
      "mcpCapabilities": {
        "http": true,
        "sse": false
      }
    },
    "agentInfo": {
      "name": "opencode",
      "title": "OpenCode",
      "version": "1.0.0"
    },
    "authMethods": []
  }
}
```

### 3.3 Client Capabilities

| 能力 | 方法 | 说明 |
|------|------|------|
| `fs.readTextFile` | `fs/read_text_file` | 读取文件内容 |
| `fs.writeTextFile` | `fs/write_text_file` | 写入文件内容 |
| `terminal` | `terminal/*` | 终端操作方法 |

### 3.4 Agent Capabilities

| 能力 | 说明 |
|------|------|
| `loadSession` | 支持 `session/load` 方法恢复会话 |
| `promptCapabilities.image` | 支持 Image 类型内容 |
| `promptCapabilities.audio` | 支持 Audio 类型内容 |
| `promptCapabilities.embeddedContext` | 支持 Resource 类型内容 |
| `mcpCapabilities.http` | 支持 HTTP 传输的 MCP 服务器 |
| `mcpCapabilities.sse` | 支持 SSE 传输的 MCP 服务器 (已弃用) |

---

## 四、会话阶段 (Session)

### 4.1 session/new - 创建新会话

**请求:**
```json
{
  "jsonrpc": "2.0",
  "id": 1,
  "method": "session/new",
  "params": {
    "cwd": "/absolute/path/to/project",
    "mcpServers": [
      {
        "name": "filesystem",
        "command": "/path/to/mcp-server",
        "args": ["--stdio"],
        "env": [
          {"name": "API_KEY", "value": "secret"}
        ]
      }
    ]
  }
}
```

**响应:**
```json
{
  "jsonrpc": "2.0",
  "id": 1,
  "result": {
    "sessionId": "sess_abc123def456"
  }
}
```

### 4.2 session/load - 恢复已有会话

**前提**: Agent 必须支持 `loadSession: true` 能力

**请求:**
```json
{
  "jsonrpc": "2.0",
  "id": 1,
  "method": "session/load",
  "params": {
    "sessionId": "sess_abc123def456",
    "cwd": "/absolute/path/to/project",
    "mcpServers": []
  }
}
```

**重要**: Agent 会通过 `session/update` 通知 replay 整个对话历史！

**响应:**
```json
{
  "jsonrpc": "2.0",
  "id": 1,
  "result": null
}
```

### 4.3 Session ID 用途

- 发送 prompt 请求 (`session/prompt`)
- 取消操作 (`session/cancel`)
- 恢复会话 (`session/load`)

---

## 五、Prompt Turn (核心交互)

### 5.1 session/prompt 请求

```json
{
  "jsonrpc": "2.0",
  "id": 2,
  "method": "session/prompt",
  "params": {
    "sessionId": "sess_abc123def456",
    "prompt": [
      {
        "type": "text",
        "text": "帮我写个python hello world函数"
      },
      {
        "type": "resource",
        "resource": {
          "uri": "file:///home/user/project/main.py",
          "mimeType": "text/x-python",
          "text": "def process_data(items):\n    for item in items:\n        print(item)"
        }
      }
    ]
  }
}
```

### 5.2 **关键：session/update 通知** 

**实际内容在通知中，不在响应中！**

通知格式 (无 id 字段):
```json
{
  "jsonrpc": "2.0",
  "method": "session/update",
  "params": {
    "sessionId": "sess_abc123def456",
    "update": {
      "sessionUpdate": "agent_message_chunk",
      "content": {
        "type": "text",
        "text": "正在为你编写代码..."
      }
    }
  }
}
```

### 5.3 sessionUpdate 类型

| sessionUpdate | 说明 | 包含字段 |
|---------------|------|---------|
| `plan` | Agent 计划 | `entries[]` |
| `user_message` | 完整用户消息 | `content` |
| `user_message_chunk` | 用户消息片段 | `content` |
| `agent_message` | 完整 Agent 消息 | `content` |
| `agent_message_chunk` | Agent 消息片段 | `content` |
| `tool_call` | 工具调用 | `toolCallId, title, kind, status` |
| `available_commands_update` | 可用命令更新 | `commands[]` |
| `mode_change` | 模式变更 | `modeId` |

### 5.4 session/prompt 最终响应

```json
{
  "jsonrpc": "2.0",
  "id": 2,
  "result": {
    "_meta": {},
    "stopReason": "end_turn",
    "usage": {
      "inputTokens": 177,
      "outputTokens": 106,
      "totalTokens": 283,
      "cachedReadTokens": 44360,
      "cachedWriteTokens": 13
    }
  }
}
```

**stopReason 可能的值:**
- `end_turn` - 正常结束
- `max_tokens` - 达到 token 上限
- `cancelled` - 被取消

---

## 六、Tool Calls (工具调用)

### 6.1 工具调用状态流程

```
pending → in_progress → completed
                     → failed
```

### 6.2 工具调用通知

**开始 (pending):**
```json
{
  "method": "session/update",
  "params": {
    "sessionId": "sess_xxx",
    "update": {
      "sessionUpdate": "tool_call",
      "toolCallId": "call_001",
      "title": "Reading configuration file",
      "kind": "read",
      "status": "pending"
    }
  }
}
```

**进行中 (in_progress):**
```json
{
  "method": "session/update",
  "params": {
    "update": {
      "sessionUpdate": "tool_call",
      "toolCallId": "call_001",
      "status": "in_progress"
    }
  }
}
```

**完成 (completed):**
```json
{
  "method": "session/update",
  "params": {
    "update": {
      "sessionUpdate": "tool_call",
      "toolCallId": "call_001",
      "status": "completed",
      "result": {
        "type": "text",
        "text": "file content here..."
      }
    }
  }
}
```

### 6.3 Tool Kind 类型

| Kind | 说明 |
|------|------|
| `read` | 读取文件或数据 |
| `edit` | 修改文件或内容 |
| `delete` | 删除文件或数据 |
| `move` | 移动或重命名文件 |
| `search` | 搜索信息 |
| `execute` | 运行命令或代码 |
| `think` | 内部推理或规划 |
| `fetch` | 获取外部数据 |
| `other` | 其他类型 (默认) |

### 6.4 权限请求

当需要用户授权时，Agent 发送 `session/request_permission`:

```json
{
  "method": "session/request_permission",
  "params": {
    "sessionId": "sess_xxx",
    "toolCallId": "call_001",
    "options": [
      {
        "id": "allow",
        "title": "Allow",
        "kind": "accept"
      },
      {
        "id": "deny", 
        "title": "Deny",
        "kind": "reject"
      }
    ]
  }
}
```

Client 响应权限决定后，Agent 继续执行。

---

## 七、内容类型 (Content Types)

### 7.1 Text
```json
{"type": "text", "text": "Hello World"}
```

### 7.2 Image
```json
{
  "type": "image",
  "source": {
    "type": "base64",
    "mediaType": "image/png",
    "data": "base64_encoded_data"
  }
}
```

### 7.3 Audio
```json
{
  "type": "audio",
  "source": {
    "type": "base64",
    "mediaType": "audio/wav",
    "data": "base64_encoded_data"
  }
}
```

### 7.4 Resource (嵌入内容)
```json
{
  "type": "resource",
  "resource": {
    "uri": "file:///path/to/file.py",
    "mimeType": "text/x-python",
    "text": "file content here"
  }
}
```

### 7.5 ResourceLink (引用)
```json
{
  "type": "resource_link",
  "uri": "file:///path/to/file.py",
  "mimeType": "text/x-python"
}
```

---

## 八、取消操作

### session/cancel 通知

```json
{
  "jsonrpc": "2.0",
  "method": "session/cancel",
  "params": {
    "sessionId": "sess_abc123def456"
  }
}
```

发送后，Agent 应立即停止当前操作，返回 `stopReason: "cancelled"` 的响应。

---

## 九、MCP 服务器配置

### 9.1 Stdio 传输 (必须支持)
```json
{
  "name": "filesystem",
  "command": "/path/to/mcp-server",
  "args": ["--stdio"],
  "env": [{"name": "API_KEY", "value": "secret"}]
}
```

### 9.2 HTTP 传输 (可选)
```json
{
  "type": "http",
  "name": "api-server",
  "url": "https://api.example.com/mcp",
  "headers": [
    {"name": "Authorization", "value": "Bearer token123"}
  ]
}
```

### 9.3 SSE 传输 (已弃用)
```json
{
  "type": "sse",
  "name": "event-stream",
  "url": "https://events.example.com/mcp",
  "headers": [{"name": "X-API-Key", "value": "apikey"}]
}
```

---

## 十、错误处理

### 标准错误格式

```json
{
  "jsonrpc": "2.0",
  "id": 1,
  "error": {
    "code": -32600,
    "message": "Invalid Request",
    "data": {"detail": "Additional info"}
  }
}
```

### 常见错误码

| Code | 说明 |
|------|------|
| -32700 | Parse error (JSON解析失败) |
| -32600 | Invalid Request |
| -32601 | Method not found |
| -32602 | Invalid params |
| -32603 | Internal error |

---

## 十一、关键要点总结

### 核心概念

| 阶段 | 方法 | 内容位置 | 是否有 id |
|------|------|---------|----------|
| 初始化 | `initialize` | response | ✓ |
| 创建会话 | `session/new` | response | ✓ |
| 恢复会话 | `session/load` | **通知** | ✗ |
| 发送消息 | `session/prompt` | **通知** | ✗ |
| 工具调用 | `session/update` | **通知** | ✗ |
| 最终响应 | `session/prompt` | response (只有 stopReason) | ✓ |

### 必须记住的规则

1. **顺序**: initialize → session/new → session/prompt
2. **内容在通知中**: 实际文本、工具调用等都在 `session/update` 通知中
3. **通知无 id**: 通知消息没有 id 字段，响应有 id 字段
4. **最终响应**: 只有 stopReason 和 usage，没有实际内容

### 实现要点

```rust
// 错误的实现 (只等待响应)
let resp = client.send_prompt(prompt).await?;
// resp 只有 stopReason，没有内容！

// 正确的实现
// 1. 发送 session/prompt
// 2. 同时收集 session/update 通知
// 3. 从通知中提取 agent_message_chunk 等内容
// 4. 最终响应只用于确认结束
```

---

## 十二、参考资源

- ACP 官方文档: https://agentclientprotocol.com/
- JSON-RPC 2.0 规范: https://www.jsonrpc.org/specification
- MCP 协议: https://modelcontextprotocol.io/

---

*文档生成时间: 2026-03-29*