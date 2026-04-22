// Agent Topology — prototype with mock data.
// No new dependencies — uses pure SVG rendering + CSS.
// Integration notes:
//   - Wire to GET /api/v1/topology/live (or WebSocket /ws/topology)
//   - Real layout: consider upgrading to @xyflow/react if interactive
//     editing / free dragging is needed. Current SVG is read-only visual.
// Add Path.AgentTopology = "/topology" in constant.ts.

import { useMemo, useState } from "react";
import { IconButton } from "./button";
import { ErrorBoundary } from "./error";
import styles from "./agent-topology.module.scss";
import ReturnIcon from "../icons/return.svg";
import ReloadIcon from "../icons/reload.svg";
import { useNavigate } from "react-router-dom";
import { Path } from "../constant";

type Kind = "main" | "named" | "sub" | "task";
type Backend = "native" | "claudecode" | "opencode" | "acp";
type Status = "running" | "idle" | "done" | "error";

interface AgentNode {
  id: string;
  name: string;
  kind: Kind;
  backend: Backend;
  status: Status;
  parent_id?: string;
  current_task?: string;
  tokens_in: number;
  tokens_out: number;
  started_at?: number;
  finished_at?: number;
  error_message?: string;
  is_remote?: boolean;
  remote_url?: string;
}

const NOW = Date.now();

const MOCK_NODES: AgentNode[] = [
  {
    id: "main",
    name: "main",
    kind: "main",
    backend: "native",
    status: "running",
    current_task: "帮我分析 3 个竞品网站的文案风格",
    tokens_in: 820,
    tokens_out: 2140,
    started_at: NOW - 4 * 60_000,
  },
  {
    id: "analyst",
    name: "analyst",
    kind: "sub",
    backend: "claudecode",
    status: "running",
    parent_id: "main",
    current_task: "分析 example.com 的文案风格",
    tokens_in: 1240,
    tokens_out: 3842,
    started_at: NOW - 2 * 60_000,
  },
  {
    id: "coder",
    name: "coder",
    kind: "sub",
    backend: "opencode",
    status: "running",
    parent_id: "main",
    current_task: "写一个 scrape 工具",
    tokens_in: 890,
    tokens_out: 1560,
    started_at: NOW - 100_000,
    is_remote: true,
    remote_url: "http://gpu-worker:18888",
  },
  {
    id: "crawl-a",
    name: "crawl-a",
    kind: "task",
    backend: "native",
    status: "running",
    parent_id: "analyst",
    current_task: "抓取 page 1-5",
    tokens_in: 120,
    tokens_out: 480,
    started_at: NOW - 60_000,
  },
  {
    id: "crawl-b",
    name: "crawl-b",
    kind: "task",
    backend: "native",
    status: "done",
    parent_id: "analyst",
    current_task: "抓取 page 6-10",
    tokens_in: 98,
    tokens_out: 320,
    started_at: NOW - 90_000,
    finished_at: NOW - 20_000,
  },
  {
    id: "review",
    name: "review",
    kind: "task",
    backend: "claudecode",
    status: "idle",
    parent_id: "coder",
    tokens_in: 0,
    tokens_out: 0,
  },
  {
    id: "test",
    name: "test",
    kind: "task",
    backend: "native",
    status: "error",
    parent_id: "coder",
    current_task: "运行单元测试",
    tokens_in: 45,
    tokens_out: 12,
    started_at: NOW - 40_000,
    finished_at: NOW - 15_000,
    error_message: "cargo: command not found on remote",
  },
];

const KIND_META: Record<Kind, { label: string; shape: "circle" | "ring" | "diamond" | "triangle" }> = {
  main: { label: "Main", shape: "circle" },
  named: { label: "Named", shape: "ring" },
  sub: { label: "Sub", shape: "diamond" },
  task: { label: "Task", shape: "triangle" },
};

const BACKEND_META: Record<Backend, { label: string; icon: string; color: string }> = {
  native: { label: "Native Rust", icon: "🦀", color: "#f97316" },
  claudecode: { label: "Claude Code", icon: "🤖", color: "#6366f1" },
  opencode: { label: "OpenCode", icon: "🧩", color: "#10b981" },
  acp: { label: "ACP", icon: "🔌", color: "#8b5cf6" },
};

const STATUS_META: Record<Status, { label: string; icon: string }> = {
  running: { label: "运行中", icon: "⚡" },
  idle: { label: "空闲", icon: "💤" },
  done: { label: "完成", icon: "✓" },
  error: { label: "失败", icon: "✗" },
};

function formatDuration(ms: number): string {
  const s = Math.floor(ms / 1000);
  if (s < 60) return `${s}s`;
  const m = Math.floor(s / 60);
  return `${m}m ${s % 60}s`;
}

/**
 * Simple tree layout: Main at top, children cascaded below.
 * Returns x/y positions for each node.
 */
