/**
 * rsclaw provider card for the Config Management → Models tab.
 *
 * Unlike the BYOK provider cards (anthropic / openai / doubao / ...)
 * that the panel renders below this, rsclaw is a **cloud-managed
 * account**: the key is minted by `api.rsclaw.ai/console` and lives
 * in the config purely as a bearer token. Editing baseUrl or pasting
 * arbitrary values isn't the intended path. The card surfaces:
 *
 *   - Connected state (presence of `models.providers.rsclaw.apiKey`)
 *   - The local key name (`_name` metadata) when set
 *   - Single primary action: "open console" → spawns the webview
 *   - Collapsed "advanced" disclosure with manual paste + disconnect
 *     for power users / token rotation
 *
 * Account-related fields (tier label, quota, plan) are deliberately
 * NOT shown — they require a live `/api/v1/console/account` endpoint
 * we haven't wired yet, and stale snapshots from install-time
 * metadata mislead more than help.
 */

import { useCallback, useEffect, useRef, useState } from "react";

import {
  applyInstalledKey,
  disconnectAccount,
  isLikelyRsclawKey,
  onKeyInstalled,
  openRsclawConsole,
  readAccountState,
  type RsclawAccountState,
} from "../lib/rsclaw-console";
import { getLang } from "../locales";

