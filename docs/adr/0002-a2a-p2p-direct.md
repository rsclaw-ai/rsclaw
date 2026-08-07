# ADR 0002: A2A P2P 内网穿透直连

- **Status**: Proposed
- **Date**: 2026-07-17

## Context

### 当前状态

RsClaw A2A 有三条跨网关通信路径，**没有一条能做内网穿透直连**：

| 路径 | 机制 | 内网穿透 | 问题 |
|---|---|---|---|
| HTTP 出站 (`dispatch.rs:665`) | `A2aClient` POST + SSE | ❌ 远端需公网可达 | 单向；无连接复用 |
| Hub-Spoke 中继 (`relay.rs`) | spoke WS→hub→转发→spoke | ❌ hub 中转所有数据 | 延迟翻倍；hub 是单点瓶颈 |
| Tailscale | `BindMode::Tailnet` | ✅ 但依赖外部网络 | 需要所有节点装 Tailscale |

**关键认知纠正**：当前 `relay.rs`（2100 行）的 hub 是**纯转发器**，不是打洞辅助。
所有 Request/Response/Event 帧都经过 hub 中转。代码里没有任何 candidate 交换、
STUN/TURN、打洞后直连的逻辑。注释说的"outbound-WS transport that lets NAT/private
nodes attach to a hub"——是让 NAT 后的节点连上 hub，然后 hub 转发，不是打洞。

### 目标

做成**真正的 P2P 内网穿透直连**：

1. 两个 NAT 后的 rsclaw gateway 通过 hub 交换地址候选（candidate）
2. 尝试 NAT 打洞，成功则 WS 直连——数据不再经过 hub
3. 打洞失败则降级回 hub 中转（现有路径，已验证可靠）

```
Phase 1: 打洞前
  Spoke A ←─WS─→ Hub ←─WS─→ Spoke B    (hub 中转，已有)

Phase 2: 打洞成功后
  Spoke A ←─────WS 直连──────→ Spoke B   (数据不经过 hub)
  Spoke A ←─WS─→ Hub ←─WS─→ Spoke B    (控制通道保留，用于路由/发现)
```

## Decision

### 架构：Hub 辅助打洞 + P2P 直连数据通道

Hub 的角色从**转发器**升级为**打洞协调器**：

- **控制通道**（spoke ↔ hub，已有）：路由注册、地址候选交换、peer 发现
- **数据通道**（spoke ↔ spoke，新增）：打洞成功后直连，传输 A2A Request/Response/Event

### 新增 RelayFrame 变体

在现有 `RelayFrame` 上扩展三个帧：

```rust
pub enum RelayFrame {
    // ... 现有帧不变 ...

    /// Spoke → Hub: 申报自己的地址候选（公网反射地址 + 本地地址）
    /// Hub 转发给目标 spoke，启动打洞流程
    PeerCandidate {
        target_node: String,      // 想连谁
        candidates: Vec<Candidate>, // 自己的地址候选
    },

    /// Hub → Spoke: 转发对端的候选地址，开始打洞
    PeerCandidateRelay {
        source_node: String,
        candidates: Vec<Candidate>,
    },

    /// Spoke → Hub → Spoke: 打洞结果通知
    /// 成功后 hub 更新路由表，后续数据帧走直连
    PeerConnected {
        peer_node: String,
        direct_url: String,       // 打洞成功的直连地址
    },
}

pub struct Candidate {
    /// 候选类型：host（本地地址）/ srflx（STUN 反射地址）/ relay（TURN 中继地址）
    pub kind: CandidateKind,
    /// WebSocket URL，如 ws://192.168.1.5:18889/a2a/peer/ws
    pub url: String,
    /// 优先级（越高越优先尝试）
    pub priority: u32,
}
```

### 打洞流程

```
Step 1: Spoke A 想直连 Spoke B
  A 收集候选地址：
    - host: ws://<lan_ip>:18889/a2a/peer/ws
    - srflx: STUN 查询得到 ws://<public_ip>:<nat_port>/a2a/peer/ws
  A → Hub: PeerCandidate { target_node: "B", candidates: [...] }

Step 2: Hub 转发给 B
  Hub → B: PeerCandidateRelay { source_node: "A", candidates: [...] }

Step 3: B 也收集自己的候选，双向尝试
  B 收集候选 → B → Hub: PeerCandidate { target_node: "A", candidates: [...] }
  Hub → A: PeerCandidateRelay { source_node: "B", candidates: [...] }

Step 4: A 和 B 同时尝试 WS 连接对方的候选地址
  A → ws://B候选1 → 失败
  A → ws://B候选2 → 成功!
  B → ws://A候选1 → 成功! (可能同时成功)

Step 5: 打洞成功方通知 hub
  A → Hub: PeerConnected { peer_node: "B", direct_url: "ws://..." }
  Hub 更新路由表: A↔B 标记为 "direct"

Step 6: 后续 A→B 的 Request/Response/Event 走直连 WS
  A → B 直连 WS: RelayFrame::Request { ... }
  B → A 直连 WS: RelayFrame::Response { ... }
```

