/**
 * Memory management page — read-only browse of the agent runtime's
 * `MemoryStore`. Lists each doc with its tier / kind / scope and a
 * relevance score, plus a stats footer (by tier / kind).
 *
 * V1 is intentionally view-only. Mutating endpoints (pin / unpin /
 * delete / importance bump) need shared-state coordination with
 * the agent's live MemoryStore and land in a follow-up.
 */

import { useCallback, useEffect, useMemo, useRef, useState } from "react";

import {
  getMemoryStats,
  listMemoryDocs,
  type MemoryDoc,
  type MemoryListFilters,
  type MemoryStatsResponse,
} from "../lib/rsclaw-api";
import { getLang } from "../locales";

type TierFilter = "all" | "core" | "working" | "peripheral";

const TIER_COLOR: Record<MemoryDoc["tier"], string> = {
  core: "#2dd4a0",
  working: "#f59e0b",
  peripheral: "#6b6877",
};

function formatRelative(unix: number, zh: boolean): string {
  if (!unix) return zh ? "—" : "—";
  const now = Math.floor(Date.now() / 1000);
  const diff = Math.max(0, now - unix);
  if (diff < 60) return zh ? `${diff} 秒前` : `${diff}s ago`;
  if (diff < 3600) return zh ? `${Math.floor(diff / 60)} 分钟前` : `${Math.floor(diff / 60)}m ago`;
  if (diff < 86400) return zh ? `${Math.floor(diff / 3600)} 小时前` : `${Math.floor(diff / 3600)}h ago`;
  const days = Math.floor(diff / 86400);
  if (days < 30) return zh ? `${days} 天前` : `${days}d ago`;
  const months = Math.floor(days / 30);
  if (months < 12) return zh ? `${months} 个月前` : `${months}mo ago`;
  return zh ? `${Math.floor(months / 12)} 年前` : `${Math.floor(months / 12)}y ago`;
}

