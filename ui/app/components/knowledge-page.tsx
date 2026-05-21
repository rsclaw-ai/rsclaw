/**
 * Knowledge Base management page. Master-detail layout:
 *   - Top: search bar (when typed, hits list replaces doc list).
 *   - Left: collection list + create button.
 *   - Right: selected collection's docs + upload entries (file picker,
 *     drag-drop, paste text, fetch URL).
 *
 * Wire contract: `docs/interfaces/knowledge-api.md`. SSE subscription
 * patches doc-status changes in place so the user sees pending → ready
 * without polling.
 *
 * Disabled state: any `/stats` 404 means the KB store didn't open
 * (storage error, etc). We render a centered hint pointing the user
 * to gateway logs — there's no enable knob, the service opens
 * automatically at startup.
 */
import { useCallback, useEffect, useRef, useState } from "react";

import {
  KbCollection,
  KbDoc,
  KbDocStatus,
  KbDocStatusEvent,
  KbSearchHit,
  KbStats,
  createCollection,
  deleteCollection,
  deleteDoc,
  getDocContent,
  getMaxDocBytes,
  getStats,
  listCollections,
  listDocs,
  patchCollection,
  reindexDoc,
  search,
  subscribeDocStatus,
  uploadDocFile,
  uploadDocFromUrl,
  uploadDocJson,
} from "../lib/knowledge";
import { toast } from "../lib/toast";
import { getLang } from "../locales";
import { isTauri } from "../utils/tauri";
import { showConfirm, showPrompt } from "./ui-lib";

// Same palette as the rest of the panel for visual continuity.
const V2 = {
  bg1: "#0f1013",
  bg2: "#141618",
  bg3: "#1a1c22",
  bg4: "#1f2126",
  bg5: "#252830",
  bd: "rgba(255,255,255,.055)",
  bd2: "rgba(255,255,255,.09)",
  t0: "#eceaf4",
  t1: "#9896a4",
  t2: "#7e7c8c",
  t3: "#5a5868",
  or: "#f97316",
  olo: "rgba(249,115,22,.09)",
  obrd: "rgba(249,115,22,.2)",
  green: "#2dd4a0",
  glo: "rgba(45,212,160,.07)",
  gbrd: "rgba(45,212,160,.18)",
  yellow: "#eab308",
  ylo: "rgba(234,179,8,.08)",
  red: "#d95f5f",
  rlo: "rgba(217,95,95,.08)",
  rbrd: "rgba(217,95,95,.18)",
  mono: "'JetBrains Mono', monospace",
};

const fInput: React.CSSProperties = {
  background: V2.bg4,
  border: `1px solid ${V2.bd2}`,
  borderRadius: 7,
  padding: "7px 10px",
  color: V2.t0,
  fontFamily: V2.mono,
  fontSize: 11.5,
  outline: "none",
};

// Hard fallback if `getMaxDocBytes()` (which reads kb.maxDocMb from
// /api/v1/config) fails. Matches the backend's own default.
const DEFAULT_maxDocBytes = 50 * 1024 * 1024;

const fmtBytes = (n: number): string => {
  if (n < 1024) return `${n} B`;
  if (n < 1024 * 1024) return `${(n / 1024).toFixed(1)} KB`;
  if (n < 1024 * 1024 * 1024) return `${(n / (1024 * 1024)).toFixed(1)} MB`;
  return `${(n / (1024 * 1024 * 1024)).toFixed(2)} GB`;
};

const fmtDate = (iso: string): string => {
  try {
    const d = new Date(iso);
    return d.toLocaleString();
  } catch {
    return iso;
  }
};

const statusColor = (s: KbDocStatus) => {
  switch (s) {
    case "ready":
      return V2.green;
    case "indexing":
    case "pending":
      return V2.or;
    case "failed":
      return V2.red;
  }
};

// ─────────────────────────────────────────────────────────────────
// Main page
// ─────────────────────────────────────────────────────────────────

