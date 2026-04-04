import { IconButton } from "./button";
import { ErrorBoundary } from "./error";
import styles from "./gateway-control.module.scss";
import ReturnIcon from "../icons/return.svg";
import ReloadIcon from "../icons/reload.svg";
import PlayIcon from "../icons/play.svg";
import StopIcon from "../icons/pause.svg";
import PowerIcon from "../icons/power.svg";
import { useNavigate } from "react-router-dom";
import { useEffect, useState, useCallback } from "react";
import { Path } from "../constant";
import Locale from "../locales";

const GATEWAY_BASE = "http://localhost:18888/api/v1";

interface AgentInfo {
  id: string;
  model: string;
  status: string;
  toolset?: string[];
  channels?: string[];
}

interface GatewayStatus {
  running: boolean;
  version?: string;
  port?: number;
  uptime?: string;
  agents?: AgentInfo[];
}

export function GatewayControlPage() {
  const navigate = useNavigate();
  const [status, setStatus] = useState<GatewayStatus>({
    running: false,
  });
  const [loading, setLoading] = useState(true);

  const fetchStatus = useCallback(async () => {
    try {
      const healthRes = await fetch(`${GATEWAY_BASE}/health`, {
        signal: AbortSignal.timeout(3000),
      });
      if (!healthRes.ok) throw new Error("Health check failed");
      const healthData = await healthRes.json();

      let agents: AgentInfo[] = [];
      try {
        const statusRes = await fetch(`${GATEWAY_BASE}/status`, {
          signal: AbortSignal.timeout(3000),
        });
        if (statusRes.ok) {
          const statusData = await statusRes.json();
          agents = statusData.agents || [];
        }
      } catch {
        // status endpoint may not be available
      }

      setStatus({
        running: true,
        version: healthData.version || "unknown",
        port: healthData.port || 18888,
        uptime: healthData.uptime || "",
        agents,
      });
    } catch {
      setStatus({ running: false });
    } finally {
      setLoading(false);
    }
  }, []);

  useEffect(() => {
    fetchStatus();
    const interval = setInterval(fetchStatus, 10000);
    return () => clearInterval(interval);
  }, [fetchStatus]);

  const handleAction = (action: string) => {
    const instructions: Record<string, string> = {
      start: "Run: rsclaw start --dev",
      stop: "Run: rsclaw stop",
      restart: "Run: rsclaw restart --dev",
    };
    // In Tauri, this would invoke a system command.
    // For now, show instructions.
    alert(instructions[action] || `Action: ${action}`);
  };

  return (
    <ErrorBoundary>
      <div className={styles["gateway-control-page"]}>
        <div className="window-header" data-tauri-drag-region>
          <div className="window-header-title">
            <div className="window-header-main-title">
              {Locale.GatewayControl.Title}
            </div>
            <div className="window-header-sub-title">
              {Locale.GatewayControl.SubTitle}
            </div>
          </div>
          <div className="window-actions">
            <div className="window-action-button">
              <IconButton
                icon={<ReloadIcon />}
                bordered
                onClick={fetchStatus}
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

        <div className={styles["gateway-control-page-body"]}>
          <div className={styles["status-banner"]}>
            <div className={styles["status-left"]}>
              <div
                className={`${styles["status-indicator"]} ${
                  status.running ? styles.running : styles.stopped
                }`}
              />
              <div className={styles["status-text"]}>
                <div className={styles["status-label"]}>
                  {loading
                    ? Locale.GatewayControl.Checking
                    : status.running
                      ? Locale.GatewayControl.Running
                      : Locale.GatewayControl.Stopped}
                </div>
                <div className={styles["status-detail"]}>
                  {status.running
                    ? `Port ${status.port || 18888} | Uptime: ${status.uptime || "N/A"}`
                    : Locale.GatewayControl.NotResponding}
                </div>
              </div>
            </div>
            <div className={styles["status-actions"]}>
              {!status.running && (
                <IconButton
                  icon={<PlayIcon />}
                  text={Locale.GatewayControl.Start}
                  bordered
                  onClick={() => handleAction("start")}
                />
              )}
              {status.running && (
                <>
                  <IconButton
                    icon={<ReloadIcon />}
                    text={Locale.GatewayControl.Restart}
                    bordered
                    onClick={() => handleAction("restart")}
                  />
                  <IconButton
                    icon={<StopIcon />}
                    text={Locale.GatewayControl.Stop}
                    bordered
                    onClick={() => handleAction("stop")}
                  />
                </>
              )}
            </div>
          </div>

          <div className={styles["info-grid"]}>
            <div className={styles["info-card"]}>
              <div className={styles["info-label"]}>{Locale.GatewayControl.Version}</div>
              <div className={styles["info-value"]}>
                {status.version || "--"}
              </div>
            </div>
            <div className={styles["info-card"]}>
              <div className={styles["info-label"]}>{Locale.GatewayControl.Port}</div>
              <div className={styles["info-value"]}>
                {status.port || "--"}
              </div>
            </div>
            <div className={styles["info-card"]}>
              <div className={styles["info-label"]}>{Locale.GatewayControl.Agents}</div>
              <div className={styles["info-value"]}>
                {status.agents?.length ?? "--"}
              </div>
            </div>
            <div className={styles["info-card"]}>
              <div className={styles["info-label"]}>{Locale.GatewayControl.StatusLabel}</div>
              <div className={styles["info-value"]}>
                {status.running ? Locale.GatewayControl.Online : Locale.GatewayControl.Offline}
              </div>
            </div>
          </div>

          <div className={styles["section-title"]}>{Locale.GatewayControl.ActiveAgents}</div>
          {status.agents && status.agents.length > 0 ? (
            <div className={styles["agent-list"]}>
              {status.agents.map((agent) => (
                <div key={agent.id} className={styles["agent-item"]}>
                  <div className={styles["agent-info"]}>
                    <div className={styles["agent-name"]}>{agent.id}</div>
                    <div className={styles["agent-detail"]}>
                      Model: {agent.model}
                      {agent.toolset && agent.toolset.length > 0
                        ? ` | Tools: ${agent.toolset.join(", ")}`
                        : ""}
                      {agent.channels && agent.channels.length > 0
                        ? ` | Channels: ${agent.channels.join(", ")}`
                        : ""}
                    </div>
                  </div>
                  <div
                    className={`${styles["agent-status"]} ${
                      styles[agent.status] || styles.idle
                    }`}
                  >
                    {agent.status}
                  </div>
                </div>
              ))}
            </div>
          ) : (
            <div className={styles["empty-state"]}>
              {status.running
                ? Locale.GatewayControl.NoAgents
                : Locale.GatewayControl.StartToSee}
            </div>
          )}
        </div>
      </div>
    </ErrorBoundary>
  );
}
