/**
 * "Recommended" card for the rsclaw.ai cloud account, rendered at the
 * top of onboarding step 2 above the regular provider grid.
 *
 * Flow:
 *   1. User clicks "一键配置" → `openRsclawConsole()` spawns the child
 *      webview pointing at `api.rsclaw.ai/console`.
 *   2. They log in / sign up on the web side.
 *   3. When they click "Install to RsClaw Desktop" on the keys page
 *      (web team work), the injected channel calls back here via the
 *      `rsclaw:console-install-key` Tauri event.
 *   4. `applyInstalledKey` writes the key into `rsclaw.json5` and we
 *      notify the parent so it can flip its provider state to
 *      `rsclaw` selected.
 *
 * Manual paste fallback lives in a disclosure below the primary CTA:
 * for users who'd rather copy a key by hand, or as a recovery path if
 * the web team's install button isn't shipped yet.
 *
 * Inline styles match the rest of onboarding.tsx (it uses an `S`
 * object of inline styles, no CSS modules) so the card visually
 * belongs to the same step.
 */

import { useEffect, useMemo, useState } from "react";

import {
  applyInstalledKey,
  closeRsclawConsole,
  isLikelyRsclawKey,
  onKeyInstalled,
  openRsclawConsole,
  type InstalledKeyData,
} from "../lib/rsclaw-console";

type Props = {
  zh: boolean;
  /**
   * Notify the parent that a key has been installed into rsclaw.json5.
   * The parent should flip its provider-state slot for `rsclaw` to
   * selected + apiKey-set so the rest of step 2 (model picker, test)
   * just works.
   */
  onInstalled: (data: InstalledKeyData) => void;
};

type State =
  | { kind: "idle" }
  | { kind: "opening" }
  | { kind: "waiting" }
  | { kind: "installing" }
  | { kind: "installed"; name?: string; tier?: string }
  | { kind: "error"; message: string };

