/**
 * First-run nudge for personalising `USER.md`. Mounted above the
 * chat message list; visible only when the current agent's USER.md
 * is still the placeholder AND the user hasn't dismissed it.
 *
 * Click "Start" opens a small 3-step wizard (name / use-cases / style)
 * that writes USER.md directly via the `write_workspace_file` Tauri
 * command — no LLM round trip. Previously we sent a prompt to the
 * agent asking it to use `ask_user` for the same data, but the model
 * consistently jammed three different question types into a single
 * ask_user call, producing a flat unfilterable radio list. Driving
 * the form from the UI makes the result deterministic.
 *
 * Dismiss (✕) is sticky per browser via localStorage. The banner
 * never reappears for users who said no, even on fresh USER.md.
 */

import JSON5 from "json5";
import { useCallback, useEffect, useRef, useState } from "react";

import { useChatStore } from "../store";
import { toast } from "../lib/toast";
import { isUserMdDefault, readUserMd } from "../lib/user-md";
import { getLang } from "../locales";
import { invoke, isTauri } from "../utils/tauri";

const DISMISS_KEY = "rsclaw-user-md-banner-dismissed";

// Resolve a workable agentId when the current session has none. The
// default empty session created on first launch has no agentId until
// the user explicitly picks an agent — without a fallback the banner
// stays hidden forever for first-run users. Cached at module scope so
// we don't re-read rsclaw.json5 on every focus tick.
let cachedDefaultAgentId: string | null = null;
let defaultAgentIdInflight: Promise<string | null> | null = null;
async function resolveDefaultAgentId(): Promise<string | null> {
  if (cachedDefaultAgentId !== null) return cachedDefaultAgentId;
  if (!isTauri) return null;
  if (defaultAgentIdInflight) return defaultAgentIdInflight;
  defaultAgentIdInflight = (async () => {
    try {
      const raw = (await invoke("read_config_file")) as string;
      const cfg = JSON5.parse(raw || "{}") as any;
      const first = cfg?.agents?.list?.[0]?.id;
      cachedDefaultAgentId = typeof first === "string" && first ? first : "";
    } catch {
      cachedDefaultAgentId = "";
    }
    return cachedDefaultAgentId;
  })();
  const result = await defaultAgentIdInflight;
  defaultAgentIdInflight = null;
  return result;
}

