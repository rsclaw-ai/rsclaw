/**
 * First-run nudge for personalising `USER.md`. Mounted above the
 * chat message list; visible only when the current agent's USER.md
 * is still the placeholder AND the user hasn't dismissed it.
 *
 * Click "Start" sends a short prompt to the active chat:
 *   "用 ask_user 问我几个问题完善 USER.md，然后写入"
 *
 * The agent picks up from there — uses the `ask_user` tool to
 * collect name / use-case / style, then `write_workspace_file` to
 * persist the markdown. As soon as USER.md is no longer the
 * placeholder, this banner self-hides on the next focus / chat
 * turn finish.
 *
 * Dismiss (✕) is sticky per browser via localStorage. The banner
 * never reappears for users who said no, even on fresh USER.md.
 */

import { useCallback, useEffect, useState } from "react";

import { useChatStore } from "../store";
import { isUserMdDefault, readUserMd } from "../lib/user-md";
import { getLang } from "../locales";

const DISMISS_KEY = "rsclaw-user-md-banner-dismissed";

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

  const zh = getLang() === "cn";
  const t = zh
    ? {
        title: "完善偏好，AI 更懂你",
        sub: "回答几个问题，自动写入 USER.md",
        start: "开始 →",
        starting: "已发送…",
        dismiss: "不再提醒",
      }
    : {
        title: "Personalize your AI",
        sub: "Answer a few questions to seed USER.md",
        start: "Start →",
        starting: "Sent…",
        dismiss: "Don't show again",
      };

  // Read USER.md on mount, on focus, and whenever the active agent
  // changes — switching agents means a different workspace + a
  // different USER.md state.
  const check = useCallback(async () => {
    if (!agentId) {
      setNeedsSetup(false);
      return;
    }
    const content = await readUserMd(agentId);
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

  // Subscribe to chat-store updates so we re-check after each turn
  // ends — the agent's call to `write_workspace_file` happens
  // inside a tool turn, and the file write itself isn't an event
  // we'd otherwise observe. Cheap: just a string compare.
  useEffect(() => {
    if (dismissed) return;
    const unsub = useChatStore.subscribe((state, prev) => {
      const cur = state.currentSession?.();
      const old = prev.currentSession?.();
      if (cur && old && cur.messages.length !== old.messages.length) {
        void check();
      }
    });
    return unsub;
  }, [check, dismissed]);

  if (!needsSetup || dismissed) return null;

  const handleStart = async () => {
    if (busy) return;
    setBusy(true);
    const prompt = zh
      ? "请用 ask_user 工具问我 3 个问题来了解我的偏好：1) 怎么称呼我；2) 主要使用场景（可多选：写代码 / 写作 / 数据分析 / 多渠道消息处理）；3) 沟通风格偏好（直接简洁 / 详细解释 / 学术严谨）。收完后用 write_workspace_file 工具把答案整理成 markdown 写入 USER.md（fileName=\"USER.md\"），结构使用「## 关于我」「## 主要场景」「## 沟通风格」三段。"
      : "Please use the ask_user tool to ask me 3 questions: 1) what to call me; 2) primary use cases (multi-select: coding / writing / data analysis / multi-channel messaging); 3) communication style (concise / detailed / academic). After collecting answers, use write_workspace_file to save them as markdown to USER.md (fileName=\"USER.md\") with sections '## About me', '## Use cases', '## Style'.";
    try {
      await useChatStore.getState().onUserInput(prompt, []);
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
    <div style={containerStyle}>
      <div style={textColStyle}>
        <span style={titleStyle}>💡 {t.title}</span>
        <span style={subStyle}>{t.sub}</span>
      </div>
      <button
        type="button"
        onClick={() => void handleStart()}
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
  );
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