export function RsclawProviderCard() {
  const [state, setState] = useState<RsclawAccountState>({ connected: false });
  const [advancedOpen, setAdvancedOpen] = useState(false);
  const [manualKey, setManualKey] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const zh = getLang() === "cn";
  const t = zh
    ? {
        connected: "已连接",
        disconnected: "未连接",
        tagline: "增量协议 · 业内首发 · 专为智能体优化",
        openConsole: "打开 console →",
        oneClick: "一键配置 →",
        opening: "打开中…",
        advanced: "高级",
        advancedDisconnected: "或者手动粘贴 key",
        apiKey: "API Key",
        replace: "更换",
        save: "保存",
        disconnect: "断开连接",
        disconnecting: "断开中…",
        pastePlaceholder: "sk-rsclaw-...",
        invalidKey: "key 格式不对（应该以 sk-rsclaw- 开头）",
        configSaved: "已写入配置",
      }
    : {
        connected: "Connected",
        disconnected: "Not connected",
        tagline: "Incremental protocol · Industry-first · Tuned for agents",
        openConsole: "Open console →",
        oneClick: "One-click setup →",
        opening: "Opening…",
        advanced: "Advanced",
        advancedDisconnected: "Or paste a key manually",
        apiKey: "API Key",
        replace: "Replace",
        save: "Save",
        disconnect: "Disconnect",
        disconnecting: "Disconnecting…",
        pastePlaceholder: "sk-rsclaw-...",
        invalidKey: "Invalid key (should start with sk-rsclaw-)",
        configSaved: "Saved",
      };

  // Refresh source of truth from disk. Called on mount, on window
  // focus, after a successful install event, and after the user's
  // own actions (manual paste / disconnect) to mirror the new state.
  const refresh = useCallback(async () => {
    const s = await readAccountState();
    setState(s);
  }, []);

  useEffect(() => {
    void refresh();
    const onFocus = () => void refresh();
    window.addEventListener("focus", onFocus);

    let cancelled = false;
    let unsub: (() => void) | undefined;
    // sidebar-account-chip is always mounted and owns the persist of
    // `rsclaw:console-install-key` payloads via applyInstalledKey().
    // This card only needs to re-read the resulting state — racing a
    // second applyInstalledKey here would cause concurrent read→merge
    // →write on rsclaw.json5 (last-write-wins, but wasteful).
    onKeyInstalled(async () => {
      if (cancelled) return;
      await refresh();
    })
      .then((fn) => {
        if (cancelled) fn();
        else unsub = fn;
      })
      .catch(() => {
        /* tauri events unavailable — focus refresh still works */
      });

    return () => {
      cancelled = true;
      unsub?.();
      window.removeEventListener("focus", onFocus);
    };
  }, [refresh]);

  // ── Handlers ────────────────────────────────────────────────────

  const handleConnect = useCallback(async () => {
    if (busy) return;
    setBusy(true);
    setError(null);
    try {
      await openRsclawConsole();
    } finally {
      setBusy(false);
    }
  }, [busy]);

  const handleManualSave = useCallback(async () => {
    if (busy) return;
    const key = manualKey.trim();
    if (!isLikelyRsclawKey(key)) {
      setError(t.invalidKey);
      return;
    }
    setError(null);
    setBusy(true);
    try {
      const res = await applyInstalledKey({ key });
      if (!res.ok) {
        setError(res.error || "save failed");
        return;
      }
      setManualKey("");
      await refresh();
    } finally {
      setBusy(false);
    }
  }, [busy, manualKey, refresh, t.invalidKey]);

  const handleDisconnect = useCallback(async () => {
    if (busy) return;
    setBusy(true);
    setError(null);
    try {
      const res = await disconnectAccount();
      if (!res.ok) {
        setError(res.error || "disconnect failed");
        return;
      }
      setAdvancedOpen(false);
      await refresh();
    } finally {
      setBusy(false);
    }
  }, [busy, refresh]);

  // ── Render ──────────────────────────────────────────────────────

  const accent = state.connected
    ? "rgba(45,212,160,0.35)"
    : "rgba(249,115,22,0.35)";
  const accentBg = state.connected
    ? "rgba(45,212,160,0.06)"
    : "rgba(249,115,22,0.06)";

  return (
    <div
      style={{
        ...cardStyle,
        background: accentBg,
        borderColor: accent,
      }}
    >
      {/* ── Header: brand + status badge ── */}
      <div style={headerStyle}>
        <span style={brandStyle}>
          <span style={{ fontSize: 18 }}>🦀</span>
          <span style={brandTextStyle}>rsclaw</span>
        </span>
        <span
          style={{
            ...badgeStyle,
            color: state.connected ? "#2dd4a0" : "#9896a4",
            borderColor: state.connected
              ? "rgba(45,212,160,0.45)"
              : "rgba(152,150,164,0.35)",
            background: state.connected
              ? "rgba(45,212,160,0.08)"
              : "rgba(152,150,164,0.06)",
          }}
        >
          {state.connected ? `✓ ${t.connected}` : t.disconnected}
        </span>
      </div>

      {/* ── Identity line: key name (when connected) + tagline ── */}
      {state.connected && state.name && (
        <div style={nameStyle}>{state.name}</div>
      )}
      <div style={taglineStyle}>{t.tagline}</div>

      {/* ── Primary action ── */}
      <div style={actionsRowStyle}>
        <button
          type="button"
          onClick={() => void handleConnect()}
          disabled={busy}
          style={state.connected ? primaryBtnStyle : ctaBtnStyle}
        >
          {busy
            ? t.opening
            : state.connected
              ? t.openConsole
              : t.oneClick}
        </button>
      </div>

      {/* ── Advanced disclosure ── */}
      <button
        type="button"
        onClick={() => setAdvancedOpen((v) => !v)}
        style={discloseBtnStyle}
      >
        {advancedOpen ? "▾" : "▸"}{" "}
        {state.connected ? t.advanced : t.advancedDisconnected}
      </button>

      {advancedOpen && (
        <div style={advancedBoxStyle}>
          {state.connected ? (
            <>
              <ManualKeyRow
                label={t.apiKey}
                placeholder={t.pastePlaceholder}
                value={manualKey}
                onChange={setManualKey}
                onSave={() => void handleManualSave()}
                saveLabel={t.replace}
                busy={busy}
              />
              <button
                type="button"
                onClick={() => void handleDisconnect()}
                disabled={busy}
                style={dangerBtnStyle}
              >
                {busy ? t.disconnecting : t.disconnect}
              </button>
            </>
          ) : (
            <ManualKeyRow
              label={t.apiKey}
              placeholder={t.pastePlaceholder}
              value={manualKey}
              onChange={setManualKey}
              onSave={() => void handleManualSave()}
              saveLabel={t.save}
              busy={busy}
            />
          )}
        </div>
      )}

      {error && <div style={errorStyle}>{error}</div>}
    </div>
  );
}

function ManualKeyRow(props: {
  label: string;
  placeholder: string;
  value: string;
  onChange: (v: string) => void;
  onSave: () => void;
  saveLabel: string;
  busy: boolean;
}) {
  const [reveal, setReveal] = useState(false);
  return (
    <div style={manualRowStyle}>
      <div style={manualLabelStyle}>{props.label}</div>
      <div style={{ display: "flex", gap: 6 }}>
        <input
          type={reveal ? "text" : "password"}
          placeholder={props.placeholder}
          value={props.value}
          onChange={(e) => props.onChange(e.target.value)}
          onKeyDown={(e) => {
            if (e.key === "Enter") props.onSave();
          }}
          style={manualInputStyle}
          spellCheck={false}
          autoComplete="off"
        />
        <button
          type="button"
          onClick={() => setReveal((v) => !v)}
          disabled={!props.value}
          style={revealBtnStyle}
          title={reveal ? "hide" : "show"}
        >
          {reveal ? "🙈" : "👁"}
        </button>
        <button
          type="button"
          onClick={props.onSave}
          disabled={props.busy || !props.value.trim()}
          style={primaryBtnStyle}
        >
          {props.saveLabel}
        </button>
      </div>
    </div>
  );
}