export function UserMdBanner() {
  const session = useChatStore((s) => s.currentSession());
  const agentId = session?.agentId || "";

  const [needsSetup, setNeedsSetup] = useState(false);
  const [dismissed, setDismissed] = useState(() => {
    try {
      return localStorage.getItem(DISMISS_KEY) === "1";
    } catch {
      return false;
    }
  });
  const [busy, setBusy] = useState(false);
  const [wizardOpen, setWizardOpen] = useState(false);

  const zh = getLang() === "cn";
  const t = zh
    ? {
        title: "完善偏好，AI 更懂你",
        sub: "回答几个问题，自动写入 USER.md",
        start: "开始 →",
        starting: "保存中…",
        dismiss: "不再提醒",
      }
    : {
        title: "Personalize your AI",
        sub: "Answer a few questions to seed USER.md",
        start: "Start →",
        starting: "Saving…",
        dismiss: "Don't show again",
      };

  // Read USER.md on mount, on focus, and whenever the active agent
  // changes — switching agents means a different workspace + a
  // different USER.md state.
  const check = useCallback(async () => {
    let id = agentId;
    if (!id) {
      // Default empty session (first launch) has no agentId. Fall back
      // to the first agent declared in rsclaw.json5 so the banner can
      // still nudge the user even before they pick an agent.
      id = (await resolveDefaultAgentId()) || "";
    }
    if (!id) {
      setNeedsSetup(false);
      return;
    }
    const content = await readUserMd(id);
    setNeedsSetup(isUserMdDefault(content));
  }, [agentId]);

  useEffect(() => {
    if (dismissed) {
      setNeedsSetup(false);
      return;
    }
    void check();
    const onFocus = () => void check();
    window.addEventListener("focus", onFocus);
    return () => window.removeEventListener("focus", onFocus);
  }, [check, dismissed]);

  // Re-check after each turn FINISHES — the agent's call to
  // `write_workspace_file` happens inside a tool turn and the file
  // write itself isn't an event we'd otherwise observe.
  //
  // Narrow signal: fire only on the streaming=true → false transition
  // of the last assistant message. The previous version subscribed
  // on every store change and re-read the disk on every message add
  // (user msg + assistant start), doubling disk reads per turn and
  // firing during keystrokes that touched any other store field.
  const lastStreamingRef = useRef<boolean | undefined>(undefined);
  useEffect(() => {
    if (dismissed) return;
    const unsub = useChatStore.subscribe((state) => {
      const cur = state.currentSession?.();
      const last = cur?.messages[cur.messages.length - 1];
      const streaming = last?.streaming;
      const prev = lastStreamingRef.current;
      lastStreamingRef.current = streaming;
      // Edge: streaming flipped from true → false (turn just ended).
      if (prev === true && streaming !== true) {
        void check();
      }
    });
    return unsub;
  }, [check, dismissed]);

  if (!needsSetup || dismissed) return null;

  const handleStart = () => {
    setWizardOpen(true);
  };

  // Wizard "Done": render the collected answers as markdown and write
  // it directly via the Tauri write_workspace_file command. No agent
  // turn, no chat message. The doc model loads USER.md on the next
  // chat turn or on agent restart, so personalisation surfaces
  // immediately to the LLM without polling.
  const handleWizardSave = async (
    name: string,
    useCases: string[],
    style: string,
  ) => {
    setBusy(true);
    try {
      let id = agentId;
      if (!id) id = (await resolveDefaultAgentId()) || "";
      if (!id) {
        toast.error(zh ? "未找到智能体" : "No agent available");
        return;
      }
      const md = buildUserMd(name, useCases, style, zh);
      if (isTauri) {
        await invoke("write_workspace_file", {
          agentId: id,
          fileName: "USER.md",
          content: md,
        });
      } else {
        // Web mode — go through the gateway's workspace endpoint.
        const { gatewayFetch } = await import("../lib/rsclaw-api");
        await gatewayFetch(`/api/v1/workspace/files/USER.md`, {
          method: "PUT",
          body: md,
          headers: { "Content-Type": "text/markdown" },
        });
      }
      setWizardOpen(false);
      setNeedsSetup(false); // hide banner immediately (file is now non-default)
      toast.success(zh ? "已保存到 USER.md" : "Saved to USER.md");
    } catch (e: any) {
      toast.fromError(zh ? "保存失败" : "Save failed", e);
    } finally {
      setBusy(false);
    }
  };

  const handleDismiss = () => {
    setDismissed(true);
    try {
      localStorage.setItem(DISMISS_KEY, "1");
    } catch {
      /* localStorage unavailable */
    }
  };

  return (
    <>
      <div style={containerStyle}>
        <div style={textColStyle}>
          <span style={titleStyle}>💡 {t.title}</span>
          <span style={subStyle}>{t.sub}</span>
        </div>
        <button
          type="button"
          onClick={handleStart}
          disabled={busy}
          style={startBtnStyle}
          onMouseEnter={(e) => {
            if (!busy) e.currentTarget.style.background = "#ea6a13";
          }}
          onMouseLeave={(e) => {
            if (!busy) e.currentTarget.style.background = "#f97316";
          }}
        >
          {busy ? t.starting : t.start}
        </button>
        <button
          type="button"
          onClick={handleDismiss}
          style={dismissBtnStyle}
          title={t.dismiss}
          aria-label={t.dismiss}
          onMouseEnter={(e) => (e.currentTarget.style.color = "#cfcdd8")}
          onMouseLeave={(e) => (e.currentTarget.style.color = "#6b6877")}
        >
          ✕
        </button>
      </div>
      {wizardOpen && (
        <UserMdWizard
          zh={zh}
          busy={busy}
          onCancel={() => setWizardOpen(false)}
          onSave={handleWizardSave}
        />
      )}
    </>
  );
}

// ─────────────────────────────────────────────────────────────────
// Wizard
// ─────────────────────────────────────────────────────────────────

const USE_CASES_ZH = [
  "私人助理",
  "产品运营",
  "数据分析",
  "软件开发",
  "数字员工",
  "电子商务",
];
const USE_CASES_EN = [
  "Personal assistant",
  "Product operations",
  "Data analysis",
  "Software development",
  "Digital workforce",
  "E-commerce",
];
const STYLES_ZH = ["直接简洁", "详细解释", "学术严谨"];
const STYLES_EN = ["Concise", "Detailed", "Academic"];

