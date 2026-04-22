// Memory Timeline — prototype with mock data.
// Integration notes for ui-dev:
//   - Wire data fetching to GET  /api/v1/memory?category=&q=&since=
//   - Edit:   PUT    /api/v1/memory/:id
//   - Delete: DELETE /api/v1/memory/:id
//   - Create: POST   /api/v1/memory   body: { content, category }
//   - Pin:    PATCH  /api/v1/memory/:id/pin
// Add Path.Memory = "/memory" in constant.ts and register route in page.tsx.

import { useMemo, useState } from "react";
import { IconButton } from "./button";
import { ErrorBoundary } from "./error";
import styles from "./memory-timeline.module.scss";
import ReturnIcon from "../icons/return.svg";
import AddIcon from "../icons/add.svg";
import EditIcon from "../icons/edit.svg";
import DeleteIcon from "../icons/delete.svg";
import { useNavigate } from "react-router-dom";
import { Path } from "../constant";
import { showConfirm, showToast } from "./ui-lib";

type Category = "preference" | "fact" | "summary" | "custom";
type Source = "user_added" | "learned" | "conversation";

interface Memory {
  id: string;
  content: string;
  category: Category;
  source: Source;
  confidence?: number;
  observed_count?: number;
  session_id?: string;
  created_at: number;
  last_accessed_at: number;
  pinned: boolean;
}

const NOW = Date.now();
const HOUR = 3600_000;
const DAY = 24 * HOUR;

// Mock data — remove when wiring real API.
const MOCK_MEMORIES: Memory[] = [
  {
    id: "m1",
    content: "用户偏好用 bun 而不是 npm / yarn",
    category: "preference",
    source: "learned",
    confidence: 0.94,
    observed_count: 12,
    created_at: NOW - 7 * DAY,
    last_accessed_at: NOW - 2 * HOUR,
    pinned: true,
  },
  {
    id: "m2",
    content: "用户名字是 Oopos，邮箱 myoopos@gmail.com",
    category: "fact",
    source: "user_added",
    created_at: NOW - 30 * DAY,
    last_accessed_at: NOW - 12 * HOUR,
    pinned: true,
  },
  {
    id: "m3",
    content: "项目 rsclaw-app 位于 ~/dev/rsclaw-app，Tauri v2 + Rust",
    category: "fact",
    source: "conversation",
    created_at: NOW - 2 * HOUR,
    last_accessed_at: NOW - HOUR,
    pinned: false,
  },
  {
    id: "m4",
    content: "讨论了 Tauri v2 tray icon 配置（iconAsTemplate）",
    category: "summary",
    source: "conversation",
    session_id: "sess-184",
    created_at: NOW - 4 * HOUR,
    last_accessed_at: NOW - 3 * HOUR,
    pinned: false,
  },
  {
    id: "m5",
    content: "用户喜欢简洁、直接的沟通风格，不喜欢过度解释",
    category: "preference",
    source: "learned",
    confidence: 0.82,
    observed_count: 8,
    created_at: NOW - 3 * DAY,
    last_accessed_at: NOW - 6 * HOUR,
    pinned: false,
  },
  {
    id: "m6",
    content: "用户工作时区：UTC+8（北京时间）",
    category: "fact",
    source: "learned",
    confidence: 0.99,
    observed_count: 45,
    created_at: NOW - 21 * DAY,
    last_accessed_at: NOW - DAY,
    pinned: false,
  },
  {
    id: "m7",
    content: "完成了 RsClaw README 的 memory-first 定位重写 + MIT/Apache 换协议",
    category: "summary",
    source: "conversation",
    session_id: "sess-182",
    created_at: NOW - 2 * DAY,
    last_accessed_at: NOW - 2 * DAY,
    pinned: false,
  },
];

const CATEGORY_META: Record<Category, { label: string; icon: string }> = {
  preference: { label: "偏好", icon: "🎓" },
  fact: { label: "事实", icon: "📝" },
  summary: { label: "对话摘要", icon: "💬" },
  custom: { label: "自定义", icon: "✏️" },
};

function timeGroup(ts: number): string {
  const diff = NOW - ts;
  if (diff < DAY) return "今天";
  if (diff < 2 * DAY) return "昨天";
  if (diff < 7 * DAY) return "本周";
  if (diff < 30 * DAY) return "本月";
  return "更早";
}