export function KnowledgePage() {
  const zh = getLang() === "cn";

  // Disabled detection: GET /stats once on mount; bare 404 → KB store off.
  const [bootState, setBootState] = useState<"loading" | "ready" | "disabled" | "error">(
    "loading",
  );
  const [bootError, setBootError] = useState<string>("");
  const [stats, setStats] = useState<KbStats | null>(null);

  const [maxDocBytes, setMaxDocBytes] = useState<number>(DEFAULT_maxDocBytes);
  const [collections, setCollections] = useState<KbCollection[]>([]);
  const [activeCollectionId, setActiveCollectionId] = useState<string | null>(null);
  const [docs, setDocs] = useState<KbDoc[]>([]);
  const [docsLoading, setDocsLoading] = useState(false);

  const [query, setQuery] = useState("");
  const [searching, setSearching] = useState(false);
  const [hits, setHits] = useState<KbSearchHit[]>([]);
  const [queryMs, setQueryMs] = useState(0);

  const [showNewCol, setShowNewCol] = useState(false);
  const [showPasteText, setShowPasteText] = useState(false);
  const [showFetchUrl, setShowFetchUrl] = useState(false);
  const [detailDoc, setDetailDoc] = useState<KbDoc | null>(null);
  const [dropActive, setDropActive] = useState(false);

  const fileInputRef = useRef<HTMLInputElement>(null);

  // ── Bootstrap ────────────────────────────────────────────────────
  const refreshStats = useCallback(async () => {
    try {
      const s = await getStats();
      setStats(s);
      setBootState("ready");
    } catch (e: any) {
      if (e?.name === "KbDisabledError") {
        setBootState("disabled");
      } else {
        setBootError(e?.message || String(e));
        setBootState("error");
      }
    }
  }, []);

  const refreshCollections = useCallback(async () => {
    try {
      const list = await listCollections();
      setCollections(list);
      // Auto-select first collection if none active and list non-empty.
      setActiveCollectionId((prev) => prev ?? (list[0]?.id ?? null));
    } catch (e: any) {
      if (e?.name !== "KbDisabledError") {
        toast.fromError(zh ? "加载知识库失败" : "Failed to load collections", e);
      }
    }
  }, [zh]);

  useEffect(() => {
    void refreshStats();
    void refreshCollections();
    void getMaxDocBytes().then(setMaxDocBytes);
  }, [refreshStats, refreshCollections]);

  const refreshDocs = useCallback(
    async (colId: string) => {
      setDocsLoading(true);
      try {
        const list = await listDocs(colId);
        setDocs(list);
      } catch (e: any) {
        if (e?.name !== "KbDisabledError") {
          toast.fromError(zh ? "加载文档失败" : "Failed to load docs", e);
        }
      }
      setDocsLoading(false);
    },
    [zh],
  );

  useEffect(() => {
    if (!activeCollectionId || bootState !== "ready") {
      setDocs([]);
      return;
    }
    void refreshDocs(activeCollectionId);
  }, [activeCollectionId, bootState, refreshDocs]);

  // ── SSE: patch doc status in place ──────────────────────────────
  // We re-fetch the affected doc to pick up chunkCount + indexedAt
  // changes, but keep the list order stable.
  useEffect(() => {
    if (bootState !== "ready") return;
    const off = subscribeDocStatus(
      async (ev: KbDocStatusEvent) => {
        setDocs((prev) => {
          const idx = prev.findIndex((d) => d.id === ev.docId);
          if (idx < 0) return prev;
          const next = [...prev];
          next[idx] = { ...next[idx], status: ev.status };
          return next;
        });
        // status_changed alone won't carry chunkCount; refresh active
        // collection so the count updates when an indexing job finishes.
        if (ev.status === "ready" || ev.status === "failed") {
          if (activeCollectionId) void refreshDocs(activeCollectionId);
          void refreshStats();
        }
      },
      () => {
        /* surface disconnect to status icon later */
      },
    );
    return off;
  }, [bootState, activeCollectionId, refreshDocs, refreshStats]);

  // ── Drag-drop file upload ────────────────────────────────────────
  // Tauri-side: subscribe to onDragDropEvent and route .md/.txt/.pdf/
  // .docx/.xlsx/.pptx to the active collection. Web-side: handled by
  // the dropzone's own onDrop below.
  const doUploadFiles = useCallback(
    async (files: { name: string; bytes: ArrayBuffer; type?: string }[]) => {
      if (!activeCollectionId) {
        toast.error(zh ? "请先选择一个知识库" : "Select a collection first");
        return;
      }
      for (const f of files) {
        if (f.bytes.byteLength > maxDocBytes) {
          toast.error(`${zh ? "文件过大" : "File too large"}: ${f.name} (${fmtBytes(f.bytes.byteLength)} > ${fmtBytes(maxDocBytes)})`);
          continue;
        }
        try {
          const blob = new Blob([f.bytes], { type: f.type || "application/octet-stream" });
          const file = new File([blob], f.name, { type: f.type });
          await uploadDocFile(activeCollectionId, file);
        } catch (e: any) {
          toast.fromError(`${zh ? "上传失败" : "Upload failed"}: ${f.name}`, e);
        }
      }
      await refreshDocs(activeCollectionId);
      await refreshStats();
    },
    [activeCollectionId, maxDocBytes, zh, refreshDocs, refreshStats],
  );

  useEffect(() => {
    if (!isTauri || bootState !== "ready") return;
    let cancelled = false;
    let off: Array<() => void> = [];
    (async () => {
      try {
        const { getCurrentWindow } = await import("@tauri-apps/api/window");
        const win = getCurrentWindow();
        const stop = await win.onDragDropEvent(async (ev: any) => {
          if (cancelled) return;
          if (ev.payload.type === "over") {
            setDropActive(true);
            return;
          }
          if (ev.payload.type === "leave") {
            setDropActive(false);
            return;
          }
          if (ev.payload.type === "drop") {
            setDropActive(false);
            const paths: string[] = ev.payload.paths || [];
            if (paths.length === 0 || !activeCollectionId) return;
            // Read each file via Tauri fs plugin and forward as Blob.
            const fs = await import("@tauri-apps/plugin-fs");
            const loaded: { name: string; bytes: ArrayBuffer; type?: string }[] = [];
            for (const p of paths) {
              try {
                const bytes = await fs.readFile(p);
                const name = p.split("/").pop() || p;
                loaded.push({
                  name,
                  bytes: (bytes as Uint8Array).buffer as ArrayBuffer,
                });
              } catch (e) {
                /* skip files we can't read */
              }
            }
            if (loaded.length > 0) await doUploadFiles(loaded);
          }
        });
        if (cancelled) stop();
        else off.push(stop);
      } catch {
        /* drag-drop unavailable */
      }
    })();
    return () => {
      cancelled = true;
      off.forEach((f) => f());
    };
  }, [bootState, activeCollectionId, doUploadFiles]);

  // ── Search ───────────────────────────────────────────────────────
  const runSearch = useCallback(async () => {
    const q = query.trim();
    if (!q) {
      setHits([]);
      return;
    }
    if (q.length > 512) {
      toast.error(zh ? "查询过长（最多 512 字符）" : "Query too long (max 512 chars)");
      return;
    }
    setSearching(true);
    try {
      const r = await search({ query: q, topK: 20 });
      setHits(r.hits);
      setQueryMs(r.queryMs);
    } catch (e: any) {
      toast.fromError(zh ? "搜索失败" : "Search failed", e);
      setHits([]);
    }
    setSearching(false);
  }, [query, zh]);

  // ── Upload entries ───────────────────────────────────────────────
  const handleFilePick = async (files: FileList | null) => {
    if (!files || !activeCollectionId) return;
    for (const f of Array.from(files)) {
      if (f.size > maxDocBytes) {
        toast.error(`${zh ? "文件过大" : "File too large"}: ${f.name} (${fmtBytes(f.size)} > ${fmtBytes(maxDocBytes)})`);
        continue;
      }
      try {
        await uploadDocFile(activeCollectionId, f);
      } catch (e: any) {
        toast.fromError(`${zh ? "上传失败" : "Upload failed"}: ${f.name}`, e);
      }
    }
    await refreshDocs(activeCollectionId);
    await refreshStats();
  };

  // ── Render: disabled / error / loading ──────────────────────────
  if (bootState === "loading") {
    return (
      <div style={fullPageMsg}>
        <div style={{ color: V2.t3, fontSize: 12 }}>...</div>
      </div>
    );
  }

  if (bootState === "disabled") {
    return (
      <div style={fullPageMsg}>
        <div style={{ fontSize: 44, opacity: 0.5 }}>📚</div>
        <div style={{ fontSize: 16, fontWeight: 600, color: V2.t0, marginTop: 12 }}>
          {zh ? "知识库未启用" : "Knowledge base not available"}
        </div>
        <div
          style={{
            fontSize: 12,
            color: V2.t1,
            lineHeight: 1.6,
            maxWidth: 480,
            textAlign: "center",
            marginTop: 12,
          }}
        >
          {zh
            ? "Gateway 启动时未能打开 ~/.rsclaw/kb/ 存储。常见原因：磁盘已满 / 文件权限错误 / 索引文件损坏。请检查 gateway 日志后重启。"
            : "Gateway failed to open the KB store at ~/.rsclaw/kb/ during startup. Common causes: disk full, permission error, corrupt index. Check gateway logs and restart."}
        </div>
        <button onClick={() => void refreshStats()} style={btnPrimary}>
          {zh ? "重试" : "Retry"}
        </button>
      </div>
    );
  }

  if (bootState === "error") {
    return (
      <div style={fullPageMsg}>
        <div style={{ fontSize: 44, opacity: 0.5 }}>⚠️</div>
        <div style={{ fontSize: 14, color: V2.t1, marginTop: 12 }}>{bootError}</div>
        <button onClick={() => void refreshStats()} style={btnPrimary}>
          {zh ? "重试" : "Retry"}
        </button>
      </div>
    );
  }

  // ── Ready state ──────────────────────────────────────────────────
  const activeCol = collections.find((c) => c.id === activeCollectionId);
  const showingHits = query.trim().length > 0;

  return (
    <div style={{ flex: 1, overflow: "hidden", display: "flex", flexDirection: "column", position: "relative" }}>
      {/* Header */}
      <div
        style={{
          padding: "24px 28px 0",
          flexShrink: 0,
          display: "flex",
          alignItems: "flex-end",
          justifyContent: "space-between",
        }}
      >
        <div>
          <div style={{ fontSize: 20, fontWeight: 700, color: V2.t0, letterSpacing: -0.4 }}>
            {zh ? "知识库管理" : "Knowledge Base"}
          </div>
          <div style={{ fontSize: 11, color: V2.t3, fontFamily: V2.mono, marginTop: 3 }}>
            ~/.rsclaw/kb/
          </div>
        </div>
        {stats && (
          <div style={{ display: "flex", gap: 14, fontFamily: V2.mono, fontSize: 11 }}>
            <StatPill label={zh ? "知识库" : "Coll."} value={String(stats.collectionCount)} />
            <StatPill label={zh ? "文档" : "Docs"} value={String(stats.docCount)} />
            <StatPill label={zh ? "片段" : "Chunks"} value={String(stats.chunkCount)} />
            <StatPill label={zh ? "占用" : "Bytes"} value={fmtBytes(stats.bytes)} />
          </div>
        )}
      </div>

      {/* Search bar */}
      <div style={{ padding: "16px 28px 0", flexShrink: 0 }}>
        <div style={{ display: "flex", gap: 8 }}>
          <input
            value={query}
            onChange={(e) => setQuery(e.target.value)}
            onKeyDown={(e) => {
              if (e.key === "Enter") void runSearch();
              if (e.key === "Escape") setQuery("");
            }}
            placeholder={zh ? "语义搜索（Enter 提交，Esc 清空）..." : "Semantic search (Enter, Esc to clear)..."}
            style={{ ...fInput, flex: 1, padding: "9px 14px", fontSize: 12 }}
          />
          {showingHits && (
            <button onClick={() => setQuery("")} style={btnSubtle}>
              {zh ? "清空" : "Clear"}
            </button>
          )}
          <button onClick={() => void runSearch()} disabled={searching || !query.trim()} style={btnPrimary}>
            {searching ? (zh ? "搜索中..." : "Searching...") : zh ? "搜索" : "Search"}
          </button>
        </div>
        {showingHits && (
          <div style={{ fontSize: 10, color: V2.t3, fontFamily: V2.mono, marginTop: 6 }}>
            {hits.length} {zh ? "个结果" : "hits"} · {queryMs}ms
          </div>
        )}
      </div>

      {/* Body */}
      <div style={{ flex: 1, overflow: "hidden", padding: "16px 28px 28px", display: "flex", gap: 14 }}>
        {/* Left: collections */}
        <div
          style={{
            width: 240,
            flexShrink: 0,
            background: V2.bg2,
            border: `1px solid ${V2.bd}`,
            borderRadius: 11,
            display: "flex",
            flexDirection: "column",
            overflow: "hidden",
          }}
        >
          <div
            style={{
              padding: "12px 14px",
              borderBottom: `1px solid ${V2.bd}`,
              display: "flex",
              alignItems: "center",
              justifyContent: "space-between",
            }}
          >
            <div style={{ fontSize: 11, fontWeight: 600, color: V2.t2, letterSpacing: 0.4, textTransform: "uppercase" }}>
              {zh ? "知识库" : "Collections"}
            </div>
            <button onClick={() => setShowNewCol(true)} style={btnTiny}>
              + {zh ? "新建" : "New"}
            </button>
          </div>
          <div style={{ flex: 1, overflowY: "auto", padding: "6px 6px 12px" }}>
            {collections.length === 0 ? (
              <div style={{ padding: "20px 8px", textAlign: "center", color: V2.t3, fontSize: 11 }}>
                {zh ? "尚未创建知识库" : "No collections yet"}
              </div>
            ) : (
              collections.map((c) => {
                const active = c.id === activeCollectionId;
                return (
                  <div
                    key={c.id}
                    onClick={() => setActiveCollectionId(c.id)}
                    title={c.id}
                    style={{
                      padding: "8px 10px",
                      margin: "2px 0",
                      borderRadius: 7,
                      cursor: "pointer",
                      background: active ? V2.bg4 : "transparent",
                      border: `1px solid ${active ? V2.bd2 : "transparent"}`,
                    }}
                  >
                    <div
                      style={{
                        fontSize: 12.5,
                        fontWeight: 600,
                        color: V2.t0,
                        whiteSpace: "nowrap",
                        overflow: "hidden",
                        textOverflow: "ellipsis",
                      }}
                    >
                      {c.name}
                    </div>
                    {c.description && (
                      <div
                        style={{
                          fontSize: 11,
                          color: V2.t1,
                          marginTop: 2,
                          lineHeight: 1.5,
                          whiteSpace: "nowrap",
                          overflow: "hidden",
                          textOverflow: "ellipsis",
                        }}
                      >
                        {c.description}
                      </div>
                    )}
                  </div>
                );
              })
            )}
          </div>
        </div>

        {/* Right: docs OR hits */}
        <div
          style={{
            flex: 1,
            background: V2.bg2,
            border: `1px solid ${V2.bd}`,
            borderRadius: 11,
            display: "flex",
            flexDirection: "column",
            overflow: "hidden",
          }}
        >
          {showingHits ? (
            <HitsPane hits={hits} zh={zh} onPick={(_h) => undefined} />
          ) : !activeCol ? (
            <div style={{ flex: 1, display: "flex", alignItems: "center", justifyContent: "center", color: V2.t3, fontSize: 12 }}>
              {zh ? "请先选择或创建一个知识库" : "Select or create a collection"}
            </div>
          ) : (
            <>
              {/* Collection toolbar — title above, two grouped menus below.
                  Upload methods (3) collapse into one "+ 添加文档" menu, and
                  destructive/rename ops live behind a kebab. Keeps the row
                  short even on narrow panels. */}
              <div
                style={{
                  padding: "12px 14px",
                  borderBottom: `1px solid ${V2.bd}`,
                  display: "flex",
                  alignItems: "center",
                  gap: 10,
                }}
              >
                <div style={{ flex: 1, minWidth: 0 }}>
                  <div
                    style={{
                      fontSize: 13,
                      fontWeight: 600,
                      color: V2.t0,
                      whiteSpace: "nowrap",
                      overflow: "hidden",
                      textOverflow: "ellipsis",
                    }}
                    title={activeCol.name}
                  >
                    {activeCol.name}
                  </div>
                  {activeCol.description && (
                    <div
                      style={{
                        fontSize: 11,
                        color: V2.t1,
                        marginTop: 3,
                        whiteSpace: "nowrap",
                        overflow: "hidden",
                        textOverflow: "ellipsis",
                      }}
                      title={activeCol.description}
                    >
                      {activeCol.description}
                    </div>
                  )}
                </div>
                <Menu
                  trigger={
                    <button style={{ ...btnPrimary, whiteSpace: "nowrap" }}>
                      {zh ? "+ 添加文档 ▾" : "+ Add doc ▾"}
                    </button>
                  }
                  items={[
                    { label: zh ? "选择文件" : "Pick files", onClick: () => fileInputRef.current?.click() },
                    { label: zh ? "粘贴文本" : "Paste text", onClick: () => setShowPasteText(true) },
                    { label: zh ? "URL 抓取" : "Fetch URL", onClick: () => setShowFetchUrl(true) },
                  ]}
                />
                <Menu
                  trigger={
                    <button style={{ ...btnTiny, whiteSpace: "nowrap", padding: "5px 10px" }}>⋯</button>
                  }
                  items={[
                    {
                      label: zh ? "重命名" : "Rename",
                      onClick: () => void renamePrompt(activeCol, refreshCollections, zh),
                    },
                    {
                      label: zh ? "删除知识库" : "Delete collection",
                      danger: true,
                      onClick: async () => {
                        // showConfirm > window.confirm: Tauri webview muffles
                        // native confirms. Explicit copy enumerates the
                        // cascade so a slip on "确定" doesn't accidentally
                        // wipe a populated KB.
                        const docCount = docs.length;
                        const msg = zh
                          ? `确定删除知识库「${activeCol.name}」？\n\n这会一并删除 ${docCount} 个文档及其全部索引向量，且无法恢复。`
                          : `Delete collection "${activeCol.name}"?\n\nThis will also delete ${docCount} doc(s) and all their index vectors. This cannot be undone.`;
                        const ok = await showConfirm(msg);
                        if (!ok) return;
                        try {
                          const r = await deleteCollection(activeCol.id);
                          toast.success(zh ? `已删除 (${r.deletedDocs} docs)` : `Deleted (${r.deletedDocs} docs)`);
                          setActiveCollectionId(null);
                          await refreshCollections();
                          await refreshStats();
                        } catch (e: any) {
                          toast.fromError(zh ? "删除失败" : "Delete failed", e);
                        }
                      },
                    },
                  ]}
                />
                <input
                  ref={fileInputRef}
                  type="file"
                  multiple
                  accept=".md,.txt,.json,.pdf,.docx,.xlsx,.pptx,text/markdown,text/plain,application/json,application/pdf"
                  style={{ display: "none" }}
                  onChange={(e) => {
                    void handleFilePick(e.target.files);
                    if (e.target) e.target.value = "";
                  }}
                />
              </div>

              {/* Doc list */}
              <div style={{ flex: 1, overflowY: "auto", padding: "8px 10px" }}>
                {docsLoading ? (
                  <div style={{ textAlign: "center", padding: 20, color: V2.t3, fontSize: 11 }}>...</div>
                ) : docs.length === 0 ? (
                  <div style={{ textAlign: "center", padding: "40px 0", color: V2.t3, fontSize: 12 }}>
                    {zh ? "尚无文档。可拖拽文件到窗口，或使用上方按钮上传。" : "No docs yet. Drag files into the window or use the buttons above."}
                  </div>
                ) : (
                  docs.map((d) => (
                    <div
                      key={d.id}
                      onClick={() => setDetailDoc(d)}
                      style={{
                        padding: "10px 12px",
                        margin: "4px 0",
                        background: V2.bg3,
                        border: `1px solid ${V2.bd}`,
                        borderRadius: 9,
                        cursor: "pointer",
                        display: "flex",
                        gap: 12,
                        alignItems: "center",
                      }}
                    >
                      <div style={{ flex: 1, minWidth: 0 }}>
                        <div style={{ fontSize: 12, fontWeight: 600, color: V2.t0, whiteSpace: "nowrap", overflow: "hidden", textOverflow: "ellipsis" }}>
                          {d.title}
                        </div>
                        <div style={{ fontSize: 10, color: V2.t3, fontFamily: V2.mono, marginTop: 2 }}>
                          {d.mime} · {fmtBytes(d.bytes)} · {d.chunkCount} chunks
                        </div>
                      </div>
                      <div
                        style={{
                          fontSize: 10,
                          fontFamily: V2.mono,
                          color: statusColor(d.status),
                          display: "flex",
                          alignItems: "center",
                          gap: 4,
                        }}
                      >
                        ● {d.status}
                      </div>
                    </div>
                  ))
                )}
              </div>
            </>
          )}
        </div>
      </div>

      {/* Drag-drop overlay highlight */}
      {dropActive && activeCollectionId && (
        <div
          style={{
            position: "absolute",
            inset: 0,
            border: `2px dashed ${V2.green}`,
            background: "rgba(45,212,160,.05)",
            display: "flex",
            alignItems: "center",
            justifyContent: "center",
            pointerEvents: "none",
            color: V2.green,
            fontSize: 14,
            fontWeight: 600,
            fontFamily: V2.mono,
            letterSpacing: 0.5,
          }}
        >
          {zh ? `松开上传到「${activeCol?.name || ""}」` : `Drop to upload into "${activeCol?.name || ""}"`}
        </div>
      )}

      {/* Modals */}
      {showNewCol && (
        <NewCollectionModal
          zh={zh}
          onClose={() => setShowNewCol(false)}
          onCreated={async (col) => {
            setShowNewCol(false);
            await refreshCollections();
            setActiveCollectionId(col.id);
            await refreshStats();
          }}
        />
      )}
      {showPasteText && activeCollectionId && (
        <PasteTextModal
          zh={zh}
          onClose={() => setShowPasteText(false)}
          onSubmit={async (title, text, mime) => {
            const bytes = new TextEncoder().encode(text).length;
            if (bytes > maxDocBytes) {
              toast.error(`${zh ? "文本过大" : "Text too large"} (${fmtBytes(bytes)} > ${fmtBytes(maxDocBytes)})`);
              return;
            }
            try {
              await uploadDocJson(activeCollectionId, { title, text, mime });
              setShowPasteText(false);
              await refreshDocs(activeCollectionId);
              await refreshStats();
            } catch (e: any) {
              toast.fromError(zh ? "上传失败" : "Upload failed", e);
            }
          }}
        />
      )}
      {showFetchUrl && activeCollectionId && (
        <FetchUrlModal
          zh={zh}
          onClose={() => setShowFetchUrl(false)}
          onSubmit={async (url) => {
            try {
              await uploadDocFromUrl(activeCollectionId, url);
              setShowFetchUrl(false);
              await refreshDocs(activeCollectionId);
              await refreshStats();
            } catch (e: any) {
              toast.fromError(zh ? "抓取失败" : "Fetch failed", e);
            }
          }}
        />
      )}
      {detailDoc && activeCollectionId && (
        <DocDetailModal
          zh={zh}
          collectionId={activeCollectionId}
          doc={detailDoc}
          onClose={() => setDetailDoc(null)}
          onDeleted={async () => {
            setDetailDoc(null);
            await refreshDocs(activeCollectionId);
            await refreshStats();
          }}
          onReindexed={async () => {
            await refreshDocs(activeCollectionId);
          }}
        />
      )}
    </div>
  );
}

