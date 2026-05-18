/**
 * Computer-use control panel.
 *
 * Three sections (top to bottom):
 *   1. Bypass switch — runtime toggle for `bypass_all`. Flipping it
 *      mutates the gateway's `RedbPermissionStore` AtomicBool; the
 *      change is NOT persisted to rsclaw.json5, so a gateway restart
 *      reloads `tools.computerUse.bypassAll`.
 *   2. Live status — subscribes to `computer_use_status` WS frames and
 *      shows the most recent run (Started → Step* → Finished). Hides
 *      a few seconds after the run terminates so the panel is empty
 *      when nothing is happening.
 *   3. Saved "Always allow" grants — fetches `GET /computer-use/
 *      permissions`, displays one row per grant, exposes a Revoke
 *      button that calls `DELETE /computer-use/permissions/:agent/:app`.
 *
 * The companion permission dialog (`computer-use-permission.tsx`)
 * handles the actual consent flow at run time; this page is the
 * settings/admin surface.
 */

import { useNavigate } from "react-router-dom";
import { useCallback, useEffect, useState } from "react";

import { ErrorBoundary } from "./error";
import { IconButton } from "./button";
import { Path } from "../constant";
import { getLang } from "../locales";
import { rsclawWs, type ComputerUseStatusPayload } from "../lib/rsclaw-ws";
import { showConfirm, showToast } from "./ui-lib";

import ReturnIcon from "../icons/return.svg";
import ReloadIcon from "../icons/reload.svg";
import DeleteIcon from "../icons/delete.svg";

import styles from "./computer-use-control.module.scss";

const GATEWAY_BASE = "http://localhost:18888/api/v1";

type SavedGrant = {
  agent_id: string;
  app: string;
  decision: "allow_once" | "allow_session" | "allow_always" | "deny";
  granted_at: number;
};

type LiveRun = {
  run_id: string;
  agent_id: string;
  app: string;
  instruction: string;
  max_steps: number;
  started_at_ms: number;
  steps: ComputerUseStatusPayload[];
  finished?: {
    outcome_kind: string;
    summary: string;
    steps: number;
    finished_at_ms: number;
  };
};

const HIDE_AFTER_FINISH_MS = 8000;

function decisionLabel(decision: SavedGrant["decision"], zh: boolean): string {
  if (decision === "allow_always") return zh ? "永久允许" : "Allow always";
  if (decision === "allow_session") return zh ? "本次会话" : "Session";
  if (decision === "allow_once") return zh ? "仅本次" : "Once";
  return zh ? "拒绝" : "Deny";
}

function outcomeLabel(kind: string, zh: boolean): string {
  if (zh) {
    return (
      {
        finished: "已完成",
        call_user: "需要用户输入",
        max_loop: "已达上限",
        user_abort: "用户中止",
        permission_denied: "已拒绝授权",
        operator_error: "执行出错",
      } as Record<string, string>
    )[kind] || kind;
  }
  return (
    {
      finished: "Completed",
      call_user: "Calling user",
      max_loop: "Hit max steps",
      user_abort: "User aborted",
      permission_denied: "Permission denied",
      operator_error: "Operator error",
    } as Record<string, string>
  )[kind] || kind;
}

function formatTime(ts: number): string {
  return new Date(ts * 1000).toLocaleString();
}