### NAT 打洞的可行性

| NAT 类型 | 打洞可行性 | 说明 |
|---|---|---|
| Full Cone | ✅ | 任何外部主机都能通过映射地址访问 |
| Restricted Cone | ✅ | 只需 A 先向 B 的地址发包"开洞" |
| Port Restricted Cone | ✅ | 同上，需先向 B 的 IP:port 包 |
| Symmetric NAT | ⚠️ 需要 TURN | 公网端口不可预测，需要 TURN 中继 |

实际部署中大多数家用/办公 NAT 是 Cone 类型，打洞成功率高。
Symmetric NAT 场景降级为 TURN 中继或 hub 转发。

### STUN / TURN

**STUN**（获取公网反射地址）：
- 不引入外部 STUN 库——用已有的 `reqwest` 发一个 STUN binding request 即可
- STUN 协议很简单：UDP 发一个 20 字节 binding request，收 20+字节 response 提取 XOR-MAPPED-ADDRESS
- 或者更简单：发 HTTP 请求到公网服务（如 `https://api.ipify.org`）获取公网 IP，
  配合已知本地端口构造候选。不如 STUN 精确但零依赖
- 支持配置自定义 STUN 服务器（`gateway.a2a.stunUrls`）

**TURN**（Symmetric NAT 兜底）：
- 不自己实现 TURN 服务器——用标准 TURN 服务（如 coturn）
- TURN 候选地址本质是一个中继 WS URL，spoke 连 TURN 而不是直连
- Phase 1 可以先不实现 TURN，Symmetric NAT 降级回 hub 中转

### Peer WS 端点

新增 WS 入口（spoke 之间直连用）：

```
GET /a2a/peer/ws?node_id=<self>&token=<token>
```

与 hub 的 `/a2a/relay/ws` 独立。peer WS handler：
1. 验证 node_id + token（或 Ed25519 公钥）
2. 执行 `RelayFrame::Hello` 握手
3. 注册到 `PeerManager`（新模块）
4. 后续帧走与 hub-spoke 相同的 `handle_spoke_request` / `RelayFrame::Request` 逻辑

### PeerManager（新模块 `a2a/peer.rs`）

```rust
pub struct PeerManager {
    /// peer_node_id → 直连 WS 连接（打洞成功后建立）
    direct_connections: DashMap<String, PeerConnection>,
    /// peer_node_id → 候选地址缓存（从 hub 转发的 PeerCandidateRelay 学习）
    peer_candidates: DashMap<String, Vec<Candidate>>,
    /// agent_ref → peer_node_id 路由
    routes: DashMap<String, RouteEntry>,
    /// request_id → oneshot 等待器（同步 RPC）
    pending: DashMap<String, (oneshot::Sender<JsonRpcResponse>, String)>,
    /// request_id → broadcast（流式 RPC）
    stream_pending: DashMap<String, StreamPending>,
    /// task_id → agent_ref
    task_routes: DashMap<String, String>,
    metrics: RelayMetrics,
}

struct PeerConnection {
    /// 直连 WS 写入端
    tx: mpsc::UnboundedSender<RelayFrame>,
    peer_node_id: String,
    connected_at: Instant,
}
```

### 路由决策（dispatch_a2a 改造）

出站路由改为四段式：

```
1. 本地 registry？ → in-process mpsc（不变）

2. PeerManager 有直连？ → 走 peer WS 直连（打洞成功，最低延迟）

3. Hub 中继可达？ → 走 hub 转发（现有路径，NAT 后兜底）

4. 降级到 HTTP → A2aClient.send_streaming_message（最后兜底）
```

```rust
// dispatch.rs dispatch_a2a 改造
if let Some(peer_mgr) = &self.peer_manager {
    let target = format!("{}/{}", peer_node_id, agent_id);
    if peer_mgr.route_for(&target).is_some() {
        // 走 peer WS 直连
        return peer_mgr.invoke_streaming_and_drain(...).await;
    }
}
if let Some(hub) = &self.relay_hub {
    if hub.route_for(&target).is_some() {
        // 走 hub 中继
        return hub.invoke_streaming_and_drain(...).await;
    }
}
// 降级到 HTTP
let client = self.a2a_client_pool.get_or_create(&ext.url);
```