// ─────────────────────────────────────────────────────────────────
// Subcomponents
// ─────────────────────────────────────────────────────────────────

function StatPill({ label, value }: { label: string; value: string }) {
  return (
    <div style={{ display: "flex", flexDirection: "column", alignItems: "flex-end" }}>
      <div style={{ fontSize: 13, fontWeight: 700, color: V2.t0 }}>{value}</div>
      <div style={{ fontSize: 9, color: V2.t3, letterSpacing: 0.5, textTransform: "uppercase" }}>{label}</div>
    </div>
  );
}

function HitsPane({
  hits,
  zh,
  onPick,
}: {
  hits: KbSearchHit[];
  zh: boolean;
  onPick: (h: KbSearchHit) => void;
}) {
  if (hits.length === 0) {
    return (
      <div style={{ flex: 1, display: "flex", alignItems: "center", justifyContent: "center", color: V2.t3, fontSize: 12 }}>
        {zh ? "没有匹配结果" : "No matches"}
      </div>
    );
  }
  return (
    <div style={{ flex: 1, overflowY: "auto", padding: "10px 12px" }}>
      {hits.map((h, i) => (
        <div
          key={`${h.docId}-${i}`}
          onClick={() => onPick(h)}
          style={{
            padding: "10px 12px",
            margin: "4px 0",
            background: V2.bg3,
            border: `1px solid ${V2.bd}`,
            borderRadius: 9,
            cursor: "pointer",
          }}
        >
          <div style={{ display: "flex", alignItems: "center", gap: 8, marginBottom: 6 }}>
            <div style={{ fontSize: 12, fontWeight: 600, color: V2.t0, flex: 1 }}>{h.sourceTitle}</div>
            {h.collectionName && (
              <div style={{ fontSize: 10, color: V2.t2, fontFamily: V2.mono }}>{h.collectionName}</div>
            )}
            <div style={{ fontSize: 10, fontFamily: V2.mono, color: V2.green }}>
              {(h.score * 100).toFixed(1)}%
            </div>
          </div>
          <div
            style={{
              fontSize: 11,
              color: V2.t1,
              lineHeight: 1.55,
              whiteSpace: "pre-wrap",
              maxHeight: 100,
              overflow: "hidden",
              textOverflow: "ellipsis",
              display: "-webkit-box",
              WebkitLineClamp: 4,
              WebkitBoxOrient: "vertical",
            }}
          >
            {h.chunkText}
          </div>
        </div>
      ))}
    </div>
  );
}

