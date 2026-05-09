/**
 * Full-screen overlay shown while a `computer_use` run is active.
 *
 * Mounted once at the app root (alongside `ComputerUsePermissionDialog`)
 * and listens to `rsclawWs.onComputerUseStatus`. The overlay surfaces
 * three things to the user the moment the GUI agent starts driving the
 * desktop:
 *
 *   1. A pulsing orange inset glow on the viewport edges so the user is
 *      visually aware their machine is being controlled.
 *   2. A centred status pill with the current action summary and step
 *      progress, so the run can be followed at a glance without
 *      switching to the Computer Use control page.
 *   3. (Tauri only) The main window shrinks to a small panel pinned to
 *      the right edge of the work area so the user can keep watching
 *      the chat while the agent operates other apps. Original size and
 *      position are restored when the run finishes.
 *
 * The overlay's outer layer is `pointer-events: none` so it never
 * blocks the user's normal interaction with the desktop. Only the
 * status pill itself is interactive.
 */

import { useCallback, useEffect, useRef, useState } from "react";

import RsClawIcon from "../icons/rsclaw-icon.svg";
import { rsclawWs, type ComputerUseStatusPayload } from "../lib/rsclaw-ws";
import { gatewayFetch } from "../lib/rsclaw-api";
import { invoke as tauriInvoke, isTauri } from "../utils/tauri";
import { getLang } from "../locales";

import styles from "./computer-use-overlay.module.scss";

/** Outcome discriminator value pulled from the `finished` variant. */
type OutcomeKind = Extract<
  ComputerUseStatusPayload,
  { kind: "finished" }
>["outcome_kind"];

/** Visual state derived from the WS event stream. */
type OverlayState = {
  run_id: string;
  agent_id: string;
  app: string;
  instruction: string;
  max_steps: number;
  step_index: number;
  action: string;
  result_ok: boolean | null;
  finished?: {
    outcome_kind: OutcomeKind;
    summary: string;
    steps: number;
  };
};

/** Linger time after a run finishes before the overlay fades out. */
const HIDE_AFTER_FINISH_MS = 1800;

/**
 * Three-tier classification for visual state. The 1.8s overlay is too
 * brief for a four-tier (ok / attention / neutral / bad) spread to be
 * legible, so user-initiated stops (`user_abort`, `permission_denied`)
 * collapse into the same `attention` bucket as `call_user`.
 */
type Tier = "running" | "ok" | "attention" | "bad";

function tierForOutcome(kind: OutcomeKind): Tier {
  if (kind === "finished") return "ok";
  if (kind === "max_loop" || kind === "operator_error") return "bad";
  // call_user / user_abort / permission_denied — non-success but not
  // an error the user needs to alarm-react to.
  return "attention";
}

/** Shrunken window geometry while a run is active. */
const SHRUNK_WIDTH = 360;
const SHRUNK_HEIGHT = 520;
/** Inset from screen edges when pinning to the right. */
const SHRUNK_MARGIN = 20;

function outcomeLabel(kind: string, zh: boolean): string {
  if (zh) {
    return (
      {
        finished: "已完成",
        call_user: "需要你确认",
        max_loop: "已达步数上限",
        user_abort: "已中止",
        permission_denied: "已拒绝授权",
        operator_error: "执行出错",
      } as Record<string, string>
    )[kind] || kind;
  }
  return (
    {
      finished: "Done",
      call_user: "Awaiting input",
      max_loop: "Hit max steps",
      user_abort: "Aborted",
      permission_denied: "Permission denied",
      operator_error: "Operator error",
    } as Record<string, string>
  )[kind] || kind;
}

/** Snapshot of window geometry to restore once the run ends. */
type WindowSnapshot = {
  width: number;
  height: number;
  x: number;
  y: number;
};

/**
 * Shrink the main window to a small panel on the right of the screen
 * and return the original geometry so we can restore it later. Best
 * effort: any failure (browser env, permission, missing API) returns
 * null and the overlay still works without window resize.
 */
