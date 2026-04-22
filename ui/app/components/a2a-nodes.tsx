// A2A Nodes — prototype with mock data.
// Integration notes:
//   - GET    /api/v1/a2a/nodes          list
//   - POST   /api/v1/a2a/nodes          add remote
//   - DELETE /api/v1/a2a/nodes/:id      remove
//   - POST   /api/v1/a2a/nodes/:id/ping manual heartbeat
//   - WS     /ws/a2a-tasks              real-time task flow
// Add Path.A2ANodes = "/a2a" in constant.ts.

import { useState } from "react";
import { IconButton } from "./button";
import { ErrorBoundary } from "./error";
import styles from "./a2a-nodes.module.scss";
import ReturnIcon from "../icons/return.svg";
import AddIcon from "../icons/add.svg";
import { useNavigate } from "react-router-dom";
import { Path } from "../constant";
import { showConfirm, showToast } from "./ui-lib";

type NodeStatus = "online" | "offline" | "degraded";

interface A2ANode {
  id: string;
  name: string;
  url: string;
  type: "local" | "remote";
  status: NodeStatus;
  active_tasks: number;
  tokens_per_sec: number;
  memory_mb: number;
  latency_ms: number;
  last_heartbeat: number;
}

interface TaskFlow {
  id: string;
  task_name: string;
  from_node: string;
  to_node: string;
  started_at: number;
  status: "running" | "done" | "failed";
}

const NOW = Date.now();

const MOCK_NODES: A2ANode[] = [
  {
    id: "local",
    name: "本机",
    url: "localhost:18888",
    type: "local",
    status: "online",
    active_tasks: 3,
    tokens_per_sec: 240,
    memory_mb: 22,
    latency_ms: 0,
    last_heartbeat: NOW,
  },
  {
    id: "gpu-worker",
    name: "gpu-worker",
    url: "http://10.0.0.5:18888",
    type: "remote",
    status: "online",
    active_tasks: 4,
    tokens_per_sec: 890,
    memory_mb: 1240,
    latency_ms: 12,
    last_heartbeat: NOW - 3000,
  },
  {
    id: "edge-pi",
    name: "edge-pi",
    url: "http://192.168.1.42:18888",
    type: "remote",
    status: "degraded",
    active_tasks: 0,
    tokens_per_sec: 0,
    memory_mb: 180,
    latency_ms: 340,
    last_heartbeat: NOW - 120_000,
  },
];

const MOCK_FLOW: TaskFlow[] = [
  {
    id: "t1",
    task_name: "crawl-b7",
    from_node: "本机",
    to_node: "gpu-worker",
    started_at: NOW - 6000,
    status: "running",
  },
  {
    id: "t2",
    task_name: "review-a2",
    from_node: "本机",
    to_node: "本机",
    started_at: NOW - 12_000,
    status: "done",
  },
  {
    id: "t3",
    task_name: "analyze-c9",
    from_node: "gpu-worker",
    to_node: "gpu-worker",
    started_at: NOW - 18_000,
    status: "running",
  },
  {
    id: "t4",
    task_name: "scrape-list",
    from_node: "本机",
    to_node: "gpu-worker",
    started_at: NOW - 36_000,
    status: "done",
  },
  {
    id: "t5",
    task_name: "edge-test",
    from_node: "本机",
    to_node: "edge-pi",
    started_at: NOW - 120_000,
    status: "failed",
  },
];

const STATUS_DOT: Record<NodeStatus, { color: string; label: string }> = {
  online: { color: "#10b981", label: "在线" },
  degraded: { color: "#f59e0b", label: "降级" },
  offline: { color: "#ef4444", label: "离线" },
};

function latencyClass(ms: number): string {
  if (ms < 50) return "good";
  if (ms < 200) return "warn";
  return "bad";
}

function formatTime(ts: number): string {
  return new Date(ts).toTimeString().slice(0, 8);
}