function NewCollectionModal({
  zh,
  onClose,
  onCreated,
}: {
  zh: boolean;
  onClose: () => void;
  onCreated: (col: KbCollection) => void;
}) {
  const [name, setName] = useState("");
  const [description, setDescription] = useState("");
  const [busy, setBusy] = useState(false);
  return (
    <ModalShell onClose={onClose} title={zh ? "新建知识库" : "New Collection"}>
      <div style={{ display: "flex", flexDirection: "column", gap: 10 }}>
        <input style={fInput} maxLength={100} placeholder={zh ? "名称（必填，≤100 字符）" : "Name (required, ≤100 chars)"} value={name} onChange={(e) => setName(e.target.value)} />
        <textarea
          style={{ ...fInput, minHeight: 60, fontFamily: "inherit", resize: "vertical" }}
          placeholder={zh ? "描述（可选）" : "Description (optional)"}
          value={description}
          onChange={(e) => setDescription(e.target.value)}
        />
      </div>
      <div style={modalFooter}>
        <button onClick={onClose} style={btnSubtle}>{zh ? "取消" : "Cancel"}</button>
        <button
          disabled={busy || !name.trim()}
          onClick={async () => {
            setBusy(true);
            try {
              const c = await createCollection({
                name: name.trim(),
                description: description.trim() || undefined,
              });
              onCreated(c);
            } catch (e: any) {
              toast.fromError(zh ? "创建失败" : "Create failed", e);
            }
            setBusy(false);
          }}
          style={btnPrimary}
        >
          {busy ? (zh ? "创建中..." : "Creating...") : zh ? "创建" : "Create"}
        </button>
      </div>
    </ModalShell>
  );
}