function UserMdWizard({
  zh,
  busy,
  onCancel,
  onSave,
}: {
  zh: boolean;
  busy: boolean;
  onCancel: () => void;
  onSave: (name: string, useCases: string[], style: string) => Promise<void>;
}) {
  // Single-page form. The earlier 3-step wizard added two extra clicks
  // for what's really just three short fields the user reads in a
  // glance. Name is the only required field; missing use-case / style
  // just produces shorter sections in the resulting USER.md.
  const [name, setName] = useState("");
  const [useCases, setUseCases] = useState<Set<string>>(new Set());
  const [otherChecked, setOtherChecked] = useState(false);
  const [otherText, setOtherText] = useState("");
  const [style, setStyle] = useState<string>("");
  // Same "Other" pattern for the single-select style: radio + inline
  // input. When the Other radio is picked, the predefined `style` is
  // cleared and the saved value comes from `otherStyleText`.
  const [otherStyleSel, setOtherStyleSel] = useState(false);
  const [otherStyleText, setOtherStyleText] = useState("");

  const cases = zh ? USE_CASES_ZH : USE_CASES_EN;
  const styles = zh ? STYLES_ZH : STYLES_EN;

  const canSave = name.trim().length > 0;

  const submit = () => {
    if (!canSave || busy) return;
    // Append the custom "Other" entry only when the box is checked AND
    // non-empty. A checked-but-empty Other is silently dropped — no
    // point pinning a bullet that says nothing.
    const merged = Array.from(useCases);
    if (otherChecked && otherText.trim()) merged.push(otherText.trim());
    const effectiveStyle = otherStyleSel ? otherStyleText.trim() : style;
    void onSave(name.trim(), merged, effectiveStyle);
  };

  const toggleCase = (c: string) => {
    setUseCases((prev) => {
      const next = new Set(prev);
      if (next.has(c)) next.delete(c);
      else next.add(c);
      return next;
    });
  };

  return (
    <div
      onClick={(e) => {
        if (e.target === e.currentTarget && !busy) onCancel();
      }}
      style={maskStyle}
    >
      <div style={cardStyle}>
        <div style={cardHeaderStyle}>
          <span style={cardTitleStyle}>{zh ? "完善偏好" : "Personalize"}</span>
          <span style={{ fontSize: 11, color: "#9896a4" }}>
            {zh ? "仅称呼必填" : "Only name required"}
          </span>
        </div>

        <div style={cardBodyStyle}>
          {/* 1. Name */}
          <label style={cardLabelStyle}>
            {zh ? "怎么称呼您？" : "What should I call you?"}
          </label>
          <input
            autoFocus
            value={name}
            onChange={(e) => setName(e.target.value)}
            onKeyDown={(e) => {
              if (e.key === "Enter" && (e.metaKey || e.ctrlKey)) submit();
            }}
            placeholder={zh ? "例如：张三 / 老李" : "e.g. Alice"}
            style={inputStyle}
            maxLength={64}
          />

          {/* 2. Use cases */}
          <label style={{ ...cardLabelStyle, marginTop: 10 }}>
            {zh ? "主要使用场景（可多选）" : "Primary use cases (multi-select)"}
          </label>
          <div style={{ display: "grid", gridTemplateColumns: "1fr 1fr", gap: 6 }}>
            {cases.map((c) => {
              const checked = useCases.has(c);
              return (
                <label
                  key={c}
                  style={{ ...optionRowStyle, borderColor: checked ? "#f97316" : "rgba(255,255,255,.09)" }}
                >
                  <input
                    type="checkbox"
                    checked={checked}
                    onChange={() => toggleCase(c)}
                    style={{ accentColor: "#f97316" }}
                  />
                  <span>{c}</span>
                </label>
              );
            })}
          </div>
          {/* "Other" — full-width row with inline text. Checking it
              auto-focuses the input; typing into the input auto-checks. */}
          <label
            style={{
              ...optionRowStyle,
              gap: 8,
              marginTop: 6,
              borderColor: otherChecked && otherText.trim() ? "#f97316" : "rgba(255,255,255,.09)",
            }}
          >
            <input
              type="checkbox"
              checked={otherChecked}
              onChange={(e) => setOtherChecked(e.target.checked)}
              style={{ accentColor: "#f97316", flexShrink: 0 }}
            />
            <span style={{ flexShrink: 0 }}>{zh ? "其他" : "Other"}</span>
            <input
              type="text"
              value={otherText}
              onChange={(e) => {
                setOtherText(e.target.value);
                if (!otherChecked && e.target.value) setOtherChecked(true);
              }}
              onFocus={() => {
                if (!otherChecked) setOtherChecked(true);
              }}
              placeholder={zh ? "输入自定义场景..." : "Type a custom case..."}
              maxLength={80}
              style={{
                flex: 1,
                background: "transparent",
                border: "none",
                color: "#eceaf4",
                fontSize: 12.5,
                outline: "none",
                padding: 0,
                minWidth: 0,
              }}
            />
          </label>

          {/* 3. Style */}
          <label style={{ ...cardLabelStyle, marginTop: 10 }}>
            {zh ? "沟通风格" : "Communication style"}
          </label>
          <div style={{ display: "flex", gap: 6, flexWrap: "wrap" }}>
            {styles.map((s) => {
              const checked = !otherStyleSel && style === s;
              return (
                <label
                  key={s}
                  style={{
                    ...optionRowStyle,
                    flex: "1 1 0",
                    minWidth: 0,
                    justifyContent: "center",
                    borderColor: checked ? "#f97316" : "rgba(255,255,255,.09)",
                  }}
                >
                  <input
                    type="radio"
                    name="ump-style"
                    checked={checked}
                    onChange={() => {
                      setStyle(s);
                      setOtherStyleSel(false);
                    }}
                    style={{ accentColor: "#f97316" }}
                  />
                  <span>{s}</span>
                </label>
              );
            })}
          </div>
          {/* Style "Other" — full-width inline input row, single-select. */}
          <label
            style={{
              ...optionRowStyle,
              gap: 8,
              marginTop: 6,
              borderColor:
                otherStyleSel && otherStyleText.trim() ? "#f97316" : "rgba(255,255,255,.09)",
            }}
          >
            <input
              type="radio"
              name="ump-style"
              checked={otherStyleSel}
              onChange={() => {
                setOtherStyleSel(true);
                setStyle("");
              }}
              style={{ accentColor: "#f97316", flexShrink: 0 }}
            />
            <span style={{ flexShrink: 0 }}>{zh ? "其他" : "Other"}</span>
            <input
              type="text"
              value={otherStyleText}
              onChange={(e) => {
                setOtherStyleText(e.target.value);
                if (e.target.value && !otherStyleSel) {
                  setOtherStyleSel(true);
                  setStyle("");
                }
              }}
              onFocus={() => {
                if (!otherStyleSel) {
                  setOtherStyleSel(true);
                  setStyle("");
                }
              }}
              placeholder={zh ? "输入自定义风格..." : "Type a custom style..."}
              maxLength={80}
              style={{
                flex: 1,
                background: "transparent",
                border: "none",
                color: "#eceaf4",
                fontSize: 12.5,
                outline: "none",
                padding: 0,
                minWidth: 0,
              }}
            />
          </label>
        </div>

        <div style={cardFooterStyle}>
          <button type="button" onClick={onCancel} disabled={busy} style={ghostBtnStyle}>
            {zh ? "取消" : "Cancel"}
          </button>
          <button
            type="button"
            onClick={submit}
            disabled={!canSave || busy}
            style={{
              ...primaryBtnStyle,
              opacity: !canSave || busy ? 0.5 : 1,
              cursor: !canSave || busy ? "not-allowed" : "pointer",
            }}
          >
            {busy ? (zh ? "保存中…" : "Saving…") : zh ? "保存" : "Save"}
          </button>
        </div>
      </div>
    </div>
  );
}

