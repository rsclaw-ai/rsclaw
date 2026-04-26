import { useCallback, useEffect, useState } from "react";
import { rsclawWs, type RestartRequiredPayload } from "../lib/rsclaw-ws";

const SNOOZE_KEY = "rsclaw.restartBanner.snoozedUntil";
const SNOOZE_MS = 30 * 60 * 1000; // 30 minutes

function readSnoozedUntil(): number {
  try {
    const raw = localStorage.getItem(SNOOZE_KEY);
    if (!raw) return 0;
    const n = Number(raw);
    return Number.isFinite(n) ? n : 0;
  } catch {
    return 0;
  }
}

export type RestartBannerState = {
  visible: boolean;
  payload: RestartRequiredPayload | null;
};

export type RestartBannerControls = RestartBannerState & {
  snooze: () => void;
  dismiss: () => void;
};

/**
 * Subscribes to the gateway's `restart.required` WS frame and exposes
 * banner visibility plus three actions (snooze, dismiss, raw payload for
 * Restart Now). Snooze persists across sessions for 30 minutes; dismiss is
 * session-only (re-shows on next event in the same window).
 */
export function useRestartBanner(): RestartBannerControls {
  const [state, setState] = useState<RestartBannerState>({
    visible: false,
    payload: null,
  });

  useEffect(() => {
    rsclawWs.connect();

    const unsubEvent = rsclawWs.onRestartRequired((payload) => {
      const now = Date.now();
      const snoozedUntil = readSnoozedUntil();
      if (snoozedUntil > now) {
        // Still inside the snooze window — ignore until it expires.
        return;
      }
      setState({ visible: true, payload });
    });

    // Each fresh handshake clears the banner. If the new gateway has a
    // latched restart.required it arrives in the very next frame and
    // re-arms the banner above. Without this, banner state leaks across
    // sidecar swaps (gateway dies → React state preserved → reconnect to
    // empty-latch gateway → banner stays visible forever, eventually
    // triggering the auto-restart timer in a loop).
    const unsubConnect = rsclawWs.onConnect(() => {
      setState({ visible: false, payload: null });
    });

    return () => {
      unsubEvent();
      unsubConnect();
    };
  }, []);

  const snooze = useCallback(() => {
    try {
      localStorage.setItem(SNOOZE_KEY, String(Date.now() + SNOOZE_MS));
    } catch {
      // localStorage unavailable (private mode) — fall through, dismiss
      // until next event.
    }
    setState((s) => ({ ...s, visible: false }));
  }, []);

  const dismiss = useCallback(() => {
    setState((s) => ({ ...s, visible: false }));
  }, []);

  return { ...state, snooze, dismiss };
}