function PasteTextModal({
  zh,
  onClose,
  onSubmit,
}: {
  zh: boolean;
  onClose: () => void;
  onSubmit: (title: string, text: string, mime: string) => Promise<void>;
}) {
  const [title, setTitle] = useState("");
  const [text, setText] = useState("");
  const [mime, setMime] = useState("text/markdown");
  const [busy, setBusy] = useState(false);
  return (
    <ModalShell onClose={onClose} title={zh ? "粘贴文本" : "Paste Text"} width={560}>
      <div style={{ display: "flex", flexDirection: "column", gap: 10 }}>
        <input style={fInput} placeholder={zh ? "标题（必填）" : "Title (required)"} value={title} onChange={(e) => setTitle(e.target.value)} />
        <select style={{ ...fInput, fontFamily: "inherit" }} value={mime} onChange={(e) => setMime(e.target.value)}>
          <option value="text/markdown">text/markdown</option>
          <option value="text/plain">text/plain</option>
          <option value="application/json">application/json</option>
        </select>
        <textarea
          style={{ ...fInput, minHeight: 220, fontFamily: V2.mono, resize: "vertical" }}
          placeholder={zh ? "在这里粘贴文本..." : "Paste content here..."}
          value={text}
          onChange={(e) => setText(e.target.value)}
        />
      </div>
      <div style={modalFooter}>
        <button onClick={onClose} style={btnSubtle}>{zh ? "取消" : "Cancel"}</button>
        <button
          disabled={busy || !title.trim() || !text.trim()}
          onClick={async () => {
            setBusy(true);
            try {
              await onSubmit(title.trim(), text, mime);
            } finally {
              setBusy(false);
            }
          }}
          style={btnPrimary}
        >
          {busy ? (zh ? "提交中..." : "Submitting...") : zh ? "上传" : "Upload"}
        </button>
      </div>
    </ModalShell>
  );
}