async function shrinkWindow(): Promise<WindowSnapshot | null> {
  if (!isTauri) return null;
  try {
    const winApi = await import("@tauri-apps/api/window");
    const win = winApi.getCurrentWindow();

    // Save current outer geometry in physical pixels.
    const size = await win.outerSize();
    const pos = await win.outerPosition();
    const snapshot: WindowSnapshot = {
      width: size.width,
      height: size.height,
      x: pos.x,
      y: pos.y,
    };

    // Compute target position from the monitor's work area, in
    // logical pixels.
    const monitor = await winApi.currentMonitor();
    if (!monitor) {
      // No monitor info — just resize without moving.
      const { LogicalSize } = winApi;
      await win.setSize(new LogicalSize(SHRUNK_WIDTH, SHRUNK_HEIGHT));
      return snapshot;
    }

    const scale = monitor.scaleFactor || 1;
    const monitorWidth = monitor.size.width / scale;
    const monitorX = monitor.position.x / scale;
    const monitorY = monitor.position.y / scale;

    const targetX = monitorX + monitorWidth - SHRUNK_WIDTH - SHRUNK_MARGIN;
    const targetY = monitorY + SHRUNK_MARGIN;

    const { LogicalSize, LogicalPosition } = winApi;
    await win.setSize(new LogicalSize(SHRUNK_WIDTH, SHRUNK_HEIGHT));
    await win.setPosition(new LogicalPosition(targetX, targetY));

    return snapshot;
  } catch {
    return null;
  }
}

/**
 * Restore the main window to a previously-captured geometry.
 * Physical pixel values are taken from `outerSize`/`outerPosition`,
 * so we restore via `PhysicalSize`/`PhysicalPosition`.
 */
async function restoreWindow(snap: WindowSnapshot): Promise<void> {
  if (!isTauri) return;
  try {
    const winApi = await import("@tauri-apps/api/window");
    const win = winApi.getCurrentWindow();
    const { PhysicalSize, PhysicalPosition } = winApi;
    await win.setSize(new PhysicalSize(snap.width, snap.height));
    await win.setPosition(new PhysicalPosition(snap.x, snap.y));
  } catch {
    /* swallow */
  }
}

/**
 * Open the native always-on-top glow overlay covering the primary
 * monitor. Returns true on success so callers know whether to suppress
 * the component-level CSS glow (the two stacked would double up around
 * the shrunken main window). Failures fall through to the CSS fallback.
 */
async function openNativeGlow(): Promise<boolean> {
  if (!isTauri) return false;
  try {
    await tauriInvoke("open_glow_overlay");
    return true;
  } catch {
    return false;
  }
}

/** Best-effort close of the native glow overlay; failures are ignored. */
async function closeNativeGlow(): Promise<void> {
  if (!isTauri) return;
  try {
    await tauriInvoke("close_glow_overlay");
  } catch {
    /* swallow */
  }
}