function layoutTree(nodes: AgentNode[]): Record<string, { x: number; y: number }> {
  const byParent: Record<string, string[]> = {};
  for (const n of nodes) {
    const parent = n.parent_id ?? "__root";
    (byParent[parent] ??= []).push(n.id);
  }

  const NODE_W = 140;
  const LEVEL_H = 110;
  const pos: Record<string, { x: number; y: number }> = {};

  function layout(id: string, depth: number, leftX: number): number {
    const children = byParent[id] ?? [];
    if (children.length === 0) {
      pos[id] = { x: leftX, y: depth * LEVEL_H };
      return leftX + NODE_W;
    }
    let x = leftX;
    for (const c of children) {
      x = layout(c, depth + 1, x);
    }
    const firstChild = children[0];
    const lastChild = children[children.length - 1];
    pos[id] = {
      x: (pos[firstChild].x + pos[lastChild].x) / 2,
      y: depth * LEVEL_H,
    };
    return x;
  }

  const roots = byParent["__root"] ?? [];
  let x = 0;
  for (const r of roots) x = layout(r, 0, x);
  return pos;
}

export function AgentTopologyPage() {
  const navigate = useNavigate();
  const [nodes] = useState(MOCK_NODES);
  const [selectedId, setSelectedId] = useState<string | null>("analyst");
  const [onlyRunning, setOnlyRunning] = useState(false);

  const visibleNodes = useMemo(
    () => (onlyRunning ? nodes.filter((n) => n.status === "running") : nodes),
    [nodes, onlyRunning],
  );

  const layout = useMemo(() => layoutTree(visibleNodes), [visibleNodes]);
  const selected = visibleNodes.find((n) => n.id === selectedId);

  const bbox = useMemo(() => {
    const xs = Object.values(layout).map((p) => p.x);
    const ys = Object.values(layout).map((p) => p.y);
    if (xs.length === 0) return { w: 400, h: 200 };
    return {
      w: Math.max(...xs) + 160,
      h: Math.max(...ys) + 100,
    };
  }, [layout]);

  function renderNode(n: AgentNode) {
    const pos = layout[n.id];
    if (!pos) return null;
    const backend = BACKEND_META[n.backend];
    const shape = KIND_META[n.kind].shape;
    const isSelected = selectedId === n.id;
    const isActive = n.status === "running";

    const cx = pos.x + 70;
    const cy = pos.y + 40;
    const r = 26;

    let shapeEl: JSX.Element;
    if (shape === "circle") {
      shapeEl = <circle cx={cx} cy={cy} r={r} fill={backend.color} />;
    } else if (shape === "ring") {
      shapeEl = (
        <circle
          cx={cx}
          cy={cy}
          r={r}
          fill="var(--white)"
          stroke={backend.color}
          strokeWidth={4}
        />
      );
    } else if (shape === "diamond") {
      const d = r;
      shapeEl = (
        <polygon
          points={`${cx},${cy - d} ${cx + d},${cy} ${cx},${cy + d} ${cx - d},${cy}`}
          fill={backend.color}
        />
      );
    } else {
      const d = r;
      shapeEl = (
        <polygon
          points={`${cx - d},${cy + d * 0.7} ${cx + d},${cy + d * 0.7} ${cx},${cy - d * 0.9}`}
          fill={backend.color}
        />
      );
    }

    return (
      <g
        key={n.id}
        className={`${styles["node-group"]} ${isSelected ? styles["selected"] : ""} ${
          isActive ? styles["pulse"] : ""
        }`}
        onClick={() => setSelectedId(n.id)}
        style={{ cursor: "pointer" }}
      >
        {isSelected && (
          <circle
            cx={cx}
            cy={cy}
            r={r + 8}
            fill="none"
            stroke={backend.color}
            strokeWidth={2}
            strokeDasharray="4 3"
          />
        )}
        {shapeEl}
        <text
          x={cx}
          y={cy + 4}
          textAnchor="middle"
          fontSize="14"
          fill="white"
          style={{ pointerEvents: "none", fontWeight: 600 }}
        >
          {backend.icon}
        </text>
        <text
          x={cx}
          y={cy + r + 16}
          textAnchor="middle"
          fontSize="12"
          fill="var(--black)"
          style={{ pointerEvents: "none" }}
        >
          {n.name}
        </text>
        <text
          x={cx}
          y={cy + r + 30}
          textAnchor="middle"
          fontSize="10"
          fill="var(--black)"
          opacity="0.6"
          style={{ pointerEvents: "none" }}
        >
          {STATUS_META[n.status].icon} {STATUS_META[n.status].label}
        </text>
      </g>
    );
  }

  function renderEdge(child: AgentNode) {
    if (!child.parent_id) return null;
    const cPos = layout[child.id];
    const pPos = layout[child.parent_id];
    if (!cPos || !pPos) return null;
    const x1 = pPos.x + 70;
    const y1 = pPos.y + 66;
    const x2 = cPos.x + 70;
    const y2 = cPos.y + 14;
    const mid = (y1 + y2) / 2;
    const dasharray = child.is_remote ? "5 4" : undefined;
    return (
      <path
        key={`edge-${child.id}`}
        d={`M ${x1} ${y1} C ${x1} ${mid}, ${x2} ${mid}, ${x2} ${y2}`}
        fill="none"
        stroke="var(--bar-color)"
        strokeWidth={1.5}
        strokeDasharray={dasharray}
      />
    );
  }

  return (
    <ErrorBoundary>
      <div className={styles["topology-page"]}>
        <div className="window-header">
          <div className="window-header-title">
            <div className="window-header-main-title">🎯 Agent 拓扑</div>
            <div className="window-header-sub-title">
              {visibleNodes.length} 个节点 ·{" "}
              {visibleNodes.filter((n) => n.status === "running").length} 运行中
            </div>
          </div>
          <div className="window-actions">
            <div className="window-action-button">
              <IconButton
                icon={<ReloadIcon />}
                bordered
                text={onlyRunning ? "全部" : "只看运行中"}
                onClick={() => setOnlyRunning((v) => !v)}
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

        <div className={styles["topology-body"]}>
          <div className={styles["legend"]}>
            <div className={styles["legend-group"]}>
              <span className={styles["legend-label"]}>生命周期:</span>
              <span className={styles["legend-item"]}>● Main</span>
              <span className={styles["legend-item"]}>○ Named</span>
              <span className={styles["legend-item"]}>◇ Sub</span>
              <span className={styles["legend-item"]}>▲ Task</span>
            </div>
            <div className={styles["legend-group"]}>
              <span className={styles["legend-label"]}>后端:</span>
              {(Object.keys(BACKEND_META) as Backend[]).map((b) => (
                <span key={b} className={styles["legend-item"]}>
                  <span
                    className={styles["legend-swatch"]}
                    style={{ background: BACKEND_META[b].color }}
                  />
                  {BACKEND_META[b].icon} {BACKEND_META[b].label}
                </span>
              ))}
            </div>
          </div>

          <div className={styles["topology-canvas"]}>
            <svg
              width={bbox.w}
              height={bbox.h}
              viewBox={`0 0 ${bbox.w} ${bbox.h}`}
              className={styles["svg-canvas"]}
            >
              <g>
                {visibleNodes.map(renderEdge)}
                {visibleNodes.map(renderNode)}
              </g>
            </svg>
          </div>

          {selected && (
            <div className={styles["detail-panel"]}>
              <div className={styles["detail-title"]}>
                {BACKEND_META[selected.backend].icon} {selected.name}
              </div>
              <div className={styles["detail-row"]}>
                <span className={styles["detail-key"]}>ID</span>
                <span>{selected.id}</span>
              </div>
              <div className={styles["detail-row"]}>
                <span className={styles["detail-key"]}>类型</span>
                <span>
                  {KIND_META[selected.kind].label} · {BACKEND_META[selected.backend].label}
                </span>
              </div>
              <div className={styles["detail-row"]}>
                <span className={styles["detail-key"]}>状态</span>
                <span>
                  {STATUS_META[selected.status].icon}{" "}
                  {STATUS_META[selected.status].label}
                </span>
              </div>
              {selected.is_remote && (
                <div className={styles["detail-row"]}>
                  <span className={styles["detail-key"]}>远程</span>
                  <span className={styles["detail-mono"]}>
                    {selected.remote_url}
                  </span>
                </div>
              )}
              {selected.current_task && (
                <div className={styles["detail-row"]}>
                  <span className={styles["detail-key"]}>当前任务</span>
                  <span>{selected.current_task}</span>
                </div>
              )}
              {selected.started_at && (
                <div className={styles["detail-row"]}>
                  <span className={styles["detail-key"]}>运行</span>
                  <span>
                    {formatDuration(
                      (selected.finished_at ?? NOW) - selected.started_at,
                    )}
                  </span>
                </div>
              )}
              <div className={styles["detail-row"]}>
                <span className={styles["detail-key"]}>Tokens</span>
                <span className={styles["detail-mono"]}>
                  {selected.tokens_in.toLocaleString()} in /{" "}
                  {selected.tokens_out.toLocaleString()} out
                </span>
              </div>
              {selected.error_message && (
                <div className={styles["detail-error"]}>
                  ✗ {selected.error_message}
                </div>
              )}
              <div className={styles["detail-actions"]}>
                {selected.status === "running" && (
                  <button className={styles["danger"]}>停止</button>
                )}
                <button>查看日志</button>
                <button>打开工作区</button>
              </div>
            </div>
          )}
        </div>
      </div>
    </ErrorBoundary>
  );
}
