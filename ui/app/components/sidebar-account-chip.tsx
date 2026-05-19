/**
 * Persistent rsclaw.ai account chip rendered in the main sidebar
 * header, between the gateway status pill and the icon-grid nav. The
 * chip is the primary in-app entry to the cloud console after the
 * user has finished onboarding.
 *
 * Two visual states:
 *
 *   Logged out (no `models.providers.rsclaw.apiKey` in config):
 *     ┌────────────────────────────┐
 *     │ ☁  连接 rsclaw.ai      →  │
 *     └────────────────────────────┘
 *     Single click — opens the console webview. The same one-tap
 *     install flow used during onboarding.
 *
 *   Logged in:
 *     ┌────────────────────────────┐
 *     │ ● rsclaw-macos · free  ⌃  │
 *     └────────────────────────────┘
 *     Click expands a dropdown with "open console" + "disconnect".
 *
 * Refresh strategy is deliberately passive (per agreed UX): re-read
 * on mount, on window focus, and on the `rsclaw:console-install-key`
 * event. No interval polling — the user explicitly didn't want it,
 * and we don't have a live account API yet anyway (name / tier are
 * snapshots from the last install).
 */

import { useCallback, useEffect, useRef, useState } from "react";

import {
  applyInstalledKey,
  disconnectAccount,
  onKeyInstalled,
  openRsclawConsole,
  readAccountState,
  type RsclawAccountState,
} from "../lib/rsclaw-console";
import { getLang } from "../locales";

type Props = {
  /**
   * Whether the parent sidebar is in its narrowed state. We render
   * an icon-only chip in that case so the sidebar layout stays
   * compact.
   */
  narrow?: boolean;
};

/**
 * Read+watch hook. Returns the current account state plus an explicit
 * `refresh()` the chip uses when it does an action (connect /
 * disconnect) and wants the UI to reflect the result immediately
 * without waiting for the focus event.
 */
function useAccountState() {
  const [state, setState] = useState<RsclawAccountState>({ connected: false });

  const refresh = useCallback(async () => {
    const s = await readAccountState();
    setState(s);
  }, []);

  useEffect(() => {
    void refresh();

    // Re-read on window focus — the user might have edited
    // rsclaw.json5 in another tool, or the gateway might have
    // mutated provider state out-of-band.
    const onFocus = () => {
      void refresh();
    };
    window.addEventListener("focus", onFocus);

    // Persist + re-read whenever the console webview reports an
    // install. The persist (applyInstalledKey) MUST happen here for
    // the main-UI flow: when the user opens the console from this
    // chip rather than from onboarding, the onboarding card isn't
    // mounted and no other listener writes the key to rsclaw.json5
    // — `refresh()` alone would just keep showing the stale state.
    // (Unsubscribe is async — onKeyInstalled returns a Promise.)
    let unsub: (() => void) | undefined;
    let cancelled = false;
    onKeyInstalled(async (data) => {
      if (cancelled) return;
      const res = await applyInstalledKey(data);
      if (cancelled) return;
      if (!res.ok) {
        // Kept as a warn so the diagnostic survives if the path
        // breaks again later — silent failure here would leave the
        // chip stuck on the old state with no indication why.
        console.warn("[account-chip] applyInstalledKey failed:", res.error);
      }
      await refresh();
    })
      .then((fn) => {
        if (cancelled) fn();
        else unsub = fn;
      })
      .catch(() => {
        /* tauri event subscription unavailable — focus refresh still
           works */
      });

    return () => {
      cancelled = true;
      unsub?.();
      window.removeEventListener("focus", onFocus);
    };
  }, [refresh]);

  return { state, refresh };
}