function formatHeartbeat(ts: number): string {
  const diff = NOW - ts;
  if (diff < 10_000) return "刚刚";
  if (diff < 60_000) return `${Math.floor(diff / 1000)} 秒前`;
  return `${Math.floor(diff / 60_000)} 分钟前`;
}

export function A2ANodesPage() {
  const navigate = useNavigate();
  const [nodes, setNodes] = useState(MOCK_NODES);
  const [flow] = useState(MOCK_FLOW);
  const [showAdd, setShowAdd] = useState(false);
  const [addUrl, setAddUrl] = useState("");
  const [addToken, setAddToken] = useState("");

  const totalActive = nodes.reduce((s, n) => s + n.active_tasks, 0);
  const totalTps = nodes.reduce((s, n) => s + n.tokens_per_sec, 0);
  const onlineCount = nodes.filter((n) => n.status === "online").length;

  async function handleRemove(n: A2ANode) {
    const ok = await showConfirm(`断开连接：${n.name}（${n.url}）？`);
    if (!ok) return;
    setNodes((prev) => prev.filter((x) => x.id !== n.id));
    showToast("已断开");
  }

  function handleAdd() {
    const url = addUrl.trim();
    if (!url) return;
    const id = `node-${Date.now()}`;
    const name = url.replace(/^https?:\/\//, "").split(":")[0];
    setNodes((prev) => [
      ...prev,
      {
        id,
        name,
        url,
        type: "remote",
        status: "online",
        active_tasks: 0,
        tokens_per_sec: 0,
        memory_mb: 0,
        latency_ms: 45,
        last_heartbeat: Date.now(),
      },
    ]);
    setAddUrl("");
    setAddToken("");
    setShowAdd(false);
    showToast(`已连接到 ${name}`);
  }

  return (
    <ErrorBoundary>
      <div className={styles["a2a-page"]}>
        <div className="window-header">
          <div className="window-header-title">
            <div className="window-header-main-title">🌐 A2A 节点</div>
            <div className="window-header-sub-title">
              {nodes.length} 个节点 · {onlineCount} 在线
            </div>
          </div>
          <div className="window-actions">
            <div className="window-action-button">
              <IconButton
                icon={<AddIcon />}
                bordered
                text="添加远程节点"
                onClick={() => setShowAdd(true)}
              />
            </div>
            <div className="window-action-button">
              <IconButton
                icon={<ReturnIcon />}
                bordered
                onClick={() => navigate(Path.Home)}
              />
            </div>
          </div>
        </div>

        <div className={styles["a2a-body"]}>
          <div className={styles["overview"]}>
            <div className={styles["stat-card"]}>
              <div className={styles["stat-value"]}>{nodes.length}</div>
              <div className={styles["stat-label"]}>
                个节点 · {onlineCount} 在线
              </div>
            </div>
            <div className={styles["stat-card"]}>
              <div className={styles["stat-value"]}>{totalActive}</div>
              <div className={styles["stat-label"]}>活跃任务</div>
            </div>
            <div className={styles["stat-card"]}>
              <div className={styles["stat-value"]}>
                {totalTps >= 1000
                  ? (totalTps / 1000).toFixed(1) + "k"
                  : totalTps}
              </div>
              <div className={styles["stat-label"]}>tok/s 全局吞吐</div>
            </div>
          </div>

          <div className={styles["section-title"]}>节点列表</div>
          <div className={styles["node-list"]}>
            {nodes.map((n) => {
              const dot = STATUS_DOT[n.status];
              return (
                <div key={n.id} className={styles["node-card"]}>
                  <div className={styles["node-card-row"]}>
                    <span className={styles["node-icon"]}>
                      {n.type === "local" ? "🖥️" : "💻"}
                    </span>
                    <span className={styles["node-name"]}>{n.name}</span>
                    <span className={styles["node-url"]}>{n.url}</span>
                    <span
                      className={styles["node-status"]}
                      style={{ color: dot.color }}
                    >
                      ● {dot.label}
                    </span>
                  </div>
                  <div className={styles["node-meta"]}>
                    {n.status === "online" && (
                      <>
                        <span>
                          {n.memory_mb >= 1024
                            ? (n.memory_mb / 1024).toFixed(1) + " GB"
                            : n.memory_mb + " MB"}{" "}
                          RAM
                        </span>
                        <span className={styles["dot"]}>·</span>
                        <span
                          className={`${styles["latency"]} ${
                            styles[latencyClass(n.latency_ms)]
                          }`}
                        >
                          延迟 {n.latency_ms}ms
                        </span>
                        <span className={styles["dot"]}>·</span>
                        <span>
                          {n.active_tasks} 个任务 · {n.tokens_per_sec} tok/s
                        </span>
                      </>
                    )}
                    {n.status !== "online" && (
                      <span className={styles["warn-text"]}>
                        上次心跳 {formatHeartbeat(n.last_heartbeat)} · 重连中
                      </span>
                    )}
                  </div>
                  <div className={styles["node-actions"]}>
                    <button>查看任务 →</button>
                    <button>日志</button>
                    {n.type === "remote" && n.status !== "online" && (
                      <button>重试</button>
                    )}
                    {n.type === "remote" && (
                      <button
                        className={styles["danger"]}
                        onClick={() => handleRemove(n)}
                      >
                        断开
                      </button>
                    )}
                  </div>
                </div>
              );
            })}
          </div>

          <div className={styles["section-title"]}>
            实时任务流{" "}
            <span className={styles["section-sub"]}>
              最近 {flow.length} 条
            </span>
          </div>
          <div className={styles["flow-list"]}>
            {flow.map((t) => (
              <div key={t.id} className={styles["flow-row"]}>
                <span className={styles["flow-time"]}>
                  {formatTime(t.started_at)}
                </span>
                <span className={styles["flow-name"]}>▸ {t.task_name}</span>
                <span className={styles["flow-route"]}>
                  {t.from_node}
                  <span className={styles["flow-arrow"]}>→</span>
                  {t.to_node}
                </span>
                <span
                  className={`${styles["flow-status"]} ${
                    styles[`flow-${t.status}`]
                  }`}
                >
                  {t.status === "running"
                    ? "⚡ 运行中"
                    : t.status === "done"
                    ? "✓ 完成"
                    : "✗ 失败"}
                </span>
              </div>
            ))}
          </div>
        </div>

        {showAdd && (
          <div
            className={styles["modal-mask"]}
            onClick={() => setShowAdd(false)}
          >
            <div
              className={styles["modal"]}
              onClick={(e) => e.stopPropagation()}
            >
              <div className={styles["modal-title"]}>添加远程 A2A 节点</div>
              <div className={styles["modal-hint"]}>
                远程节点必须启用 A2A 协议并暴露{" "}
                <code>/.well-known/agent.json</code>
              </div>
              <label className={styles["modal-label"]}>节点 URL</label>
              <input
                autoFocus
                className={styles["modal-input"]}
                placeholder="http://10.0.0.5:18888"
                value={addUrl}
                onChange={(e) => setAddUrl(e.target.value)}
              />
              <label className={styles["modal-label"]}>
                认证 Token（可选）
              </label>
              <input
                className={styles["modal-input"]}
                placeholder="Bearer token"
                type="password"
                value={addToken}
                onChange={(e) => setAddToken(e.target.value)}
              />
              <div className={styles["modal-actions"]}>
                <button onClick={() => setShowAdd(false)}>取消</button>
                <button
                  className={styles["primary"]}
                  onClick={handleAdd}
                  disabled={!addUrl.trim()}
                >
                  连接
                </button>
              </div>
            </div>
          </div>
        )}
      </div>
    </ErrorBoundary>
  );
}
