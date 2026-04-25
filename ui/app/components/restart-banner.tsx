"use client";

import { useEffect, useRef, useState } from "react";
import Locale from "../locales";
import { toast } from "../lib/toast";
import { isTauri, invoke as tauriInvokeV2 } from "../utils/tauri";
import { useRestartBanner } from "../hooks/useRestartBanner";
import styles from "./rsclaw-panel.module.scss";

/**
 * Auto-restart countdown duration once a `restart.required` event is received,
 * in seconds. After the countdown elapses the banner triggers `onRestartNow`
 * automatically; the user can pre-empt it via Restart Now / Later / Dismiss.
 */
const AUTO_RESTART_SECONDS = 60;

/**
 * Surfaces gateway-issued `restart.required` events directly under the gateway
 * status banner on the Status page.
 *
 * Behaviour:
 *   - In Tauri desktop mode, starts a 60-second auto-restart countdown when a
 *     new event arrives. On expiry it calls `Restart Now` automatically. The
 *     countdown is cancelled if the user clicks Later, Dismiss, or Restart Now.
 *   - In web (non-Tauri) mode, no countdown — the desktop shell owns the
 *     gateway sidecar; a browser tab cannot restart it. The banner just shows
 *     the message text with Later / Dismiss controls.
 */
export function RestartBanner() {
  const banner = useRestartBanner();
  const [restarting, setRestarting] = useState(false);
  const [secondsLeft, setSecondsLeft] = useState<number>(AUTO_RESTART_SECONDS);
  const intervalRef = useRef<ReturnType<typeof setInterval> | null>(null);
  const onRestartNowRef = useRef<() => Promise<void>>(async () => {});

  const clearCountdown = () => {
    if (intervalRef.current !== null) {
      clearInterval(intervalRef.current);
      intervalRef.current = null;
    }
  };

  const onRestartNow = async () => {
    if (!isTauri) return;
    clearCountdown();
    setRestarting(true);
    try {
      await tauriInvokeV2("set_gateway_user_stopped", { stopped: false }).catch(
        () => {},
      );
      await tauriInvokeV2("stop_gateway");
      await new Promise((r) => setTimeout(r, 1500));
      await tauriInvokeV2("start_gateway");
      banner.dismiss();
    } catch (e) {
      toast.fromError(Locale.RsClawPanel.RestartBanner.Failed, e);
    } finally {
      setRestarting(false);
    }
  };

  // Keep a stable ref to the latest onRestartNow so the countdown effect can
  // call it without re-subscribing on every render.
  onRestartNowRef.current = onRestartNow;

  // Restart the countdown whenever a fresh event arrives (visible flips false→true
  // or payload identity changes).
  useEffect(() => {
    clearCountdown();
    if (!banner.visible || !isTauri) return;
    setSecondsLeft(AUTO_RESTART_SECONDS);
    intervalRef.current = setInterval(() => {
      setSecondsLeft((s) => {
        if (s <= 1) {
          clearCountdown();
          void onRestartNowRef.current();
          return 0;
        }
        return s - 1;
      });
    }, 1000);
    return clearCountdown;
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [banner.visible, banner.payload]);

  if (!banner.visible) return null;

  // Compose the message: prefer the backend-supplied reason text, then append
  // the countdown hint when auto-restart is active.
  const baseMessage =
    banner.payload?.message || Locale.RsClawPanel.RestartBanner.DefaultMessage;
  const countdownText =
    isTauri && !restarting
      ? Locale.RsClawPanel.RestartBanner.AutoRestartCountdown(secondsLeft)
      : "";

  return (
    <div
      className={styles["restart-banner"]}
      role="alert"
      aria-live="polite"
    >
      <div className={styles["restart-banner-icon"]} aria-hidden="true">
        !
      </div>
      <div className={styles["restart-banner-msg"]}>
        <div>{baseMessage}</div>
        {countdownText && (
          <div className={styles["restart-banner-countdown"]}>
            {countdownText}
          </div>
        )}
      </div>
      <div className={styles["restart-banner-actions"]}>
        {isTauri && (
          <button
            className={`${styles["btn"]} ${styles["restart-banner-primary"]}`}
            onClick={onRestartNow}
            disabled={restarting}
          >
            {restarting
              ? Locale.RsClawPanel.RestartBanner.Restarting
              : Locale.RsClawPanel.RestartBanner.RestartNow}
          </button>
        )}
        <button
          className={styles["btn"]}
          onClick={() => {
            clearCountdown();
            banner.snooze();
          }}
          disabled={restarting}
        >
          {Locale.RsClawPanel.RestartBanner.Later}
        </button>
        <button
          className={styles["btn"]}
          onClick={() => {
            clearCountdown();
            banner.dismiss();
          }}
          disabled={restarting}
        >
          {Locale.RsClawPanel.RestartBanner.Dismiss}
        </button>
      </div>
    </div>
  );
}