### Hub 路由表升级

`RelayHub.routes` 升级为两种路由：

```rust
pub struct RouteEntry {
    pub agent_ref: String,
    pub node_id: String,
    pub epoch: u64,
    pub expires_at: Instant,
    /// 新增：路由模式
    pub mode: RouteMode,
}

pub enum RouteMode {
    /// 通过 hub 中转（现有）
    Relayed,
    /// spoke 之间已打洞直连，hub 只做控制通道
    Direct,
}
```

`invoke_jsonrpc` / `invoke_streaming` 检查 `route.mode`：
- `Relayed` → 通过 hub WS 发 Request（现有逻辑）
- `Direct` → 告诉调用方"走 PeerManager 直连"，hub 不中转数据

### 配置

```json5
{
  gateway: {
    a2a_relay: {
      mode: "spoke",           // 或 "hub" / "peer"
      hubUrls: ["wss://hub.example.com/a2a/relay/ws"],
      nodeId: "node-a",
      // 新增：打洞配置
      peer: {
        enabled: true,
        stunUrls: ["stun:stun.l.google.com:19302"],
        // 可选：TURN 服务器（Symmetric NAT 兜底）
        turnUrls: ["turn:turn.example.com:3478"],
        turnUsername: "...",
        turnCredential: "...",
        // peer WS 监听端口（默认与 gateway.port 相同）
        listenPort: 18889,
      },
    },
  },
  agents: {
    a2a: [
      {
        id: "peer-b",
        url: "http://peer-b:18889",
        mode: "peer",
        nodeId: "node-b",
        publicKey: "base64-ed25519-public-key",
        authToken: "...",
        description: "Peer B 的能力",
      }
    ]
  }
}
```

## Implementation Plan

### Phase 1: PeerManager + Peer WS 端点（3 天）

- [ ] 新建 `crates/rsclaw-runtime/src/a2a/peer.rs`
  - `PeerManager` struct（connections / routes / pending / stream_pending / task_routes）
  - `invoke_jsonrpc` / `invoke_streaming` / `forward_stream_event` /
    `sweep_expired_streams`（从 `RelayHub` 提取共享逻辑）
  - `handle_peer_request`（复用 `handle_spoke_request` 的入站处理逻辑）
- [ ] 新增 `GET /a2a/peer/ws` WS upgrade handler
  - token / Ed25519 握手（复用 `relay_identity.rs`）
  - 注册到 `PeerManager`
- [ ] `A2aPeerConfig` 添加 `mode` / `node_id` / `public_key` 字段
- 验证：两个同网 gateway 通过 peer WS 直连，互发 SendMessage

### Phase 2: 地址候选收集 + STUN（2 天）

- [ ] `Candidate` 类型 + 收集逻辑
  - host 候选：枚举本地网卡 IP
  - srflx 候选：STUN binding request（自实现，20 字节 UDP 协议，零外部依赖）
  - 或 HTTP 公网 IP 查询（简化版，不如 STUN 精确但可用）
- [ ] `gateway.a2a_relay.peer` 配置块（stunUrls / listenPort）
- 验证：spoke 能收集并打印自己的候选地址列表

### Phase 3: 打洞协调 + 直连建立（3 天）

- [ ] 新增 `RelayFrame::PeerCandidate` / `PeerCandidateRelay` / `PeerConnected`
- [ ] Hub 转发候选地址（`handle_hub_frame` 新增分支）
- [ ] Spoke 收到候选后尝试 WS 连接（ICE-lite 风格：按优先级串行尝试）
- [ ] 打洞成功 → `PeerConnected` 通知 hub → hub 更新路由 mode=Direct
- [ ] 打洞失败 → 保持 mode=Relayed（降级回 hub 中转，现有路径）
- [ ] `RouteMode` enum + `RouteEntry.mode` 字段
- 验证：两个 NAT 后的 gateway 打洞成功，数据走直连 WS；关闭直连后降级回 hub

### Phase 4: 路由集成 + 出站决策（2 天）

- [ ] `dispatch_a2a` 四段式路由（local → peer direct → hub relay → HTTP）
- [ ] `RelayHub.invoke_*` 检查 `route.mode`，Direct 模式委托给 `PeerManager`
- [ ] `A2aClientPool` — per-peer `reqwest::Client` 共享（HTTP 降级路径）
- [ ] `task_routes` 在 peer 直连模式下也工作（从 Event 帧嗅探）
- 验证：LLM 调用 `agent_peer_b`，走 peer 直连，流式回传，Cancel 传播