function FetchUrlModal({
  zh,
  onClose,
  onSubmit,
}: {
  zh: boolean;
  onClose: () => void;
  onSubmit: (url: string) => Promise<void>;
}) {
  const [url, setUrl] = useState("");
  const [busy, setBusy] = useState(false);
  return (
    <ModalShell onClose={onClose} title={zh ? "从 URL 抓取" : "Fetch from URL"}>
      <div style={{ display: "flex", flexDirection: "column", gap: 10 }}>
        <input style={fInput} placeholder="https://..." value={url} onChange={(e) => setUrl(e.target.value)} />
        <div style={{ fontSize: 10, color: V2.t3, lineHeight: 1.5 }}>
          {zh
            ? "Gateway 后端会抓取并规范化页面内容，自动入索引队列。标题由后端从 URL 派生。"
            : "Gateway backend fetches and canonicalizes the page, then queues indexing. Title is derived server-side from the URL."}
        </div>
      </div>
      <div style={modalFooter}>
        <button onClick={onClose} style={btnSubtle}>{zh ? "取消" : "Cancel"}</button>
        <button
          disabled={busy || !url.trim()}
          onClick={async () => {
            setBusy(true);
            try {
              await onSubmit(url.trim());
            } finally {
              setBusy(false);
            }
          }}
          style={btnPrimary}
        >
          {busy ? (zh ? "提交中..." : "Submitting...") : zh ? "抓取并入库" : "Fetch & ingest"}
        </button>
      </div>
    </ModalShell>
  );
}