export function MemoryPage() {
  const zh = getLang() === "cn";

  const [docs, setDocs] = useState<MemoryDoc[]>([]);
  const [total, setTotal] = useState(0);
  const [stats, setStats] = useState<MemoryStatsResponse | null>(null);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const [query, setQuery] = useState("");
  const [tierFilter, setTierFilter] = useState<TierFilter>("all");
  const [kindFilter, setKindFilter] = useState<string>("");
  const [scopeFilter, setScopeFilter] = useState<string>("");
  // Client-side pinned toggle. Server doesn't expose a `pinned` filter
  // on /memory/docs yet, so we apply it locally over the fetched list.
  const [pinnedOnly, setPinnedOnly] = useState(false);

  // Debounce search input — the semantic search has to embed the
  // query and run an HNSW scan, no point firing on every keystroke.
  const debounceRef = useRef<ReturnType<typeof setTimeout> | null>(null);
  const [debouncedQuery, setDebouncedQuery] = useState("");
  useEffect(() => {
    if (debounceRef.current) clearTimeout(debounceRef.current);
    debounceRef.current = setTimeout(() => setDebouncedQuery(query), 250);
    return () => {
      if (debounceRef.current) clearTimeout(debounceRef.current);
    };
  }, [query]);

  const load = useCallback(async () => {
    setLoading(true);
    setError(null);
    const filters: MemoryListFilters = {
      q: debouncedQuery.trim() || undefined,
      kind: kindFilter || undefined,
      scope: scopeFilter || undefined,
      limit: 200,
    };
    try {
      const [docResp, statsResp] = await Promise.all([
        listMemoryDocs(filters),
        getMemoryStats(),
      ]);
      setDocs(docResp.docs || []);
      setTotal(docResp.total || 0);
      setStats(statsResp);
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
      setDocs([]);
      setTotal(0);
    } finally {
      setLoading(false);
    }
  }, [debouncedQuery, kindFilter, scopeFilter]);

  useEffect(() => {
    void load();
  }, [load]);

  // Client-side tier + pinned filter (the server filter only supports
  // kind / scope; the rest is a small enum + boolean we can cut down
  // in-memory cheaply).
  const visibleDocs = useMemo(() => {
    let out = docs;
    if (tierFilter !== "all") out = out.filter((d) => d.tier === tierFilter);
    if (pinnedOnly) out = out.filter((d) => d.pinned);
    return out;
  }, [docs, tierFilter, pinnedOnly]);

  const kindOptions = useMemo(() => {
    if (!stats) return [];
    return Object.keys(stats.by_kind).sort();
  }, [stats]);

  const scopeOptions = useMemo(() => {
    if (!stats) return [];
    return Object.keys(stats.by_scope).sort();
  }, [stats]);

  return (
    <div style={pageStyle}>
      {/* Header — search + counts */}
      <div style={headerStyle}>
        <div style={{ flex: 1, minWidth: 200 }}>
          <input
            type="text"
            placeholder={zh ? "搜索记忆（语义检索）…" : "Search memories (semantic)…"}
            value={query}
            onChange={(e) => setQuery(e.target.value)}
            style={inputStyle}
          />
        </div>
        <button onClick={() => void load()} style={refreshBtnStyle} disabled={loading}>
          {loading ? (zh ? "加载中…" : "Loading…") : (zh ? "刷新" : "Refresh")}
        </button>
      </div>

      {/* Stats row */}
      {stats && (
        <div style={statsRowStyle}>
          <StatPill
            label={zh ? "总计" : "Total"}
            value={stats.total}
            accent="#a8a6b2"
            tooltip={zh ? "全部记忆条目（含永久）。点击清除当前筛选" : "All memory docs (incl. permanent). Click to clear filters."}
            // "Total" doubles as a clear-all-filters affordance — clicking
            // wipes tier + pinned in one go (kind/scope live in their own
            // dropdowns and aren't touched).
            onClick={() => { setTierFilter("all"); setPinnedOnly(false); }}
            active={tierFilter === "all" && !pinnedOnly}
          />
          <StatPill
            label={zh ? "核心" : "Core"}
            value={stats.by_tier.core || 0}
            accent={TIER_COLOR.core}
            tooltip={zh ? "核心层：高频访问 / 高重要度，衰减最慢。系统自动按 importance + access_count + 时间评分" : "Core tier: high-frequency / high-importance, slowest decay. Auto-classified by importance × access_count × age."}
            onClick={() => setTierFilter(tierFilter === "core" ? "all" : "core")}
            active={tierFilter === "core"}
          />
          <StatPill
            label={zh ? "工作" : "Working"}
            value={stats.by_tier.working || 0}
            accent={TIER_COLOR.working}
            tooltip={zh ? "工作层：近期活跃但尚未沉淀的记忆，标准衰减速度" : "Working tier: recently active but not yet consolidated. Standard decay rate."}
            onClick={() => setTierFilter(tierFilter === "working" ? "all" : "working")}
            active={tierFilter === "working"}
          />
          <StatPill
            label={zh ? "外围" : "Peripheral"}
            value={stats.by_tier.peripheral || 0}
            accent={TIER_COLOR.peripheral}
            tooltip={zh ? "外围层：低频 / 低重要度，衰减最快，可能被清理" : "Peripheral tier: low-frequency / low-importance, fastest decay, candidate for cleanup."}
            onClick={() => setTierFilter(tierFilter === "peripheral" ? "all" : "peripheral")}
            active={tierFilter === "peripheral"}
          />
          <StatPill
            label={zh ? "永久 📌" : "Permanent 📌"}
            value={stats.pinned}
            accent="#f97316"
            tooltip={zh
              ? "永久记忆：关键事实（电话/姓名/订单号等）永不衰减、永远锁在核心层。横切维度，会与核心/工作/外围重叠计数。"
              : "Permanent: critical facts (names, phone numbers, IDs, etc.) — never decay, locked in Core tier. Orthogonal to tier; counted alongside Core/Working/Peripheral."}
            onClick={() => setPinnedOnly((v) => !v)}
            active={pinnedOnly}
          />
          {(tierFilter !== "all" || pinnedOnly) && (
            <button
              onClick={() => { setTierFilter("all"); setPinnedOnly(false); }}
              style={clearTierBtnStyle}
            >
              {zh ? "清除筛选 ×" : "Clear ×"}
            </button>
          )}
        </div>
      )}

      {/* Secondary filters */}
      <div style={filterRowStyle}>
        <div style={filterControlsStyle}>
          <select
            value={kindFilter}
            onChange={(e) => setKindFilter(e.target.value)}
            style={selectStyle}
          >
            <option value="">{zh ? "全部类型" : "All kinds"}</option>
            {kindOptions.map((k) => (
              <option key={k} value={k}>
                {k} ({stats?.by_kind[k] || 0})
              </option>
            ))}
          </select>
          <select
            value={scopeFilter}
            onChange={(e) => setScopeFilter(e.target.value)}
            style={selectStyle}
          >
            <option value="">{zh ? "全部 scope" : "All scopes"}</option>
            {scopeOptions.map((s) => (
              <option key={s} value={s}>
                {s} ({stats?.by_scope[s] || 0})
              </option>
            ))}
          </select>
        </div>
        <span style={countLabelStyle}>
          {zh ? "显示" : "Showing"} {visibleDocs.length}
          {tierFilter !== "all" ? ` / ${docs.length}` : ""} {zh ? "条，共" : "of"} {total}
        </span>
      </div>

      {/* Error banner */}
      {error && (
        <div style={errorStyle}>
          {zh ? "加载失败：" : "Failed: "} {error}
        </div>
      )}

      {/* Doc list */}
      <div style={listStyle}>
        {visibleDocs.length === 0 && !loading && !error && (
          <div style={emptyStyle}>
            {debouncedQuery
              ? (zh ? "没有匹配的记忆" : "No matching memories")
              : (zh
                  ? "暂时还没有记忆 — agent 在会话中产生记忆后会出现在这里"
                  : "No memories yet — agent-collected memories will appear here as you chat")}
          </div>
        )}
        {visibleDocs.map((d) => (
          <MemoryCard key={d.id} doc={d} zh={zh} />
        ))}
      </div>
    </div>
  );
}