function formatRelative(ts: number): string {
  const diff = NOW - ts;
  if (diff < HOUR) return `${Math.max(1, Math.floor(diff / 60000))} 分钟前`;
  if (diff < DAY) return `${Math.floor(diff / HOUR)} 小时前`;
  if (diff < 7 * DAY) return `${Math.floor(diff / DAY)} 天前`;
  return new Date(ts).toISOString().slice(0, 10);
}

export function MemoryTimelinePage() {
  const navigate = useNavigate();
  const [memories, setMemories] = useState<Memory[]>(MOCK_MEMORIES);
  const [query, setQuery] = useState("");
  const [activeCategory, setActiveCategory] = useState<Category | "all">("all");
  const [editingId, setEditingId] = useState<string | null>(null);
  const [editDraft, setEditDraft] = useState("");
  const [showAdd, setShowAdd] = useState(false);
  const [addContent, setAddContent] = useState("");
  const [addCategory, setAddCategory] = useState<Category>("fact");

  const counts = useMemo(() => {
    const c: Record<string, number> = { all: memories.length };
    for (const m of memories) c[m.category] = (c[m.category] ?? 0) + 1;
    return c;
  }, [memories]);

  const filtered = useMemo(() => {
    const q = query.trim().toLowerCase();
    return memories.filter((m) => {
      if (activeCategory !== "all" && m.category !== activeCategory) return false;
      if (q && !m.content.toLowerCase().includes(q)) return false;
      return true;
    });
  }, [memories, query, activeCategory]);

  const grouped = useMemo(() => {
    const pinned = filtered.filter((m) => m.pinned);
    const others = filtered.filter((m) => !m.pinned);
    const byGroup: Record<string, Memory[]> = {};
    for (const m of others) {
      const g = timeGroup(m.created_at);
      (byGroup[g] ??= []).push(m);
    }
    return { pinned, byGroup };
  }, [filtered]);

  function startEdit(m: Memory) {
    setEditingId(m.id);
    setEditDraft(m.content);
  }

  function saveEdit() {
    if (!editingId) return;
    setMemories((prev) =>
      prev.map((m) => (m.id === editingId ? { ...m, content: editDraft } : m)),
    );
    setEditingId(null);
    showToast("已更新");
  }

  function togglePin(m: Memory) {
    setMemories((prev) =>
      prev.map((x) => (x.id === m.id ? { ...x, pinned: !x.pinned } : x)),
    );
  }

  async function handleDelete(m: Memory) {
    const ok = await showConfirm(
      `删除这条记忆？\n\n"${m.content.slice(0, 60)}${m.content.length > 60 ? "..." : ""}"`,
    );
    if (!ok) return;
    setMemories((prev) => prev.filter((x) => x.id !== m.id));
    showToast("已删除");
  }

  function handleAdd() {
    const content = addContent.trim();
    if (!content) return;
    const id = `m-${Date.now()}`;
    setMemories((prev) => [
      {
        id,
        content,
        category: addCategory,
        source: "user_added",
        created_at: Date.now(),
        last_accessed_at: Date.now(),
        pinned: false,
      },
      ...prev,
    ]);
    setAddContent("");
    setShowAdd(false);
    showToast("已添加到记忆");
  }

  const renderCard = (m: Memory) => {
    const meta = CATEGORY_META[m.category];
    const isEditing = editingId === m.id;
    return (
      <div key={m.id} className={styles["memory-card"]}>
        <div className={styles["memory-card-header"]}>
          <span className={styles["memory-icon"]}>{meta.icon}</span>
          <span className={styles["memory-category"]}>{meta.label}</span>
          <span className={styles["memory-dot"]}>·</span>
          <span className={styles["memory-source"]}>
            {m.source === "learned"
              ? `学到 · 观察 ${m.observed_count} 次 · 置信度 ${Math.round(
                  (m.confidence ?? 0) * 100,
                )}%`
              : m.source === "user_added"
              ? "手动添加"
              : "对话中提及"}
          </span>
        </div>

        {isEditing ? (
          <div className={styles["memory-edit"]}>
            <textarea
              autoFocus
              value={editDraft}
              onChange={(e) => setEditDraft(e.target.value)}
              onKeyDown={(e) => {
                if (e.key === "Escape") setEditingId(null);
                if (e.key === "Enter" && (e.metaKey || e.ctrlKey)) saveEdit();
              }}
            />
            <div className={styles["memory-edit-actions"]}>
              <button onClick={() => setEditingId(null)}>取消</button>
              <button className={styles["primary"]} onClick={saveEdit}>
                保存
              </button>
            </div>
          </div>
        ) : (
          <div className={styles["memory-content"]}>{m.content}</div>
        )}

        <div className={styles["memory-card-footer"]}>
          <span className={styles["memory-time"]}>
            添加于 {formatRelative(m.created_at)} · 最近用于{" "}
            {formatRelative(m.last_accessed_at)}
          </span>
          {!isEditing && (
            <div className={styles["memory-actions"]}>
              <button onClick={() => togglePin(m)}>
                {m.pinned ? "取消置顶" : "置顶"}
              </button>
              {m.session_id && (
                <button onClick={() => showToast("跳转到对话 " + m.session_id)}>
                  查看原对话
                </button>
              )}
              <button onClick={() => startEdit(m)}>编辑</button>
              <button
                className={styles["danger"]}
                onClick={() => handleDelete(m)}
              >
                删除
              </button>
            </div>
          )}
        </div>
      </div>
    );
  };

  return (
    <ErrorBoundary>
      <div className={styles["memory-page"]}>
        <div className="window-header">
          <div className="window-header-title">
            <div className="window-header-main-title">🧠 记忆</div>
            <div className="window-header-sub-title">
              RsClaw 记住的关于你的 {memories.length} 条信息
            </div>
          </div>
          <div className="window-actions">
            <div className="window-action-button">
              <IconButton
                icon={<AddIcon />}
                bordered
                text="添加记忆"
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

        <div className={styles["memory-body"]}>
          <div className={styles["search-row"]}>
            <input
              type="text"
              className={styles["search-input"]}
              placeholder="🔍 搜索我的记忆..."
              value={query}
              onChange={(e) => setQuery(e.target.value)}
            />
          </div>

          <div className={styles["category-tabs"]}>
            {(["all", "preference", "fact", "summary"] as const).map((c) => (
              <button
                key={c}
                className={`${styles["tab"]} ${
                  activeCategory === c ? styles["tab-active"] : ""
                }`}
                onClick={() => setActiveCategory(c as any)}
              >
                {c === "all" ? "全部" : CATEGORY_META[c as Category].label}{" "}
                <span className={styles["tab-count"]}>{counts[c] ?? 0}</span>
              </button>
            ))}
          </div>

          {filtered.length === 0 && (
            <div className={styles["empty-state"]}>
              {query
                ? `没有匹配 "${query}" 的记忆`
                : "AI 还没学到关于你的事。多聊几次，它会记住你。"}
            </div>
          )}

          {grouped.pinned.length > 0 && (
            <section className={styles["memory-group"]}>
              <div className={styles["group-title"]}>📌 置顶</div>
              <div className={styles["memory-list"]}>
                {grouped.pinned.map(renderCard)}
              </div>
            </section>
          )}

          {Object.entries(grouped.byGroup).map(([group, items]) => (
            <section key={group} className={styles["memory-group"]}>
              <div className={styles["group-title"]}>{group}</div>
              <div className={styles["memory-list"]}>
                {items.map(renderCard)}
              </div>
            </section>
          ))}
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
              <div className={styles["modal-title"]}>添加记忆</div>
              <textarea
                autoFocus
                className={styles["modal-textarea"]}
                placeholder="记住什么？例如：用户偏好用 bun 而不是 npm"
                value={addContent}
                onChange={(e) => setAddContent(e.target.value)}
              />
              <div className={styles["modal-row"]}>
                <span>分类:</span>
                <select
                  value={addCategory}
                  onChange={(e) => setAddCategory(e.target.value as Category)}
                >
                  <option value="fact">事实</option>
                  <option value="preference">偏好</option>
                  <option value="custom">自定义</option>
                </select>
              </div>
              <div className={styles["modal-actions"]}>
                <button onClick={() => setShowAdd(false)}>取消</button>
                <button
                  className={styles["primary"]}
                  onClick={handleAdd}
                  disabled={!addContent.trim()}
                >
                  添加
                </button>
              </div>
            </div>
          </div>
        )}
      </div>
    </ErrorBoundary>
  );
}
