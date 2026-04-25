"use client";

import { useState } from "react";
import Locale from "../locales";
import { toast } from "../lib/toast";
import { isTauri, invoke as tauriInvokeV2 } from "../utils/tauri";
import { useRestartBanner } from "../hooks/useRestartBanner";
import styles from "./rsclaw-panel.module.scss";

/**
 * Top-of-panel banner that surfaces gateway-issued `restart.required` events.
 * Three actions:
 *   - Restart Now: stops + starts the gateway via Tauri sidecar commands.
 *   - Later: snoozes the banner for 30 minutes (persisted to localStorage).
 *   - Dismiss: hides for the current session only (no persistence).
 *
 * In web (non-Tauri) mode the Restart Now button is hidden — the desktop
 * shell owns the gateway sidecar process; a browser tab can't restart it.
 */
export function RestartBanner() {
  const banner = useRestartBanner();
  const [restarting, setRestarting] = useState(false);

  const onRestartNow = async () => {
    if (!isTauri) return;
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

  if (!banner.visible) return null;

  const message =
    banner.payload?.message || Locale.RsClawPanel.RestartBanner.DefaultMessage;

  return (
    <div
      className={styles["restart-banner"]}
      role="alert"
      aria-live="polite"
    >
      <div className={styles["restart-banner-icon"]} aria-hidden="true">
        !
      </div>
      <div className={styles["restart-banner-msg"]}>{message}</div>
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
          onClick={banner.snooze}
          disabled={restarting}
        >
          {Locale.RsClawPanel.RestartBanner.Later}
        </button>
        <button
          className={styles["btn"]}
          onClick={banner.dismiss}
          disabled={restarting}
        >
          {Locale.RsClawPanel.RestartBanner.Dismiss}
        </button>
      </div>
    </div>
  );
}