// Build the markdown that goes into USER.md. Keeps a stable section
// structure so the agent can reliably extract preferences later.
function buildUserMd(name: string, useCases: string[], style: string, zh: boolean): string {
  if (zh) {
    const parts = ["# USER.md", ""];
    parts.push("## 关于我");
    if (name) parts.push(`请称呼我「${name}」。`);
    parts.push("");
    parts.push("## 主要场景");
    for (const c of useCases) parts.push(`- ${c}`);
    parts.push("");
    parts.push("## 沟通风格");
    if (style) parts.push(style);
    parts.push("");
    return parts.join("\n");
  }
  const parts = ["# USER.md", ""];
  parts.push("## About me");
  if (name) parts.push(`Please call me "${name}".`);
  parts.push("");
  parts.push("## Use cases");
  for (const c of useCases) parts.push(`- ${c}`);
  parts.push("");
  parts.push("## Communication style");
  if (style) parts.push(style);
  parts.push("");
  return parts.join("\n");
}

// ── styles ──
// Subtle brand-orange tint, single line, sits as a thin strip above
// the chat. Visual weight should read as "tip" not "alert".

const containerStyle: React.CSSProperties = {
  display: "flex",
  alignItems: "center",
  gap: 12,
  padding: "8px 14px",
  margin: "8px 12px 0",
  borderRadius: 8,
  background: "rgba(249, 115, 22, 0.06)",
  border: "1px solid rgba(249, 115, 22, 0.22)",
};