function MemoryCard({ doc, zh }: { doc: MemoryDoc; zh: boolean }) {
  const [expanded, setExpanded] = useState(false);
  const preview = doc.text.length > 240 && !expanded ? doc.text.slice(0, 240) + "…" : doc.text;

  return (
    <div style={cardStyle}>
      <div style={cardHeaderStyle}>
        <span
          style={{
            ...tierChipStyle,
            color: TIER_COLOR[doc.tier],
            borderColor: TIER_COLOR[doc.tier] + "55",
            background: TIER_COLOR[doc.tier] + "15",
          }}
        >
          {doc.tier}
        </span>
        <span style={kindChipStyle}>{doc.kind}</span>
        <span style={scopeStyle}>{doc.scope}</span>
        {doc.pinned && <span style={pinnedStyle}>📌</span>}
        <span style={spacerStyle} />
        <span style={scoreStyle} title={zh ? "相关性评分" : "Relevance score"}>
          ★ {doc.relevance_score.toFixed(2)}
        </span>
      </div>

      {doc.abstract_text && <div style={abstractStyle}>{doc.abstract_text}</div>}

      <div style={textStyle} onClick={() => setExpanded(!expanded)}>
        {preview}
      </div>

      <div style={cardFooterStyle}>
        <span title={new Date(doc.created_at * 1000).toLocaleString()}>
          {zh ? "创建" : "Created"} {formatRelative(doc.created_at, zh)}
        </span>
        <span style={dotStyle}>·</span>
        <span title={new Date(doc.accessed_at * 1000).toLocaleString()}>
          {zh ? "最近访问" : "Last accessed"} {formatRelative(doc.accessed_at, zh)}
        </span>
        <span style={dotStyle}>·</span>
        <span>
          {zh ? "访问" : "Accessed"} {doc.access_count}×
        </span>
        <span style={dotStyle}>·</span>
        <span>
          {zh ? "重要性" : "Importance"} {(doc.importance * 100).toFixed(0)}%
        </span>
        {doc.tags.length > 0 && (
          <>
            <span style={dotStyle}>·</span>
            <span>{doc.tags.join(", ")}</span>
          </>
        )}
      </div>
    </div>
  );
}

function StatPill({
  label,
  value,
  accent,
  onClick,
  active,
  tooltip,
}: {
  label: string;
  value: number;
  accent: string;
  onClick?: () => void;
  active?: boolean;
  tooltip?: string;
}) {
  return (
    <button
      type="button"
      onClick={onClick}
      disabled={!onClick}
      title={tooltip}
      style={{
        ...statPillStyle,
        cursor: onClick ? "pointer" : "default",
        background: active ? accent + "22" : "rgba(255,255,255,0.04)",
        borderColor: active ? accent : "rgba(255,255,255,0.08)",
      }}
    >
      <span style={{ color: accent, fontWeight: 700, fontSize: 14 }}>{value}</span>
      <span style={{ color: "#9896a4", fontSize: 11 }}>{label}</span>
    </button>
  );
}

// ── styles ──────────────────────────────────────────────────────

const pageStyle: React.CSSProperties = {
  padding: "20px 24px",
  height: "100%",
  overflowY: "auto",
  display: "flex",
  flexDirection: "column",
  gap: 12,
};

const headerStyle: React.CSSProperties = {
  display: "flex",
  alignItems: "center",
  gap: 10,
};

const inputStyle: React.CSSProperties = {
  width: "100%",
  padding: "8px 12px",
  background: "#1f2126",
  border: "1px solid rgba(255,255,255,0.09)",
  borderRadius: 7,
  color: "#eceaf4",
  fontSize: 13,
  outline: "none",
  fontFamily: "inherit",
};

const refreshBtnStyle: React.CSSProperties = {
  padding: "8px 14px",
  background: "rgba(255,255,255,0.04)",
  border: "1px solid rgba(255,255,255,0.09)",
  borderRadius: 7,
  color: "#a8a6b2",
  fontSize: 12,
  cursor: "pointer",
  fontFamily: "inherit",
};

