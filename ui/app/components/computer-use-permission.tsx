/**
 * Computer-use permission dialog.
 *
 * Mounted at the top level (sidebar.tsx) so it overlays whatever screen
 * the user is on when an agent fires a `permission_request` event. The
 * subscription lives in `useComputerUsePermission`; this component is
 * just the visual layer.
 *
 * Visual style: red-accented card on a dimmed full-screen mask. The
 * border / title use the same destructive palette as Claude Code's
 * "Bypass permissions" toggle so security-significant prompts read as
 * security prompts at a glance.
 */

import { useEffect } from "react";

import { getLang } from "../locales";
import { useComputerUsePermission } from "../hooks/useComputerUsePermission";

import styles from "./computer-use-permission.module.scss";

export function ComputerUsePermissionDialog() {
  const { pending, respond } = useComputerUsePermission();

  // Esc dismisses as "Deny" — same security-conservative default as
  // Claude Code: if the user is unsure, we don't run.
  useEffect(() => {
    if (!pending) return;
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") {
        void respond("deny");
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [pending, respond]);

  if (!pending) return null;

  const zh = getLang() === "cn";
  const appLabel = pending.app && pending.app.length > 0
    ? pending.app
    : (zh ? "你的桌面" : "your desktop");

  const title = zh
    ? "RsClaw 即将控制你的电脑"
    : "RsClaw is about to control your computer";

  const subtitle = zh
    ? "请确认本次自动化操作的范围。"
    : "Please confirm the scope of this automation.";

  return (
    <div
      className={styles.mask}
      role="dialog"
      aria-modal="true"
      aria-labelledby="cu-perm-title"
    >
      <div className={styles.card}>
        <div className={styles.title} id="cu-perm-title">
          <span className={styles.titleDot} aria-hidden="true" />
          <span>{title}</span>
        </div>
        <div className={styles.subtitle}>{subtitle}</div>

        <div className={styles.detailGrid}>
          <div className={styles.detailLabel}>{zh ? "应用" : "App"}</div>
          <div className={styles.detailValue}>{appLabel}</div>

          <div className={styles.detailLabel}>{zh ? "任务" : "Task"}</div>
          <div className={styles.detailValue}>{pending.reason}</div>

          <div className={styles.detailLabel}>{zh ? "最多步数" : "Max steps"}</div>
          <div className={styles.detailValue}>{pending.estimated_steps}</div>

          <div className={styles.detailLabel}>{zh ? "代理" : "Agent"}</div>
          <div className={styles.detailValue}>{pending.agent_id}</div>
        </div>

        <div className={styles.actions}>
          <button
            type="button"
            className={styles.btnDanger}
            onClick={() => void respond("deny")}
          >
            {zh ? "拒绝" : "Deny"}
          </button>
          <button
            type="button"
            className={styles.btn}
            onClick={() => void respond("allow_once")}
            autoFocus
          >
            {zh ? "仅本次" : "Allow once"}
          </button>
          <button
            type="button"
            className={styles.btn}
            onClick={() => void respond("allow_session")}
          >
            {zh ? "本次会话" : "Allow this session"}
          </button>
          <button
            type="button"
            className={styles.btn}
            onClick={() => void respond("allow_always")}
          >
            {pending.app
              ? (zh ? `永久允许 ${pending.app}` : `Always allow ${pending.app}`)
              : (zh ? "永久允许" : "Always allow")}
          </button>
        </div>
      </div>
    </div>
  );
}