export function ComputerUseControlPage() {
  const navigate = useNavigate();
  const zh = getLang() === "cn";

  const [bypass, setBypass] = useState<boolean | null>(null);
  const [bypassPending, setBypassPending] = useState(false);
  const [grants, setGrants] = useState<SavedGrant[]>([]);
  const [grantsLoading, setGrantsLoading] = useState(true);
  const [run, setRun] = useState<LiveRun | null>(null);

  // 1. Bypass: read current value on mount.
  const fetchBypass = useCallback(async () => {
    try {
      const res = await fetch(`${GATEWAY_BASE}/computer-use/bypass`, {
        signal: AbortSignal.timeout(3000),
      });
      if (!res.ok) throw new Error(`status ${res.status}`);
      const data = await res.json();
      setBypass(Boolean(data.enabled));
    } catch {
      setBypass(null);
    }
  }, []);

  // 2. Saved grants: read list on mount + on Reload click.
  const fetchGrants = useCallback(async () => {
    setGrantsLoading(true);
    try {
      const res = await fetch(`${GATEWAY_BASE}/computer-use/permissions`, {
        signal: AbortSignal.timeout(5000),
      });
      if (!res.ok) throw new Error(`status ${res.status}`);
      const data = await res.json();
      setGrants(Array.isArray(data.grants) ? data.grants : []);
    } catch {
      setGrants([]);
    } finally {
      setGrantsLoading(false);
    }
  }, []);

  useEffect(() => {
    fetchBypass();
    fetchGrants();
  }, [fetchBypass, fetchGrants]);

  // 3. Live status: subscribe to WS events.
  useEffect(() => {
    rsclawWs.connect();
    let hideTimer: ReturnType<typeof setTimeout> | null = null;
    const unsub = rsclawWs.onComputerUseStatus((ev) => {
      if (ev.kind === "started") {
        if (hideTimer) {
          clearTimeout(hideTimer);
          hideTimer = null;
        }
        setRun({
          run_id: ev.run_id,
          agent_id: ev.agent_id,
          app: ev.app,
          instruction: ev.instruction,
          max_steps: ev.max_steps,
          started_at_ms: Date.now(),
          steps: [],
        });
      } else if (ev.kind === "step") {
        setRun((prev) => {
          if (!prev || prev.run_id !== ev.run_id) return prev;
          // Cap retained steps so a long-running task doesn't grow the
          // panel unboundedly. The status frames are best-effort, so a
          // truncated tail is fine for the UI.
          const next = [...prev.steps, ev].slice(-30);
          return { ...prev, steps: next };
        });
      } else if (ev.kind === "finished") {
        setRun((prev) => {
          if (!prev || prev.run_id !== ev.run_id) return prev;
          return {
            ...prev,
            finished: {
              outcome_kind: ev.outcome_kind,
              summary: ev.summary,
              steps: ev.steps,
              finished_at_ms: Date.now(),
            },
          };
        });
        // If a saved-grant was minted by this run (Allow always),
        // refresh the list so the new entry appears.
        void fetchGrants();
        if (hideTimer) clearTimeout(hideTimer);
        hideTimer = setTimeout(() => setRun(null), HIDE_AFTER_FINISH_MS);
      }
    });
    return () => {
      unsub();
      if (hideTimer) clearTimeout(hideTimer);
    };
  }, [fetchGrants]);

  const onToggleBypass = useCallback(async () => {
    if (bypass === null || bypassPending) return;
    const target = !bypass;
    if (target) {
      const ok = await showConfirm(
        zh
          ? "打开后所有 GUI 自动化都将自动放行，没有任何弹窗确认。仅在你完全信任当前任务时使用。"
          : "Once enabled, every GUI automation request will be auto-approved with no confirmation. Only enable if you fully trust the current task.",
      );
      if (!ok) return;
    }
    setBypassPending(true);
    try {
      const res = await fetch(`${GATEWAY_BASE}/computer-use/bypass`, {
        method: "PUT",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ enabled: target }),
      });
      if (!res.ok) throw new Error(`status ${res.status}`);
      const data = await res.json();
      setBypass(Boolean(data.enabled));
      showToast(
        target
          ? zh ? "已开启 Bypass" : "Bypass enabled"
          : zh ? "已关闭 Bypass" : "Bypass disabled",
      );
    } catch {
      showToast(zh ? "切换失败" : "Failed to toggle");
    } finally {
      setBypassPending(false);
    }
  }, [bypass, bypassPending, zh]);

  const onRevokeGrant = useCallback(
    async (grant: SavedGrant) => {
      const ok = await showConfirm(
        zh
          ? `撤销 ${grant.agent_id} 对 ${grant.app || "桌面"} 的永久授权？下次该代理控制此应用时会重新弹窗确认。`
          : `Revoke ${grant.agent_id}'s saved permission for ${grant.app || "desktop"}? The next request will prompt again.`,
      );
      if (!ok) return;
      try {
        const url = `${GATEWAY_BASE}/computer-use/permissions/${encodeURIComponent(
          grant.agent_id,
        )}/${encodeURIComponent(grant.app || "")}`;
        const res = await fetch(url, { method: "DELETE" });
        if (!res.ok) throw new Error(`status ${res.status}`);
        showToast(zh ? "已撤销" : "Revoked");
        fetchGrants();
      } catch {
        showToast(zh ? "撤销失败" : "Revoke failed");
      }
    },
    [fetchGrants, zh],
  );

  const subTitle = zh
    ? "GUI 代理 (computer_use) 的权限与活动监控"
    : "Permissions and live activity for the GUI agent (computer_use)";

  return (
    <ErrorBoundary>
      <div className={styles.page}>
        <div className="window-header" data-tauri-drag-region>
          <div className="window-header-title">
            <div className="window-header-main-title">
              {zh ? "Computer Use 控制台" : "Computer Use"}
            </div>
            <div className="window-header-sub-title">{subTitle}</div>
          </div>
          <div className="window-actions">
            <div className="window-action-button">
              <IconButton
                icon={<ReloadIcon />}
                bordered
                onClick={() => {
                  fetchBypass();
                  fetchGrants();
                }}
              />
            </div>
            <div className="window-action-button">
              <IconButton
                icon={<ReturnIcon />}
                bordered
                onClick={() => navigate(Path.Home)}
              />
            </div>
          </div>
        </div>

        <div className={styles.body}>
          {/* ---- Bypass ---- */}
          <div className={styles.card}>
            <div className={styles.cardHeader}>
              <div className={styles.cardTitle}>
                {zh ? "Bypass 总开关" : "Bypass"}
              </div>
              <div className={styles.cardHint}>
                {zh
                  ? "运行时开关。重启后会被 tools.computerUse.bypassAll 配置覆盖。"
                  : "Runtime toggle. Restart resets to tools.computerUse.bypassAll."}
              </div>
            </div>
            <div className={styles.bypassRow}>
              <div className={styles.bypassLabel}>
                {bypass === null
                  ? zh ? "未连接到网关" : "Gateway not reachable"
                  : bypass
                    ? zh ? "已开启 — 所有动作自动放行" : "Enabled — all actions auto-approved"
                    : zh ? "已关闭 — 所有动作走权限弹窗" : "Disabled — every action goes through the consent dialog"}
              </div>
              <button
                type="button"
                className={
                  bypass
                    ? `${styles.toggleBtn} ${styles.toggleOn}`
                    : styles.toggleBtn
                }
                disabled={bypass === null || bypassPending}
                onClick={onToggleBypass}
                aria-pressed={bypass ?? false}
              >
                <span className={styles.toggleKnob} />
              </button>
            </div>
            {bypass === true && (
              <div className={styles.warnBanner}>
                {zh
                  ? "警告：在 Bypass 模式下，任何代理都可以无确认地控制你的电脑。"
                  : "Warning: in Bypass mode, any agent can drive your computer without confirmation."}
              </div>
            )}
          </div>

          {/* ---- Live status ---- */}
          {run && (
            <div className={styles.card}>
              <div className={styles.cardHeader}>
                <div className={styles.cardTitle}>
                  <span className={styles.liveDot} aria-hidden="true" />
                  {zh ? "实时活动" : "Live activity"}
                </div>
                <div className={styles.cardHint}>
                  {run.agent_id}
                  {" · "}
                  {run.app || (zh ? "桌面" : "desktop")}
                </div>
              </div>
              <div className={styles.runMeta}>
                <div className={styles.runInstr}>{run.instruction}</div>
                <div className={styles.runProgress}>
                  {zh ? "步骤" : "Step"}{" "}
                  {run.finished
                    ? run.finished.steps
                    : run.steps.length}
                  {" / "}
                  {run.max_steps}
                  {run.finished && (
                    <span
                      className={
                        run.finished.outcome_kind === "finished"
                          ? styles.outcomeOk
                          : styles.outcomeBad
                      }
                    >
                      {" · "}
                      {outcomeLabel(run.finished.outcome_kind, zh)}
                    </span>
                  )}
                </div>
              </div>
              <div className={styles.stepList}>
                {run.steps.length === 0 && !run.finished && (
                  <div className={styles.stepEmpty}>
                    {zh ? "等待第一个动作…" : "Waiting for first action…"}
                  </div>
                )}
                {run.steps.map((ev, i) =>
                  ev.kind === "step" ? (
                    <div
                      key={`${ev.run_id}-${ev.step_index}-${i}`}
                      className={
                        ev.result_ok
                          ? styles.stepRow
                          : `${styles.stepRow} ${styles.stepRowBad}`
                      }
                    >
                      <div className={styles.stepIdx}>{ev.step_index}</div>
                      <div className={styles.stepBody}>
                        <div className={styles.stepAction}>{ev.action_summary}</div>
                        {ev.result_message && (
                          <div className={styles.stepMsg}>{ev.result_message}</div>
                        )}
                      </div>
                    </div>
                  ) : null,
                )}
                {run.finished && run.finished.summary && (
                  <div className={styles.summaryRow}>{run.finished.summary}</div>
                )}
              </div>
            </div>
          )}

          {/* ---- Saved grants ---- */}
          <div className={styles.card}>
            <div className={styles.cardHeader}>
              <div className={styles.cardTitle}>
                {zh ? "永久授权列表" : "Saved permissions"}
              </div>
              <div className={styles.cardHint}>
                {zh
                  ? "用户在权限弹窗中选了 「永久允许」 的代理 / 应用对。"
                  : "Agent / app pairs the user marked “Always allow”."}
              </div>
            </div>
            {grantsLoading ? (
              <div className={styles.empty}>{zh ? "加载中…" : "Loading…"}</div>
            ) : grants.length === 0 ? (
              <div className={styles.empty}>
                {zh ? "还没有永久授权。" : "No saved permissions yet."}
              </div>
            ) : (
              <div className={styles.grantList}>
                {grants.map((g) => (
                  <div
                    key={`${g.agent_id}\0${g.app}`}
                    className={styles.grantRow}
                  >
                    <div className={styles.grantCell}>
                      <div className={styles.grantPrimary}>
                        {g.app || (zh ? "(任意应用)" : "(any app)")}
                      </div>
                      <div className={styles.grantSecondary}>
                        {zh ? "代理" : "Agent"}: {g.agent_id}
                      </div>
                    </div>
                    <div className={styles.grantCell}>
                      <div className={styles.grantPrimary}>
                        {decisionLabel(g.decision, zh)}
                      </div>
                      <div className={styles.grantSecondary}>
                        {formatTime(g.granted_at)}
                      </div>
                    </div>
                    <div className={styles.grantActions}>
                      <IconButton
                        icon={<DeleteIcon />}
                        text={zh ? "撤销" : "Revoke"}
                        bordered
                        onClick={() => onRevokeGrant(g)}
                      />
                    </div>
                  </div>
                ))}
              </div>
            )}
          </div>
        </div>
      </div>
    </ErrorBoundary>
  );
}