export function RsclawRecommendedCard(props: Props) {
  const { zh, onInstalled } = props;

  const [state, setState] = useState<State>({ kind: "idle" });
  const [manualOpen, setManualOpen] = useState(false);
  const [manualKey, setManualKey] = useState("");

  const t = useMemo(
    () =>
      zh
        ? {
            badge: "推荐",
            title: "rsclaw",
            tagline: "专为智能体优化 · 业内首发增量传输协议",
            sub: "专用模型 + 免费版 50 次/天 · 无需自备 OpenAI / Anthropic key",
            oneClick: "一键配置",
            opening: "打开 console…",
            waiting: "等待你在 console 创建 key…",
            installing: "正在写入配置…",
            installed: "已连接",
            reopen: "打开 console",
            reconnect: "重新连接",
            manualToggle: "或手动粘贴 key",
            pastePlaceholder: "sk-rsclaw-...",
            install: "配置",
            invalidKey: "key 格式不对（应该以 sk-rsclaw- 开头）",
            installError: "写入配置失败",
          }
        : {
            badge: "Recommended",
            title: "rsclaw",
            tagline: "Tuned for agents · Industry-first delta protocol",
            sub: "Specialized models + 50 free calls/day · No OpenAI / Anthropic key needed",
            oneClick: "One-click setup",
            opening: "Opening console…",
            waiting: "Waiting for you to create a key…",
            installing: "Writing config…",
            installed: "Connected",
            reopen: "Open console",
            reconnect: "Reconnect",
            manualToggle: "Or paste a key manually",
            pastePlaceholder: "sk-rsclaw-...",
            install: "Configure",
            invalidKey: "Invalid key (should start with sk-rsclaw-)",
            installError: "Failed to write config",
          },
    [zh],
  );

  // Subscribe to install events from the webview. The listener stays
  // armed across the card's whole lifetime so a key landing after the
  // user closes-and-reopens the console (or hits the link from
  // another part of onboarding later) still gets picked up.
  useEffect(() => {
    let cancelled = false;
    let unlisten: (() => void) | undefined;

    onKeyInstalled(async (data) => {
      if (cancelled) return;
      setState({ kind: "installing" });
      const res = await applyInstalledKey(data);
      if (cancelled) return;
      if (!res.ok) {
        setState({
          kind: "error",
          message: res.error || t.installError,
        });
        return;
      }
      setState({
        kind: "installed",
        name: data.name,
        tier: data.tier,
      });
      // Auto-close the console so the user lands back on onboarding.
      // Best-effort: webview close is fire-and-forget anyway.
      void closeRsclawConsole();
      onInstalled(data);
    })
      .then((fn) => {
        unlisten = fn;
      })
      .catch(() => {
        /* tauri event subscription failed — manual paste still works */
      });

    return () => {
      cancelled = true;
      unlisten?.();
    };
  }, [onInstalled, t.installError]);

  const handleOneClick = async () => {
    setState({ kind: "opening" });
    try {
      // Land directly on the keys page — the user comes here to
      // create+install, not to read marketing copy.
      await openRsclawConsole({ path: "/keys" });
      setState({ kind: "waiting" });
    } catch (e) {
      setState({
        kind: "error",
        message: e instanceof Error ? e.message : String(e),
      });
    }
  };

  const handleManualInstall = async () => {
    const key = manualKey.trim();
    if (!isLikelyRsclawKey(key)) {
      setState({ kind: "error", message: t.invalidKey });
      return;
    }
    setState({ kind: "installing" });
    const res = await applyInstalledKey({ key });
    if (!res.ok) {
      setState({ kind: "error", message: res.error || t.installError });
      return;
    }
    setState({ kind: "installed" });
    onInstalled({ key });
  };

  // ── inline styles, matching onboarding's `S` aesthetic ──
  const borderColor =
    state.kind === "installed"
      ? "rgba(45,212,160,0.55)"
      : state.kind === "error"
        ? "rgba(217,95,95,0.55)"
        : "rgba(249,115,22,0.35)";

  const cardStyle: React.CSSProperties = {
    border: `1px solid ${borderColor}`,
    background:
      "linear-gradient(135deg, rgba(249,115,22,0.06) 0%, rgba(249,115,22,0.02) 100%)",
    borderRadius: 10,
    padding: 14,
    marginBottom: 12,
    boxShadow: "0 0 24px rgba(249,115,22,0.08)",
    transition: "border-color 0.15s, box-shadow 0.15s",
  };

  const badgeStyle: React.CSSProperties = {
    display: "inline-block",
    fontSize: 10,
    fontWeight: 700,
    letterSpacing: 0.6,
    textTransform: "uppercase",
    color: "#f97316",
    background: "rgba(249,115,22,0.12)",
    padding: "2px 8px",
    borderRadius: 999,
    fontFamily: "'JetBrains Mono', monospace",
  };

  const titleRowStyle: React.CSSProperties = {
    display: "flex",
    alignItems: "center",
    gap: 10,
    marginTop: 8,
  };

  const titleStyle: React.CSSProperties = {
    fontSize: 16,
    fontWeight: 700,
    color: "#eceaf4",
    fontFamily: "'JetBrains Mono', monospace",
  };

  const taglineStyle: React.CSSProperties = {
    fontSize: 12,
    color: "#9896a4",
    marginTop: 4,
    lineHeight: 1.5,
  };

  const subStyle: React.CSSProperties = {
    fontSize: 11,
    color: "#6e6c7b",
    marginTop: 2,
    lineHeight: 1.5,
  };

  const primaryBtnStyle: React.CSSProperties = {
    background:
      state.kind === "installed"
        ? "rgba(45,212,160,0.16)"
        : state.kind === "opening" || state.kind === "waiting"
          ? "rgba(249,115,22,0.16)"
          : "#f97316",
    color:
      state.kind === "installed"
        ? "#2dd4a0"
        : state.kind === "opening" || state.kind === "waiting"
          ? "#f97316"
          : "#fff",
    border:
      state.kind === "installed"
        ? "1px solid rgba(45,212,160,0.4)"
        : "1px solid transparent",
    padding: "8px 16px",
    borderRadius: 7,
    fontSize: 12,
    fontWeight: 600,
    cursor:
      state.kind === "opening" || state.kind === "installing"
        ? "wait"
        : "pointer",
    fontFamily: "'JetBrains Mono', monospace",
    transition: "background 0.12s, color 0.12s",
    opacity: state.kind === "installing" ? 0.7 : 1,
  };

  const ghostBtnStyle: React.CSSProperties = {
    background: "transparent",
    color: "#9896a4",
    border: "1px solid rgba(255,255,255,0.09)",
    padding: "8px 14px",
    borderRadius: 7,
    fontSize: 11.5,
    fontWeight: 500,
    cursor: "pointer",
    fontFamily: "'JetBrains Mono', monospace",
  };

  const manualToggleStyle: React.CSSProperties = {
    background: "transparent",
    border: "none",
    color: "#6e6c7b",
    fontSize: 11,
    cursor: "pointer",
    fontFamily: "'JetBrains Mono', monospace",
    textDecoration: "underline",
    textUnderlineOffset: 3,
    textDecorationColor: "rgba(110,108,123,0.4)",
    padding: 0,
  };

  const manualInputStyle: React.CSSProperties = {
    flex: 1,
    background: "#1f2126",
    border: "1px solid rgba(255,255,255,0.09)",
    borderRadius: 6,
    padding: "6px 10px",
    color: "#eceaf4",
    fontSize: 11.5,
    fontFamily: "'JetBrains Mono', monospace",
    outline: "none",
  };

  const actionRowStyle: React.CSSProperties = {
    display: "flex",
    alignItems: "center",
    gap: 10,
    marginTop: 14,
    flexWrap: "wrap",
  };

  const primaryButtonLabel = (() => {
    switch (state.kind) {
      case "opening":
        return t.opening;
      case "waiting":
        return t.waiting;
      case "installing":
        return t.installing;
      case "installed":
        return `✓ ${t.installed}`;
      default:
        return `${t.oneClick} →`;
    }
  })();

  return (
    <div style={cardStyle}>
      <div style={{ display: "flex", alignItems: "center", gap: 8 }}>
        <span style={badgeStyle}>⭐ {t.badge}</span>
        {state.kind === "installed" && (
          <span style={{ fontSize: 10.5, color: "#2dd4a0" }}>
            {state.tier
              ? `${state.name || "rsclaw"} · ${state.tier}`
              : state.name || ""}
          </span>
        )}
      </div>

      <div style={titleRowStyle}>
        <span style={{ fontSize: 18 }}>🦀</span>
        <span style={titleStyle}>{t.title}</span>
      </div>
      <div style={taglineStyle}>{t.tagline}</div>
      <div style={subStyle}>{t.sub}</div>

      <div style={actionRowStyle}>
        <button
          type="button"
          style={primaryBtnStyle}
          onClick={
            state.kind === "installed" ? handleOneClick : handleOneClick
          }
          disabled={state.kind === "installing"}
        >
          {primaryButtonLabel}
        </button>

        {state.kind === "installed" && (
          <button
            type="button"
            style={ghostBtnStyle}
            onClick={() => {
              setState({ kind: "idle" });
              setManualOpen(false);
              setManualKey("");
            }}
          >
            {t.reconnect}
          </button>
        )}

        <button
          type="button"
          style={manualToggleStyle}
          onClick={() => setManualOpen((v) => !v)}
        >
          {t.manualToggle} {manualOpen ? "▴" : "▾"}
        </button>
      </div>

      {manualOpen && (
        <div
          style={{
            marginTop: 10,
            display: "flex",
            gap: 8,
            alignItems: "stretch",
          }}
        >
          <input
            type="text"
            style={manualInputStyle}
            placeholder={t.pastePlaceholder}
            value={manualKey}
            onChange={(e) => setManualKey(e.target.value)}
            onKeyDown={(e) => {
              if (e.key === "Enter") void handleManualInstall();
            }}
          />
          <button
            type="button"
            style={{ ...primaryBtnStyle, padding: "6px 14px" }}
            onClick={() => void handleManualInstall()}
            disabled={state.kind === "installing"}
          >
            {t.install}
          </button>
        </div>
      )}

      {state.kind === "error" && (
        <div
          style={{
            marginTop: 10,
            fontSize: 11,
            color: "#d95f5f",
            fontFamily: "'JetBrains Mono', monospace",
          }}
        >
          {state.message}
        </div>
      )}
    </div>
  );
}