const statsRowStyle: React.CSSProperties = {
  display: "flex",
  alignItems: "center",
  gap: 8,
  flexWrap: "wrap",
};

const statPillStyle: React.CSSProperties = {
  display: "flex",
  alignItems: "center",
  gap: 6,
  padding: "5px 11px",
  borderRadius: 999,
  border: "1px solid rgba(255,255,255,0.08)",
  background: "rgba(255,255,255,0.04)",
  fontFamily: "inherit",
  transition: "background 0.12s, border-color 0.12s",
};

const clearTierBtnStyle: React.CSSProperties = {
  padding: "4px 10px",
  background: "transparent",
  border: "1px solid rgba(255,255,255,0.12)",
  borderRadius: 999,
  color: "#9896a4",
  fontSize: 11,
  cursor: "pointer",
  fontFamily: "inherit",
};

const filterRowStyle: React.CSSProperties = {
  display: "flex",
  alignItems: "center",
  justifyContent: "space-between",
  gap: 10,
  flexWrap: "wrap",
};

const filterControlsStyle: React.CSSProperties = {
  display: "flex",
  alignItems: "center",
  gap: 10,
  flexWrap: "wrap",
};

const selectStyle: React.CSSProperties = {
  padding: "6px 10px",
  background: "#1f2126",
  border: "1px solid rgba(255,255,255,0.09)",
  borderRadius: 6,
  color: "#eceaf4",
  fontSize: 12,
  cursor: "pointer",
  fontFamily: "inherit",
};

const countLabelStyle: React.CSSProperties = {
  fontSize: 11,
  color: "#6b6877",
  fontFamily: "inherit",
  whiteSpace: "nowrap",
};

const errorStyle: React.CSSProperties = {
  padding: "10px 14px",
  background: "rgba(217,95,95,0.08)",
  border: "1px solid rgba(217,95,95,0.4)",
  borderRadius: 7,
  color: "#fca5a5",
  fontSize: 12,
};

const listStyle: React.CSSProperties = {
  display: "flex",
  flexDirection: "column",
  gap: 8,
  paddingBottom: 24,
};

const emptyStyle: React.CSSProperties = {
  padding: 40,
  textAlign: "center",
  color: "#6b6877",
  fontSize: 13,
  border: "1px dashed rgba(255,255,255,0.06)",
  borderRadius: 8,
};

const cardStyle: React.CSSProperties = {
  padding: "12px 14px",
  background: "#1a1c22",
  border: "1px solid rgba(255,255,255,0.06)",
  borderRadius: 8,
};

const cardHeaderStyle: React.CSSProperties = {
  display: "flex",
  alignItems: "center",
  gap: 8,
  marginBottom: 8,
};

const tierChipStyle: React.CSSProperties = {
  padding: "2px 8px",
  fontSize: 10,
  fontWeight: 700,
  letterSpacing: 0.4,
  textTransform: "uppercase",
  borderRadius: 999,
  border: "1px solid",
  fontFamily: "'JetBrains Mono', monospace",
};

const kindChipStyle: React.CSSProperties = {
  padding: "2px 8px",
  fontSize: 10,
  fontWeight: 600,
  color: "#a8a6b2",
  borderRadius: 999,
  border: "1px solid rgba(255,255,255,0.08)",
  background: "rgba(255,255,255,0.03)",
  fontFamily: "'JetBrains Mono', monospace",
};

const scopeStyle: React.CSSProperties = {
  fontSize: 11,
  color: "#9896a4",
  fontFamily: "'JetBrains Mono', monospace",
};

const pinnedStyle: React.CSSProperties = {
  fontSize: 12,
};

const spacerStyle: React.CSSProperties = {
  flex: 1,
};

const scoreStyle: React.CSSProperties = {
  fontSize: 11,
  color: "#f97316",
  fontFamily: "'JetBrains Mono', monospace",
  whiteSpace: "nowrap",
};

const abstractStyle: React.CSSProperties = {
  fontSize: 12,
  fontStyle: "italic",
  color: "#cfcdd8",
  marginBottom: 6,
  lineHeight: 1.5,
};

const textStyle: React.CSSProperties = {
  fontSize: 13,
  color: "#eceaf4",
  lineHeight: 1.55,
  whiteSpace: "pre-wrap",
  wordBreak: "break-word",
  cursor: "pointer",
};

const cardFooterStyle: React.CSSProperties = {
  display: "flex",
  alignItems: "center",
  gap: 4,
  marginTop: 10,
  fontSize: 10.5,
  color: "#6b6877",
  flexWrap: "wrap",
};

const dotStyle: React.CSSProperties = {
  color: "#4a4856",
};
