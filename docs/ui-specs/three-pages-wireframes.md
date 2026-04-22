# 3 个新页面 · 线框图与交互设计

设计语言对齐现有 UI：
- 主色：`#f97316`（橙）· 次色：`#fef3e6` light / `#161618` dark
- 卡片：圆角 10px、1px border、`--white` 背景、首/尾圆角
- 字色：`--black`（主）、opacity 0.6（次要）
- 动画：`slide-in 0.3s ease`
- 风格参照：现有 `agent-manager.module.scss`

导航位置建议：现有左侧 sidebar 增加三个入口（Memory / Topology / A2A Nodes）

---

## 页面 1 · 🧠 记忆时间线

**作用**：让用户直接看到、搜索、编辑、删除 AI 记住的所有事情。对应首页 Highlights 第 1 条。

### 布局

```
┌────────────────────────────────────────────────────────┐
│  记忆                             [+ 手动添加记忆]     │   ← toolbar
├────────────────────────────────────────────────────────┤
│  🔍 搜索我的记忆...                                    │   ← search
│                                                        │
│  [全部 243]  [偏好 47]  [事实 112]  [对话摘要 84]     │   ← category tabs
│  [全部时间 ▾]  [置顶优先 ▾]                           │   ← filters
├────────────────────────────────────────────────────────┤
│                                                        │
│  📌 置顶                                               │
│  ┌────────────────────────────────────────────────┐   │
│  │ 🎓 用户偏好用 bun，不用 npm/yarn               │   │
│  │    偏好 · 从 12 次对话学到 · 置信度 94%        │   │
│  │    添加于 2026-04-10 · 最近用于 2 小时前       │   │
│  │                           [编辑] [取消置顶] [×] │   │
│  └────────────────────────────────────────────────┘   │
│                                                        │
│  今天                                                  │
│  ┌────────────────────────────────────────────────┐   │
│  │ 📝 项目 rsclaw-app 位于 ~/dev/rsclaw-app       │   │
│  │    事实 · 对话中提及 · 14:22                    │   │
│  │                           [编辑] [置顶] [删除]  │   │
│  ├────────────────────────────────────────────────┤   │
│  │ 💬 讨论了 Tauri v2 tray icon 配置               │   │
│  │    对话摘要 · Session #184 · 11:08              │   │
│  │                           [查看原对话] [删除]   │   │
│  └────────────────────────────────────────────────┘   │
│                                                        │
│  本周                                                  │
│  ...                                                   │
└────────────────────────────────────────────────────────┘
```

### 数据结构

```ts
interface Memory {
  id: string;
  content: string;
  category: 'preference' | 'fact' | 'summary' | 'custom';
  source: 'user_added' | 'learned' | 'conversation';
  confidence?: number;        // 0-1, 仅 learned 有
  observed_count?: number;    // 学到这条事实的对话次数
  session_id?: string;        // summary 关联的原对话
  created_at: number;
  last_accessed_at: number;
  pinned: boolean;
}
```

### 关键交互

| 动作 | 交互 | 触发后端 |
|---|---|---|
| 搜索 | 输入 200ms debounce | hnsw_rs 向量检索 + tantivy 全文 |
| 编辑 | 行内编辑，Esc 取消 / Enter 保存 | `PUT /api/memory/:id` |
| 删除 | 二次确认（modal） | `DELETE /api/memory/:id` |
| 手动添加 | 右上角按钮 → 弹窗（内容 + 分类） | `POST /api/memory` |
| 置顶 | 同步到 redb，列表顶部"置顶"分组 | 更新 `pinned` 字段 |
| 查看原对话 | 跳转到 chat 页面 + 定位到那条 | 路由跳转 + scrollTo |

### 空态 & 边界

- 空态："AI 还没学到关于你的事。[立即开始对话]"
- 搜索无结果：`没有匹配"{query}"的记忆。`
- 新手引导：第一次打开加一个 "3 个示例记忆" 的 banner

### 官网截图价值

**极高**。这一页直接把"长期记忆"从抽象概念变成可视的产品形态，用户一眼理解 RsClaw 在"记什么"。

---

## 页面 2 · 🎯 Agent 拓扑图

**作用**：可视化当前运行中的 agent 层级、backend 分布、任务状态。补充 `agent-manager`（它是列表/配置视图，这个是运行时视图）。

### 布局