const textColStyle: React.CSSProperties = {
  flex: 1,
  display: "flex",
  alignItems: "baseline",
  gap: 10,
  minWidth: 0,
};

const titleStyle: React.CSSProperties = {
  fontSize: 12.5,
  fontWeight: 600,
  color: "#eceaf4",
  whiteSpace: "nowrap",
};

const subStyle: React.CSSProperties = {
  fontSize: 11.5,
  color: "#9896a4",
  overflow: "hidden",
  textOverflow: "ellipsis",
  whiteSpace: "nowrap",
};

// ── Wizard styles ──

const maskStyle: React.CSSProperties = {
  position: "fixed",
  inset: 0,
  background: "rgba(5,5,7,.72)",
  backdropFilter: "blur(3px)",
  display: "flex",
  alignItems: "center",
  justifyContent: "center",
  zIndex: 100,
};

const cardStyle: React.CSSProperties = {
  width: 420,
  maxWidth: "92vw",
  background: "#1a1c22",
  border: "1px solid rgba(255,255,255,.09)",
  borderRadius: 14,
  overflow: "hidden",
  boxShadow: "0 20px 60px rgba(0,0,0,.6)",
  display: "flex",
  flexDirection: "column",
};

const cardHeaderStyle: React.CSSProperties = {
  padding: "16px 22px 0",
  display: "flex",
  alignItems: "center",
  justifyContent: "space-between",
};

const cardTitleStyle: React.CSSProperties = {
  fontSize: 14,
  fontWeight: 700,
  color: "#eceaf4",
};

const stepDotsStyle: React.CSSProperties = {
  display: "flex",
  gap: 5,
};

const cardBodyStyle: React.CSSProperties = {
  padding: "18px 22px",
  display: "flex",
  flexDirection: "column",
  gap: 10,
};

const cardLabelStyle: React.CSSProperties = {
  fontSize: 12.5,
  color: "#9896a4",
  marginBottom: 4,
};

const inputStyle: React.CSSProperties = {
  background: "#141618",
  border: "1px solid rgba(255,255,255,.09)",
  borderRadius: 8,
  padding: "9px 12px",
  color: "#eceaf4",
  fontSize: 13,
  outline: "none",
};

const optionRowStyle: React.CSSProperties = {
  display: "flex",
  alignItems: "center",
  gap: 10,
  padding: "9px 12px",
  borderRadius: 8,
  border: "1px solid rgba(255,255,255,.09)",
  background: "#141618",
  fontSize: 12.5,
  color: "#eceaf4",
  cursor: "pointer",
  transition: "border-color .1s",
};

const cardFooterStyle: React.CSSProperties = {
  padding: "12px 22px 18px",
  display: "flex",
  justifyContent: "space-between",
  gap: 8,
};

const ghostBtnStyle: React.CSSProperties = {
  padding: "7px 16px",
  borderRadius: 8,
  border: "1px solid rgba(255,255,255,.09)",
  background: "transparent",
  color: "#9896a4",
  fontSize: 12,
  cursor: "pointer",
};

const primaryBtnStyle: React.CSSProperties = {
  padding: "7px 18px",
  borderRadius: 8,
  border: "1px solid #f97316",
  background: "#f97316",
  color: "#fff",
  fontSize: 12,
  fontWeight: 600,
  cursor: "pointer",
};

const startBtnStyle: React.CSSProperties = {
  padding: "5px 12px",
  fontSize: 12,
  fontWeight: 600,
  color: "#fff",
  background: "#f97316",
  border: "1px solid #f97316",
  borderRadius: 6,
  cursor: "pointer",
  fontFamily: "inherit",
  transition: "background 0.12s",
  flexShrink: 0,
};

const dismissBtnStyle: React.CSSProperties = {
  width: 24,
  height: 24,
  display: "flex",
  alignItems: "center",
  justifyContent: "center",
  background: "transparent",
  border: "none",
  color: "#6b6877",
  fontSize: 13,
  cursor: "pointer",
  flexShrink: 0,
  transition: "color 0.12s",
};