### Phase 5: 双向调用 + Peer 发现（2 天）

- [ ] Hello 帧交换 AgentCard + `peer_cards` 缓存
- [ ] 定时 `refresh_peer_card`（TTL 5min）从 `/.well-known/agent.json` 拉取
- [ ] `build_agent_card` 在 peer 模式下 advertise 自己的 agent 列表
- [ ] 双向：B 也能主动调用 A 的 agent（A 的 peer WS handler 接收入站 Request）
- 验证：A→B 和 B→A 双向调用；agent card 自动发现

### Phase 6: 集成测试 + 文档（2 天）

- [ ] `tests/a2a_p2p.rs` — 全链路测试
  - 打洞成功：直连 + SendMessage + SendStreamingMessage + Cancel
  - 打洞失败：降级 hub 中转
  - 双向调用 A↔B
  - GetTask follow-up 通过直连
  - peer 断线后降级 hub
- [ ] 更新 `docs/interfaces/` A2A 接口文档
- [ ] 更新 `AGENTS.md` A2A 章节

### 总工时：~2 周

| Phase | 工时 | 产出 |
|---|---|---|
| 1 PeerManager + Peer WS | 3 天 | 同网直连可用 |
| 2 候选收集 + STUN | 2 天 | 地址候选可用 |
| 3 打洞协调 + 直连 | 3 天 | NAT 穿透可用 |
| 4 路由集成 | 2 天 | 出站自动走直连 |
| 5 双向 + 发现 | 2 天 | 双向调用 + agent card |
| 6 测试 + 文档 | 2 天 | 全链路验证 |

## Consequences

### 正面

- **内网穿透**：NAT 后的 gateway 之间直接通信，不需要公网暴露
- **低延迟**：打洞成功后数据不经过 hub，延迟减半
- **自动降级**：打洞失败 → hub 中转 → HTTP，三级降级保证可用性
- **高复用**：`RelayFrame` 协议、Ed25519 握手、`handle_spoke_request`、
  `RelayStreamGuard`、`task_routes` 全部复用
- **不引入重依赖**：STUN 协议自实现（20 字节 UDP），不需要 WebRTC/ICE 库

### 负面

- **NAT 类型限制**：Symmetric NAT 需要 TURN 兜底（Phase 1 可不实现，降级 hub）
- **hub 仍需部署**：打洞协调需要 hub 在线（但 hub 不中转数据，负载极低）
- **复杂度**：新增 peer.rs 模块 + 打洞逻辑，但通过从 RelayHub 提取共享逻辑控制

### 风险

1. **STUN 自实现**：STUN binding request 协议简单（RFC 5389 §6），但需处理
   XOR-MAPPED-ADDRESS 解析。备选：HTTP 公网 IP 查询（不精确但零依赖）。
2. **打洞时序**：双向同时连接可能产生竞争。ICE-lite 串行尝试可避免。
3. **WS 打洞 vs UDP 打洞**：WS 走 TCP，TCP 打洞比 UDP 复杂（需要同时 SYN）。
   备选方案：先用 UDP 打洞建立 NAT 映射，再在映射地址上建 WS。但这增加复杂度。
   更实际的方案：如果两端有任一端公网可达，直接 WS 连；都不可达时降级 hub。
4. **peer 连接与 relay 连接共存**：路由表需区分 Direct / Relayed 模式，
   `RouteMode` enum 已设计。

## Alternatives Considered

### A. 纯 HTTP 直连 + 连接池

不做 WS P2P，只给现有 HTTP 路径加连接池。

- 不能内网穿透（远程需公网可达）
- **排除**：不满足核心需求

### B. 纯 hub 中转（现有，不改）

不改架构，所有跨网关通信走 hub。

- hub 是瓶颈和单点
- 延迟翻倍
- **排除**：不满足"直连"需求

### C. WebRTC（libdatachannel）

用 WebRTC 的 ICE/STUN/TURN 完整栈做打洞。

- 优点：成熟的 NAT 穿透实现
- 缺点：引入 `libdatachannel` C++ 依赖 + DataChannel API；与现有 WS/RelayFrame
  协议栈不兼容；过度重量级
- **排除**：不符合 codebase 技术栈

### D. Tailscale / WireGuard

依赖 Tailscale 组网，所有节点在同一虚拟网络。

- 已部分支持（`BindMode::Tailnet`）
- 但要求所有节点安装 Tailscale 客户端
- **保留**：作为推荐部署方案，但不是唯一方案