function DocDetailModal({
  zh,
  collectionId,
  doc,
  onClose,
  onDeleted,
  onReindexed,
}: {
  zh: boolean;
  collectionId: string;
  doc: KbDoc;
  onClose: () => void;
  onDeleted: () => Promise<void>;
  onReindexed: () => Promise<void>;
}) {
  const [content, setContent] = useState<string | null>(null);
  const [loadingContent, setLoadingContent] = useState(false);
  const [busy, setBusy] = useState(false);

  const loadContent = async () => {
    setLoadingContent(true);
    try {
      const text = await getDocContent(collectionId, doc.id);
      setContent(text);
    } catch (e: any) {
      toast.fromError(zh ? "加载内容失败" : "Failed to load content", e);
    }
    setLoadingContent(false);
  };

  return (
    <ModalShell onClose={onClose} title={doc.title} width={640}>
      <div style={{ fontSize: 10, fontFamily: V2.mono, color: V2.t3, marginBottom: 12 }}>
        {doc.id} · {doc.mime} · {fmtBytes(doc.bytes)} · {doc.chunkCount} chunks · ● <span style={{ color: statusColor(doc.status) }}>{doc.status}</span>
      </div>
      {doc.indexedAt && (
        <div style={{ fontSize: 10, color: V2.t3, marginBottom: 8 }}>
          {zh ? "索引完成于" : "Indexed at"} {fmtDate(doc.indexedAt)}
        </div>
      )}
      <div style={{ fontSize: 10, color: V2.t3, marginBottom: 12 }}>
        {zh ? "创建于" : "Created at"} {fmtDate(doc.createdAt)}
      </div>
      {content === null ? (
        <button onClick={() => void loadContent()} style={btnSubtle} disabled={loadingContent}>
          {loadingContent ? (zh ? "加载中..." : "Loading...") : zh ? "查看原文" : "View content"}
        </button>
      ) : (
        <pre
          style={{
            background: V2.bg1,
            border: `1px solid ${V2.bd}`,
            borderRadius: 7,
            padding: "10px 12px",
            color: V2.t1,
            fontFamily: V2.mono,
            fontSize: 11,
            lineHeight: 1.55,
            maxHeight: 360,
            overflowY: "auto",
            whiteSpace: "pre-wrap",
          }}
        >
          {content}
        </pre>
      )}
      <div style={modalFooter}>
        <button
          disabled={busy}
          onClick={async () => {
            const ok = await showConfirm(
              zh
                ? `确定删除文档「${doc.title}」？\n\n会一并删除 ${doc.chunkCount} 个索引片段，且无法恢复。`
                : `Delete doc "${doc.title}"?\n\nThis will also delete ${doc.chunkCount} index chunk(s). This cannot be undone.`,
            );
            if (!ok) return;
            setBusy(true);
            try {
              await deleteDoc(collectionId, doc.id);
              toast.success(zh ? "已删除" : "Deleted");
              await onDeleted();
            } catch (e: any) {
              toast.fromError(zh ? "删除失败" : "Delete failed", e);
            }
            setBusy(false);
          }}
          style={btnDanger}
        >
          {zh ? "删除" : "Delete"}
        </button>
        <button
          disabled={busy}
          onClick={async () => {
            setBusy(true);
            try {
              await reindexDoc(collectionId, doc.id);
              toast.success(zh ? "重建索引中" : "Reindexing");
              await onReindexed();
            } catch (e: any) {
              toast.fromError(zh ? "操作失败" : "Action failed", e);
            }
            setBusy(false);
          }}
          style={btnSubtle}
        >
          {zh ? "重建索引" : "Reindex"}
        </button>
        <button onClick={onClose} style={btnSubtle}>{zh ? "关闭" : "Close"}</button>
      </div>
    </ModalShell>
  );
}

interface MenuItem {
  label: string;
  onClick: () => void;
  danger?: boolean;
  disabled?: boolean;
}

/**
 * Tiny popover menu. Click trigger → flyout right below it. Closes on
 * outside click, Esc, or after picking an item. No portals — z-index 50
 * is enough since modals run at 100.
 */