export function ComputerUseOverlay() {
  const [state, setState] = useState<OverlayState | null>(null);
  const [visible, setVisible] = useState(false);
  // True between the user clicking Stop and the gateway emitting the
  // matching `finished` event. Disables the button + swaps the action
  // text so it's clear the request is in flight.
  const [aborting, setAborting] = useState(false);

  // Window geometry to restore. Stored in a ref so it survives the
  // re-renders driven by step events.
  const snapshotRef = useRef<WindowSnapshot | null>(null);
  // Hide-fade-out timer between `finished` arrival and unmount.
  const hideTimerRef = useRef<ReturnType<typeof setTimeout> | null>(null);
  // Whether `open_glow_overlay` was invoked successfully on the
  // current run. Tracked in a ref because the WS subscriber closure is
  // captured once and would otherwise see a stale state value when
  // deciding whether to call `close_glow_overlay`.
  const nativeGlowOpenRef = useRef(false);
  // Render-time mirror of the ref. Kept on for the entire run lifetime
  // INCLUDING the post-finish linger so the in-window CSS glow doesn't
  // pop in around the small window during the 1.8s outcome flash.
  const [nativeGlow, setNativeGlow] = useState(false);

  const zh = getLang() === "cn";

  useEffect(() => {
    rsclawWs.connect();

    const unsub = rsclawWs.onComputerUseStatus(
      (ev: ComputerUseStatusPayload) => {
        if (ev.kind === "started") {
          // New run beginning. Cancel any pending fade-out from the
          // previous run, snapshot+shrink the window, and prime the
          // overlay with the run header.
          if (hideTimerRef.current) {
            clearTimeout(hideTimerRef.current);
            hideTimerRef.current = null;
          }
          setVisible(true);
          setAborting(false);
          setState({
            run_id: ev.run_id,
            agent_id: ev.agent_id,
            app: ev.app,
            instruction: ev.instruction,
            max_steps: ev.max_steps,
            step_index: 0,
            action: "",
            result_ok: null,
          });
          if (!snapshotRef.current) {
            void shrinkWindow().then((snap) => {
              snapshotRef.current = snap;
            });
          }
          // Open the desktop-wide native glow overlay. On success the
          // in-window CSS glow is suppressed (would otherwise double up
          // around the shrunken main window).
          if (!nativeGlowOpenRef.current) {
            void openNativeGlow().then((ok) => {
              if (ok) {
                nativeGlowOpenRef.current = true;
                setNativeGlow(true);
              }
            });
          }
          return;
        }

        if (ev.kind === "step") {
          setState((prev) => {
            if (!prev || prev.run_id !== ev.run_id) return prev;
            return {
              ...prev,
              step_index: ev.step_index,
              action: ev.action_summary || prev.action,
              result_ok: ev.result_ok,
            };
          });
          return;
        }

        if (ev.kind === "finished") {
          setState((prev) => {
            if (!prev || prev.run_id !== ev.run_id) return prev;
            return {
              ...prev,
              finished: {
                outcome_kind: ev.outcome_kind,
                summary: ev.summary,
                steps: ev.steps,
              },
            };
          });
          // Restore the window AND close the native glow as soon as
          // we get the finished event, in parallel — the desktop
          // "releasing" is itself a strong signal that the agent has
          // stopped. The pill lingers in-window for 1.8s afterward
          // with the outcome badge for readability.
          if (snapshotRef.current) {
            void restoreWindow(snapshotRef.current);
            snapshotRef.current = null;
          }
          if (nativeGlowOpenRef.current) {
            void closeNativeGlow();
            nativeGlowOpenRef.current = false;
            // Note: not flipping `nativeGlow` here. We keep the
            // in-window CSS glow suppressed for the full linger so the
            // tier-colored ring doesn't suddenly appear around the
            // shrunken window for 1.8s.
          }
          if (hideTimerRef.current) clearTimeout(hideTimerRef.current);
          hideTimerRef.current = setTimeout(() => {
            setVisible(false);
            // Drop the state on the next tick so the fade-out animation
            // has time to render with the final content.
            setTimeout(() => {
              setState(null);
              setNativeGlow(false);
            }, 320);
          }, HIDE_AFTER_FINISH_MS);
        }
      },
    );

    return () => {
      unsub();
      if (hideTimerRef.current) {
        clearTimeout(hideTimerRef.current);
        hideTimerRef.current = null;
      }
      // If the component unmounts mid-run, restore the window AND
      // close the native glow so the user isn't left with a tiny
      // panel + a permanent orange screen border.
      if (snapshotRef.current) {
        void restoreWindow(snapshotRef.current);
        snapshotRef.current = null;
      }
      if (nativeGlowOpenRef.current) {
        void closeNativeGlow();
        nativeGlowOpenRef.current = false;
      }
    };
  }, []);

  // Stop the currently active run. Best-effort POST to the gateway;
  // we don't inspect the response body — the real signal is the
  // `finished{user_abort}` event the backend emits next, which is
  // handled in the same WS subscriber as natural completion. The
  // contract also returns 200 `{aborted: false}` if the run already
  // completed; in that case the corresponding `finished` event has
  // already arrived (or is in-flight) so we still don't need to do
  // anything special here.
  const abortRun = useCallback(async () => {
    if (!state || state.finished || aborting) return;
    setAborting(true);
    try {
      await gatewayFetch(
        `/api/v1/computer-use/runs/${encodeURIComponent(state.run_id)}/abort`,
        {
          method: "POST",
          signal: AbortSignal.timeout(3000),
        },
      );
    } catch {
      // Network / gateway down. Drop the aborting state so the user
      // can retry. The run is still going; nothing else changes.
      setAborting(false);
    }
  }, [state, aborting]);

  // Esc shortcut while the overlay is mounted and a run is active.
  useEffect(() => {
    if (!state || state.finished) return;
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") {
        e.preventDefault();
        void abortRun();
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [state, abortRun]);

  if (!state) return null;

  const finished = state.finished;
  const stepsShown = finished ? finished.steps : state.step_index;
  const tier: Tier = finished
    ? tierForOutcome(finished.outcome_kind)
    : "running";

  const title = zh
    ? "RsClaw 正在控制你的电脑"
    : "RsClaw is controlling your computer";

  const tierClass = (() => {
    switch (tier) {
      case "ok":
        return styles.outcomeOk;
      case "attention":
        return styles.outcomeAttention;
      case "bad":
        return styles.outcomeBad;
      default:
        return "";
    }
  })();

  return (
    <div
      className={styles.overlay}
      data-visible={visible ? "true" : "false"}
      data-status={tier}
      aria-live="polite"
      aria-atomic="true"
    >
      {/* In-window CSS glow as fallback when the native desktop-wide
          overlay isn't available (browser dev mode, invoke failure).
          Suppressed when the native overlay is owning the visuals so
          the two don't double up. */}
      {!nativeGlow && <div className={styles.glow} aria-hidden="true" />}
      <div className={styles.pill}>
        <div className={styles.iconWrap}>
          <RsClawIcon />
          <span className={styles.dot} aria-hidden="true" />
        </div>
        <div className={styles.body}>
          <div className={styles.title}>{title}</div>
          <div className={styles.meta}>
            <span className={styles.metaAgent}>{state.agent_id}</span>
            {state.app && (
              <>
                <span className={styles.metaSep}>·</span>
                <span className={styles.metaApp}>{state.app}</span>
              </>
            )}
            <span className={styles.metaSep}>·</span>
            <span className={styles.metaSteps}>
              {zh ? "步骤" : "Step"} {stepsShown}/{state.max_steps}
            </span>
          </div>
          <div className={styles.action}>
            {finished
              ? finished.summary || outcomeLabel(finished.outcome_kind, zh)
              : aborting
                ? zh ? "正在中止…" : "Aborting…"
                : state.action ||
                  (zh ? "等待第一个动作…" : "Waiting for first action…")}
          </div>
        </div>
        {finished ? (
          <div className={`${styles.outcome} ${tierClass}`}>
            {outcomeLabel(finished.outcome_kind, zh)}
          </div>
        ) : (
          <button
            type="button"
            className={styles.stopBtn}
            onClick={() => void abortRun()}
            disabled={aborting}
            title={zh ? "停止 (Esc)" : "Stop (Esc)"}
            aria-label={zh ? "停止当前运行" : "Stop current run"}
          >
            <svg
              width="14"
              height="14"
              viewBox="0 0 14 14"
              fill="none"
              aria-hidden="true"
            >
              <rect
                x="3.5"
                y="3.5"
                width="7"
                height="7"
                rx="1.5"
                fill="currentColor"
              />
            </svg>
            <span>{zh ? "停止" : "Stop"}</span>
          </button>
        )}
      </div>
    </div>
  );
}