```
┌────────────────────────────────────────────────────────┐
│  Agent 拓扑                     [只看运行中] [全部]     │   ← toolbar
├────────────────────────────────────────────────────────┤
│  Legend:                                                │
│  生命周期: ● Main  ○ Named  ◇ Sub  ▷ Task              │
│  后端:    🦀 Native  🤖 ClaudeCode  🧩 OpenCode  🔌 ACP│
│  状态:    ⚡ 运行中  💤 空闲  ✓ 完成  ✗ 失败           │
├────────────────────────────────────────────────────────┤
│                                                        │
│                    ●  main (🦀 Native) ⚡               │
│                    │   "帮我分析 3 个网站"              │
│                    │                                    │
│            ┌───────┴───────┐                           │
│            │               │                           │
│           ◇ analyst       ◇ coder                      │
│           (🤖 Claude) ⚡   (🧩 OpenCode) ⚡             │
│            │               │                           │
│        ┌───┴───┐       ┌───┴───┐                       │
│       ▷ crawl  ▷ crawl ▷ review ▷ test                 │
│       (🦀) ⚡  (🦀) ⚡ (🤖) 💤  (🦀) ✗                  │
│                                                        │
├────────────────────────────────────────────────────────┤
│  选中 analyst:                                          │
│  ────────────────────                                   │
│  ID: analyst-sub-a4f2                                   │
│  Kind: Sub · Backend: Claude Code                       │
│  Started: 14:22:08 · Running for 1m 42s                 │
│  Current task: "分析 example.com 的文案风格"            │
│  Tokens: 1,240 in / 3,842 out                           │
│  Spawned tasks: 2 running, 0 done                       │
│  [停止] [查看日志] [打开工作区]                         │
└────────────────────────────────────────────────────────┘
```

### 图的渲染

- **技术**：React + `reactflow` 或 `@xyflow/react`（轻量、免费、支持 dark mode）
- **节点形状**：按生命周期区分（Main=实心圆, Named=空心圆, Sub=菱形, Task=三角）
- **节点颜色**：按 backend 区分（4 色）
- **边缘**：实线 = spawn 关系；虚线 = A2A 远程调用
- **动画**：运行中的节点呼吸效果 `pulse 1.5s`

### 数据结构

```ts
interface AgentNode {
  id: string;
  name: string;
  kind: 'main' | 'named' | 'sub' | 'task';
  backend: 'native' | 'claudecode' | 'opencode' | 'acp';
  status: 'running' | 'idle' | 'done' | 'error';
  parent_id?: string;         // 层级关系
  current_task?: string;
  tokens_in: number;
  tokens_out: number;
  started_at?: number;
  finished_at?: number;
  error_message?: string;
  workspace_path?: string;
  is_remote?: boolean;        // A2A 远程 agent
  remote_gateway_url?: string;
}
```

### 关键交互

| 动作 | 交互 |
|---|---|
| 点击节点 | 右侧面板展示详情 |
| 双击节点 | 跳转到该 agent 的日志/对话 |
| 右键节点 | 菜单：停止 / 查看日志 / 打开工作区 |
| 拖动 | 自由布局（保存到 localStorage） |
| 缩放 | 鼠标滚轮 / 双指 |
| 筛选 "只看运行中" | 隐藏 idle/done/error 节点 |

### 实时更新

- WebSocket 订阅 `/ws/topology` channel
- 节点状态变化（started/finished/error）即时反映
- 刷新节流 200ms，避免闪烁

### 官网截图价值

**高**。一张图直接说清"多 backend + 多生命周期 + 并行"的差异化能力。

---

## 页面 3 · 🌐 跨机器任务分发（A2A 节点）

**作用**：管理本机 + 远程 A2A gateway 节点，展示任务在节点间的分发和健康状态。对应 Highlights 第 3 条"跨机协作"。

### 布局