function Menu({ trigger, items }: { trigger: React.ReactNode; items: MenuItem[] }) {
  const [open, setOpen] = useState(false);
  const wrapRef = useRef<HTMLDivElement>(null);
  useEffect(() => {
    if (!open) return;
    const onDoc = (e: MouseEvent) => {
      if (!wrapRef.current?.contains(e.target as Node)) setOpen(false);
    };
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") setOpen(false);
    };
    document.addEventListener("mousedown", onDoc);
    document.addEventListener("keydown", onKey);
    return () => {
      document.removeEventListener("mousedown", onDoc);
      document.removeEventListener("keydown", onKey);
    };
  }, [open]);
  return (
    <div ref={wrapRef} style={{ position: "relative", flexShrink: 0 }}>
      <div onClick={() => setOpen((v) => !v)}>{trigger}</div>
      {open && (
        <div
          style={{
            position: "absolute",
            top: "calc(100% + 4px)",
            right: 0,
            minWidth: 160,
            background: V2.bg3,
            border: `1px solid ${V2.bd2}`,
            borderRadius: 9,
            padding: 4,
            boxShadow: "0 10px 30px rgba(0,0,0,.4)",
            zIndex: 50,
          }}
        >
          {items.map((it, i) => (
            <button
              key={i}
              disabled={it.disabled}
              onClick={() => {
                setOpen(false);
                if (!it.disabled) it.onClick();
              }}
              style={{
                display: "block",
                width: "100%",
                textAlign: "left",
                padding: "8px 12px",
                fontSize: 12,
                background: "transparent",
                border: "none",
                color: it.disabled ? V2.t3 : it.danger ? V2.red : V2.t0,
                cursor: it.disabled ? "not-allowed" : "pointer",
                borderRadius: 6,
                whiteSpace: "nowrap",
              }}
              onMouseEnter={(e) => {
                if (!it.disabled) e.currentTarget.style.background = V2.bg4;
              }}
              onMouseLeave={(e) => {
                e.currentTarget.style.background = "transparent";
              }}
            >
              {it.label}
            </button>
          ))}
        </div>
      )}
    </div>
  );
}

function ModalShell({
  onClose,
  title,
  width = 460,
  children,
}: {
  onClose: () => void;
  title: string;
  width?: number;
  children: React.ReactNode;
}) {
  return (
    <div
      onClick={(e) => {
        if (e.target === e.currentTarget) onClose();
      }}
      style={{
        position: "fixed",
        inset: 0,
        background: "rgba(5,5,7,.72)",
        backdropFilter: "blur(3px)",
        display: "flex",
        alignItems: "center",
        justifyContent: "center",
        zIndex: 100,
      }}
    >
      <div
        style={{
          width,
          maxWidth: "92vw",
          background: V2.bg3,
          border: `1px solid ${V2.bd2}`,
          borderRadius: 14,
          overflow: "hidden",
          boxShadow: "0 20px 60px rgba(0,0,0,.6)",
        }}
      >
        <div style={{ padding: "16px 22px 0", display: "flex", alignItems: "center", justifyContent: "space-between" }}>
          <div style={{ fontSize: 15, fontWeight: 700, color: V2.t0 }}>{title}</div>
          <button
            onClick={onClose}
            style={{
              width: 26,
              height: 26,
              borderRadius: "50%",
              border: `1px solid ${V2.bd2}`,
              background: "transparent",
              color: V2.t2,
              fontSize: 14,
              cursor: "pointer",
            }}
          >
            ✕
          </button>
        </div>
        <div style={{ padding: "18px 22px 20px" }}>{children}</div>
      </div>
    </div>
  );
}

// ─────────────────────────────────────────────────────────────────
// Helpers
// ─────────────────────────────────────────────────────────────────

const fullPageMsg: React.CSSProperties = {
  flex: 1,
  display: "flex",
  flexDirection: "column",
  alignItems: "center",
  justifyContent: "center",
  gap: 16,
  padding: 40,
};

const modalFooter: React.CSSProperties = {
  display: "flex",
  justifyContent: "flex-end",
  gap: 8,
  marginTop: 16,
};

const btnPrimary: React.CSSProperties = {
  padding: "7px 16px",
  borderRadius: 8,
  border: `1px solid ${V2.gbrd}`,
  background: V2.glo,
  color: V2.green,
  fontSize: 12,
  fontWeight: 600,
  cursor: "pointer",
  fontFamily: V2.mono,
  whiteSpace: "nowrap",
};

const btnSubtle: React.CSSProperties = {
  padding: "7px 14px",
  borderRadius: 8,
  border: `1px solid ${V2.bd2}`,
  background: V2.bg4,
  color: V2.t1,
  fontSize: 12,
  cursor: "pointer",
  whiteSpace: "nowrap",
};

const btnDanger: React.CSSProperties = {
  padding: "7px 14px",
  borderRadius: 8,
  border: `1px solid ${V2.rbrd}`,
  background: V2.rlo,
  color: V2.red,
  fontSize: 12,
  fontWeight: 600,
  cursor: "pointer",
  whiteSpace: "nowrap",
};

const btnTiny: React.CSSProperties = {
  padding: "4px 10px",
  borderRadius: 6,
  border: `1px solid ${V2.bd2}`,
  background: V2.bg4,
  color: V2.t1,
  fontSize: 10.5,
  cursor: "pointer",
  whiteSpace: "nowrap",
};

// In-app rename modal. We can't use window.prompt — Tauri's WKWebView
// silently suppresses native prompts on most platforms (the dialog
// either never paints or is dismissed before the user sees it). Hence
// the project's own showPrompt() helper from ui-lib, which renders a
// proper modal that works identically in browser and Tauri.
async function renamePrompt(
  col: KbCollection,
  refresh: () => Promise<void>,
  zh: boolean,
) {
  const next = await showPrompt(zh ? "重命名知识库" : "Rename collection", col.name, 1);
  const trimmed = (next || "").trim();
  if (!trimmed || trimmed === col.name) return;
  if (trimmed.length > 100) {
    toast.error(zh ? "名称过长（≤100 字符）" : "Name too long (≤100 chars)");
    return;
  }
  try {
    await patchCollection(col.id, { name: trimmed });
    toast.success(zh ? "已重命名" : "Renamed");
    await refresh();
  } catch (e: any) {
    toast.fromError(zh ? "重命名失败" : "Rename failed", e);
  }
}