export function SidebarAccountChip({ narrow }: Props) {
  const { state, refresh } = useAccountState();
  const [menuOpen, setMenuOpen] = useState(false);
  const [busy, setBusy] = useState(false);
  const containerRef = useRef<HTMLDivElement | null>(null);

  const zh = getLang() === "cn";
  const t = zh
    ? {
        connect: "连接 rsclaw.ai",
        connected: "已连接",
        openConsole: "打开 console",
        disconnect: "断开连接",
        disconnecting: "断开中…",
        opening: "打开中…",
      }
    : {
        connect: "Connect rsclaw.ai",
        connected: "Connected",
        openConsole: "Open console",
        disconnect: "Disconnect",
        disconnecting: "Disconnecting…",
        opening: "Opening…",
      };

  // Close the dropdown on outside click. Native popup menus are nice
  // but this is small enough that a manual close handler is simpler.
  useEffect(() => {
    if (!menuOpen) return;
    const onDocClick = (e: MouseEvent) => {
      if (!containerRef.current) return;
      if (!containerRef.current.contains(e.target as Node)) {
        setMenuOpen(false);
      }
    };
    document.addEventListener("mousedown", onDocClick);
    return () => document.removeEventListener("mousedown", onDocClick);
  }, [menuOpen]);

  const handleConnect = useCallback(async () => {
    if (busy) return;
    setBusy(true);
    try {
      await openRsclawConsole();
    } finally {
      setBusy(false);
    }
  }, [busy]);

  const handleDisconnect = useCallback(async () => {
    if (busy) return;
    setBusy(true);
    try {
      await disconnectAccount();
      await refresh();
      setMenuOpen(false);
    } finally {
      setBusy(false);
    }
  }, [busy, refresh]);

  // Compact inline button. Hosted in `SideBarTail`'s middle slot, so
  // the slot's flex layout owns horizontal positioning — the chip
  // itself doesn't need any outer padding. Visually a "status pill"
  // between the Settings icon (left) and New Chat button (right).
  const baseRowStyle: React.CSSProperties = {
    width: "100%",
    display: "flex",
    alignItems: "center",
    gap: 7,
    padding: "5px 9px",
    margin: 0,
    border: "1px solid rgba(255,255,255,0.06)",
    borderRadius: 6,
    background: "transparent",
    color: "#7a7886",
    fontSize: 11,
    fontFamily: "inherit",
    cursor: busy ? "wait" : "pointer",
    transition: "background 0.12s, border-color 0.12s, color 0.12s",
    textAlign: "left",
  };

  const dotStyle = (color: string): React.CSSProperties => ({
    width: 7,
    height: 7,
    borderRadius: "50%",
    background: color,
    flexShrink: 0,
  });

  // Narrow sidebar: just the dot, no text — Settings + New Chat
  // become icon-only in this mode too, so the chip should match
  // their compactness.
  if (narrow) {
    return (
      <div ref={containerRef} style={{ position: "relative" }}>
        <button
          type="button"
          onClick={() => {
            if (state.connected) setMenuOpen((v) => !v);
            else void handleConnect();
          }}
          style={{
            ...baseRowStyle,
            justifyContent: "center",
            padding: 6,
          }}
          onMouseEnter={(e) => {
            e.currentTarget.style.background = "rgba(255,255,255,0.05)";
            e.currentTarget.style.borderColor = "rgba(255,255,255,0.12)";
          }}
          onMouseLeave={(e) => {
            e.currentTarget.style.background = "transparent";
            e.currentTarget.style.borderColor = "rgba(255,255,255,0.06)";
          }}
          title={
            state.connected
              ? `${state.name || "rsclaw"}${state.tier ? " · " + state.tier : ""}`
              : t.connect
          }
        >
          <span
            style={dotStyle(state.connected ? "#2dd4a0" : "rgba(168,166,178,0.55)")}
          />
        </button>
        {menuOpen && state.connected && renderMenu()}
      </div>
    );
  }

  if (!state.connected) {
    return (
      <div style={{ width: "100%", position: "relative" }}>
        <button
          type="button"
          onClick={() => void handleConnect()}
          disabled={busy}
          style={baseRowStyle}
          onMouseEnter={(e) => {
            e.currentTarget.style.background = "rgba(255,255,255,0.05)";
            e.currentTarget.style.borderColor = "rgba(255,255,255,0.12)";
            e.currentTarget.style.color = "#a8a6b2";
          }}
          onMouseLeave={(e) => {
            e.currentTarget.style.background = "transparent";
            e.currentTarget.style.borderColor = "rgba(255,255,255,0.06)";
            e.currentTarget.style.color = "#7a7886";
          }}
          title={t.connect}
        >
          <span style={dotStyle("rgba(168,166,178,0.55)")} />
          <span
            style={{
              flex: 1,
              overflow: "hidden",
              textOverflow: "ellipsis",
              whiteSpace: "nowrap",
            }}
          >
            {busy ? t.opening : t.connect}
          </span>
          <span style={{ opacity: 0.55, fontSize: 10 }}>→</span>
        </button>
      </div>
    );
  }

  const label = state.name || (zh ? "已连接" : t.connected);
  const sub = state.tier ? ` · ${state.tier}` : "";

  return (
    <div ref={containerRef} style={{ width: "100%", position: "relative" }}>
      <button
        type="button"
        onClick={() => setMenuOpen((v) => !v)}
        disabled={busy}
        style={baseRowStyle}
        onMouseEnter={(e) => {
          e.currentTarget.style.background = "rgba(255,255,255,0.05)";
          e.currentTarget.style.borderColor = "rgba(255,255,255,0.12)";
          e.currentTarget.style.color = "#cfcdd8";
        }}
        onMouseLeave={(e) => {
          e.currentTarget.style.background = "transparent";
          e.currentTarget.style.borderColor = "rgba(255,255,255,0.06)";
          e.currentTarget.style.color = "#7a7886";
        }}
        title={`${label}${sub}`}
      >
        <span style={dotStyle("#2dd4a0")} />
        <span
          style={{
            flex: 1,
            overflow: "hidden",
            textOverflow: "ellipsis",
            whiteSpace: "nowrap",
          }}
        >
          {label}
          {sub && <span style={{ opacity: 0.7 }}>{sub}</span>}
        </span>
        <span
          style={{
            fontSize: 9,
            opacity: 0.6,
            transform: menuOpen ? "rotate(180deg)" : "rotate(0)",
            transition: "transform 0.12s",
          }}
        >
          ▾
        </span>
      </button>
      {menuOpen && renderMenu()}
    </div>
  );

  function renderMenu() {
    return (
      <div
        style={{
          // Anchored to the chip and pops UPWARD because the chip
          // sits at the sidebar footer — downward would clip into
          // the action buttons (Settings / New Chat) below.
          position: "absolute",
          bottom: "calc(100% + 4px)",
          left: 0,
          right: 0,
          minWidth: 180,
          background: "#1a1c22",
          border: "1px solid rgba(255,255,255,0.09)",
          borderRadius: 7,
          boxShadow: "0 -8px 24px rgba(0,0,0,0.45)",
          zIndex: 50,
          overflow: "hidden",
        }}
      >
        <button
          type="button"
          onClick={() => {
            setMenuOpen(false);
            void handleConnect();
          }}
          style={menuItemStyle}
          onMouseEnter={(e) => (e.currentTarget.style.background = "#22252c")}
          onMouseLeave={(e) => (e.currentTarget.style.background = "transparent")}
        >
          <span>{t.openConsole}</span>
          <span style={{ opacity: 0.55 }}>→</span>
        </button>
        <div style={{ borderTop: "1px solid rgba(255,255,255,0.06)" }} />
        <button
          type="button"
          onClick={() => void handleDisconnect()}
          disabled={busy}
          style={{
            ...menuItemStyle,
            color: "#d95f5f",
          }}
          onMouseEnter={(e) =>
            (e.currentTarget.style.background = "rgba(217,95,95,0.08)")
          }
          onMouseLeave={(e) => (e.currentTarget.style.background = "transparent")}
        >
          <span>{busy ? t.disconnecting : t.disconnect}</span>
          <span style={{ opacity: 0.55 }}>✕</span>
        </button>
      </div>
    );
  }
}

// Shared styling for the dropdown items — declared outside the
// component to dodge re-allocation per render.
const menuItemStyle: React.CSSProperties = {
  display: "flex",
  alignItems: "center",
  justifyContent: "space-between",
  width: "100%",
  padding: "8px 12px",
  background: "transparent",
  border: "none",
  color: "#eceaf4",
  fontSize: 11.5,
  fontFamily: "'JetBrains Mono', monospace",
  cursor: "pointer",
  transition: "background 0.1s",
};