```
┌────────────────────────────────────────────────────────┐
│  A2A 节点                     [+ 添加远程节点]          │   ← toolbar
├────────────────────────────────────────────────────────┤
│  概览                                                   │
│  ┌─────────────┐ ┌─────────────┐ ┌─────────────┐      │
│  │ 3 个节点    │ │ 7 个任务    │ │ 1.2k tok/s  │      │
│  │ 全部在线    │ │ 运行中      │ │ 全局吞吐    │      │
│  └─────────────┘ └─────────────┘ └─────────────┘      │
├────────────────────────────────────────────────────────┤
│  节点列表                                               │
│                                                        │
│  ┌────────────────────────────────────────────────┐   │
│  │ 🖥️  本机 · localhost:18888            ● 在线   │   │
│  │     Native Rust · 20MB RAM · 延迟 0ms           │   │
│  │     ▸ 3 个任务运行中 · 240 tok/s                │   │
│  │                      [查看任务 →] [打开日志]   │   │
│  ├────────────────────────────────────────────────┤   │
│  │ 💻  gpu-worker · http://10.0.0.5:18888 ● 在线 │   │
│  │     Native Rust · 1.2GB RAM · 延迟 12ms         │   │
│  │     ▸ 4 个任务运行中 · 890 tok/s                │   │
│  │     ↓ 接收来自 localhost 的任务 (3)             │   │
│  │                      [查看任务 →] [断开]       │   │
│  ├────────────────────────────────────────────────┤   │
│  │ 🔴  edge-pi · http://192.168.1.42:18888  ⚠    │   │
│  │     上次心跳 2 分钟前 · 重连中                  │   │
│  │                            [重试] [移除]       │   │
│  └────────────────────────────────────────────────┘   │
├────────────────────────────────────────────────────────┤
│  实时任务流（最新 20 条）                               │
│                                                        │
│  14:22:18  ▷ crawl-b7    localhost → gpu-worker  ⚡   │
│  14:22:15  ▷ review-a2   localhost → localhost   ✓   │
│  14:22:10  ▷ analyze-c9  gpu-worker (local)      ⚡   │
│  ...                                                   │
└────────────────────────────────────────────────────────┘
```

### 数据结构

```ts
interface A2ANode {
  id: string;
  name: string;
  url: string;
  type: 'local' | 'remote';
  status: 'online' | 'offline' | 'degraded';
  agent_card?: AgentCard;        // A2A /.well-known/agent.json
  metrics: {
    active_tasks: number;
    tokens_per_sec: number;
    memory_mb: number;
    latency_ms: number;          // 本机→该节点的 RTT
  };
  last_heartbeat: number;
  added_at: number;
  auth_token?: string;           // masked in UI
}

interface TaskFlow {
  id: string;
  task_name: string;
  from_node: string;
  to_node: string;
  started_at: number;
  status: 'running' | 'done' | 'failed';
  tokens_total?: number;
}
```

### 关键交互

| 动作 | 交互 | 后端 |
|---|---|---|
| 添加远程节点 | 弹窗：URL + token，自动拉 agent_card 验证 | `POST /api/a2a/nodes` |
| 点击节点 | 展开 → 显示当前任务列表 | 获取该节点 `/tasks` |
| 断开 | 二次确认 | 停止心跳，保留配置 |
| 重试 | 立即发心跳 | 检测后更新状态 |
| 实时任务流 | WebSocket `/ws/a2a-tasks` | 后端事件总线 |

### 关键视觉

- 节点状态用颜色编码：🟢 在线 / 🟡 降级 / 🔴 离线
- 任务流向用箭头可视化：`localhost → gpu-worker`
- 延迟数字用色阶（<50ms 绿 / 50-200ms 黄 / >200ms 红）

### 边界情况

- 没有远程节点：只显示 localhost + "[+ 添加你的第一个远程节点] 学习 A2A →"（引导 + 文档链接）
- 节点离线 > 5 分钟：自动标记 degraded，不再占任务调度
- Token 过期：弹提示 + 快捷跳转到节点设置

### 官网截图价值

**中高**。这页是 RsClaw 相对其他 agent 框架的独特卖点。但视觉复杂度高，截图里需要有"本机 + 2 个远程节点"的演示场景才能讲清楚。

---

## 技术栈建议

| 组件 | 推荐 | 理由 |
|---|---|---|
| 拓扑图 | `reactflow` / `@xyflow/react` | 支持自动布局、缩放、拖拽，免费、v12 支持 dark mode |
| 图表（页 3 概览） | `recharts` 或纯 CSS | 只是数字卡片，不需要重库 |
| 搜索 | 现有 `use-deferred-value` + 后端 API | 不引入新搜索库 |
| WebSocket | 现有 `useRsClawSocket` hook | 已有基础设施 |
| Toast/通知 | 现有的通知组件 | 保持一致 |

**新依赖**：只需要 `@xyflow/react` 一个（约 60KB gzipped）。

---

## 实施优先级建议

如果要挑顺序：

1. **Memory Timeline**（2-3 天） → 用户价值最高、实现难度中
2. **A2A 节点页面**（2 天） → 差异化，但部分 UI 可复用现有列表样式
3. **Agent 拓扑图**（3-4 天） → 最炫，但要引入新库 + 布局算法

**Phase 1 MVP**：只做 Memory Timeline。官网能放一张真实截图。
**Phase 2**：加 A2A 节点页。
**Phase 3**：拓扑图。

---

## 下一步

确认方向 OK 后：
- 要写 React 原型（能跑的 mock 版）？
- 或写实施 spec（给 ui-dev 落地用）？
- 或先只做 Memory Timeline 一页，快速验证？