// ── Styles ────────────────────────────────────────────────────────

const cardStyle: React.CSSProperties = {
  padding: 16,
  marginBottom: 18,
  borderRadius: 10,
  border: "1px solid",
  transition: "background 0.15s, border-color 0.15s",
};

const headerStyle: React.CSSProperties = {
  display: "flex",
  alignItems: "center",
  justifyContent: "space-between",
  marginBottom: 10,
};

const brandStyle: React.CSSProperties = {
  display: "inline-flex",
  alignItems: "center",
  gap: 8,
};

const brandTextStyle: React.CSSProperties = {
  fontSize: 16,
  fontWeight: 700,
  color: "#eceaf4",
  fontFamily: "'JetBrains Mono', monospace",
};

const badgeStyle: React.CSSProperties = {
  padding: "3px 9px",
  fontSize: 11,
  fontWeight: 600,
  borderRadius: 999,
  border: "1px solid",
  fontFamily: "'JetBrains Mono', monospace",
};

const nameStyle: React.CSSProperties = {
  fontSize: 13,
  color: "#cfcdd8",
  fontFamily: "'JetBrains Mono', monospace",
  marginBottom: 4,
};

const taglineStyle: React.CSSProperties = {
  fontSize: 11.5,
  color: "#9896a4",
  marginBottom: 14,
  lineHeight: 1.5,
};

const actionsRowStyle: React.CSSProperties = {
  display: "flex",
  alignItems: "center",
  gap: 10,
};

const ctaBtnStyle: React.CSSProperties = {
  padding: "8px 16px",
  fontSize: 12,
  fontWeight: 600,
  color: "#fff",
  background: "#f97316",
  border: "1px solid #f97316",
  borderRadius: 7,
  cursor: "pointer",
  fontFamily: "inherit",
};

const primaryBtnStyle: React.CSSProperties = {
  padding: "7px 14px",
  fontSize: 12,
  fontWeight: 500,
  color: "#eceaf4",
  background: "rgba(255,255,255,0.05)",
  border: "1px solid rgba(255,255,255,0.12)",
  borderRadius: 7,
  cursor: "pointer",
  fontFamily: "inherit",
};

const revealBtnStyle: React.CSSProperties = {
  padding: "7px 10px",
  fontSize: 13,
  color: "#eceaf4",
  background: "rgba(255,255,255,0.04)",
  border: "1px solid rgba(255,255,255,0.10)",
  borderRadius: 7,
  cursor: "pointer",
  fontFamily: "inherit",
  lineHeight: 1,
};

const dangerBtnStyle: React.CSSProperties = {
  marginTop: 12,
  padding: "7px 14px",
  fontSize: 12,
  fontWeight: 500,
  color: "#fca5a5",
  background: "transparent",
  border: "1px solid rgba(217,95,95,0.4)",
  borderRadius: 7,
  cursor: "pointer",
  fontFamily: "inherit",
};

const discloseBtnStyle: React.CSSProperties = {
  marginTop: 14,
  padding: 0,
  background: "transparent",
  border: "none",
  color: "#6b6877",
  fontSize: 11,
  fontFamily: "inherit",
  cursor: "pointer",
  textAlign: "left",
};

const advancedBoxStyle: React.CSSProperties = {
  marginTop: 10,
  padding: 12,
  background: "rgba(0,0,0,0.18)",
  border: "1px solid rgba(255,255,255,0.05)",
  borderRadius: 7,
};

const manualRowStyle: React.CSSProperties = {
  marginBottom: 4,
};

const manualLabelStyle: React.CSSProperties = {
  fontSize: 10,
  color: "#6b6877",
  letterSpacing: 0.4,
  marginBottom: 6,
  fontFamily: "'JetBrains Mono', monospace",
};

const manualInputStyle: React.CSSProperties = {
  flex: 1,
  padding: "7px 10px",
  background: "#1f2126",
  border: "1px solid rgba(255,255,255,0.09)",
  borderRadius: 6,
  color: "#eceaf4",
  fontFamily: "'JetBrains Mono', monospace",
  fontSize: 11.5,
  outline: "none",
};

const errorStyle: React.CSSProperties = {
  marginTop: 10,
  fontSize: 11,
  color: "#fca5a5",
  fontFamily: "inherit",
};
